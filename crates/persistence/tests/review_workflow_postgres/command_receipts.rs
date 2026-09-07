//! Command receipts coverage.

use super::*;

/// exact review-command replay and effect recovery preserve one result.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn review_workflow_command_receipts_replay_and_recover() -> Result<(), Box<dyn Error>> {
    const TARGET_IDENTITY: u128 = 0x760;
    const RECOVERED_TARGET_IDENTITY: u128 = 0x761;
    const COMMAND_IDENTITY: u128 = 0x762;
    const RECOVERY_COMMAND_IDENTITY: u128 = 0x763;

    let (_container, pool) = migrated_postgres().await?;
    let store = ReviewWorkflowStore::new(pool);
    let target = ReviewTarget::try_new(
        ReviewTargetId::from_uuid(uuid(TARGET_IDENTITY)),
        key("provider"),
        key("repository"),
        ReviewTargetSubject::ChangeRequest(
            ReviewChangeRequestNumber::try_new(42).expect("fixture number is positive"),
        ),
        key("head"),
        Some(key("base")),
        None,
    )
    .expect("target fixture is admitted");
    let command_id = DurableCommandId::from_uuid(uuid(COMMAND_IDENTITY));
    let command = ReviewWorkflowCommand::new(
        command_id,
        [7; 32],
        ReviewWorkflowOperation::CreateTarget(target.clone()),
    );
    let mut service = ReviewWorkflowCommandService::new(store.clone());
    let expected =
        ReviewWorkflowCommandOutcome::Recorded(ReviewWorkflowCommandResult::TargetCreated {
            target: target.id(),
        });

    assert_eq!(service.execute(command.clone()).await?, expected);
    assert_eq!(service.execute(command).await?, expected);
    assert_eq!(
        store
            .load_command_outcome(
                command_id,
                [7; 32],
                ReviewWorkflowOperationKind::CreateTarget,
            )
            .await?,
        Some(expected.clone()),
    );
    assert_eq!(store.load_target(target.id()).await?, Some(target.clone()));
    assert_eq!(
        store
            .load_command_outcome(
                command_id,
                [8; 32],
                ReviewWorkflowOperationKind::CreateTarget,
            )
            .await?,
        Some(ReviewWorkflowCommandOutcome::ConflictingReuse { command_id }),
    );
    assert_eq!(
        service
            .execute(ReviewWorkflowCommand::new(
                command_id,
                [8; 32],
                ReviewWorkflowOperation::CreateTarget(target),
            ))
            .await?,
        ReviewWorkflowCommandOutcome::ConflictingReuse { command_id },
    );

    let recovered_target = ReviewTarget::try_new(
        ReviewTargetId::from_uuid(uuid(RECOVERED_TARGET_IDENTITY)),
        key("provider"),
        key("repository"),
        ReviewTargetSubject::ChangeRequest(
            ReviewChangeRequestNumber::try_new(43).expect("fixture number is positive"),
        ),
        key("later-head"),
        Some(key("later-base")),
        None,
    )
    .expect("recovery target fixture is admitted");
    store.insert_target(&recovered_target).await?;
    let recovery_command_id = DurableCommandId::from_uuid(uuid(RECOVERY_COMMAND_IDENTITY));
    let recovery_command = ReviewWorkflowCommand::new(
        recovery_command_id,
        [9; 32],
        ReviewWorkflowOperation::CreateTarget(recovered_target.clone()),
    );
    let recovered =
        ReviewWorkflowCommandOutcome::Recorded(ReviewWorkflowCommandResult::TargetCreated {
            target: recovered_target.id(),
        });

    assert_eq!(service.execute(recovery_command.clone()).await?, recovered);
    assert_eq!(service.execute(recovery_command).await?, recovered);
    assert_eq!(
        store.load_target(recovered_target.id()).await?,
        Some(recovered_target),
    );
    Ok(())
}

