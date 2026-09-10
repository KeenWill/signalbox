//! Production poll and webhook workflow admission without provider credentials.
//! Exercises docs/spec/repo-watch.md and docs/spec/workflows.md.

use super::*;
use signalbox_application::{InProcessEligibilityWorkSource, InProcessToolDispatchGate};
use signalbox_persistence::scheduler::PostgresEligibilitySweep;
use signalboxd::{
    SessionTemplateConfiguration,
    repo_watch_runtime::{RepositoryWatchRuntime, RepositoryWatchServices},
    workflows::WorkflowRuntime,
};
use std::sync::Arc;

// A fixture deadline bounds failure to observe durable progress.
const WORKFLOW_TIMEOUT: Duration = Duration::from_secs(30);

async fn configured_runtime(
    fixture: &Fixture,
    hook: &RuntimeHookFixture<'_>,
) -> Result<RepositoryWatchRuntime, Box<dyn Error>> {
    let source = runtime_configuration_source(hook)?
        .replace(
            "repository = \"runtime/project\"",
            "repository = \"example/project\"",
        )
        .replace(
            "[repository_watch]\nversion = 1",
            "[repository_watch]\nversion = 1\nworkflows_enabled = true",
        );
    // Exercise a subsequent timer poll even when the startup wake wins admission.
    let source = source.replace("poll_interval_seconds = 60", "poll_interval_seconds = 1");
    let models = signalboxd::HubModelConfiguration::parse(&source)?;
    let path = hook.secret.with_extension("templates.toml");
    std::fs::write(
        &path,
        "version = 1\n[[templates]]\nname = \"watch\"\nversion = 1\nalias = \"540ce009-c2ec-4a04-b823-c411ea189778\"\ndangerous_tool_auto_approval = false\nsystem_prompt = \"Inspect repository activity.\"\n",
    )?;
    let templates = SessionTemplateConfiguration::read(&path, || None, &models)?;
    let (nudge, _work) =
        InProcessEligibilityWorkSource::new(PostgresEligibilitySweep::new(fixture.core.clone()));
    Ok(RepositoryWatchRuntime::new(
        fixture.module.clone(),
        models.repository_watch().cloned(),
        RepositoryWatchServices {
            goal_resumption: signalboxd::PostgresGoalPassDisposition::new(
                fixture.core.clone(),
                models.clone(),
                nudge.clone(),
                signalboxd::GoalModeNumericBounds::new(None, None, None, None, None),
            ),
            checkout_runner: None,
            core_pool: fixture.core.clone(),
            models: Arc::new(models),
            templates: Arc::new(templates),
            eligibility_nudge: nudge,
            tool_dispatch_gate: InProcessToolDispatchGate::default(),
        },
    )
    .await
    .expect("configured observation runtime"))
}

async fn observation_requests(core: &PgPool) -> Result<Vec<Vec<u8>>, sqlx::Error> {
    sqlx::query_scalar("SELECT payload_inline FROM program_run_journal_entry WHERE effect_method='repo.observe' ORDER BY run_id, journal_position")
        .fetch_all(core).await
}

