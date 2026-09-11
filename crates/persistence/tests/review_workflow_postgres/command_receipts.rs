//! Command receipts coverage.

use super::*;

/// a deferred constraint failure at commit proves the claim and effect rolled back.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn definite_commit_failure_rolls_back_claim_and_effect() -> Result<(), Box<dyn Error>> {
    const TARGET_IDENTITY: u128 = 0x74e;
    const COMMAND_IDENTITY: u128 = 0x74f;

    let (_container, pool) = migrated_postgres_with_max_connections(1).await?;
    sqlx::raw_sql(
        "CREATE FUNCTION reject_review_target_at_commit() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN
             RAISE EXCEPTION 'injected deferred commit failure' USING ERRCODE = '23514';
         END $$;
         CREATE CONSTRAINT TRIGGER reject_review_target_at_commit
         AFTER INSERT ON review_target
         DEFERRABLE INITIALLY DEFERRED
         FOR EACH ROW EXECUTE FUNCTION reject_review_target_at_commit();",
    )
    .execute(&pool)
    .await?;
    let store = ReviewWorkflowStore::new(pool.clone());
    let target = ReviewTarget::try_new(
        ReviewTargetId::from_uuid(uuid(TARGET_IDENTITY)),
        key("provider"),
        key("repository"),
        ReviewTargetSubject::Commit,
        key("head"),
        None,
        None,
    )
    .expect("target fixture is admitted");
    let command_id = DurableCommandId::from_uuid(uuid(COMMAND_IDENTITY));
    let command = ReviewWorkflowCommand::new(
        command_id,
        [4; 32],
        ReviewWorkflowOperation::CreateTarget(target.clone()),
    );
    let expected =
        ReviewWorkflowCommandOutcome::Recorded(ReviewWorkflowCommandResult::TargetCreated {
            target: target.id(),
        });
    let mut service = ReviewWorkflowCommandService::new(store.clone());

    let error = service
        .execute(command.clone())
        .await
        .expect_err("deferred constraint rejects commit");
    let ReviewWorkflowStoreError::Database(error) = error else {
        panic!("definite commit failure must be an ordinary database error");
    };
    assert_sqlstate(&error, "23514");
    assert_eq!(store.load_target(target.id()).await?, None);
    assert_eq!(
        store
            .load_command_outcome(
                command_id,
                [4; 32],
                ReviewWorkflowOperationKind::CreateTarget,
            )
            .await?,
        None,
    );

    sqlx::raw_sql(
        "DROP TRIGGER reject_review_target_at_commit ON review_target;
         DROP FUNCTION reject_review_target_at_commit();",
    )
    .execute(&pool)
    .await?;
    assert_eq!(service.execute(command).await?, expected);
    assert_eq!(store.load_target(target.id()).await?, Some(target));
    assert_eq!(
        store
            .load_command_outcome(
                command_id,
                [4; 32],
                ReviewWorkflowOperationKind::CreateTarget,
            )
            .await?,
        Some(expected),
    );
    Ok(())
}

/// a review command applies its effect and receipt through one pool connection.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn review_workflow_command_uses_one_pool_connection() -> Result<(), Box<dyn Error>> {
    const TARGET_IDENTITY: u128 = 0x75e;
    const COMMAND_IDENTITY: u128 = 0x75f;

    let (_container, pool) = migrated_postgres_with_max_connections(1).await?;
    let target = ReviewTarget::try_new(
        ReviewTargetId::from_uuid(uuid(TARGET_IDENTITY)),
        key("provider"),
        key("repository"),
        ReviewTargetSubject::Commit,
        key("head"),
        None,
        None,
    )
    .expect("target fixture is admitted");
    let command = ReviewWorkflowCommand::new(
        DurableCommandId::from_uuid(uuid(COMMAND_IDENTITY)),
        [6; 32],
        ReviewWorkflowOperation::CreateTarget(target.clone()),
    );
    let expected =
        ReviewWorkflowCommandOutcome::Recorded(ReviewWorkflowCommandResult::TargetCreated {
            target: target.id(),
        });
    let mut service = ReviewWorkflowCommandService::new(ReviewWorkflowStore::new(pool));

    let outcome = tokio::time::timeout(std::time::Duration::from_secs(2), service.execute(command))
        .await
        .expect("one-connection command handling does not wait for another connection")?;
    assert_eq!(outcome, expected);
    Ok(())
}