/// a formerly legal run-only commit remains loadable and its exact
/// command retry completes the admitted pass.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn start_run_recovers_a_loadable_run_only_commit() -> Result<(), Box<dyn Error>> {
    const COMMAND_IDENTITY: u128 = 0x777;

    let (_container, pool) = migrated_postgres().await?;
    let fixture = review_command_admission_fixture(&pool).await;
    fixture.store.insert_run(&fixture.run).await?;
    let partial = fixture
        .store
        .load_run_with_pass(fixture.run.reference().run())
        .await?
        .expect("run-only admission remains loadable");
    assert_eq!(partial.0.reference(), fixture.run.reference());
    assert_eq!(partial.0.workflow(), fixture.run.workflow());
    assert_eq!(partial.0.policy(), fixture.run.policy());
    assert_eq!(partial.0.recorded_pass(), None);
    assert_eq!(partial.1, None);

    let command = ReviewWorkflowCommand::new(
        DurableCommandId::from_uuid(uuid(COMMAND_IDENTITY)),
        [13; 32],
        ReviewWorkflowOperation::StartRun {
            run: fixture.run.clone(),
            pass: fixture.pass.clone(),
        },
    );
    let expected =
        ReviewWorkflowCommandOutcome::Recorded(ReviewWorkflowCommandResult::RunStarted {
            run: fixture.run.reference().run(),
            pass: fixture.pass.reference().pass(),
        });
    let mut service = ReviewWorkflowCommandService::new(fixture.store.clone());

    assert_eq!(service.execute(command).await?, expected);
    assert_eq!(
        fixture
            .store
            .load_run_with_pass(fixture.run.reference().run())
            .await?,
        Some((fixture.run, Some(fixture.pass))),
    );
    Ok(())
}

/// a rejected fresh admission cannot leave a run-only aggregate.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn start_run_rolls_back_run_when_pass_admission_fails() -> Result<(), Box<dyn Error>> {
    const COMMAND_IDENTITY: u128 = 0x778;

    let (_container, pool) = migrated_postgres().await?;
    let fixture = review_command_admission_fixture(&pool).await;
    fixture
        .store
        .insert_run_and_pass(&fixture.run, &fixture.pass)
        .await?;
    let run_reference = ReviewRunRef::new(fixture.target, ReviewRunId::from_uuid(uuid(0x779)));
    let pass_reference = ReviewPassRef::new(run_reference, ReviewPassId::from_uuid(uuid(0x77a)));
    let mut rejected_run = ReviewRun::new(
        run_reference,
        ReviewWorkflowKind::ReadOnlyReview,
        ReviewPolicy::version_one(),
    );
    let rejected_pass = ReviewPass::try_new(
        pass_reference,
        ReviewPassKind::ReadOnlyReview,
        &mut rejected_run,
        fixture.session,
        ReviewPassAcceptedInputEvidence::new(
            fixture.accepted_input,
            fixture.session,
            Some(fixture.origin_turn),
        ),
    )
    .expect("the conflicting pass fixture is domain-valid");
    let command = ReviewWorkflowCommand::new(
        DurableCommandId::from_uuid(uuid(COMMAND_IDENTITY)),
        [14; 32],
        ReviewWorkflowOperation::StartRun {
            run: rejected_run,
            pass: rejected_pass,
        },
    );
    let mut service = ReviewWorkflowCommandService::new(fixture.store.clone());

    assert!(service.execute(command).await.is_err());
    assert_eq!(fixture.store.load_run(run_reference.run()).await?, None);
    assert_eq!(fixture.store.load_pass(pass_reference.pass()).await?, None);
    Ok(())
}

/// run admission recovery ignores later lifecycle advancement.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn start_run_receipt_recovers_after_lifecycle_advancement() -> Result<(), Box<dyn Error>> {
    const COMMAND_IDENTITY: u128 = 0x764;

    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let queued_run = fixture
        .store
        .load_run(fixture.run.run())
        .await?
        .expect("queued fixture run exists");
    let queued_pass = fixture
        .store
        .load_pass(fixture.pass.pass())
        .await?
        .expect("queued fixture pass exists");
    let command_id = DurableCommandId::from_uuid(uuid(COMMAND_IDENTITY));
    let command = ReviewWorkflowCommand::new(
        command_id,
        [10; 32],
        ReviewWorkflowOperation::StartRun {
            run: queued_run.clone(),
            pass: queued_pass.clone(),
        },
    );
    let expected =
        ReviewWorkflowCommandOutcome::Recorded(ReviewWorkflowCommandResult::RunStarted {
            run: fixture.run.run(),
            pass: fixture.pass.pass(),
        });

    let (running_pass, _turn) = start_review_pass(&fixture.store, fixture.pass).await;
    let running_run = fixture
        .store
        .load_run(fixture.run.run())
        .await?
        .expect("running fixture run exists");
    assert_ne!(running_run.state(), queued_run.state());
    assert_ne!(running_pass.state(), queued_pass.state());

    let mut service = ReviewWorkflowCommandService::new(fixture.store);
    assert_eq!(service.execute(command.clone()).await?, expected);
    assert_eq!(service.execute(command).await?, expected);
    Ok(())
}

