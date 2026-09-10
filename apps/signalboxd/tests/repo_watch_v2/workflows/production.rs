//! Registered programs reach the production repository-watch command services.
//! Exercises docs/spec/workflows.md and docs/spec/repo-watch.md.

use super::*;
use signalbox_application::{InProcessEligibilityWorkSource, InProcessToolDispatchGate};
use signalbox_persistence::scheduler::PostgresEligibilitySweep;
use signalboxd::{
    SessionTemplateConfiguration,
    repo_watch_runtime::{
        RepositoryWatchRuntime, RepositoryWatchServices, connect_repository_watch_pool,
    },
    workflows::WorkflowRuntime,
};
use std::sync::Arc;

// Bounds only a stuck fixture; completion is observed from durable state.
const WORKFLOW_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, Debug)]
enum ReceiptRunEnd {
    Stopped,
    Cancelled,
    Faulted,
    Completed,
}

async fn finish_receipt_run(
    journals: &ProgramJournalRepository,
    run: ProgramRunId,
    end: ReceiptRunEnd,
) -> Result<(), Box<dyn Error>> {
    use signalbox_domain::{DeliveryKind, ProgramFault, RequestKind};
    let evidence = InlineFramePayload::new(b"receipt fixture".as_slice());
    match end {
        ReceiptRunEnd::Stopped => {}
        ReceiptRunEnd::Cancelled => {
            journals
                .append_delivery(run, DeliveryKind::RunCancel(evidence))
                .await?;
        }
        ReceiptRunEnd::Faulted => {
            journals
                .append_delivery(
                    run,
                    DeliveryKind::Fault(ProgramFault::ProgramError(evidence)),
                )
                .await?;
        }
        ReceiptRunEnd::Completed => {
            let terminal = journals
                .append_request(run, None, RequestKind::Terminal(evidence))
                .await?;
            journals
                .append_delivery(
                    run,
                    DeliveryKind::Answer {
                        resolves: terminal.ordinal(),
                        payload: InlineFramePayload::default(),
                    },
                )
                .await?;
        }
    }
    Ok(())
}