async fn wait_for_requests(
    fixture: &Fixture,
    count: usize,
) -> Result<Vec<Vec<u8>>, Box<dyn Error>> {
    tokio::time::timeout(WORKFLOW_TIMEOUT, async {
        loop {
            let requests = observation_requests(&fixture.core).await?;
            if requests.len() >= count {
                return Ok::<_, Box<dyn Error>>(requests);
            }
            // Requests do not publish the answer notifications consumed by workflow waits.
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await?
}

async fn wait_for_result(fixture: &Fixture, id: Uuid) -> Result<ObserveAnswer, Box<dyn Error>> {
    tokio::time::timeout(WORKFLOW_TIMEOUT, async {
        let mut changed = fixture.journal.listen_all().await?;
        loop {
            let loaded = fixture
                .journal
                .load(ProgramRunId::from_uuid(id))
                .await?
                .expect("observation run");
            if let Some(result) = loaded.result() {
                break Ok::<_, Box<dyn Error>>(ObserveAnswer::decode(result.as_bytes())?);
            }
            changed.changed().await?;
        }
    })
    .await?
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL"]
async fn a_webhook_wake_during_a_workflow_poll_waits_for_that_observation()
-> Result<(), Box<dyn Error>> {
    let fixture = Fixture::new().await?;
    let files = tempfile::tempdir()?;
    let secret = files.path().join("hook-secret");
    const HOOK_SECRET: &[u8] = b"observation-fixture-secret";
    write_private_credential(&secret, HOOK_SECRET)?;
    let hook = RuntimeHookFixture {
        address: unused_webhook_address().await?,
        path: "/observe",
        id: 23,
        secret: &secret,
        enabled: true,
        rule_version: 1,
        template: "watch",
        mode: "primary",
        retention: "604800s",
    };
    let runtime = configured_runtime(&fixture, &hook).await?;
    let (_, runner) = WorkflowRuntime::new(fixture.core.clone())?;
    let runner = runner.with_repository_watch(Some(runtime.clone()));
    let (stop_workflows, workflow_stopped) = tokio::sync::oneshot::channel();
    let workflow_task = tokio::spawn(runner.run(async {
        let _ = workflow_stopped.await;
    }));
    let (stop_repositories, repositories_stopped) = tokio::sync::watch::channel(false);
    let repository_task = runtime.spawn(repositories_stopped).await;
    // Startup queues both a timer and an existing wake; either may be admitted first.
    wait_for_requests(&fixture, 2).await?;
    let startup_ids: Vec<Uuid> = sqlx::query_scalar("SELECT DISTINCT run_id FROM program_run_journal_entry WHERE effect_method='repo.observe' ORDER BY run_id").fetch_all(&fixture.core).await?;
    for id in startup_ids {
        wait_for_result(&fixture, id).await?;
    }
    let mut lease = fixture.core.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('observation:example/project', 0))")
        .execute(&mut *lease)
        .await?;
    tokio::time::timeout(WORKFLOW_TIMEOUT, async {
        loop {
            let pending: Vec<Vec<u8>> = sqlx::query_scalar("SELECT request.payload_inline FROM program_run_journal_entry request WHERE request.effect_method='repo.observe' AND NOT EXISTS (SELECT 1 FROM program_run_journal_entry answer WHERE answer.run_id=request.run_id AND answer.resolves_request_ordinal=request.request_ordinal)")
                .fetch_all(&fixture.core).await?;
            if pending.iter().any(|bytes| ObserveInput::decode(bytes).is_ok_and(|input| input.producer() == EventProducer::Poll)) {
                break Ok::<_, Box<dyn Error>>(());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }).await??;
    let before_wake = observation_requests(&fixture.core).await?.len();
    let status = webhook_delivery_status(&hook, HOOK_SECRET, &RuntimeWebhookDelivery {
        id: Uuid::now_v7(), event: "pull_request",
        body: r#"{"action":"opened","repository":{"full_name":"example/project"},"pull_request":{"number":1}}"#,
    }).await?;
    assert_eq!(status, reqwest::StatusCode::ACCEPTED);
    assert_eq!(
        observation_requests(&fixture.core).await?.len(),
        before_wake,
        "webhook admission cannot launch a second observation while the poll is blocked"
    );
    lease.commit().await?;
    let webhook_index = tokio::time::timeout(WORKFLOW_TIMEOUT, async {
        loop {
            let requests = observation_requests(&fixture.core).await?;
            if let Some(index) =
                requests
                    .iter()
                    .enumerate()
                    .skip(before_wake)
                    .find_map(|(index, bytes)| {
                        ObserveInput::decode(bytes)
                            .is_ok_and(|input| input.producer() == EventProducer::Webhook)
                            .then_some(index)
                    })
            {
                break Ok::<_, Box<dyn Error>>(index);
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await??;
    let ids: Vec<Uuid> = sqlx::query_scalar("SELECT DISTINCT run_id FROM program_run_journal_entry WHERE effect_method='repo.observe' ORDER BY run_id").fetch_all(&fixture.core).await?;
    assert!(matches!(
        wait_for_result(&fixture, ids[webhook_index]).await?,
        ObserveAnswer::Observed(ObservationResult {
            outcome: ObservationOutcome::Failed,
            ..
        })
    ));
    stop_repositories.send(true)?;
    tokio::time::timeout(WORKFLOW_TIMEOUT, repository_task)
        .await??
        .expect("repository runtime drains");
    let _ = stop_workflows.send(());
    tokio::time::timeout(WORKFLOW_TIMEOUT, workflow_task).await???;
    Ok(())
}