/// activation recovery recognizes the same pass after completion.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn activation_receipt_recovers_after_pass_completion() -> Result<(), Box<dyn Error>> {
    const COMMAND_IDENTITY: u128 = 0x768;

    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let (running_pass, turn) = start_review_pass(&fixture.store, fixture.pass).await;
    let running_run = fixture
        .store
        .load_run(fixture.run.run())
        .await?
        .expect("running fixture run exists");
    let command = ReviewWorkflowCommand::new(
        DurableCommandId::from_uuid(uuid(COMMAND_IDENTITY)),
        [12; 32],
        ReviewWorkflowOperation::ActivatePass {
            run: running_run,
            pass: running_pass,
        },
    );
    let expected =
        ReviewWorkflowCommandOutcome::Recorded(ReviewWorkflowCommandResult::PassActivated {
            run: fixture.run.run(),
            pass: fixture.pass.pass(),
        });

    fail_review_turn(&pool, turn).await;
    conclude_review_pass(
        &fixture.store,
        fixture.pass,
        ReviewPassState::Failed { turn },
    )
    .await;

    let mut service = ReviewWorkflowCommandService::new(fixture.store);
    assert_eq!(service.execute(command.clone()).await?, expected);
    assert_eq!(service.execute(command).await?, expected);
    Ok(())
}

/// the generic result-free terminal command commits and replays one
/// exact run/pass completion receipt.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn complete_pass_commits_and_replays_terminal_status() -> Result<(), Box<dyn Error>> {
    const COMMAND_IDENTITY: u128 = 0x790;
    const PASS_IDENTITY: u128 = 0x791;

    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let pass_ref = insert_fixture_pass(&fixture, PASS_IDENTITY, ReviewPassKind::Judge).await;
    let (running_pass, turn) = start_review_pass(&fixture.store, pass_ref).await;
    let running_run = fixture
        .store
        .load_run(pass_ref.run().run())
        .await?
        .expect("running run exists");
    let output_frontier = complete_review_turn(&pool, turn).await;
    let session = running_pass.session();
    let accepted_input = running_pass.accepted_input();
    let policy = running_run.policy();
    let terminal_pass = running_pass
        .transition(
            ReviewPassState::Succeeded {
                turn,
                output_frontier,
                result: None,
            },
            Some(ReviewPassTurnEvidence::new(
                turn,
                session,
                accepted_input,
                ReviewPassTurnOutcome::Completed,
                Some(output_frontier),
            )),
        )
        .expect("terminal fixture pass is valid");
    let terminal_run = running_run
        .transition(
            ReviewRunState::Succeeded {
                concluding_pass: pass_ref,
            },
            Some(ReviewPassEvidence::from_pass(&terminal_pass, policy)),
        )
        .expect("terminal fixture run is valid");
    let command = ReviewWorkflowCommand::new(
        DurableCommandId::from_uuid(uuid(COMMAND_IDENTITY)),
        [0x79; 32],
        ReviewWorkflowOperation::CompletePass {
            run: terminal_run.clone(),
            pass: terminal_pass.clone(),
        },
    );
    let expected =
        ReviewWorkflowCommandOutcome::Recorded(ReviewWorkflowCommandResult::PassCompleted {
            run: pass_ref.run().run(),
            pass: pass_ref.pass(),
            status: ReviewPassCompletionStatus::Succeeded,
        });
    let mut service = ReviewWorkflowCommandService::new(fixture.store.clone());

    assert_eq!(service.execute(command.clone()).await?, expected);
    assert_eq!(service.execute(command).await?, expected);
    assert_eq!(
        fixture.store.load_run(pass_ref.run().run()).await?,
        Some(terminal_run)
    );
    assert_eq!(
        fixture.store.load_pass(pass_ref.pass()).await?,
        Some(terminal_pass)
    );
    Ok(())
}
