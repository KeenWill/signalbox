//! Checked rejection isolation, receipt selection, and shared runtime shutdown.
//! Exercises docs/spec/workflows.md and docs/spec/repo-watch.md.

use super::production::production_fixture;
use super::*;
use signalbox_domain::{DeliveryKind, ProgramFault};
use signalboxd::workflows::WorkflowRuntime;

// Bounds a stuck fixture; readiness and completion come from durable state.
const BOUNDARY_TIMEOUT: Duration = Duration::from_secs(30);

#[tokio::test]
#[ignore = "requires disposable PostgreSQL"]
async fn module_storage_failures_remain_runtime_failures() -> Result<(), Box<dyn Error>> {
    let (_database, core, module, _store, runtime, repository, rule, _files) =
        production_fixture().await?;
    let request = RepoWatchRequest::NextRuleEvent {
        repository,
        rule: rule.id().clone(),
    }
    .encode()?;
    let run = start(
        &core,
        &request,
        ProgramGrants::new([ProgramCapability::RepoWatch]),
    )
    .await?;
    sqlx::query("ALTER TABLE rule RENAME TO unavailable_rule_fixture")
        .execute(&module)
        .await?;
    let journals = ProgramJournalRepository::new(core.clone());
    let (_service, runner) = WorkflowRuntime::new(core)?;
    let task = tokio::spawn(
        runner
            .with_repository_watch(Some(runtime))
            .run(std::future::pending()),
    );
    let failure = tokio::time::timeout(BOUNDARY_TIMEOUT, task)
        .await??
        .expect_err("database failure stops the runtime");
    assert!(
        matches!(failure, signalboxd::workflows::WorkflowRuntimeError::Attempt { source, .. }
        if matches!(*source, signalbox_workflow_runtime::WorkflowHostError::LiveDelivery(_)))
    );
    assert!(
        journals
            .load(run)
            .await?
            .expect("recoverable run")
            .terminal_delivery()
            .is_none()
    );
    Ok(())
}