async fn wait_for_receipt_release(store: &RepoWatchStore) -> Result<(), Box<dyn Error>> {
    tokio::time::timeout(WORKFLOW_TIMEOUT, async {
        while !store.evaluation_receipts().await?.is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        Ok::<_, Box<dyn Error>>(())
    })
    .await?
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL"]
async fn restart_releases_delivered_evaluations_for_every_run_outcome() -> Result<(), Box<dyn Error>>
{
    use signalbox_domain::{DeliveryKind, RequestKind};
    for end in [
        ReceiptRunEnd::Stopped,
        ReceiptRunEnd::Cancelled,
        ReceiptRunEnd::Faulted,
        ReceiptRunEnd::Completed,
    ] {
        let (_database, core, store, runtime, repository, rule, _files) =
            production_fixture().await?;
        let context = store
            .next_rule_context(&repository, &rule)
            .await?
            .expect("first event");
        let request = RepoWatchRequest::CommitEvaluation {
            effect: Uuid::now_v7(),
            plan: context.plan(),
            context: Box::new(context),
        }
        .encode()?;
        let journals = ProgramJournalRepository::new(core.clone());
        let run = start(
            &core,
            &request,
            ProgramGrants::new([ProgramCapability::RepoWatch]),
        )
        .await?;
        let frame = journals
            .append_request(run, None, RequestKind::Effect(request.clone()))
            .await?;
        let mut effects = Effects {
            store: store.clone(),
            rules: [(repository.clone(), vec![rule.clone()])].into(),
            ids: FixedDispatchIds {
                value: 100,
                calls: 0,
            },
            factory: FixtureSessionFactory {
                next_command: 200,
                model: 300,
            },
            codec: FixtureCommandCodec,
            sink: ConflictingSink::default(),
            source: signalbox_session_ownership::LifecycleEventSource::new(core.clone()),
        };
        let answer = effects
            .execute(EffectInvocation {
                run,
                ordinal: frame.ordinal(),
                request: &request,
            })
            .await?;
        effects.acknowledge_delivered_receipts(&journals).await?;
        assert_eq!(
            store.evaluation_receipts().await?.len(),
            1,
            "undelivered receipt remains adoptable: {end:?}"
        );
        journals
            .append_delivery(
                run,
                DeliveryKind::Answer {
                    resolves: frame.ordinal(),
                    payload: answer,
                },
            )
            .await?;
        finish_receipt_run(&journals, run, end).await?;
        assert!(
            store.next_rule_context(&repository, &rule).await?.is_none(),
            "receipt blocks the next event: {end:?}"
        );

        let (_service, runner) = WorkflowRuntime::new(core.clone())?;
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(runner.with_repository_watch(Some(runtime)).run(async {
            let _ = stopped.await;
        }));
        wait_for_receipt_release(&store).await?;
        assert!(
            store.next_rule_context(&repository, &rule).await?.is_some(),
            "restart releases the next event: {end:?}"
        );
        stop.send(()).expect("runner running");
        task.await??;
    }
    Ok(())
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL"]
async fn program_error_after_evaluation_answer_releases_the_next_event()
-> Result<(), Box<dyn Error>> {
    let (_database, core, store, runtime, repository, rule, _files) = production_fixture().await?;
    let context = store
        .next_rule_context(&repository, &rule)
        .await?
        .expect("first event");
    let request = RepoWatchRequest::CommitEvaluation {
        effect: Uuid::now_v7(),
        plan: context.plan(),
        context: Box::new(context),
    }
    .encode()?;
    let (service, runner) = WorkflowRuntime::new(core.clone())?;
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(runner.with_repository_watch(Some(runtime)).run(async {
        let _ = stopped.await;
    }));
    let artifact = r#"import { effect } from "@signalbox/program-sdk/v1";
export default async function(input) {
  await effect("repo-watch", "repo.commitEvaluation", input);
  throw new Error("fixture fails after durable evaluation");
}"#
    .to_owned();
    let registration = service
        .register_javascript(
            signalbox_domain::ProgramRegistrationId::from_uuid(Uuid::now_v7()),
            ProgramRegistrationRequest {
                name: "fault-after-evaluation".into(),
                revision: "fixture".into(),
                source: artifact.as_bytes().to_vec(),
                artifact,
                grants: ProgramGrants::new([ProgramCapability::RepoWatch]),
            },
        )
        .await?;
    let run = service
        .start(
            ProgramRunId::from_uuid(Uuid::now_v7()),
            registration.id,
            request.payload().as_bytes(),
        )
        .await?;
    let journals = ProgramJournalRepository::new(core);
    let terminal = tokio::time::timeout(WORKFLOW_TIMEOUT, async {
        loop {
            let journal = journals.load(run).await?.expect("admitted run");
            if let Some(terminal) = journal.terminal_delivery() {
                return Ok::<_, Box<dyn Error>>(terminal.clone());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await??;
    assert!(matches!(
        terminal.kind(),
        signalbox_domain::DeliveryKind::Fault(signalbox_domain::ProgramFault::ProgramError(_))
    ));
    wait_for_receipt_release(&store).await?;
    assert!(store.next_rule_context(&repository, &rule).await?.is_some());
    stop.send(()).expect("runner running");
    task.await??;
    Ok(())
}

async fn production_fixture() -> Result<
    (
        TestDatabase,
        PgPool,
        RepoWatchStore,
        RepositoryWatchRuntime,
        RepositorySlug,
        RepoWatchRule,
        tempfile::TempDir,
    ),
    Box<dyn Error>,
> {
    let (database, core, _) = postgres().await?;
    let module = connect_repository_watch_pool(&core)
        .await
        .expect("module login");
    let store = RepoWatchStore::new(module.clone());
    let repository = RepositorySlug::try_new("runtime/project".into())?;
    store
        .ingest_observation(
            &store.ingest_baseline(&repository).await?,
            &dispatch_observation(&repository, 1, OffsetDateTime::now_utc()),
            EventProducer::Poll,
            MERGED_RETENTION,
        )
        .await?;

    let files = tempfile::tempdir()?;
    let secret = files.path().join("hook-secret");
    write_private_credential(&secret, b"workflow-fixture-secret")?;
    let hook = RuntimeHookFixture {
        address: unused_webhook_address().await?,
        path: "/workflow",
        id: 19,
        secret: &secret,
        enabled: true,
        rule_version: 1,
        template: "watch",
        mode: "primary",
        retention: "604800s",
    };
    let models = runtime_configuration(&hook)?;
    let rule = models.repository_watch().unwrap().rules()[0].clone();
    let template_path = files.path().join("templates.toml");
    std::fs::write(
        &template_path,
        "version = 1\n[[templates]]\nname = \"watch\"\nversion = 1\nalias = \"540ce009-c2ec-4a04-b823-c411ea189778\"\ndangerous_tool_auto_approval = false\nsystem_prompt = \"Inspect repository activity.\"\n",
    )?;
    let templates = SessionTemplateConfiguration::read(&template_path, || None, &models)?;
    let (nudge, _work) =
        InProcessEligibilityWorkSource::new(PostgresEligibilitySweep::new(core.clone()));
    let repository_watch = RepositoryWatchRuntime::new(
        module.clone(),
        models.repository_watch().cloned(),
        RepositoryWatchServices {
            goal_resumption: signalboxd::PostgresGoalPassDisposition::new(
                core.clone(),
                models.clone(),
                nudge.clone(),
                signalboxd::GoalModeNumericBounds::new(None, None, None, None, None),
            ),
            checkout_runner: None,
            core_pool: core.clone(),
            models: Arc::new(models),
            templates: Arc::new(templates),
            eligibility_nudge: nudge,
            tool_dispatch_gate: InProcessToolDispatchGate::default(),
        },
    )
    .await
    .expect("production services");
    for run in [2, 3] {
        store
            .ingest_observation(
                &store.ingest_baseline(&repository).await?,
                &dispatch_observation(&repository, run, OffsetDateTime::now_utc()),
                EventProducer::Poll,
                MERGED_RETENTION,
            )
            .await?;
    }

    Ok((
        database,
        core,
        store,
        repository_watch,
        repository,
        rule,
        files,
    ))
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL"]
async fn production_runtime_reads_commits_and_submits_repository_watch_effects()
-> Result<(), Box<dyn Error>> {
    let (_database, core, store, repository_watch, repository, rule, _files) =
        production_fixture().await?;
    let (service, runner) = WorkflowRuntime::new(core.clone())?;
    let runner = runner.with_repository_watch(Some(repository_watch.clone()));
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(runner.run(async {
        let _ = stopped.await;
    }));
    let journals = ProgramJournalRepository::new(core.clone());
    let malformed_artifact = r#"import { effect } from "@signalbox/program-sdk/v1";
export default async function() {
  await effect("repo-watch", "repo.nextRuleEvent", new Uint8Array());
  return new Uint8Array();
}"#
    .to_owned();
    let malformed = service
        .register_javascript(
            signalbox_domain::ProgramRegistrationId::from_uuid(Uuid::now_v7()),
            ProgramRegistrationRequest {
                name: "malformed-repository-watch".into(),
                revision: "fixture".into(),
                source: malformed_artifact.as_bytes().to_vec(),
                artifact: malformed_artifact,
                grants: ProgramGrants::new([ProgramCapability::RepoWatch]),
            },
        )
        .await?;
    let malformed_run = service
        .start(ProgramRunId::from_uuid(Uuid::now_v7()), malformed.id, &[])
        .await?;
    tokio::time::timeout(WORKFLOW_TIMEOUT, async {
        loop {
            let journal = journals
                .load(malformed_run)
                .await?
                .expect("malformed run admitted");
            if let Some(terminal) = journal.terminal_delivery() {
                assert!(matches!(
                    terminal.kind(),
                    signalbox_domain::DeliveryKind::Fault(
                        signalbox_domain::ProgramFault::ProgramError(_)
                    )
                ));
                return Ok::<_, Box<dyn Error>>(());
            }
            assert!(
                !task.is_finished(),
                "malformed input must not stop the shared runner"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await??;
    let artifact = format!(
        r#"import {{ effect, jsonCodec }} from "@signalbox/program-sdk/v1";
const json = jsonCodec(value => value);
async function call(method, input) {{
  const reply = await effect("repo-watch", method, json.encode({{ method, ...input }}));
  if (reply.kind !== "answer") throw new Error("effect refused");
  return new Uint8Array(reply.payload);
}}
export default async function() {{
  const selection = {{ repository: "runtime/project", rule: "ci" }};
  const context = await call("repo.nextRuleEvent", selection);
  if (!context.length) throw new Error("missing event");
  const committed = await call("repo.commitEvaluation", {{ effect: "{}", context: Array.from(context), plan: ["watch"] }});
  const outcome = String.fromCharCode(...committed);
  if (!outcome.startsWith("dispatched:")) throw new Error("dispatch missing");
  const submitted = await call("repo.submitPending", {{ effect: "{}", dispatch: outcome.slice("dispatched:".length) }});
  if (String.fromCharCode(...submitted) !== "submitted") throw new Error("submission missing");
  return call("repo.nextRuleEvent", selection);
}}"#,
        Uuid::now_v7(),
        Uuid::now_v7()
    );
    let registration = service
        .register_javascript(
            signalbox_domain::ProgramRegistrationId::from_uuid(Uuid::now_v7()),
            ProgramRegistrationRequest {
                name: "production-repository-watch".into(),
                revision: "fixture".into(),
                source: artifact.as_bytes().to_vec(),
                artifact,
                grants: ProgramGrants::new([ProgramCapability::RepoWatch]),
            },
        )
        .await?;
    let run = service
        .start(
            ProgramRunId::from_uuid(Uuid::now_v7()),
            registration.id,
            &[],
        )
        .await?;
    let result = tokio::time::timeout(WORKFLOW_TIMEOUT, async {
        loop {
            let journal = journals.load(run).await?.expect("admitted run");
            if let Some(result) = journal.result()
                && store.evaluation_receipts().await?.is_empty()
                && store.submission_receipts().await?.is_empty()
            {
                return Ok::<_, Box<dyn Error>>(result.clone());
            }
            assert!(
                journal.terminal_delivery().is_none() || journal.result().is_some(),
                "unexpected terminal journal: {journal:?}"
            );
            assert!(!task.is_finished(), "production runner stopped");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await??;
    let next = RuleContext::decode(result.as_bytes())
        .expect("next event is available after durable receipt acknowledgement");
    assert_eq!(
        store.next_rule_context(&repository, &rule).await?,
        Some(next)
    );
    let created: i64 = sqlx::query_scalar("SELECT count(*) FROM create_session_command")
        .fetch_one(&core)
        .await?;
    assert_eq!(created, 1, "submission reaches the core creation handler");
    stop.send(()).expect("runner running");
    task.await??;
    Ok(())
}
