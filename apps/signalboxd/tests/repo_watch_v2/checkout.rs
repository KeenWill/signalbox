use super::*;
use signalbox_application::{InProcessEligibilityWorkSource, InProcessToolDispatchGate};
use signalbox_persistence::scheduler::PostgresEligibilitySweep;
use signalbox_persistence::test_support::postgres::TestDatabase;
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
    supervisor: signalbox_tools_exec::TokioProcessRunner,
    redirect: Option<String>,
    steps: Arc<Mutex<Vec<String>>>,
    push_authorizations: Arc<Mutex<Vec<String>>>,
}

impl ProcessRunner for LocalGitRunner {
    fn sandbox_launcher_program(&self) -> &Path {
        self.supervisor.sandbox_launcher_program()
    }
    fn sandbox_launcher_descriptor(&self) -> Option<i32> {
        self.supervisor.sandbox_launcher_descriptor()
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
                .all(|argument| !argument.to_string_lossy().contains(TOKEN)
                    && !argument.to_string_lossy().contains("push-fixture-token"))
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
        let pushing = request.arguments[0] == "push";
        let push_transport = pushing || request.arguments[0] == "ls-remote";
        for argument in &mut request.arguments {
            if argument == "https://github.com/checkout/project.git"
                || argument == "https://github.com/contributor/project.git"
            {
                if push_transport {
                    let resolved = tokio::process::Command::new("git")
                        .args(["ls-remote", "--get-url"])
                        .arg(&argument)
                        .current_dir(&request.working_directory)
                        .env_clear()
                        .envs(&request.environment)
                        .output()
                        .await
                        .expect("resolve authenticated push URL");
                    assert!(resolved.status.success());
                    let url = std::str::from_utf8(&resolved.stdout)
                        .expect("authenticated push URL is UTF-8")
                        .trim();
                    assert!(url.starts_with("https://x-access-token:"));
                    assert!(url.ends_with("@github.com/checkout/project.git"));
                    if pushing {
                        self.push_authorizations
                            .lock()
                            .expect("push authorizations")
                            .push(url.to_owned());
                    }
                } else {
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

/// Suspends the clone after it has begun writing into the published directory.
#[cfg(target_os = "linux")]
#[derive(Clone)]
struct PausingCloneRunner {
    started: Arc<tokio::sync::Notify>,
}

#[cfg(target_os = "linux")]
impl ProcessRunner for PausingCloneRunner {
    fn sandbox_launcher_program(&self) -> &Path {
        Path::new("/unused-checkout-fixture-launcher")
    }
    fn sandbox_launcher_descriptor(&self) -> Option<i32> {
        None
    }
    async fn bwrap_availability(&mut self, _: ProcessRequest) -> BwrapAvailability {
        BwrapAvailability::Missing
    }
    async fn run(&mut self, request: ProcessRequest) -> ProcessRunResult {
        assert_eq!(request.arguments[0], "clone");
        std::fs::create_dir_all(request.working_directory.join(".git/objects"))
            .expect("partial clone directory");
        std::fs::write(
            request.working_directory.join(".git/objects/partial"),
            b"partial clone",
        )
        .expect("partial clone data");
        self.started.notify_one();
        std::future::pending().await
    }
}

struct CheckoutFixture {
    _container: TestDatabase,
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

/// Holds provisioning after clone, before fetch and checkout finish the repository.
#[derive(Clone)]
struct CheckoutBarrierRunner {
    runner: LocalGitRunner,
    cloned: Arc<tokio::sync::Notify>,
    resume: Arc<tokio::sync::Notify>,
}

impl ProcessRunner for CheckoutBarrierRunner {
    fn sandbox_launcher_program(&self) -> &Path {
        self.runner.sandbox_launcher_program()
    }

    fn sandbox_launcher_descriptor(&self) -> Option<i32> {
        self.runner.sandbox_launcher_descriptor()
    }

    async fn bwrap_availability(&mut self, request: ProcessRequest) -> BwrapAvailability {
        self.runner.bwrap_availability(request).await
    }

    async fn run(&mut self, request: ProcessRequest) -> ProcessRunResult {
        let cloning = request.arguments[0] == "clone";
        let result = self.runner.run(request).await;
        if cloning {
            self.cloned.notify_one();
            self.resume.notified().await;
        }
        result
    }
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL"]
async fn kickoff_precedes_input_deferred_during_checkout() -> Result<(), Box<dyn Error>> {
    assert_input_during_checkout_waits(None).await
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL"]
async fn release_start_during_checkout_cannot_admit_input_ahead_of_kickoff()
-> Result<(), Box<dyn Error>> {
    assert_input_during_checkout_waits(Some(SessionLifecycleOperation::ReleaseStart)).await
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL"]
async fn release_ownership_during_checkout_cannot_admit_input_ahead_of_kickoff()
-> Result<(), Box<dyn Error>> {
    assert_input_during_checkout_waits(Some(SessionLifecycleOperation::Release)).await
}

async fn assert_input_during_checkout_waits(
    early_release: Option<SessionLifecycleOperation>,
) -> Result<(), Box<dyn Error>> {
    use signalbox_application::{
        EligibilityWorkSource, StartEligibleTurnOutcome, StartEligibleTurnService,
        SubmitInputOutcome, SubmitInputRequest, SubmitInputService,
        UuidV7StartEligibleTurnIdGenerator, UuidV7SubmitInputIdGenerator,
    };
    use signalbox_domain::{
        AcceptedInputTurnActivationIdentities, ContextFrontierId, DeliveryRequest,
        ModelSelectionOverride, PerInputConfigurationChoices, SemanticTranscriptEntryId,
        SessionConfigurationDefaultsVersion, SubmitInputAppliedResult, SubmitInputResult,
        TurnAttemptId, UserContent,
    };
    use signalbox_persistence::{
        start_eligible_turn::{CommitActivationPreviewOutcome, StartEligibleTurnRepository},
        submit_input::{SubmitInputRepository, SubmitInputRepositoryError},
    };

    // A reconciliation hint must not substitute for checkout completion's nudge.
    struct NoReconciliation;
    impl signalbox_application::EligibilitySweep for NoReconciliation {
        type Error = std::convert::Infallible;

        async fn find_sessions(
            &mut self,
        ) -> Result<signalbox_application::EligibilitySweepBatch, Self::Error> {
            Ok(signalbox_application::EligibilitySweepBatch::new(
                Vec::new(),
                false,
            ))
        }
    }

    let mut fixture = CheckoutFixture::new().await?;
    let (catalog, executor) = fixture.daemon_tools()?;
    let (nudge, mut work) = InProcessEligibilityWorkSource::new(NoReconciliation);
    fixture.sink.eligibility_nudge = nudge.clone();
    let mut submit = SubmitInputService::new(
        UuidV7SubmitInputIdGenerator,
        SubmitInputRepository::new(fixture.core.clone()),
        nudge,
        InProcessToolDispatchGate::default(),
    );
    let repository = StartEligibleTurnRepository::new(fixture.core.clone());
    let mut start =
        StartEligibleTurnService::new(UuidV7StartEligibleTurnIdGenerator, repository.clone());
    let identities = AcceptedInputTurnActivationIdentities::new(
        SemanticTranscriptEntryId::from_uuid(Uuid::now_v7()),
        SemanticTranscriptEntryId::from_uuid(Uuid::now_v7()),
        ContextFrontierId::from_uuid(Uuid::now_v7()),
        TurnAttemptId::from_uuid(Uuid::now_v7()),
    );
    let cloned = Arc::new(tokio::sync::Notify::new());
    let resume = Arc::new(tokio::sync::Notify::new());
    let runner = CheckoutBarrierRunner {
        runner: fixture.runner.clone(),
        cloned: cloned.clone(),
        resume: resume.clone(),
    };
    let configuration = fixture
        .sink
        .models
        .repository_watch()
        .expect("configuration")
        .clone();
    let provisioning =
        submit_pending_with_runner(&fixture.store, &configuration, &mut fixture.sink, runner);
    let while_cloned = async {
        cloned.notified().await;
        let session = SessionId::from_uuid(
            sqlx::query_scalar(
                "SELECT checkout_session_id FROM dispatch_ledger WHERE command_id = $1",
            )
            .bind(fixture.command.into_uuid())
            .fetch_one(&fixture.module)
            .await
            .expect("session created with provisioning hold"),
        );
        let inputs: i64 = sqlx::query_scalar("SELECT count(*) FROM accepted_input")
            .fetch_one(&fixture.core)
            .await
            .expect("input count during clone");
        assert_eq!(inputs, 0, "kickoff waits until checkout is recorded");
        let request = SubmitInputRequest::try_new(
            DurableCommandId::from_uuid(Uuid::now_v7()),
            session,
            UserContent::try_text("Inspect the checkout".to_owned()).expect("input text"),
            DeliveryRequest::StartWhenNoActiveTurn {
                configuration: PerInputConfigurationChoices::new(
                    SessionConfigurationDefaultsVersion::first(),
                    ModelSelectionOverride::ReplaceWith(ModelSelectionRequest::Direct(
                        DirectModelSelection::from_uuid(Uuid::now_v7()),
                    )),
                ),
            },
        )
        .expect("input request");
        assert!(matches!(
            submit.execute(request.clone()).await,
            Err(SubmitInputRepositoryError::CheckoutProvisioningPending)
        ));
        if let Some(operation) = early_release {
            use signalbox_domain::{CommandPrincipal, SessionLifecycleCommandResult};
            use signalbox_persistence::session_lifecycle_command::{
                SessionLifecycleCommandHandlingOutcome, SessionLifecycleCommandRepository,
            };
            let released = SessionLifecycleCommandRepository::new(fixture.core.clone())
                .handle(
                    SessionLifecycleCommand::new(
                        DurableCommandId::from_uuid(Uuid::now_v7()),
                        session,
                        operation,
                    ),
                    CommandPrincipal::Operator,
                )
                .await
                .expect("operator release");
            assert!(matches!(
                released,
                SessionLifecycleCommandHandlingOutcome::Recorded(
                    SessionLifecycleCommandResult::Applied(_)
                )
            ));
            let start_gate_held: bool = sqlx::query_scalar(
                "SELECT start_gate_held FROM session_lifecycle WHERE session_id = $1",
            )
            .bind(session.into_uuid())
            .fetch_one(&fixture.core)
            .await
            .expect("ordinary start gate");
            assert!(
                !start_gate_held,
                "the public command released the ordinary start gate"
            );
        }
        assert!(matches!(
            submit.execute(request.clone()).await,
            Err(SubmitInputRepositoryError::CheckoutProvisioningPending)
        ));
        assert!(
            SubmitInputRepository::new(fixture.core.clone())
                .load(request.command_id())
                .await
                .expect("deferred command lookup")
                .is_none(),
            "deferred client input must not claim its command identity"
        );
        assert_eq!(
            start.execute(session).await.expect("eligibility pass"),
            StartEligibleTurnOutcome::NoEligibleTurn
        );
        assert!(
            repository
                .preview(session, identities)
                .await
                .expect("activation preview")
                .is_none()
        );
        resume.notify_one();
        (session, request)
    };
    let (provisioned, (session, request)) = tokio::join!(provisioning, while_cloned);
    provisioned.expect("checkout completes");
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(10), work.next()).await??,
        session,
        "checkout completion wakes queued input"
    );
    let operator_command = request.command_id();
    let SubmitInputOutcome::Recorded(SubmitInputResult::Applied(
        SubmitInputAppliedResult::TurnOrigin(operator_origin),
    )) = submit.execute(request).await?
    else {
        panic!("the deferred command is accepted after kickoff");
    };
    let kickoff: Uuid =
        sqlx::query_scalar("SELECT kickoff_command_id FROM dispatch_ledger WHERE command_id = $1")
            .bind(fixture.command.into_uuid())
            .fetch_one(&fixture.module)
            .await?;
    let recorded = SubmitInputRepository::new(fixture.core.clone())
        .load(DurableCommandId::from_uuid(kickoff))
        .await?
        .expect("kickoff recorded");
    let SubmitInputResult::Applied(SubmitInputAppliedResult::TurnOrigin(kickoff_origin)) =
        recorded.result()
    else {
        panic!("kickoff owns the first queued turn");
    };
    let queued: Vec<Uuid> = sqlx::query_scalar("SELECT accepting_command_id FROM accepted_input WHERE session_id = $1 ORDER BY acceptance_position")
        .bind(session.into_uuid()).fetch_all(&fixture.core).await?;
    assert_eq!(queued, [kickoff, operator_command.into_uuid()]);
    let preview = repository
        .preview(session, identities)
        .await?
        .expect("provisioned input eligible");
    let CommitActivationPreviewOutcome::Activated(activated) =
        repository.commit_preview(preview).await?
    else {
        panic!("first turn activates after provisioning");
    };
    assert_eq!(activated.turn(), kickoff_origin.turn());
    assert_ne!(activated.turn(), operator_origin.turn());
    let status = run_git_tool(
        &catalog,
        &executor,
        session,
        activated.turn(),
        "git_status",
        "{}",
    )
    .await;
    assert_eq!(status["branch"], "review");
    assert_eq!(status["head"], fixture.head.as_str());
    Ok(())
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL"]
async fn kickoff_replays_after_provisioning_and_after_input_commit() -> Result<(), Box<dyn Error>> {
    use signalbox_domain::{DeliveryRequest, SubmitInputResult};
    use signalbox_persistence::submit_input::SubmitInputRepository;

    let mut fixture =
        CheckoutFixture::with_rule("checkout/project", "labeled-review-response").await?;
    // Fail only kickoff, after creation and checkout have committed.
    sqlx::raw_sql("CREATE FUNCTION reject_kickoff() RETURNS trigger LANGUAGE plpgsql AS $$
        BEGIN IF NEW.command_kind = 'submit_input' THEN RAISE EXCEPTION 'fixture kickoff interruption'; END IF;
        RETURN NEW; END $$;
        CREATE TRIGGER reject_kickoff BEFORE INSERT ON durable_command FOR EACH ROW EXECUTE FUNCTION reject_kickoff();")
        .execute(&fixture.core).await?;
    let configuration = fixture
        .sink
        .models
        .repository_watch()
        .expect("configuration")
        .clone();
    assert!(
        submit_pending_with_runner(
            &fixture.store,
            &configuration,
            &mut fixture.sink,
            fixture.runner.clone()
        )
        .await
        .is_err()
    );
    let (kickoff, head, pending): (Uuid, String, bool) = sqlx::query_as(
        "SELECT kickoff_command_id, checkout_head_sha, submission_pending FROM dispatch_ledger WHERE command_id = $1",
    ).bind(fixture.command.into_uuid()).fetch_one(&fixture.module).await?;
    assert_eq!(head, fixture.head.as_str());
    assert!(pending);
    let repository = SubmitInputRepository::new(fixture.core.clone());
    assert!(
        repository
            .load(DurableCommandId::from_uuid(kickoff))
            .await?
            .is_none()
    );
    sqlx::raw_sql(
        "DROP TRIGGER reject_kickoff ON durable_command; DROP FUNCTION reject_kickoff();",
    )
    .execute(&fixture.core)
    .await?;
    // Replay retains the publication instruction even when push becomes available.
    fixture.enable_push()?;
    // A later observation cannot rewrite dispatch-time instructions during replay.
    sqlx::query("UPDATE repository_state SET comparison_baseline = jsonb_set(comparison_baseline, '{pull_requests}', '[]')")
        .execute(&fixture.module).await?;
    fixture.store = RepoWatchStore::new(fixture.module.clone());
    fixture.dispatch().await;
    let recorded = repository
        .load(DurableCommandId::from_uuid(kickoff))
        .await?
        .expect("kickoff committed");
    assert!(matches!(recorded.result(), SubmitInputResult::Applied(_)));
    assert!(matches!(
        recorded.command().delivery(),
        DeliveryRequest::StartWhenNoActiveTurn { .. }
    ));
    let text = recorded
        .command()
        .content()
        .single_text()
        .expect("kickoff text")
        .as_str();
    assert!(text.contains("This session has no Git push authority."));
    assert!(text.contains("No unresolved review threads were present at dispatch time."));
    assert!(text.contains("one-turn convergence check of mergeability and gating checks"));
    assert!(text.contains("Post a plain reply on the pull request"));
    assert!(text.contains("Target pull request: checkout/project#1"));
    assert!(text.contains("Title: Review the retained head"));
    assert!(text.contains("Head branch: review"));
    assert!(text.contains(fixture.head.as_str()));
    assert!(text.contains("Base branch: main"));
    let issuer: (String, String) = sqlx::query_as(
        "SELECT issuer_kind, issuer_module FROM durable_command WHERE command_id = $1",
    )
    .bind(kickoff)
    .fetch_one(&fixture.core)
    .await?;
    assert_eq!(issuer, ("module".to_owned(), "repo_watch".to_owned()));
    // Recreate the restart window after core acceptance but before module acknowledgement.
    sqlx::query("UPDATE dispatch_ledger SET submission_pending = true WHERE command_id = $1")
        .bind(fixture.command.into_uuid())
        .execute(&fixture.module)
        .await?;
    fixture.store = RepoWatchStore::new(fixture.module.clone());
    fixture.dispatch().await;
    let replay = repository
        .load(DurableCommandId::from_uuid(kickoff))
        .await?
        .expect("same kickoff");
    assert_eq!(
        replay.command().command_id(),
        recorded.command().command_id()
    );
    assert_eq!(replay.command(), recorded.command());
    assert_eq!(replay.result(), recorded.result());
    let inputs: i64 = sqlx::query_scalar("SELECT count(*) FROM accepted_input")
        .fetch_one(&fixture.core)
        .await?;
    assert_eq!(inputs, 1);
    assert_eq!(
        *fixture.runner.steps.lock().expect("Git steps"),
        ["clone", "fetch", "checkout"]
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL"]
async fn kickoff_uses_defaults_replaced_during_provisioning() -> Result<(), Box<dyn Error>> {
    use signalbox_domain::{
        DeliveryRequest, ModelSelectionOverride, PerInputConfigurationChoices, SubmitInputResult,
    };
    use signalbox_persistence::submit_input::SubmitInputRepository;

    let mut fixture = CheckoutFixture::new().await?;
    let cloned = Arc::new(tokio::sync::Notify::new());
    let resume = Arc::new(tokio::sync::Notify::new());
    let runner = CheckoutBarrierRunner {
        runner: fixture.runner.clone(),
        cloned: cloned.clone(),
        resume: resume.clone(),
    };
    let configuration = fixture
        .sink
        .models
        .repository_watch()
        .expect("configuration")
        .clone();
    let provisioning =
        submit_pending_with_runner(&fixture.store, &configuration, &mut fixture.sink, runner);
    let replace = async {
        cloned.notified().await;
        let session = SessionId::from_uuid(
            sqlx::query_scalar(
                "SELECT checkout_session_id FROM dispatch_ledger WHERE command_id = $1",
            )
            .bind(fixture.command.into_uuid())
            .fetch_one(&fixture.module)
            .await?,
        );
        let version = replace_kickoff_defaults(&fixture.core, session).await?;
        resume.notify_one();
        Ok::<_, Box<dyn Error>>(version)
    };
    let (submitted, version) = tokio::join!(provisioning, replace);
    submitted.expect("kickoff after defaults replacement");
    let version = version?;
    let kickoff: Uuid =
        sqlx::query_scalar("SELECT kickoff_command_id FROM dispatch_ledger WHERE command_id = $1")
            .bind(fixture.command.into_uuid())
            .fetch_one(&fixture.module)
            .await?;
    let recorded = SubmitInputRepository::new(fixture.core.clone())
        .load(DurableCommandId::from_uuid(kickoff))
        .await?
        .expect("recorded kickoff");
    assert!(matches!(recorded.result(), SubmitInputResult::Applied(_)));
    assert_eq!(
        recorded.command().delivery(),
        DeliveryRequest::StartWhenNoActiveTurn {
            configuration: PerInputConfigurationChoices::new(
                version,
                ModelSelectionOverride::UseSessionDefault
            ),
        }
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL"]
async fn rejected_kickoff_retries_once_and_keeps_the_hold_until_recovery()
-> Result<(), Box<dyn Error>> {
    use signalbox_domain::SubmitInputResult;
    use signalbox_persistence::submit_input::SubmitInputRepository;

    let mut fixture = CheckoutFixture::new().await?;
    // Fixture locks 1 and 2 pause each attempt after defaults are read, before core validation.
    sqlx::raw_sql(
        "CREATE SEQUENCE kickoff_attempt;
        CREATE FUNCTION pause_kickoff() RETURNS trigger LANGUAGE plpgsql AS $$
        BEGIN IF NEW.command_kind = 'submit_input' THEN
            PERFORM pg_advisory_xact_lock(nextval('kickoff_attempt'));
        END IF; RETURN NEW; END $$;
        CREATE TRIGGER pause_kickoff BEFORE INSERT ON durable_command
        FOR EACH ROW EXECUTE FUNCTION pause_kickoff();",
    )
    .execute(&fixture.core)
    .await?;
    let mut first = fixture.core.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(1)")
        .execute(&mut *first)
        .await?;
    let mut second = fixture.core.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(2)")
        .execute(&mut *second)
        .await?;
    let configuration = fixture
        .sink
        .models
        .repository_watch()
        .expect("configuration")
        .clone();
    let provisioning = submit_pending_with_runner(
        &fixture.store,
        &configuration,
        &mut fixture.sink,
        fixture.runner.clone(),
    );
    let replace = async {
        wait_for_kickoff_attempt(&fixture.core, 1).await?;
        let session = SessionId::from_uuid(
            sqlx::query_scalar(
                "SELECT checkout_session_id FROM dispatch_ledger WHERE command_id = $1",
            )
            .bind(fixture.command.into_uuid())
            .fetch_one(&fixture.module)
            .await?,
        );
        replace_kickoff_defaults(&fixture.core, session).await?;
        first.commit().await?;
        wait_for_kickoff_attempt(&fixture.core, 2).await?;
        let held: bool = sqlx::query_scalar("SELECT checkout_provisioning_pending AND start_gate_held FROM session_lifecycle WHERE session_id = $1")
            .bind(session.into_uuid()).fetch_one(&fixture.core).await?;
        assert!(
            held,
            "a recorded rejection cannot release the hold before retry"
        );
        replace_kickoff_defaults(&fixture.core, session).await?;
        second.commit().await?;
        Ok::<_, Box<dyn Error>>(session)
    };
    let (submitted, session) = tokio::join!(provisioning, replace);
    assert!(matches!(
        submitted,
        Err(
            signalbox_module_repo_watch_v2::dispatch::SubmissionError::Sink(
                signalboxd::repo_watch_dispatch::RepositoryWatchCommandError::KickoffDefaultsChanged
            )
        )
    ));
    let session = session?;
    let held: bool = sqlx::query_scalar("SELECT checkout_provisioning_pending AND start_gate_held FROM session_lifecycle WHERE session_id = $1")
        .bind(session.into_uuid()).fetch_one(&fixture.core).await?;
    assert!(held, "a rejected retry leaves provisioning held");
    let attempts: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM durable_command WHERE command_kind = 'submit_input'",
    )
    .fetch_one(&fixture.core)
    .await?;
    assert_eq!(attempts, 2, "one initial attempt and one retry");
    let inputs: i64 = sqlx::query_scalar("SELECT count(*) FROM accepted_input")
        .fetch_one(&fixture.core)
        .await?;
    assert_eq!(inputs, 0);
    let (rejected, pending): (Uuid, bool) = sqlx::query_as(
        "SELECT kickoff_command_id, submission_pending FROM dispatch_ledger WHERE command_id = $1",
    )
    .bind(fixture.command.into_uuid())
    .fetch_one(&fixture.module)
    .await?;
    assert!(pending);
    let repository = SubmitInputRepository::new(fixture.core.clone());
    let rejection = repository
        .load(DurableCommandId::from_uuid(rejected))
        .await?
        .expect("recorded rejection");
    assert!(matches!(
        rejection.result(),
        SubmitInputResult::Rejected(
            signalbox_domain::SubmitInputRejectedResult::SessionDefaultsVersionMismatch { .. }
        )
    ));
    fixture.store = RepoWatchStore::new(fixture.module.clone());
    fixture.dispatch().await;
    let accepted: Uuid =
        sqlx::query_scalar("SELECT kickoff_command_id FROM dispatch_ledger WHERE command_id = $1")
            .bind(fixture.command.into_uuid())
            .fetch_one(&fixture.module)
            .await?;
    assert_ne!(accepted, rejected);
    let recorded = repository
        .load(DurableCommandId::from_uuid(accepted))
        .await?
        .expect("accepted retry");
    assert!(matches!(recorded.result(), SubmitInputResult::Applied(_)));
    let held: bool = sqlx::query_scalar("SELECT checkout_provisioning_pending OR start_gate_held FROM session_lifecycle WHERE session_id = $1")
        .bind(session.into_uuid()).fetch_one(&fixture.core).await?;
    assert!(!held);
    // Recovery after acceptance must preserve delivery even if defaults have advanced again.
    replace_kickoff_defaults(&fixture.core, session).await?;
    sqlx::query("UPDATE dispatch_ledger SET submission_pending = true WHERE command_id = $1")
        .bind(fixture.command.into_uuid())
        .execute(&fixture.module)
        .await?;
    fixture.store = RepoWatchStore::new(fixture.module.clone());
    fixture.dispatch().await;
    let replay = repository
        .load(DurableCommandId::from_uuid(accepted))
        .await?
        .expect("same accepted kickoff");
    assert_eq!(replay.command(), recorded.command());
    assert_eq!(replay.result(), recorded.result());
    let inputs: i64 = sqlx::query_scalar("SELECT count(*) FROM accepted_input")
        .fetch_one(&fixture.core)
        .await?;
    assert_eq!(inputs, 1);
    Ok(())
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL"]
async fn unavailable_kickoff_alias_retires_once_across_stop_recovery() -> Result<(), Box<dyn Error>>
{
    use signalbox_domain::{SubmitInputRejectedResult, SubmitInputResult};
    use signalbox_module_repo_watch_v2::checkout::CheckoutRetirementReason;
    use signalbox_persistence::submit_input::SubmitInputRepository;

    let mut fixture = CheckoutFixture::new().await?;
    // Interrupt publication so the next command-loop tick uses a reloaded model catalog.
    sqlx::raw_sql("CREATE FUNCTION interrupt_kickoff() RETURNS trigger LANGUAGE plpgsql AS $$
        BEGIN IF NEW.command_kind = 'submit_input' THEN RAISE EXCEPTION 'fixture kickoff interruption'; END IF;
        RETURN NEW; END $$;
        CREATE TRIGGER interrupt_kickoff BEFORE INSERT ON durable_command
        FOR EACH ROW EXECUTE FUNCTION interrupt_kickoff();").execute(&fixture.core).await?;
    let configuration = fixture
        .sink
        .models
        .repository_watch()
        .expect("configuration")
        .clone();
    assert!(
        submit_pending_with_runner(
            &fixture.store,
            &configuration,
            &mut fixture.sink,
            fixture.runner.clone()
        )
        .await
        .is_err()
    );
    let kickoff: Uuid =
        sqlx::query_scalar("SELECT kickoff_command_id FROM dispatch_ledger WHERE command_id = $1")
            .bind(fixture.command.into_uuid())
            .fetch_one(&fixture.module)
            .await?;
    // This is the alias selected by the fixture's watch template; reload removes that identity.
    fixture.sink.models = Arc::new(HubModelConfiguration::parse(&fixture.catalog.replace(
        "540ce009-c2ec-4a04-b823-c411ea189778",
        &Uuid::now_v7().to_string(),
    ))?);
    sqlx::raw_sql(
        "DROP TRIGGER interrupt_kickoff ON durable_command;
        DROP FUNCTION interrupt_kickoff();
        CREATE FUNCTION interrupt_stop() RETURNS trigger LANGUAGE plpgsql AS $$
        BEGIN RAISE EXCEPTION 'fixture stop interruption'; END $$;
        CREATE TRIGGER interrupt_stop BEFORE INSERT ON session_lifecycle_command
        FOR EACH ROW EXECUTE FUNCTION interrupt_stop();",
    )
    .execute(&fixture.core)
    .await?;
    assert!(
        submit_pending_with_runner(
            &fixture.store,
            &configuration,
            &mut fixture.sink,
            fixture.runner.clone()
        )
        .await
        .is_err()
    );
    let checkout = fixture
        .store
        .dispatch_checkout(fixture.command)
        .await?
        .expect("retired checkout");
    assert_eq!(
        checkout.retired_reason,
        Some(CheckoutRetirementReason::KickoffRejected)
    );
    let stop = checkout.stop_command.expect("durable stop identity");
    let rejected = SubmitInputRepository::new(fixture.core.clone())
        .load(DurableCommandId::from_uuid(kickoff))
        .await?
        .expect("recorded rejection");
    assert!(matches!(
        rejected.result(),
        SubmitInputResult::Rejected(SubmitInputRejectedResult::UnknownModelAlias { .. })
    ));
    sqlx::raw_sql(
        "DROP TRIGGER interrupt_stop ON session_lifecycle_command; DROP FUNCTION interrupt_stop();",
    )
    .execute(&fixture.core)
    .await?;
    fixture.store = RepoWatchStore::new(fixture.module.clone());
    fixture.submit_without_lifecycle_settlement().await;
    let lifecycle = signalbox_session_ownership::LifecycleEventSource::new(fixture.core.clone());
    let mut factory = FixtureSessionFactory {
        next_command: Uuid::now_v7().as_u128(),
        model: Uuid::now_v7().as_u128(),
    };
    while let Some(event) = lifecycle.next().await? {
        fixture
            .store
            .react_to_lifecycle(
                &event,
                &mut factory,
                &mut RepositoryWatchCommandCodec,
                &lifecycle,
            )
            .await?;
        lifecycle.acknowledge(&event).await?;
    }
    fixture.dispatch().await;
    fixture.dispatch().await;
    // Replay after terminal settlement must retain both identities and never retry the kickoff.
    sqlx::query("UPDATE dispatch_ledger SET submission_pending = true WHERE command_id = $1")
        .bind(fixture.command.into_uuid())
        .execute(&fixture.module)
        .await?;
    fixture.store = RepoWatchStore::new(fixture.module.clone());
    fixture.dispatch().await;
    let session = fixture.session().await;
    let terminal: bool = sqlx::query_scalar("SELECT state_kind = 'terminal' AND terminal_outcome_kind = 'stopped' AND NOT terminal_stop_sticky FROM session_lifecycle WHERE session_id = $1")
        .bind(session.into_uuid()).fetch_one(&fixture.core).await?;
    assert!(terminal);
    let settled: bool = sqlx::query_scalar("SELECT NOT submission_pending AND singleton_released_at IS NOT NULL AND kickoff_command_id = $2 AND checkout_stop_command_id = $3 AND checkout_retired_reason = 'kickoff_rejected' FROM dispatch_ledger WHERE command_id = $1")
        .bind(fixture.command.into_uuid()).bind(kickoff).bind(stop.into_uuid()).fetch_one(&fixture.module).await?;
    assert!(settled);
    let attempts: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM durable_command WHERE command_kind = 'submit_input'",
    )
    .fetch_one(&fixture.core)
    .await?;
    assert_eq!(
        attempts, 1,
        "non-recoverable rejection never mints another kickoff identity"
    );
    let inputs: i64 = sqlx::query_scalar("SELECT count(*) FROM accepted_input")
        .fetch_one(&fixture.core)
        .await?;
    assert_eq!(inputs, 0);
    let stops: i64 =
        sqlx::query_scalar("SELECT count(*) FROM session_lifecycle_command WHERE session_id = $1")
            .bind(session.into_uuid())
            .fetch_one(&fixture.core)
            .await?;
    assert_eq!(stops, 1, "stop recovery replays the retained identity");
    Ok(())
}

async fn replace_kickoff_defaults(
    pool: &PgPool,
    session: SessionId,
) -> Result<signalbox_domain::SessionConfigurationDefaultsVersion, Box<dyn Error>> {
    use signalbox_persistence::{
        replace_session_defaults::{
            ReplaceSessionDefaultsHandlingOutcome, ReplaceSessionDefaultsRepository,
        },
        session::SessionRepository,
    };
    let current = SessionRepository::new(pool.clone())
        .load_session(session)
        .await?
        .expect("current session");
    let defaults = current.current_configuration_defaults();
    let result = ReplaceSessionDefaultsRepository::new(pool.clone())
        .handle(signalbox_domain::ReplaceSessionDefaults::new(
            DurableCommandId::from_uuid(Uuid::now_v7()),
            session,
            defaults.version(),
            defaults.defaults().clone(),
        ))
        .await?;
    assert!(matches!(
        result,
        ReplaceSessionDefaultsHandlingOutcome::Applied(_)
    ));
    Ok(defaults.version().checked_next().expect("next version"))
}

async fn wait_for_kickoff_attempt(pool: &PgPool, attempt: i64) -> Result<(), Box<dyn Error>> {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let waiting: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_locks WHERE locktype = 'advisory' AND NOT granted AND objid::bigint = $1 AND database = (SELECT oid FROM pg_database WHERE datname = current_database()))")
                .bind(attempt).fetch_one(pool).await?;
            if waiting { return Ok::<_, sqlx::Error>(()); }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }).await??;
    Ok(())
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL"]
async fn kickoff_with_unresolved_threads_requests_repair_and_thread_replies()
-> Result<(), Box<dyn Error>> {
    let mut fixture = CheckoutFixture::with_threads(
        "checkout/project",
        "labeled-review-response",
        vec![RepoWatchThreadObservation::new(
            ReviewThreadId::try_new("fixture-thread".to_owned())?,
            RepoWatchThreadState::Open,
        )],
    )
    .await?;
    fixture.enable_push()?;
    fixture.dispatch().await;
    let text: String =
        sqlx::query_scalar("SELECT kickoff_text FROM dispatch_ledger WHERE command_id = $1")
            .bind(fixture.command.into_uuid())
            .fetch_one(&fixture.module)
            .await?;
    assert!(text.contains("fix every unresolved review thread"));
    assert!(text.contains("push with git_push_configured to the head branch"));
    assert!(
        text.contains("Reply on each addressed thread naming the fixing commit and resolve it")
    );
    assert!(!text.contains("No unresolved review threads"));
    Ok(())
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL"]
async fn kickoff_for_renovate_requests_the_template_merge_forward() -> Result<(), Box<dyn Error>> {
    let mut fixture =
        CheckoutFixture::with_rule("checkout/project", "renovate-merge-forward").await?;
    fixture.enable_push()?;
    fixture.dispatch().await;
    let text: String =
        sqlx::query_scalar("SELECT kickoff_text FROM dispatch_ledger WHERE command_id = $1")
            .bind(fixture.command.into_uuid())
            .fetch_one(&fixture.module)
            .await?;
    assert!(
        text.contains("base branch forward into its head branch, resolve only merge conflicts")
    );
    assert!(text.contains("push with git_push_configured to the head branch"));
    assert!(text.contains("intended change survives the merge"));
    assert!(text.contains("For each conflict hunk, report which side was retained"));
    Ok(())
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL"]
async fn kickoff_without_push_credentials_requests_a_reviewable_diff() -> Result<(), Box<dyn Error>>
{
    let mut fixture = CheckoutFixture::with_threads(
        "checkout/project",
        "labeled-review-response",
        vec![RepoWatchThreadObservation::new(
            ReviewThreadId::try_new("fixture-thread".to_owned())?,
            RepoWatchThreadState::Open,
        )],
    )
    .await?;
    assert!(
        fixture
            .sink
            .models
            .repository_watch()
            .expect("configuration")
            .repositories()[0]
            .push_credential_file()
            .is_none()
    );
    fixture.dispatch().await;
    let text: String =
        sqlx::query_scalar("SELECT kickoff_text FROM dispatch_ledger WHERE command_id = $1")
            .bind(fixture.command.into_uuid())
            .fetch_one(&fixture.module)
            .await?;
    assert!(text.contains("fix every unresolved review thread"));
    assert!(text.contains("This session has no Git push authority. Do not push."));
    assert!(text.contains(
        "Provide any fix as a reviewable diff in a plain pull request reply and finish cleanly."
    ));
    assert!(text.contains("Leave unresolved review threads open"));
    assert!(!text.contains("push with git_push_configured"));
    assert!(!text.contains("and resolve it"));
    Ok(())
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL"]
async fn kickoff_for_a_fork_head_requests_a_diff_despite_configured_push_credentials()
-> Result<(), Box<dyn Error>> {
    let mut fixture =
        CheckoutFixture::with_rule("contributor/project", "renovate-merge-forward").await?;
    fixture.enable_push()?;
    assert!(
        fixture
            .sink
            .models
            .repository_watch()
            .expect("configuration")
            .repositories()[0]
            .push_credential_file()
            .is_some()
    );
    fixture.dispatch().await;
    let text: String =
        sqlx::query_scalar("SELECT kickoff_text FROM dispatch_ledger WHERE command_id = $1")
            .bind(fixture.command.into_uuid())
            .fetch_one(&fixture.module)
            .await?;
    assert!(
        text.contains("base branch forward into its head branch, resolve only merge conflicts")
    );
    assert!(text.contains("This session has no Git push authority. Do not push."));
    assert!(text.contains(
        "Provide any fix as a reviewable diff in a plain pull request reply and finish cleanly."
    ));
    assert!(!text.contains("push with git_push_configured"));
    assert!(!text.contains("and resolve it"));
    Ok(())
}

impl CheckoutFixture {
    fn enable_push(&mut self) -> Result<PathBuf, Box<dyn Error>> {
        let credential = self._files.path().join("push-token");
        std::fs::write(&credential, "push-fixture-token")?;
        std::fs::set_permissions(
            &credential,
            std::os::unix::fs::PermissionsExt::from_mode(0o600),
        )?;
        let catalog_text = self.catalog.replace(
            "\ncredential_file =",
            &format!(
                "\npush_credential_file = \"{}\"\ncredential_file =",
                credential.display()
            ),
        );
        self.sink.models = Arc::new(HubModelConfiguration::parse(&catalog_text)?);
        Ok(credential)
    }

    async fn new() -> Result<Self, Box<dyn Error>> {
        Self::with_head_repository("checkout/project").await
    }

    async fn with_head_repository(head_repository: &str) -> Result<Self, Box<dyn Error>> {
        Self::with_rule(head_repository, "review").await
    }

    async fn with_rule(head_repository: &str, rule: &str) -> Result<Self, Box<dyn Error>> {
        Self::with_threads(head_repository, rule, Vec::new()).await
    }

    async fn with_threads(
        head_repository: &str,
        rule: &str,
        threads: Vec<RepoWatchThreadObservation>,
    ) -> Result<Self, Box<dyn Error>> {
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
        std::fs::set_permissions(
            &credential,
            std::os::unix::fs::PermissionsExt::from_mode(0o600),
        )?;
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
id = "{rule}"
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
                        threads,
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
                    merged_at: std::collections::BTreeMap::new(),
                    repository: repository.clone(),
                    default_branch: BranchName::try_new(String::from("main"))?,
                    default_head: head.clone(),
                    observed_at: now,
                    observation,
                },
                EventProducer::Poll,
                MERGED_RETENTION,
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
                goal_resumption: signalboxd::PostgresGoalPassDisposition::new(
                    core.clone(),
                    models.clone(),
                    eligibility_nudge.clone(),
                    signalboxd::GoalModeNumericBounds::new(None, None, None, None, None),
                ),
                checkout_runner: None,
                pool: core,
                models: Arc::new(models),
                eligibility_nudge,
                tool_dispatch_gate: InProcessToolDispatchGate::default(),
            },
            runner: LocalGitRunner {
                bare,
                supervisor: signalbox_tools_exec::TokioProcessRunner::try_new(
                    &std::env::current_exe()?,
                )?,
                redirect: None,
                steps: Arc::default(),
                push_authorizations: Arc::default(),
            },
            command,
            head,
            catalog,
        })
    }

    async fn dispatch(&mut self) {
        self.submit_without_lifecycle_settlement().await;
        self.settle().await;
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

    async fn settle(&self) {
        let lifecycle = signalbox_session_ownership::LifecycleEventSource::new(self.core.clone());
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

    /// Stops after core creation and staging mkdir, before publication or lifecycle settlement.
    async fn stage_before_publication(&mut self) -> Result<(SessionId, PathBuf), Box<dyn Error>> {
        use signalbox_module_repo_watch_v2::dispatch::{CommandSubmission, SessionCommandSink};
        use std::os::unix::ffi::OsStrExt;

        let pending = self
            .store
            .recover_pending_commands(&mut RepositoryWatchCommandCodec)
            .await?;
        sqlx::query("UPDATE dispatch_ledger SET submission_pending = true WHERE command_id = $1")
            .bind(self.command.into_uuid())
            .execute(&self.module)
            .await?;
        let result = self
            .sink
            .submit(pending[0].command().clone())
            .await
            .expect("held core creation");
        let CommandSubmission::Creation(CreateSessionOutcome::Applied(applied)) = result else {
            panic!("core creation must be applied");
        };
        let session = applied.session();
        self.store
            .retain_checkout_location(
                self.command,
                session,
                self.sink
                    .models
                    .daemon_tools()
                    .expect("tools")
                    .workspace_root()
                    .as_os_str()
                    .as_bytes(),
            )
            .await?;
        let staged = self
            .root(session)
            .with_file_name(format!(".checkout-{}", pending[0].dispatch().into_uuid()));
        std::fs::create_dir_all(&staged)?;
        Ok((session, staged))
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
    assert_eq!(state, (false, false));
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
async fn dispatched_git_tools_accept_large_unrelated_loose_objects() -> Result<(), Box<dyn Error>> {
    assert_dispatched_git_tools(ArchiveStorage::Loose).await
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL"]
async fn dispatched_git_tools_accept_large_unrelated_packed_objects() -> Result<(), Box<dyn Error>>
{
    assert_dispatched_git_tools(ArchiveStorage::Packed).await
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL"]
async fn dispatched_push_advances_only_its_retained_head_and_survives_recomposition()
-> Result<(), Box<dyn Error>> {
    assert_push_retained_fences(PushSessionOrigin::RepositoryDispatch).await
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL"]
async fn commissioned_push_advances_only_its_retained_head_without_repository_watch()
-> Result<(), Box<dyn Error>> {
    assert_push_retained_fences(PushSessionOrigin::Commissioned).await
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL"]
async fn commissioned_push_uses_its_bound_configured_workspace() -> Result<(), Box<dyn Error>> {
    assert_push_retained_fences(PushSessionOrigin::CommissionedConfiguredRoot).await
}

enum PushSessionOrigin {
    RepositoryDispatch,
    Commissioned,
    CommissionedConfiguredRoot,
}

async fn assert_push_retained_fences(origin: PushSessionOrigin) -> Result<(), Box<dyn Error>> {
    use signalbox_application::ToolExecutorEvidence;
    use signalbox_domain::TurnId;
    let mut fixture = CheckoutFixture::new().await?;
    use signalbox_application::ToolCatalog;
    assert!(
        fixture
            .daemon_tools()?
            .0
            .definition(
                &signalbox_domain::ToolName::try_new("git_push_configured".to_owned())
                    .expect("push tool name")
            )
            .is_none()
    );
    let credential = fixture.enable_push()?;
    let (session, root) = match origin {
        PushSessionOrigin::RepositoryDispatch => {
            fixture.dispatch().await;
            let session = fixture.session().await;
            (session, fixture.root(session))
        }
        PushSessionOrigin::Commissioned => {
            let session = fixture.commission_push().await?;
            (session, fixture.root(session))
        }
        PushSessionOrigin::CommissionedConfiguredRoot => {
            let session = fixture.commission_push().await?;
            let configured = fixture
                .sink
                .models
                .daemon_tools()
                .expect("tools")
                .workspace_root()
                .to_owned();
            std::fs::remove_dir_all(&configured)?;
            std::fs::rename(fixture.root(session), &configured)?;
            (session, configured)
        }
    };
    let (catalog, executor) = fixture.push_tools().await?;
    let turn = TurnId::from_uuid(Uuid::now_v7());
    std::fs::write(root.join("review.txt"), "fixed by dispatched session\n")?;
    run_git_tool(
        &catalog,
        &executor,
        session,
        turn,
        "git_stage",
        r#"{"paths":["review.txt"]}"#,
    )
    .await;
    let commit = run_git_tool(
        &catalog,
        &executor,
        session,
        turn,
        "git_create_commit",
        r#"{"message":"Fix dispatched review"}"#,
    )
    .await;
    let local = git2::Repository::open(&root)?;
    let head = local.head()?.target().expect("new commit");
    assert_ne!(head.to_string(), fixture.head.as_str());
    assert!(commit.is_object());
    local.branch("other", &local.find_commit(head)?, false)?;
    // Mutable remote and URL rewriting must not redirect authenticated transport.
    local
        .config()?
        .set_str("remote.origin.pushurl", "/unavailable/other.git")?;
    let redirected_path = fixture._files.path().join("redirected.git");
    let redirected = git2::build::RepoBuilder::new().bare(true).clone(
        fixture.runner.bare.to_str().expect("fixture path"),
        &redirected_path,
    )?;
    local.config()?.set_str(
        &format!("url.{}.insteadOf", redirected_path.display()),
        fixture.runner.bare.to_str().expect("fixture path"),
    )?;
    // The fixture substitutes this local destination for the deployment URL.
    let rewritten = std::process::Command::new("git")
        .arg("-C")
        .arg(&root)
        .args(["ls-remote", "--get-url"])
        .arg(&fixture.runner.bare)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()?;
    assert!(rewritten.status.success());
    assert_eq!(
        std::str::from_utf8(&rewritten.stdout)?.trim(),
        redirected_path.to_str().expect("fixture path")
    );
    let rejected = run_git_tool_evidence(
        &catalog,
        &executor,
        session,
        turn,
        "git_push_configured",
        r#"{"branch":"other"}"#,
    )
    .await;
    assert!(matches!(rejected, ToolExecutorEvidence::KnownFailed { .. }));
    let remote = git2::Repository::open_bare(&fixture.runner.bare)?;
    remote.reference(
        "refs/heads/nested/refs/heads/review",
        git2::Oid::from_str(fixture.head.as_str())?,
        false,
        "Matching suffix is a different remote branch",
    )?;
    assert_eq!(
        remote
            .find_reference("refs/heads/review")?
            .target()
            .expect("old remote head")
            .to_string(),
        fixture.head.as_str()
    );
    assert!(remote.find_reference("refs/heads/other").is_err());
    let pushed = run_git_tool(
        &catalog,
        &executor,
        session,
        turn,
        "git_push_configured",
        r#"{"branch":"review"}"#,
    )
    .await;
    assert_eq!(pushed["commit"], head.to_string());
    assert_eq!(
        remote.find_reference("refs/heads/review")?.target(),
        Some(head)
    );
    assert_eq!(
        redirected.find_reference("refs/heads/review")?.target(),
        Some(git2::Oid::from_str(fixture.head.as_str())?)
    );
    // A new executor and turn reread both durable authority and a rotated file.
    std::fs::write(&credential, "rotated-push-fixture-token\n")?;
    let (catalog, executor) = fixture.push_tools().await?;
    let pushed = run_git_tool(
        &catalog,
        &executor,
        session,
        TurnId::from_uuid(Uuid::now_v7()),
        "git_push_configured",
        r#"{"branch":"review"}"#,
    )
    .await;
    assert_eq!(pushed["commit"], head.to_string());
    assert_eq!(
        remote.find_reference("refs/heads/review")?.target(),
        Some(head)
    );
    {
        let authorizations = fixture
            .runner
            .push_authorizations
            .lock()
            .expect("push authorizations");
        assert_eq!(
            authorizations.as_slice(),
            [
                "https://x-access-token:push-fixture-token@github.com/checkout/project.git",
                "https://x-access-token:rotated-push-fixture-token@github.com/checkout/project.git",
            ],
            "each push resolves the current credential into its effective URL"
        );
    }
    local.find_reference("refs/heads/review")?.set_target(
        git2::Oid::from_str(fixture.head.as_str())?,
        "attempt non-fast-forward",
    )?;
    let rejected = run_git_tool_evidence(
        &catalog,
        &executor,
        session,
        turn,
        "git_push_configured",
        r#"{"branch":"review"}"#,
    )
    .await;
    assert!(matches!(rejected, ToolExecutorEvidence::KnownFailed { .. }));
    assert_eq!(
        remote.find_reference("refs/heads/review")?.target(),
        Some(head)
    );
    assert!(!std::fs::read_to_string(root.join(".git/config"))?.contains("push-fixture-token"));
    Ok(())
}

enum ArchiveStorage {
    Loose,
    Packed,
}

async fn assert_dispatched_git_tools(storage: ArchiveStorage) -> Result<(), Box<dyn Error>> {
    use signalbox_domain::TurnId;
    let mut fixture = CheckoutFixture::new().await?;
    let remote = git2::Repository::open_bare(&fixture.runner.bare)?;
    // The current checkout is small; another branch exceeds the object-content read bound.
    let blob = remote.blob(&vec![b'x'; 1024 * 1024 + 1])?;
    let mut tree = remote.treebuilder(None)?;
    tree.insert("archive.bin", blob, 0o100644)?;
    let tree = remote.find_tree(tree.write()?)?;
    let signature = git2::Signature::now("Checkout fixture", "checkout@example.test")?;
    remote.commit(
        Some("refs/heads/archive"),
        &signature,
        &signature,
        "Archived data",
        &tree,
        &[],
    )?;
    if matches!(storage, ArchiveStorage::Packed) {
        // The authority supports pack/index pairs without optional Git sidecar indexes.
        let output = std::process::Command::new("git")
            .args(["-c", "pack.writeReverseIndex=false"])
            .arg("-C")
            .arg(&fixture.runner.bare)
            .args(["repack", "-ad", "--no-write-bitmap-index"])
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .output()?;
        assert!(output.status.success(), "pack fixture: {output:?}");
    }
    let (catalog, executor) = fixture.daemon_tools()?;
    fixture.dispatch().await;
    let session = fixture.session().await;
    let root = fixture.root(session);
    assert!(!root.join("archive.bin").exists());
    assert_eq!(
        std::fs::read_to_string(root.join("review.txt"))?,
        "retained head\n"
    );
    let turn = TurnId::from_uuid(Uuid::now_v7());
    let status = run_git_tool(&catalog, &executor, session, turn, "git_status", "{}").await;
    assert_eq!(status["branch"], "review");
    assert_eq!(status["head"], fixture.head.as_str());
    let log = run_git_tool(&catalog, &executor, session, turn, "git_log", "{}").await;
    assert_eq!(log["commits"][0]["commit"], fixture.head.as_str());
    std::fs::write(root.join("review.txt"), "reviewed head\n")?;
    let diff = run_git_tool(
        &catalog,
        &executor,
        session,
        turn,
        "git_diff",
        r#"{"scope":"worktree"}"#,
    )
    .await;
    assert!(
        diff["patch"]
            .as_str()
            .expect("patch")
            .contains("+reviewed head")
    );
    let staged = run_git_tool(
        &catalog,
        &executor,
        session,
        turn,
        "git_stage",
        r#"{"paths":["review.txt"]}"#,
    )
    .await;
    assert_eq!(staged["staged_paths"], 1);
    let committed = run_git_tool(
        &catalog,
        &executor,
        session,
        turn,
        "git_create_commit",
        r#"{"message":"Review completed"}"#,
    )
    .await;
    let next_turn = TurnId::from_uuid(Uuid::now_v7());
    let status = run_git_tool(&catalog, &executor, session, next_turn, "git_status", "{}").await;
    assert_eq!(status["head"], committed["commit"]);
    assert_eq!(status["entries"], serde_json::json!([]));
    drop(executor);
    // Reconstruct the daemon composition against the persisted session checkout.
    let (catalog, restarted) = fixture.daemon_tools()?;
    let status = run_git_tool(&catalog, &restarted, session, next_turn, "git_status", "{}").await;
    assert_eq!(status["head"], committed["commit"]);
    assert_eq!(status["entries"], serde_json::json!([]));
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
    #[cfg(target_os = "linux")]
    {
        let mut publication = [0; uuid::fmt::Hyphenated::LENGTH];
        assert_eq!(
            rustix::fs::getxattr(&root, "user.signalbox.dispatch", &mut publication),
            Err(rustix::io::Errno::NODATA),
            "completed clone hands ownership evidence to the file marker"
        );
    }
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
    assert!(!held);
    let inputs: i64 = sqlx::query_scalar("SELECT count(*) FROM accepted_input")
        .fetch_one(&fixture.core)
        .await?;
    assert_eq!(inputs, 1);
    assert_git_status(&root, tools.git_identity().clone(), session).await;
    Ok(())
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL"]
#[cfg(target_os = "linux")]
async fn checkout_keeps_the_composed_runner_after_supervisor_removal() -> Result<(), Box<dyn Error>>
{
    assert_checkout_keeps_composed_runner(SupervisorChange::Removed).await
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL"]
#[cfg(target_os = "linux")]
async fn checkout_keeps_the_composed_runner_after_supervisor_replacement()
-> Result<(), Box<dyn Error>> {
    assert_checkout_keeps_composed_runner(SupervisorChange::Replaced).await
}

#[cfg(target_os = "linux")]
enum SupervisorChange {
    Removed,
    Replaced,
}

#[cfg(target_os = "linux")]
async fn assert_checkout_keeps_composed_runner(
    change: SupervisorChange,
) -> Result<(), Box<dyn Error>> {
    use signalbox_model_runtime::CredentialReference;
    use signalbox_tools_code_host::GitHubCodeHostTransport;
    use signalbox_tools_github::GitHubEgressPolicy;
    use signalbox_tools_web::WebFetchEgressPolicy;
    use signalboxd::{
        CodeHostNumericBounds, DaemonTools, FileCredentialAccess, MappedDaemonCredentialInputs,
    };
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let mut fixture = CheckoutFixture::new().await?;
    let executable = std::env::current_exe()?;
    let supervisor = fixture._files.path().join("supervisor");
    let ran_replacement = fixture._files.path().join("replacement-ran");
    std::fs::copy(&executable, &supervisor)?;
    let models = HubModelConfiguration::parse(&fixture.catalog.replace(
        executable.to_str().expect("fixture executable"),
        supervisor.to_str().expect("fixture supervisor"),
    ))?;
    fixture.sink.models = Arc::new(models);
    let configuration = fixture.sink.models.daemon_tools().expect("tools");
    let credentials = FileCredentialAccess::new(
        fixture._files.path().join("poll-token"),
        CredentialReference::new("fixture-credential"),
    );
    let tools = DaemonTools::try_new_production(
        || std::time::SystemTime::UNIX_EPOCH,
        fixture.core.clone(),
        fixture.sink.eligibility_nudge.clone(),
        MappedDaemonCredentialInputs {
            web_search: credentials.clone(),
            code_host: credentials.clone(),
            github: credentials,
        },
        GitHubCodeHostTransport::try_new(CodeHostNumericBounds::new(
            None, None, None, None, None, None,
        ))?,
        GitHubEgressPolicy::github_api_only(),
        configuration.workspace_root(),
        configuration.git_identity().clone(),
        configuration.exec_supervisor_executable(),
        None,
        &Default::default(),
        None,
        None,
        WebFetchEgressPolicy::deny_all(),
    )?;
    fixture.sink.checkout_runner = tools.process_runner();
    let identity = std::fs::metadata(&supervisor)?;
    match change {
        SupervisorChange::Removed => std::fs::remove_file(&supervisor)?,
        SupervisorChange::Replaced => {
            let replacement = supervisor.with_extension("new");
            std::fs::write(
                &replacement,
                format!("#!/bin/sh\n: > '{}'\nexit 99\n", ran_replacement.display()),
            )?;
            std::fs::set_permissions(&replacement, std::fs::Permissions::from_mode(0o700))?;
            std::fs::rename(&replacement, &supervisor)?;
        }
    }
    let pinned = fixture
        .sink
        .checkout_runner
        .as_ref()
        .expect("composed runner");
    assert_eq!(
        std::fs::metadata(pinned.sandbox_launcher_program())?.ino(),
        identity.ino()
    );
    let watch = fixture
        .sink
        .models
        .repository_watch()
        .expect("watch")
        .clone();
    signalboxd::repo_watch_dispatch::submit_pending(&fixture.store, &watch, &mut fixture.sink)
        .await
        .expect("submit using composed runner");
    let step: String = sqlx::query_scalar(
        "SELECT checkout_failure_step FROM dispatch_ledger WHERE command_id = $1",
    )
    .bind(fixture.command.into_uuid())
    .fetch_one(&fixture.module)
    .await?;
    // The pinned test harness rejects supervisor arguments, so reaching clone proves the runner was retained.
    assert_eq!(step, "clone");
    assert!(
        !ran_replacement.exists(),
        "checkout must never execute the replacement binary"
    );
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
    let inputs: i64 = sqlx::query_scalar("SELECT count(*) FROM accepted_input")
        .fetch_one(&fixture.core)
        .await?;
    assert_eq!(inputs, 0, "failed provisioning submits no kickoff");
    let kickoff: Option<Uuid> =
        sqlx::query_scalar("SELECT kickoff_command_id FROM dispatch_ledger WHERE command_id = $1")
            .bind(fixture.command.into_uuid())
            .fetch_one(&fixture.module)
            .await?;
    assert!(kickoff.is_none());
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
#[cfg(target_os = "linux")]
async fn cancellation_during_clone_keeps_the_published_checkout_removable()
-> Result<(), Box<dyn Error>> {
    assert_interrupted_clone_cleanup(RemovalLocation::Original).await
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL"]
#[cfg(target_os = "linux")]
async fn cancellation_during_clone_keeps_a_renamed_checkout_removable() -> Result<(), Box<dyn Error>>
{
    assert_interrupted_clone_cleanup(RemovalLocation::Sibling).await
}

#[cfg(target_os = "linux")]
async fn assert_interrupted_clone_cleanup(location: RemovalLocation) -> Result<(), Box<dyn Error>> {
    use signalboxd::repo_watch_dispatch::scavenge_checkouts;

    let mut fixture = CheckoutFixture::new().await?;
    let started = Arc::new(tokio::sync::Notify::new());
    let runner = PausingCloneRunner {
        started: started.clone(),
    };
    let watch = fixture
        .sink
        .models
        .repository_watch()
        .expect("watch")
        .clone();
    let mut submission = Box::pin(submit_pending_with_runner(
        &fixture.store,
        &watch,
        &mut fixture.sink,
        runner,
    ));
    tokio::select! {
        result = &mut submission => panic!("clone must remain pending: {result:?}"),
        result = tokio::time::timeout(Duration::from_secs(10), started.notified()) => result?,
    }
    drop(submission);
    let checkout = fixture
        .store
        .dispatch_checkout(fixture.command)
        .await?
        .expect("checkout");
    let session = checkout.location.expect("retained location").session;
    let root = fixture.root(session);
    assert!(root.join(".git/objects/partial").is_file());
    assert!(
        !root.join(".git/signalbox-dispatch").exists(),
        "clone was cancelled before the file marker could be written"
    );
    let mut evidence = [0; uuid::fmt::Hyphenated::LENGTH];
    let count = rustix::fs::getxattr(&root, "user.signalbox.dispatch", &mut evidence)?;
    assert_eq!(
        &evidence[..count],
        checkout.dispatch.into_uuid().to_string().as_bytes()
    );
    let retained_path = match location {
        RemovalLocation::Original => root.clone(),
        RemovalLocation::Sibling => {
            let renamed = root.with_file_name("interrupted-clone");
            std::fs::rename(&root, &renamed)?;
            renamed
        }
    };
    fixture.stop(session).await;
    let restarted = RepoWatchStore::new(fixture.module.clone());
    scavenge_checkouts(&restarted, &fixture.core)
        .await
        .expect("interrupted clone removed using publication evidence");
    assert!(!retained_path.exists());
    assert!(restarted.checkout_removal_candidates().await?.is_empty());
    Ok(())
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL"]
async fn cancellation_during_identity_retention_preserves_staging_for_replay()
-> Result<(), Box<dyn Error>> {
    use signalbox_module_repo_watch_v2::checkout::CheckoutDirectoryIdentity;
    use std::os::unix::fs::MetadataExt;

    let mut fixture = CheckoutFixture::new().await?;
    let (session, staged) = fixture.stage_before_publication().await?;
    std::fs::remove_dir(&staged)?;
    let core = fixture.core.clone();
    // This fixture lock pauses only the ownership UPDATE, after prepare has created staging.
    sqlx::query("CREATE FUNCTION pause_checkout_identity() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM pg_advisory_xact_lock(1); RETURN NEW; END $$")
        .execute(&fixture.module).await?;
    sqlx::query("CREATE TRIGGER pause_checkout_identity BEFORE UPDATE OF checkout_device ON dispatch_ledger FOR EACH ROW EXECUTE FUNCTION pause_checkout_identity()")
        .execute(&fixture.module).await?;
    let mut blocker = core.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(1)")
        .execute(&mut *blocker)
        .await?;
    let mut submission = Box::pin(fixture.submit_without_lifecycle_settlement());
    let blocked = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let waiting: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE datname = current_database() AND wait_event = 'advisory')")
                .fetch_one(&core).await?;
            if waiting {
                return Ok::<_, sqlx::Error>(());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    });
    tokio::select! {
        () = &mut submission => panic!("identity retention must wait for the fixture lock"),
        result = blocked => result??,
    }
    let metadata = std::fs::metadata(&staged)?;
    drop(submission);
    assert!(
        staged.is_dir(),
        "cancellation must preserve staging once identity retention starts"
    );
    blocker.commit().await?;
    let identity = CheckoutDirectoryIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    };
    fixture
        .store
        .retain_checkout_identity(fixture.command, identity, true)
        .await?;
    fixture.dispatch().await;
    let root = fixture.root(session);
    assert_eq!(std::fs::metadata(&root)?.ino(), identity.inode);
    assert!(root.join(".git").is_dir());
    assert!(
        fixture
            .store
            .dispatch_checkout(fixture.command)
            .await?
            .expect("checkout")
            .retired_reason
            .is_none()
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL"]
async fn replay_publishes_staging_with_its_retained_identity() -> Result<(), Box<dyn Error>> {
    use signalbox_module_repo_watch_v2::checkout::CheckoutDirectoryIdentity;
    use signalboxd::repo_watch_dispatch::scavenge_checkouts;
    use std::os::unix::fs::MetadataExt;

    let mut fixture = CheckoutFixture::new().await?;
    let (session, staged) = fixture.stage_before_publication().await?;
    let metadata = std::fs::metadata(&staged)?;
    let identity = CheckoutDirectoryIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    };
    fixture
        .store
        .retain_checkout_identity(fixture.command, identity, true)
        .await?;
    fixture.store = RepoWatchStore::new(fixture.module.clone());
    scavenge_checkouts(&fixture.store, &fixture.core)
        .await
        .expect("active staging preserved at startup");
    assert!(staged.is_dir());
    fixture.dispatch().await;
    let root = fixture.root(session);
    let published = std::fs::metadata(&root)?;
    assert_eq!(published.dev(), identity.device);
    assert_eq!(
        published.ino(),
        identity.inode,
        "replay publishes the retained directory"
    );
    assert!(!staged.exists());
    let checkout = fixture
        .store
        .dispatch_checkout(fixture.command)
        .await?
        .expect("checkout");
    assert_eq!(checkout.head.as_ref(), Some(&fixture.head));
    assert!(checkout.retired_reason.is_none());
    assert!(checkout.stop_command.is_none());
    assert_eq!(
        *fixture.runner.steps.lock().expect("Git steps"),
        ["clone", "fetch", "checkout"]
    );
    fixture.dispatch().await;
    assert_eq!(
        *fixture.runner.steps.lock().expect("Git steps"),
        ["clone", "fetch", "checkout"],
        "settled replay does not provision again"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL"]
async fn replay_refuses_staging_with_a_different_retained_identity() -> Result<(), Box<dyn Error>> {
    use signalbox_module_repo_watch_v2::checkout::CheckoutDirectoryIdentity;
    use std::os::unix::fs::MetadataExt;

    let mut fixture = CheckoutFixture::new().await?;
    let (session, staged) = fixture.stage_before_publication().await?;
    let original = staged.with_file_name("original-staging");
    std::fs::rename(&staged, &original)?;
    let metadata = std::fs::metadata(&original)?;
    fixture
        .store
        .retain_checkout_identity(
            fixture.command,
            CheckoutDirectoryIdentity {
                device: metadata.dev(),
                inode: metadata.ino(),
            },
            true,
        )
        .await?;
    std::fs::create_dir(&staged)?;
    fixture.dispatch().await;
    assert!(fixture.runner.steps.lock().expect("Git steps").is_empty());
    assert!(!fixture.root(session).exists());
    assert!(
        staged.is_dir(),
        "rejected staging is not discarded by preparation"
    );
    assert!(original.is_dir());
    let checkout = fixture
        .store
        .dispatch_checkout(fixture.command)
        .await?
        .expect("checkout");
    assert_eq!(
        checkout.retired_reason,
        Some(
            signalbox_module_repo_watch_v2::checkout::CheckoutRetirementReason::ProvisioningFailed
        )
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL"]
async fn replay_does_not_adopt_staging_without_retained_ownership() -> Result<(), Box<dyn Error>> {
    let mut fixture = CheckoutFixture::new().await?;
    let (session, staged) = fixture.stage_before_publication().await?;
    fixture.dispatch().await;
    assert!(fixture.runner.steps.lock().expect("Git steps").is_empty());
    assert!(!fixture.root(session).exists());
    assert!(
        staged.is_dir(),
        "unowned staging is not discarded by preparation"
    );
    let candidates = fixture.store.checkout_removal_candidates().await?;
    let candidate = candidates.first().expect("pending staging cleanup");
    assert!(
        !candidate.created,
        "reopening must not claim creation ownership"
    );
    assert!(candidate.identity.is_none());
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
async fn removal_migration_settles_existing_checkouts_without_inventing_locations()
-> Result<(), Box<dyn Error>> {
    let parent = sqlx::migrate::Migrator {
        migrations: signalbox_persistence::MIGRATOR
            .iter()
            .filter(|migration| migration.version <= 202609071400)
            .cloned()
            .collect::<Vec<_>>()
            .into(),
        ..sqlx::migrate::Migrator::DEFAULT
    };
    let (container, core, url) = unmigrated_postgres().await?;
    parent.run(&core).await?;
    sqlx::query("ALTER ROLE mod_repo_watch PASSWORD 'signalbox-test-only'")
        .execute(&core)
        .await?;
    let module = module_pool(&url).await?;
    let command = DurableCommandId::from_uuid(Uuid::now_v7());
    let head = CommitSha::try_new("a".repeat(40))?;
    signalbox_persistence::test_support::seed_historical_repository_checkout(
        &module, command, &head,
    )
    .await?;

    migrate(&core).await?;
    let checkout: (String, String, bool, bool, bool) = sqlx::query_as(
        "SELECT checkout_path, checkout_head_sha, checkout_removed,
                checkout_workspace_root IS NULL, checkout_session_id IS NULL
         FROM dispatch_ledger WHERE command_id = $1",
    )
    .bind(command.into_uuid())
    .fetch_one(&module)
    .await?;
    assert_eq!(
        checkout,
        (
            String::from("."),
            head.as_str().to_owned(),
            true,
            true,
            true
        )
    );
    assert!(
        RepoWatchStore::new(module.clone())
            .checkout_removal_candidates()
            .await?
            .is_empty()
    );
    migrate(&core).await?;
    module.close().await;
    core.close().await;
    drop(container);
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
async fn approval_judge_loads_repo_watch_authority_before_ledger_settlement()
-> Result<(), Box<dyn Error>> {
    use signalbox_application::{
        ApprovalJudgeDispatchAuthority, ApprovalJudgeDispatchProvenance,
        ApprovalJudgePullRequestAuthority, ApprovalJudgePullRequestAuthorityInput,
    };
    use signalboxd::repo_watch_runtime::{RepositoryWatchRuntime, RepositoryWatchServices};

    let mut fixture = CheckoutFixture::with_head_repository("contributor/project").await?;
    fixture.submit_without_lifecycle_settlement().await;
    let checkout = fixture
        .store
        .dispatch_checkout(fixture.command)
        .await?
        .expect("the dispatch retained its checkout");
    let session = checkout.location.expect("checkout has a session").session;
    assert!(
        fixture
            .store
            .reaction_origin_for_session(session)
            .await?
            .is_none(),
        "the lifecycle-created session has not settled on the module ledger",
    );
    let templates = signalboxd::SessionTemplateConfiguration::read(
        &fixture._files.path().join("templates.toml"),
        || None,
        &fixture.sink.models,
    )?;
    let runtime = RepositoryWatchRuntime::unstarted(
        fixture.module.clone(),
        RepositoryWatchServices {
            goal_resumption: fixture.sink.goal_resumption.clone(),
            checkout_runner: None,
            core_pool: fixture.core.clone(),
            models: fixture.sink.models.clone(),
            templates: Arc::new(templates),
            eligibility_nudge: fixture.sink.eligibility_nudge.clone(),
            tool_dispatch_gate: fixture.sink.tool_dispatch_gate.clone(),
        },
    );
    assert_eq!(
        runtime.approval_judge_authority(session).await?,
        Some(ApprovalJudgeDispatchAuthority::PullRequest(
            ApprovalJudgePullRequestAuthority::new(ApprovalJudgePullRequestAuthorityInput {
                dispatch: ApprovalJudgeDispatchProvenance::RepoWatch(checkout.dispatch),
                repository: RepositorySlug::try_new(String::from("checkout/project"))?,
                pull_request: PullRequestNumber::new(NonZeroU64::MIN),
                head_sha: fixture.head.clone(),
                head_repository: RepositorySlug::try_new(String::from("contributor/project"))?,
                head_branch: BranchName::try_new(String::from("review"))?,
                base_branch: BranchName::try_new(String::from("main"))?,
            }),
        )),
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL"]
async fn disabled_runtime_scavenges_checkouts_without_submitting_pending_commands()
-> Result<(), Box<dyn Error>> {
    use signalboxd::repo_watch_runtime::{RepositoryWatchRuntime, RepositoryWatchServices};
    use sqlx::Connection as _;

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
            goal_resumption: fixture.sink.goal_resumption.clone(),
            checkout_runner: None,
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
    let mut module = sqlx::PgConnection::connect_with(&fixture.module.connect_options()).await?;
    let pending: bool =
        sqlx::query_scalar("SELECT submission_pending FROM dispatch_ledger WHERE command_id = $1")
            .bind(fixture.command.into_uuid())
            .fetch_one(&mut module)
            .await?;
    assert!(
        pending,
        "disabled runtime must not submit retained commands"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL"]
async fn startup_removes_staging_before_ownership_retention() -> Result<(), Box<dyn Error>> {
    use signalbox_module_repo_watch_v2::dispatch::{CommandSubmission, SessionCommandSink};
    use signalboxd::repo_watch_dispatch::scavenge_checkouts;
    use std::os::unix::ffi::OsStrExt;

    let mut fixture = CheckoutFixture::new().await?;
    let pending = fixture
        .store
        .recover_pending_commands(&mut RepositoryWatchCommandCodec)
        .await?;
    sqlx::query("UPDATE dispatch_ledger SET submission_pending = true WHERE command_id = $1")
        .bind(fixture.command.into_uuid())
        .execute(&fixture.module)
        .await?;
    let result = fixture
        .sink
        .submit(pending[0].command().clone())
        .await
        .expect("held core creation");
    let CommandSubmission::Creation(CreateSessionOutcome::Applied(applied)) = result else {
        panic!("core creation must be applied");
    };
    let session = applied.session();
    fixture
        .store
        .retain_checkout_location(
            fixture.command,
            session,
            fixture
                .sink
                .models
                .daemon_tools()
                .expect("tools")
                .workspace_root()
                .as_os_str()
                .as_bytes(),
        )
        .await?;
    let root = fixture
        .root(session)
        .with_file_name(format!(".checkout-{}", pending[0].dispatch().into_uuid()));
    std::fs::create_dir_all(&root)?;
    let restarted = RepoWatchStore::new(fixture.module.clone());
    scavenge_checkouts(&restarted, &fixture.core)
        .await
        .expect("active prepared directory retained");
    assert!(root.is_dir());
    fixture.stop(session).await;
    scavenge_checkouts(&restarted, &fixture.core)
        .await
        .expect("terminal prepared directory removed");
    assert!(!root.exists());
    let flags: (bool, bool, bool, bool) = sqlx::query_as(
        "SELECT submission_pending, checkout_removed,
                checkout_device IS NULL AND checkout_inode IS NULL,
                checkout_path IS NULL AND created_session_id IS NULL
         FROM dispatch_ledger WHERE command_id = $1",
    )
    .bind(fixture.command.into_uuid())
    .fetch_one(&fixture.module)
    .await?;
    assert_eq!(flags, (true, true, true, true));
    scavenge_checkouts(&restarted, &fixture.core)
        .await
        .expect("repeated cleanup is idempotent");
    assert!(restarted.checkout_removal_candidates().await?.is_empty());
    assert!(fixture.runner.steps.lock().expect("Git steps").is_empty());
    Ok(())
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL"]
async fn pending_replay_preserves_cleanup_of_an_unrecorded_checkout() -> Result<(), Box<dyn Error>>
{
    use signalboxd::repo_watch_dispatch::scavenge_checkouts;

    let mut fixture = CheckoutFixture::new().await?;
    fixture.submit_without_lifecycle_settlement().await;
    let session = fixture
        .store
        .dispatch_checkout(fixture.command)
        .await?
        .expect("checkout row")
        .location
        .expect("retained location")
        .session;
    // Filesystem work survived the crash; checkout and command settlement did not.
    sqlx::query("UPDATE dispatch_ledger SET checkout_path = NULL, checkout_head_sha = NULL, submission_pending = true WHERE command_id = $1")
        .bind(fixture.command.into_uuid()).execute(&fixture.module).await?;
    let root = fixture.root(session);
    let restarted = RepoWatchStore::new(fixture.module.clone());
    scavenge_checkouts(&restarted, &fixture.core)
        .await
        .expect("active checkout retained before path recording");
    assert!(root.join(".git").is_dir());
    fixture.stop(session).await;
    scavenge_checkouts(&restarted, &fixture.core)
        .await
        .expect("terminal checkout removed without path recording");
    assert!(!root.exists());
    let flags: (bool, bool, bool, bool) = sqlx::query_as(
        "SELECT submission_pending, checkout_removed, checkout_path IS NULL,
                created_session_id IS NULL FROM dispatch_ledger WHERE command_id = $1",
    )
    .bind(fixture.command.into_uuid())
    .fetch_one(&fixture.module)
    .await?;
    assert_eq!(flags, (true, true, true, true));
    scavenge_checkouts(&restarted, &fixture.core)
        .await
        .expect("repeated cleanup is idempotent");
    assert!(restarted.checkout_removal_candidates().await?.is_empty());
    let steps = fixture.runner.steps.lock().expect("Git steps").clone();
    fixture.submit_without_lifecycle_settlement().await;
    assert_eq!(*fixture.runner.steps.lock().expect("Git steps"), steps);
    assert!(
        !root.exists(),
        "pending replay must not recreate the checkout"
    );
    let flags: (bool, bool, bool, bool) = sqlx::query_as(
        "SELECT submission_pending, checkout_removed, checkout_path IS NULL,
                created_session_id IS NULL FROM dispatch_ledger WHERE command_id = $1",
    )
    .bind(fixture.command.into_uuid())
    .fetch_one(&fixture.module)
    .await?;
    assert_eq!(flags, (false, true, true, true));
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
async fn dispatch_never_adopts_or_removes_a_preexisting_directory() -> Result<(), Box<dyn Error>> {
    use signalbox_module_repo_watch_v2::dispatch::{CommandSubmission, SessionCommandSink};
    use signalboxd::repo_watch_dispatch::scavenge_checkouts;

    let mut fixture = CheckoutFixture::new().await?;
    let pending = fixture
        .store
        .recover_pending_commands(&mut RepositoryWatchCommandCodec)
        .await?;
    let result = fixture
        .sink
        .submit(pending[0].command().clone())
        .await
        .expect("held creation");
    let CommandSubmission::Creation(CreateSessionOutcome::Applied(applied)) = result else {
        panic!("core creation must be applied");
    };
    let root = fixture.root(applied.session());
    git2::Repository::init(&root)?;
    std::fs::write(root.join("keep"), "preexisting contents")?;
    fixture.dispatch().await;
    scavenge_checkouts(&fixture.store, &fixture.core)
        .await
        .expect("unowned cleanup settles without deletion");
    assert_eq!(
        std::fs::read_to_string(root.join("keep"))?,
        "preexisting contents"
    );
    assert!(root.join(".git").is_dir());
    assert!(fixture.runner.steps.lock().expect("Git steps").is_empty());
    let flags: (bool, bool, bool) = sqlx::query_as(
        "SELECT checkout_created, checkout_removed, checkout_device IS NULL AND checkout_inode IS NULL
         FROM dispatch_ledger WHERE command_id = $1",
    ).bind(fixture.command.into_uuid()).fetch_one(&fixture.module).await?;
    assert_eq!(flags, (false, true, true));
    Ok(())
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL"]
async fn cleanup_preserves_reused_inode_without_dispatch_marker() -> Result<(), Box<dyn Error>> {
    assert_cleanup_preserves_reused_inode(ReusedMarker::Absent, RemovalLocation::Sibling).await
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL"]
async fn cleanup_preserves_reused_inode_with_another_dispatch_marker() -> Result<(), Box<dyn Error>>
{
    assert_cleanup_preserves_reused_inode(ReusedMarker::AnotherDispatch, RemovalLocation::Sibling)
        .await
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL"]
async fn cleanup_preserves_original_path_without_dispatch_marker() -> Result<(), Box<dyn Error>> {
    assert_cleanup_preserves_reused_inode(ReusedMarker::Absent, RemovalLocation::Original).await
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL"]
async fn cleanup_preserves_original_path_with_another_dispatch_marker() -> Result<(), Box<dyn Error>>
{
    assert_cleanup_preserves_reused_inode(ReusedMarker::AnotherDispatch, RemovalLocation::Original)
        .await
}

enum ReusedMarker {
    Absent,
    AnotherDispatch,
}

enum RemovalLocation {
    Original,
    Sibling,
}

async fn assert_cleanup_preserves_reused_inode(
    marker: ReusedMarker,
    location: RemovalLocation,
) -> Result<(), Box<dyn Error>> {
    use signalboxd::repo_watch_dispatch::scavenge_checkouts;
    use std::os::unix::fs::MetadataExt;

    let mut fixture = CheckoutFixture::new().await?;
    fixture.dispatch().await;
    let session = fixture.session().await;
    let root = fixture.root(session);
    let dispatch = fixture
        .store
        .dispatch_checkout(fixture.command)
        .await?
        .expect("checkout")
        .dispatch;
    assert_eq!(
        std::fs::read_to_string(root.join(".git/signalbox-dispatch"))?,
        dispatch.into_uuid().to_string()
    );
    std::fs::remove_dir_all(&root)?;
    let unrelated = match location {
        RemovalLocation::Original => root.clone(),
        RemovalLocation::Sibling => root.with_file_name("unrelated-checkout"),
    };
    std::fs::create_dir_all(unrelated.join(".git"))?;
    std::fs::write(unrelated.join("keep"), b"unrelated contents")?;
    if matches!(marker, ReusedMarker::AnotherDispatch) {
        std::fs::write(
            unrelated.join(".git/signalbox-dispatch"),
            Uuid::now_v7().to_string(),
        )?;
    }
    let identity = std::fs::metadata(&unrelated)?;
    // Model inode reuse deterministically instead of depending on allocator timing.
    sqlx::query("UPDATE dispatch_ledger SET checkout_device = $2::bigint, checkout_inode = $3::bigint WHERE command_id = $1")
        .bind(fixture.command.into_uuid())
        .bind(i64::try_from(identity.dev())?)
        .bind(i64::try_from(identity.ino())?)
        .execute(&fixture.module).await?;
    fixture.stop(session).await;
    let restarted = RepoWatchStore::new(fixture.module.clone());
    scavenge_checkouts(&restarted, &fixture.core)
        .await
        .expect("missing checkout settled");
    assert_eq!(
        std::fs::read(unrelated.join("keep"))?,
        b"unrelated contents"
    );
    assert!(restarted.checkout_removal_candidates().await?.is_empty());
    Ok(())
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL"]
async fn cleanup_restores_search_permission_before_reading_a_renamed_marker()
-> Result<(), Box<dyn Error>> {
    use signalboxd::repo_watch_dispatch::scavenge_checkouts;
    use std::os::unix::fs::PermissionsExt;

    let mut fixture = CheckoutFixture::new().await?;
    fixture.dispatch().await;
    let session = fixture.session().await;
    let root = fixture.root(session);
    let renamed = root.with_file_name("unsearchable-checkout");
    std::fs::rename(&root, &renamed)?;
    std::fs::set_permissions(renamed.join(".git"), std::fs::Permissions::from_mode(0o000))?;
    std::fs::set_permissions(&renamed, std::fs::Permissions::from_mode(0o000))?;
    fixture.stop(session).await;
    scavenge_checkouts(&fixture.store, &fixture.core)
        .await
        .expect("search permission restored before marker lookup");
    assert!(!renamed.exists());
    assert!(
        fixture
            .store
            .checkout_removal_candidates()
            .await?
            .is_empty()
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL"]
async fn cleanup_finds_a_renamed_checkout_among_its_siblings() -> Result<(), Box<dyn Error>> {
    use signalboxd::repo_watch_dispatch::scavenge_checkouts;

    let mut fixture = CheckoutFixture::new().await?;
    fixture.dispatch().await;
    let session = fixture.session().await;
    let root = fixture.root(session);
    let renamed = root.with_file_name("renamed-checkout");
    let unrelated = root.with_file_name("unrelated");
    std::fs::create_dir(&unrelated)?;
    std::fs::write(unrelated.join("keep"), "unrelated directory")?;
    std::fs::rename(&root, &renamed)?;
    fixture.stop(session).await;
    let restarted = RepoWatchStore::new(fixture.module.clone());
    scavenge_checkouts(&restarted, &fixture.core)
        .await
        .expect("renamed checkout removed by identity");
    assert!(!renamed.exists());
    assert!(!root.exists());
    assert!(root.parent().expect("derived parent").is_dir());
    assert_eq!(
        std::fs::read_to_string(unrelated.join("keep"))?,
        "unrelated directory"
    );
    assert!(restarted.checkout_removal_candidates().await?.is_empty());
    scavenge_checkouts(&restarted, &fixture.core)
        .await
        .expect("repeated cleanup is idempotent");
    Ok(())
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL"]
async fn cleanup_rejects_a_different_directory_at_the_provisioned_path()
-> Result<(), Box<dyn Error>> {
    use signalboxd::repo_watch_dispatch::scavenge_checkouts;
    use std::os::unix::fs::PermissionsExt;

    let mut fixture = CheckoutFixture::new().await?;
    fixture.dispatch().await;
    let session = fixture.session().await;
    let root = fixture.root(session);
    let retained = fixture._files.path().join("retained");
    std::fs::rename(&root, &retained)?;
    std::fs::create_dir(&root)?;
    std::fs::write(root.join("keep"), "substituted directory")?;
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o500))?;
    fixture.stop(session).await;
    let restarted = RepoWatchStore::new(fixture.module.clone());
    scavenge_checkouts(&restarted, &fixture.core)
        .await
        .expect("identity mismatch leaves removal pending");
    assert_eq!(
        std::fs::read_to_string(root.join("keep"))?,
        "substituted directory"
    );
    assert_eq!(
        std::fs::metadata(&root)?.permissions().mode() & 0o777,
        0o500
    );
    assert_eq!(
        std::fs::read_to_string(retained.join("review.txt"))?,
        "retained head\n"
    );
    assert_eq!(restarted.checkout_removal_candidates().await?.len(), 1);

    let substitute = fixture._files.path().join("substitute");
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700))?;
    std::fs::rename(&root, &substitute)?;
    std::fs::rename(&retained, &root)?;
    scavenge_checkouts(&restarted, &fixture.core)
        .await
        .expect("restored original identity permits removal");
    assert!(!root.exists());
    assert!(restarted.checkout_removal_candidates().await?.is_empty());
    assert_eq!(
        std::fs::read_to_string(substitute.join("keep"))?,
        "substituted directory"
    );
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
    let evidence = recorded.take();
    assert!(
        matches!(evidence, Some(ToolExecutorEvidence::CompletedText(_))),
        "git_status evidence: {evidence:?}"
    );
}

impl CheckoutFixture {
    async fn commission_push(&mut self) -> Result<SessionId, Box<dyn Error>> {
        use signalbox_application::{
            CommissionDispatchRequest, CommissionedDispatchFence,
            UuidV7CommissionedDispatchIdGenerator, UuidV7SubmitInputIdGenerator,
        };
        use signalbox_persistence::commissioned_dispatch::{
            CommissionDispatchOutcome, PostgresCommissionedDispatchStore,
        };
        let credential = self._files.path().join("push-token");
        let catalog = self
            .catalog
            .split("[[repository_watch.rules]]")
            .next()
            .expect("catalog prefix")
            .replace("enabled = true", "enabled = false")
            .replace(
                "\ncredential_file =",
                &format!(
                    "\npush_credential_file = \"{}\"\ncredential_file =",
                    credential.display()
                ),
            );
        self.sink.models = Arc::new(HubModelConfiguration::parse(&catalog)?);
        let templates = signalboxd::SessionTemplateConfiguration::read(
            &self._files.path().join("templates.toml"),
            || None,
            &self.sink.models,
        )?;
        let name = signalbox_domain::SessionTemplateName::try_new("watch".to_owned())?;
        let template = templates.resolve(&name).expect("commission template");
        let request = CommissionDispatchRequest::try_new(
            DurableCommandId::from_uuid(Uuid::now_v7()),
            name,
            CommissionedDispatchFence::PullRequest {
                repository: RepositorySlug::try_new("checkout/project".to_owned())?,
                pull_request: PullRequestNumber::new(
                    NonZeroU64::new(1).expect("positive fixture PR"),
                ),
                head_sha: self.head.clone(),
                head_repository: RepositorySlug::try_new("checkout/project".to_owned())?,
                head_branch: BranchName::try_new("review".to_owned())?,
                base_branch: BranchName::try_new("main".to_owned())?,
            },
            signalbox_domain::GoalStatement::try_new("Push the reviewed change.".to_owned())?,
            signalbox_domain::UserContent::try_text("Commit and push to review.".to_owned())
                .expect("fixture input is admitted"),
        )?;
        let prepared = request.prepare(
            &mut UuidV7CommissionedDispatchIdGenerator,
            template.provenance().clone(),
            template.defaults().clone(),
        )?;
        let outcome = PostgresCommissionedDispatchStore::new(
            self.core.clone(),
            self.sink.models.session_credential_pin(),
        )
        .commission(prepared, &mut UuidV7SubmitInputIdGenerator, |alias| {
            self.sink.models.resolve_alias(alias)
        })
        .await?;
        let CommissionDispatchOutcome::Dispatched { session, .. } = outcome else {
            panic!("fresh commission: {outcome:?}");
        };
        git2::Repository::clone(
            self.runner.bare.to_str().expect("fixture remote"),
            self.root(session),
        )?;
        let dispatched: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM dispatch_ledger WHERE created_session_id = $1",
        )
        .bind(session.into_uuid())
        .fetch_one(&self.module)
        .await?;
        assert_eq!(dispatched, 0);
        Ok(session)
    }

    async fn push_tools(
        &self,
    ) -> Result<
        (
            signalboxd::DaemonToolCatalog,
            impl signalbox_application::ToolExecutor<Error = signalboxd::DaemonToolExecutorError>
            + Clone
            + Send
            + use<>,
        ),
        Box<dyn Error>,
    > {
        use signalboxd::repo_watch_runtime::{RepositoryWatchRuntime, RepositoryWatchServices};
        use signalboxd::{
            DaemonTools, FileCredentialAccess, MappedDaemonCredentialInputs,
            PostgresConversationIntrospection, PostgresSessionStatusWriter,
        };
        let configuration = self.sink.models.daemon_tools().expect("tools configured");
        let credentials = FileCredentialAccess::new(
            self._files.path().join("unused"),
            signalbox_model_runtime::CredentialReference::new("unused"),
        );
        let tools = DaemonTools::try_new(
            signalbox_tools_basic::SystemCurrentTimeClock,
            signalbox_tools_web::ReqwestWebFetchTransport::try_new(
                std::time::Duration::from_secs(10),
            )?,
            MappedDaemonCredentialInputs {
                web_search: credentials.clone(),
                code_host: credentials.clone(),
                github: credentials,
            },
            signalbox_tools_web::ReqwestWebSearchTransport::try_new(
                std::time::Duration::from_secs(10),
            )?,
            PostgresSessionStatusWriter::new(self.core.clone()),
            signalbox_tools_code_host::GitHubCodeHostTransport::try_new(
                signalbox_tools_code_host::CodeHostNumericBounds::new(
                    None, None, None, None, None, None,
                ),
            )?,
            signalbox_tools_github::GitHubApiTransport::try_new()?,
            configuration.github_egress_policy(),
            signalboxd::PinnedWorkspaceFileSystem::try_new(configuration.workspace_root())?,
            configuration.workspace_root(),
            configuration.git_identity().clone(),
            self.runner.clone(),
            PostgresConversationIntrospection::new(self.core.clone()),
            signalbox_persistence::plan::SessionPlanRepository::new(self.core.clone()),
            self.sink.models.web_fetch_egress_policy(),
        )?;
        let watch = RepositoryWatchRuntime::new(
            self.module.clone(),
            self.sink.models.repository_watch().cloned(),
            RepositoryWatchServices {
                goal_resumption: self.sink.goal_resumption.clone(),
                core_pool: self.core.clone(),
                checkout_runner: None,
                models: self.sink.models.clone(),
                templates: Arc::new(signalboxd::SessionTemplateConfiguration::read(
                    &self._files.path().join("templates.toml"),
                    || None,
                    &self.sink.models,
                )?),
                eligibility_nudge: self.sink.eligibility_nudge.clone(),
                tool_dispatch_gate: self.sink.tool_dispatch_gate.clone(),
            },
        )
        .await
        .map_err(|error| format!("watch construction: {error:?}"))?;
        let (catalog, executor) = tools.into_parts();
        Ok((
            catalog.with_repository_push(
                self.sink.models.repository_watch(),
                signalboxd::DaemonToolComposition::WithMappedFamilies,
            )?,
            executor.with_repository_watch(Some(watch)),
        ))
    }

    fn daemon_tools(
        &self,
    ) -> Result<
        (
            signalboxd::DaemonToolCatalog,
            impl signalbox_application::ToolExecutor<Error = signalboxd::DaemonToolExecutorError>
            + Clone
            + Send
            + use<>,
        ),
        Box<dyn Error>,
    > {
        use signalbox_tools_code_host::{CodeHostNumericBounds, GitHubCodeHostTransport};
        use signalboxd::{DaemonTools, FileCredentialAccess, MappedDaemonCredentialInputs};
        let configuration = self.sink.models.daemon_tools().expect("tool configuration");
        let unused_credentials = FileCredentialAccess::new(
            self._files.path().join("unused-tool-credential"),
            signalbox_model_runtime::CredentialReference::new("unused-checkout-tool-credential"),
        );
        let (nudge, _) =
            InProcessEligibilityWorkSource::new(PostgresEligibilitySweep::new(self.core.clone()));
        let tools = DaemonTools::try_new_production(
            signalbox_tools_basic::SystemCurrentTimeClock,
            self.core.clone(),
            nudge,
            MappedDaemonCredentialInputs {
                web_search: unused_credentials.clone(),
                code_host: unused_credentials.clone(),
                github: unused_credentials,
            },
            GitHubCodeHostTransport::try_new(CodeHostNumericBounds::new(
                None, None, None, None, None, None,
            ))?,
            configuration.github_egress_policy(),
            configuration.workspace_root(),
            configuration.git_identity().clone(),
            configuration.exec_supervisor_executable(),
            configuration.cargo_registry_cache(),
            configuration.sandbox(),
            self.sink
                .models
                .numeric_bounds()
                .integer("max_git_object_bytes")
                .flatten()
                .map(|bytes| bytes as usize),
            configuration.sandboxed_exec_timeout_bound(),
            self.sink.models.web_fetch_egress_policy(),
        )?;
        Ok(tools.into_parts())
    }
}

async fn run_git_tool(
    catalog: &signalboxd::DaemonToolCatalog,
    executor: &(
         impl signalbox_application::ToolExecutor<Error = signalboxd::DaemonToolExecutorError>
         + Clone
         + Send
     ),
    session: SessionId,
    turn: signalbox_domain::TurnId,
    name: &str,
    arguments: &str,
) -> serde_json::Value {
    match run_git_tool_evidence(catalog, executor, session, turn, name, arguments).await {
        signalbox_application::ToolExecutorEvidence::CompletedText(text) => {
            serde_json::from_str(&text).expect("Git result JSON")
        }
        evidence => panic!("{name} evidence: {evidence:?}"),
    }
}

async fn run_git_tool_evidence(
    catalog: &signalboxd::DaemonToolCatalog,
    executor: &(
         impl signalbox_application::ToolExecutor<Error = signalboxd::DaemonToolExecutorError>
         + Clone
         + Send
     ),
    session: SessionId,
    turn: signalbox_domain::TurnId,
    name: &str,
    arguments: &str,
) -> signalbox_application::ToolExecutorEvidence {
    use signalbox_application::*;
    use signalbox_domain::{
        ContextFrontierId, ModelCallId, ToolAttemptId, ToolRequestId, TurnAttemptId,
    };
    let name = signalbox_domain::ToolName::try_new(name.to_owned()).expect("tool name");
    let definition = catalog.definition(&name).expect("daemon Git declaration");
    let batch = prepared_single_attempt_batch(
        PreparedAttemptIdentities {
            session,
            turn,
            producing_call: ModelCallId::from_uuid(Uuid::now_v7()),
            request: ToolRequestId::from_uuid(Uuid::now_v7()),
            attempt: ToolAttemptId::from_uuid(Uuid::now_v7()),
            issuing_turn_attempt: TurnAttemptId::from_uuid(Uuid::now_v7()),
            frontier: ContextFrontierId::from_uuid(Uuid::now_v7()),
        },
        PreparedAttemptProposal {
            name: name.clone(),
            arguments: signalbox_domain::NormalizedToolArguments::try_from_provider_text(
                arguments.to_owned(),
            )
            .expect("arguments"),
            effect_class: definition.effect_class(),
            approval: PreparedAttemptApproval::UserConfirmation {
                command: DurableCommandId::from_uuid(Uuid::now_v7()),
            },
        },
    );
    let (executor, recorded) = RecordingToolExecutor::new(executor.clone());
    let mut service = ToolExecutionService::new(
        UuidV7ToolLoopIdGenerator,
        FixtureToolExecutionTransaction::new(
            batch.clone(),
            FixtureTransactionFailures {
                domain_rejection: signalbox_tools_git::LocalGitExecutorError,
                declined_crash_classification: signalbox_tools_git::LocalGitExecutorError,
            },
        ),
        catalog.clone(),
        executor,
        InProcessToolDispatchGate::default(),
    );
    service
        .execute(session, turn)
        .await
        .expect("execute daemon Git tool");
    recorded.take().expect("Git execution evidence")
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL"]
async fn dispatched_session_projects_retained_origin_after_rule_removal()
-> Result<(), Box<dyn Error>> {
    let mut fixture = CheckoutFixture::new().await?;
    let planned = fixture
        .store
        .recover_pending_commands(&mut RepositoryWatchCommandCodec)
        .await?[0]
        .clone();
    fixture.dispatch().await;
    let session = fixture.session().await;
    let stored = signalbox_persistence::session::SessionRepository::new(fixture.core.clone())
        .load_session(session)
        .await?
        .expect("created session");
    assert_eq!(
        stored.creation_provenance().cause(),
        SessionCreationCause::ModuleDispatched {
            dispatch: ModuleDispatch::RepositoryWatch {
                dispatch: planned.dispatch()
            }
        }
    );
    let actor: (String, Option<String>) = sqlx::query_as(
        "SELECT actor_kind, actor_module FROM session_lifecycle WHERE session_id = $1",
    )
    .bind(session.into_uuid())
    .fetch_one(&fixture.core)
    .await?;
    assert_eq!(
        actor,
        (String::from("module"), Some(String::from("repo_watch")))
    );
    fixture
        .store
        .reconcile_rules(
            &[RepositoryRuleSet::new(planned.repository(), &[])],
            OffsetDateTime::now_utc(),
        )
        .await?;
    let restarted = RepoWatchStore::new(fixture.module.clone());
    let origin = restarted
        .reaction_origin_for_session(session)
        .await?
        .expect("retained origin");
    assert_eq!(origin.dispatch(), planned.dispatch());
    assert_eq!(origin.event_id(), planned.event_id());
    assert_eq!(origin.rule_id(), planned.rule_id());
    assert_eq!(origin.action_ordinal().get(), planned.action_ordinal());
    assert_eq!(origin.pull_request().map(PullRequestNumber::get), Some(1));
    assert_eq!(
        origin.event_kind(),
        RepoWatchEventKindNameV1::PullRequestOpened
    );

    assert_projected_origin(&fixture, &planned, session).await
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL"]
async fn dispatched_session_projects_origin_before_ledger_settlement() -> Result<(), Box<dyn Error>>
{
    let mut fixture = CheckoutFixture::new().await?;
    let planned = fixture
        .store
        .recover_pending_commands(&mut RepositoryWatchCommandCodec)
        .await?[0]
        .clone();
    fixture.submit_without_lifecycle_settlement().await;
    let session = SessionId::from_uuid(
        sqlx::query_scalar(
            "SELECT created_session_id FROM create_session_command WHERE command_id = $1",
        )
        .bind(fixture.command.into_uuid())
        .fetch_one(&fixture.core)
        .await?,
    );
    let ledger: (String, Option<Uuid>) = sqlx::query_as(
        "SELECT status, created_session_id FROM dispatch_ledger WHERE command_id = $1",
    )
    .bind(fixture.command.into_uuid())
    .fetch_one(&fixture.module)
    .await?;
    assert_eq!(ledger, (String::from("pending"), None));
    assert_projected_origin(&fixture, &planned, session).await?;
    fixture.settle().await;
    assert_eq!(fixture.session().await, session);
    assert_projected_origin(&fixture, &planned, session).await
}

async fn assert_projected_origin(
    fixture: &CheckoutFixture,
    planned: &signalbox_module_repo_watch_v2::PlannedCommand,
    session: SessionId,
) -> Result<(), Box<dyn Error>> {
    use signalbox_process_protocol::{
        CanonicalUuid, ClientFrame, ClientRequest, ProtocolVersion, RequestId, ServerMessage,
        decode_server_line, encode_client_line,
    };
    use signalbox_web_contract::WebSessionTimelineDescriptor;
    use signalboxd::{
        LocalProcessListener, ProcessRuntime,
        repo_watch_runtime::{RepositoryWatchRuntime, RepositoryWatchServices},
    };
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tower::ServiceExt;

    use signalbox_domain::SessionWorkspaceRootKind;
    use signalbox_persistence::session_workspace::{WorkspaceRootBinding, record_binding};
    assert_eq!(
        record_binding(&fixture.core, session, WorkspaceRootBinding::Configured).await?,
        SessionWorkspaceRootKind::Configured
    );
    assert_eq!(
        record_binding(
            &fixture.core,
            session,
            WorkspaceRootBinding::Derived {
                dispatch_marker: None
            }
        )
        .await?,
        SessionWorkspaceRootKind::Derived
    );
    assert_eq!(
        record_binding(
            &fixture.core,
            session,
            WorkspaceRootBinding::Derived {
                dispatch_marker: Some(planned.dispatch())
            }
        )
        .await?,
        SessionWorkspaceRootKind::Provisioned
    );
    let models = (*fixture.sink.models).clone();
    let templates = signalboxd::SessionTemplateConfiguration::default();
    let watch = RepositoryWatchRuntime::unstarted(
        fixture.module.clone(),
        RepositoryWatchServices {
            goal_resumption: fixture.sink.goal_resumption.clone(),
            core_pool: fixture.core.clone(),
            checkout_runner: None,
            models: Arc::new(models.clone()),
            templates: Arc::new(templates.clone()),
            eligibility_nudge: fixture.sink.eligibility_nudge.clone(),
            tool_dispatch_gate: fixture.sink.tool_dispatch_gate.clone(),
        },
    );
    let reload = signalboxd::configuration_reload::ConfigurationReload::new(
        fixture.core.clone(),
        models.clone(),
        templates,
        fixture._files.path().join("models.toml"),
        fixture._files.path().join("templates.toml"),
        None,
    )
    .expect("reload composition")
    .with_repository_watch(watch);
    let router = signalboxd::web_http::production_router(
        None,
        Some(fixture.core.clone()),
        None,
        Some(models.clone()),
        None,
        None,
        None,
    )
    .layer(axum::Extension(reload.clone()));
    let response = router
        .oneshot(
            axum::http::Request::builder()
                .uri(format!("/api/sessions/{}", session.into_uuid()))
                .header("host", "127.0.0.1")
                .body(axum::body::Body::empty())?,
        )
        .await?;
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await?;
    let descriptor: WebSessionTimelineDescriptor = serde_json::from_slice(&bytes)?;
    assert_eq!(
        descriptor.workspace_root_kind,
        Some(signalbox_web_contract::WebSessionWorkspaceRootKind::Provisioned)
    );
    let web = descriptor.repository_watch.expect("browser origin");
    assert_eq!(
        serde_json::to_value(&web.dispatch_id)?,
        planned.dispatch().into_uuid().to_string()
    );
    assert_eq!(
        serde_json::to_value(&web.event_id)?,
        planned.event_id().into_uuid().to_string()
    );
    assert_eq!(
        web.action_ordinal.as_str(),
        planned.action_ordinal().to_string()
    );
    assert_eq!(web.rule_id, planned.rule_id().as_str());
    assert_eq!(web.repository, planned.repository().as_str());
    assert_eq!(
        web.pull_request.as_ref().map(|number| number.as_str()),
        Some("1")
    );

    let sockets = tempfile::tempdir_in("/tmp")?;
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(sockets.path(), std::fs::Permissions::from_mode(0o700))?;
    let socket = sockets.path().join("hub.sock");
    let runtime = ProcessRuntime::new(
        LocalProcessListener::bind(&socket)?,
        fixture.core.clone(),
        fixture.sink.eligibility_nudge.clone(),
        fixture.sink.tool_dispatch_gate.clone(),
        models,
    )
    .with_configuration_reload(reload);
    let (shutdown, receiver) = tokio::sync::watch::channel(false);
    let task = tokio::spawn(runtime.run(receiver));
    let stream = tokio::net::UnixStream::connect(&socket).await?;
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let request = ClientFrame::try_new_for_version(
        ProtocolVersion::One,
        RequestId::try_new(1)?,
        ClientRequest::ReadTranscript {
            session_id: CanonicalUuid::from_uuid(session.into_uuid()),
        },
    )?;
    writer.write_all(&encode_client_line(&request)?).await?;
    let mut line = Vec::new();
    tokio::time::timeout(Duration::from_secs(30), reader.read_until(b'\n', &mut line)).await??;
    let frame = decode_server_line(&line)?;
    let ServerMessage::TranscriptSnapshotStart {
        workspace_root_kind: Some(signalbox_process_protocol::SessionWorkspaceRootKind::Provisioned),
        repository_watch: Some(wire),
        ..
    } = frame.message()
    else {
        panic!(
            "snapshot must expose retained origin: {:?}",
            frame.message()
        );
    };
    assert_eq!(wire.dispatch_id.into_uuid(), planned.dispatch().into_uuid());
    assert_eq!(wire.event_id.into_uuid(), planned.event_id().into_uuid());
    assert_eq!(wire.action_ordinal.value(), planned.action_ordinal());
    assert_eq!(wire.rule_id, planned.rule_id().as_str());
    assert_eq!(wire.repository, planned.repository().as_str());
    assert_eq!(wire.pull_request.map(|number| number.value()), Some(1));
    shutdown.send(true)?;
    drop(writer);
    drop(reader);
    task.await??;
    Ok(())
}