async fn terminal(
    journals: &ProgramJournalRepository,
    run: ProgramRunId,
) -> Result<DeliveryKind, Box<dyn Error>> {
    tokio::time::timeout(BOUNDARY_TIMEOUT, async {
        loop {
            let journal = journals.load(run).await?.expect("admitted run");
            if let Some(delivery) = journal.terminal_delivery() {
                return Ok::<_, Box<dyn Error>>(delivery.kind().clone());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await?
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL"]
async fn checked_module_rejections_fault_only_the_requesting_run() -> Result<(), Box<dyn Error>> {
    let (_database, core, _module, store, runtime, repository, rule, _files) =
        production_fixture().await?;
    let context = store
        .next_rule_context(&repository, &rule)
        .await?
        .expect("first event");
    let wrong_plan = RepoWatchRequest::CommitEvaluation {
        effect: Uuid::now_v7(),
        context: Box::new(context.clone()),
        plan: vec![],
    }
    .encode()?;
    let mut stale = context.clone();
    stale.ordinal += 1;
    let stale_context = RepoWatchRequest::CommitEvaluation {
        effect: Uuid::now_v7(),
        context: Box::new(stale),
        plan: context.plan(),
    }
    .encode()?;
    let absent_dispatch = RepoWatchRequest::SubmitPending {
        effect: Uuid::now_v7(),
        dispatch: RepoWatchDispatchId::from_uuid(Uuid::now_v7()),
    }
    .encode()?;
    let (service, runner) = WorkflowRuntime::new(core.clone())?;
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(runner.with_repository_watch(Some(runtime)).run(async {
        let _ = stopped.await;
    }));
    let journals = ProgramJournalRepository::new(core.clone());
    for (case, request) in [
        ("wrong plan", wrong_plan),
        ("stale context", stale_context),
        ("absent dispatch", absent_dispatch),
    ] {
        let run = start(
            &core,
            &request,
            ProgramGrants::new([ProgramCapability::RepoWatch]),
        )
        .await?;
        // Starting the existing admission through the service wakes the production runner.
        let registered = journals
            .registrations()
            .for_run(run)
            .await?
            .expect("registration");
        service
            .start(run, registered.id, request.payload().as_bytes())
            .await?;
        assert!(
            matches!(
                terminal(&journals, run).await?,
                DeliveryKind::Fault(ProgramFault::ProgramError(_))
            ),
            "{case}"
        );
        assert!(
            !task.is_finished(),
            "checked rejection stopped the runner: {case}"
        );
    }
    let read = RepoWatchRequest::NextRuleEvent {
        repository,
        rule: rule.id().clone(),
    }
    .encode()?;
    let run = start(
        &core,
        &read,
        ProgramGrants::new([ProgramCapability::RepoWatch]),
    )
    .await?;
    let registered = journals
        .registrations()
        .for_run(run)
        .await?
        .expect("registration");
    service
        .start(run, registered.id, read.payload().as_bytes())
        .await?;
    assert!(matches!(
        terminal(&journals, run).await?,
        DeliveryKind::Answer { .. }
    ));
    assert_eq!(
        RuleContext::decode(
            journals
                .load(run)
                .await?
                .expect("read run")
                .result()
                .expect("read result")
                .as_bytes()
        ),
        Some(context)
    );
    stop.send(()).expect("runner running");
    task.await??;
    Ok(())
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL"]
async fn pending_evaluation_receipts_block_the_existing_evaluator() -> Result<(), Box<dyn Error>> {
    for case in [Case::Dispatch, Case::Suppression, Case::Nonmatch] {
        let (_database, core, _module, mut effects, repository, rule) = fixture(case).await?;
        let context = effects
            .store
            .next_rule_context(&repository, &rule)
            .await?
            .expect("first event");
        let request = RepoWatchRequest::CommitEvaluation {
            effect: Uuid::now_v7(),
            plan: context.plan(),
            context: Box::new(context),
        }
        .encode()?;
        let run = start(
            &core,
            &request,
            ProgramGrants::new([ProgramCapability::RepoWatch]),
        )
        .await?;
        let journals = ProgramJournalRepository::new(core);
        let host = WorkflowHost::new(journals.clone());
        assert!(
            host.execute_registered(run, &mut NoPrimitives, &mut LoseAnswer(&mut effects))
                .await
                .is_err()
        );
        let receipts = effects.store.evaluation_receipts().await?;
        let dispatch_ids = effects.ids.calls;
        effects
            .store
            .ingest_observation(
                &effects.store.ingest_baseline(&repository).await?,
                &dispatch_observation(&repository, 4, OffsetDateTime::now_utc()),
                EventProducer::Poll,
                MERGED_RETENTION,
            )
            .await?;
        assert!(
            effects
                .store
                .next_rule_event(&repository, &rule)
                .await?
                .is_none(),
            "{case:?}"
        );
        assert!(
            !effects
                .store
                .evaluate_next(
                    &repository,
                    &rule,
                    &mut effects.ids,
                    &mut effects.factory,
                    &mut effects.codec,
                    OffsetDateTime::now_utc()
                )
                .await
                .expect("existing evaluator"),
            "{case:?}"
        );
        assert_eq!(
            effects.ids.calls, dispatch_ids,
            "pending receipt prevents dispatch planning: {case:?}"
        );
        assert_eq!(
            effects.store.evaluation_receipts().await?,
            receipts,
            "{case:?}"
        );
        host.execute_registered(run, &mut NoPrimitives, &mut effects)
            .await?;
        effects
            .acknowledge_receipt(&journals, run, &receipts[0])
            .await?;
        assert!(
            effects
                .store
                .evaluate_next(
                    &repository,
                    &rule,
                    &mut effects.ids,
                    &mut effects.factory,
                    &mut effects.codec,
                    OffsetDateTime::now_utc()
                )
                .await
                .expect("evaluation resumes after adoption"),
            "{case:?}"
        );
    }
    Ok(())
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL"]
async fn repository_shutdown_keeps_the_pool_available_for_workflow_reconciliation()
-> Result<(), Box<dyn Error>> {
    let (_database, core, _module, store, repository_watch, _repository, _rule, _files) =
        production_fixture().await?;
    repository_watch
        .reload_configuration(None)
        .await
        .expect("remove repository configuration");
    let (_shutdown, stopped) = tokio::sync::watch::channel(true);
    repository_watch
        .clone()
        .run(stopped)
        .await
        .expect("repository worker stops cleanly");

    let (_service, runner) = WorkflowRuntime::new(core)?;
    runner
        .with_repository_watch(Some(repository_watch))
        .run(async {})
        .await?;
    assert!(store.evaluation_receipts().await?.is_empty());
    Ok(())
}