/// a receipt insertion failure is a proven rollback of the claim and effect.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn receipt_insert_failure_rolls_back_claim_and_effect() -> Result<(), Box<dyn Error>> {
    const TARGET_IDENTITY: u128 = 0x750;
    const COMMAND_IDENTITY: u128 = 0x751;

    let (_container, pool) = migrated_postgres_with_max_connections(1).await?;
    sqlx::raw_sql(
        "CREATE FUNCTION reject_review_workflow_receipt() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN RAISE EXCEPTION 'injected receipt failure' USING ERRCODE = '23514'; END $$;
         CREATE TRIGGER reject_review_workflow_receipt
         BEFORE INSERT ON review_workflow_command
         FOR EACH ROW EXECUTE FUNCTION reject_review_workflow_receipt();",
    )
    .execute(&pool)
    .await?;
    let store = ReviewWorkflowStore::new(pool);
    let target = ReviewTarget::try_new(
        ReviewTargetId::from_uuid(uuid(TARGET_IDENTITY)),
        key("provider"),
        key("repository"),
        ReviewTargetSubject::Commit,
        key("head"),
        None,
        None,
    )
    .expect("target fixture is admitted");
    let command_id = DurableCommandId::from_uuid(uuid(COMMAND_IDENTITY));
    let command = ReviewWorkflowCommand::new(
        command_id,
        [5; 32],
        ReviewWorkflowOperation::CreateTarget(target.clone()),
    );
    let mut service = ReviewWorkflowCommandService::new(store.clone());

    let error = service
        .execute(command)
        .await
        .expect_err("receipt insertion is rejected");
    let ReviewWorkflowStoreError::Database(error) = error else {
        panic!("pre-commit receipt failure must be an ordinary database error");
    };
    assert_sqlstate(&error, "23514");
    assert_eq!(store.load_target(target.id()).await?, None);
    assert_eq!(
        store
            .load_command_outcome(
                command_id,
                [5; 32],
                ReviewWorkflowOperationKind::CreateTarget,
            )
            .await?,
        None,
    );
    Ok(())
}

/// an attachment command waits for a concurrent external-link update before
/// loading the multi-row aggregate projection.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn attach_command_loads_after_concurrent_external_link_commit() -> Result<(), Box<dyn Error>>
{
    const COMMAND_IDENTITY: u128 = 0x752;
    const PUBLISH_PASS_IDENTITY: u128 = 0x753;
    const IMPORT_PASS_IDENTITY: u128 = 0x754;
    const LINK_IDENTITY: u128 = 0x755;

    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let publish_pass =
        insert_fixture_pass(&fixture, PUBLISH_PASS_IDENTITY, ReviewPassKind::Publish).await;
    let import_pass = insert_fixture_pass(
        &fixture,
        IMPORT_PASS_IDENTITY,
        ReviewPassKind::ImportExternalContext,
    )
    .await;
    let evidence =
        succeed_fixture_passes(&pool, &fixture.store, &[publish_pass, import_pass]).await;
    let link = ReviewExternalLinkId::from_uuid(uuid(LINK_IDENTITY));
    fixture
        .store
        .reserve_external_link(
            ReviewExternalLink::try_reserve(
                link,
                ReviewExternalLinkAssociation::Target(fixture.target),
                key("example-code-host"),
                ReviewExternalObjectKind::ReviewComment,
                &fixture.target_snapshot,
            )
            .expect("reservation matches the target"),
        )
        .await?;
    let requested_attachment = attachment(link, evidence[0].clone(), key("comment-755"));
    let concurrent_observation = observation(
        link,
        ReviewEventOrdinal::one(),
        evidence[1].clone(),
        ReviewExternalObjectState::Current,
    );

    let mut writer = pool.begin().await?;
    sqlx::query(
        "SELECT external_link_id
           FROM review_external_link
          WHERE external_link_id = $1
          FOR NO KEY UPDATE",
    )
    .bind(link.into_uuid())
    .fetch_one(&mut *writer)
    .await?;
    sqlx::query(
        "LOCK TABLE review_external_link_observation
         IN ACCESS EXCLUSIVE MODE",
    )
    .execute(&mut *writer)
    .await?;

    let command = ReviewWorkflowCommand::new(
        DurableCommandId::from_uuid(uuid(COMMAND_IDENTITY)),
        [0x75; 32],
        ReviewWorkflowOperation::AttachExternalLink {
            link,
            attachment: requested_attachment.clone(),
        },
    );
    let mut service = ReviewWorkflowCommandService::new(fixture.store.clone());
    let mut handling = tokio::spawn(async move { service.execute(command).await });
    assert!(
        blocked_backends_reached(&pool, 1).await?,
        "attachment command waits for the external-link transition lock"
    );

    sqlx::query(
        "UPDATE review_pass
            SET result_kind = 'external_link_attachment',
                result_external_link_id = $2,
                result_external_object_key = $3
          WHERE pass_id = $1",
    )
    .bind(publish_pass.pass().into_uuid())
    .bind(link.into_uuid())
    .bind(requested_attachment.external_object().as_str())
    .execute(&mut *writer)
    .await?;
    sqlx::query(
        "INSERT INTO review_external_link_attachment
            (external_link_id, target_id, pass_run_id, pass_id,
             provider_key, object_kind, external_object_key)
         VALUES ($1, $2, $3, $4, 'example-code-host',
                 'review_comment', $5)",
    )
    .bind(link.into_uuid())
    .bind(fixture.target.into_uuid())
    .bind(publish_pass.run().run().into_uuid())
    .bind(publish_pass.pass().into_uuid())
    .bind(requested_attachment.external_object().as_str())
    .execute(&mut *writer)
    .await?;
    sqlx::query(
        "UPDATE review_pass
            SET result_kind = 'external_link_observation',
                result_external_link_id = $2,
                result_event_ordinal = 1,
                result_observation_state = 'current'
          WHERE pass_id = $1",
    )
    .bind(import_pass.pass().into_uuid())
    .bind(link.into_uuid())
    .execute(&mut *writer)
    .await?;
    sqlx::query(
        "INSERT INTO review_external_link_observation
            (external_link_id, observation_ordinal, target_id,
             pass_run_id, pass_id, object_state)
         VALUES ($1, 1, $2, $3, $4, 'current')",
    )
    .bind(link.into_uuid())
    .bind(fixture.target.into_uuid())
    .bind(import_pass.run().run().into_uuid())
    .bind(import_pass.pass().into_uuid())
    .execute(&mut *writer)
    .await?;
    writer.commit().await?;

    let outcome = tokio::time::timeout(std::time::Duration::from_secs(5), &mut handling)
        .await
        .expect("attachment command completes after the concurrent commit")
        .expect("attachment command task remains live")?;
    assert_eq!(
        outcome,
        ReviewWorkflowCommandOutcome::Recorded(ReviewWorkflowCommandResult::ExternalLinkAttached {
            link,
            external_object: requested_attachment.external_object().clone(),
        },)
    );
    let loaded = fixture
        .store
        .load_external_link(link)
        .await?
        .expect("committed external link remains loadable");
    assert_eq!(loaded.attachment(), Some(&requested_attachment));
    assert_eq!(loaded.observations(), &[concurrent_observation]);
    Ok(())
}

/// exact review-command replay and effect recovery preserve one result.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn review_workflow_command_receipts_replay_and_recover() -> Result<(), Box<dyn Error>> {
    const TARGET_IDENTITY: u128 = 0x760;
    const RECOVERED_TARGET_IDENTITY: u128 = 0x761;
    const COMMAND_IDENTITY: u128 = 0x762;
    const RECOVERY_COMMAND_IDENTITY: u128 = 0x763;

    let (_container, pool) = migrated_postgres_with_max_connections(1).await?;
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

    let (_container, pool) = migrated_postgres_with_max_connections(1).await?;
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

    let (_container, pool) = migrated_postgres_with_max_connections(1).await?;
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

    let (_container, pool) = migrated_postgres_with_max_connections(1).await?;
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

    let (_container, pool) = migrated_postgres_with_max_connections(1).await?;
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

    let (_container, pool) = migrated_postgres_with_max_connections(1).await?;
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
