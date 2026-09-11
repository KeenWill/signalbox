//! Run pass lifecycle coverage.

use super::*;

/// an accepted orchestration input is owned by at most one review
/// pass.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn accepted_input_owns_at_most_one_review_pass() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let run_ref = ReviewRunRef::new(fixture.target, ReviewRunId::from_uuid(uuid(0x215)));
    let pass_ref = ReviewPassRef::new(run_ref, ReviewPassId::from_uuid(uuid(0x216)));
    let mut run = ReviewRun::new(
        run_ref,
        ReviewWorkflowKind::ReadOnlyReview,
        ReviewPolicy::version_one(),
    );
    let duplicate = ReviewPass::try_new(
        pass_ref,
        ReviewPassKind::ReadOnlyReview,
        &mut run,
        SessionId::from_uuid(uuid(0x201)),
        ReviewPassAcceptedInputEvidence::new(
            AcceptedInputId::from_uuid(uuid(0x202)),
            SessionId::from_uuid(uuid(0x201)),
            Some(TurnId::from_uuid(uuid(0x203))),
        ),
    )
    .expect("domain construction cannot inspect the global pass inventory");
    fixture.store.insert_run(&run).await?;

    let error = fixture
        .store
        .insert_pass(&duplicate)
        .await
        .expect_err("the canonical accepted input already belongs to a pass");
    let ReviewWorkflowStoreError::Database(error) = error else {
        panic!("expected the unique ownership constraint to reject the pass");
    };
    assert_sqlstate(&error, "23505");
    Ok(())
}

/// pass failure is the workflow-operation outcome and may follow a
/// canonically completed turn.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn failed_pass_accepts_completed_turn_evidence() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let (_, turn) = start_review_pass(&fixture.store, fixture.pass).await;
    complete_review_turn(&pool, turn).await;

    let evidence = conclude_review_pass(
        &fixture.store,
        fixture.pass,
        ReviewPassState::Failed { turn },
    )
    .await;

    assert_eq!(evidence.state(), &ReviewPassState::Failed { turn });
    assert_eq!(
        fixture
            .store
            .load_pass(fixture.pass.pass())
            .await?
            .expect("failed pass remains loadable")
            .state(),
        &ReviewPassState::Failed { turn }
    );
    Ok(())
}

/// loading a queued run retains its already-recorded pass, so the
/// store rejects a passless cancellation before issuing an update.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn queued_run_cannot_discard_recorded_pass() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;

    let error = fixture
        .store
        .transition_run(
            fixture.run.run(),
            ReviewRunState::Cancelled { last_pass: None },
        )
        .await
        .expect_err("queued run loading must retain its canonical pass");
    assert!(matches!(
        error,
        ReviewWorkflowStoreError::InvalidTransition(ReviewWorkflowTransitionError::Run(_))
    ));

    Ok(())
}

/// pre-start cancellation updates the run and pass atomically.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn queued_run_and_pass_cancel_together() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let (run, pass) = fixture
        .store
        .transition_run_and_pass(
            fixture.run.run(),
            fixture.pass.pass(),
            ReviewRunState::Cancelled {
                last_pass: Some(fixture.pass),
            },
            ReviewPassState::Cancelled { turn: None },
        )
        .await?
        .expect("queued run and pass exist");
    assert_eq!(
        run.state(),
        ReviewRunState::Cancelled {
            last_pass: Some(fixture.pass),
        }
    );
    assert_eq!(pass.state(), &ReviewPassState::Cancelled { turn: None });
    Ok(())
}

/// a running pass may load while its canonical turn has reached a
/// terminal outcome not yet projected into the pass.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn running_pass_admits_monotonic_terminal_turn_lag() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let turn = TurnId::from_uuid(uuid(0x203));
    start_review_pass(&fixture.store, fixture.pass).await;
    complete_review_turn(&pool, turn).await;
    let loaded = fixture
        .store
        .load_pass(fixture.pass.pass())
        .await?
        .expect("running pass still loads after its turn concludes");
    assert_eq!(loaded.state(), &ReviewPassState::Running { turn });
    Ok(())
}

/// a queued run and pass activate atomically when their canonical turn
/// completed before the projection was committed.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn queued_run_and_pass_activate_from_terminal_turn_evidence() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let turn = TurnId::from_uuid(uuid(0x203));
    complete_review_turn(&pool, turn).await;

    let (run, pass) = fixture
        .store
        .transition_run_and_pass(
            fixture.run.run(),
            fixture.pass.pass(),
            ReviewRunState::Running {
                active_pass: fixture.pass,
            },
            ReviewPassState::Running { turn },
        )
        .await?
        .expect("queued run and pass exist");

    assert_eq!(
        run.state(),
        ReviewRunState::Running {
            active_pass: fixture.pass,
        }
    );
    assert_eq!(pass.state(), &ReviewPassState::Running { turn });
    Ok(())
}

/// terminal pass effects require their exact finding-event,
/// attachment, or observation child row in the same transaction.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn pass_results_require_exact_child_rows() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let publish_pass = insert_fixture_pass(&fixture, 0x7a0, ReviewPassKind::Publish).await;
    let import_pass =
        insert_fixture_pass(&fixture, 0x7a1, ReviewPassKind::ImportExternalContext).await;
    let judge_pass = insert_fixture_pass(&fixture, 0x7a3, ReviewPassKind::Judge).await;
    let evidence = succeed_fixture_passes(
        &pool,
        &fixture.store,
        &[fixture.pass, publish_pass, import_pass, judge_pass],
    )
    .await;
    let finding_ref = ReviewFindingRef::new(fixture.pass, ReviewFindingId::from_uuid(uuid(0x7a4)));
    let review_evidence = pass_with_produced_findings(vec![finding_ref], evidence[0].clone());
    fixture
        .store
        .insert_finding(&finding(
            finding_ref,
            review_evidence,
            &fixture.target_snapshot,
        ))
        .await?;
    let link = ReviewExternalLinkId::from_uuid(uuid(0x7a2));
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

    let mut attachment_only = pool.begin().await?;
    sqlx::query(
        "UPDATE review_pass
            SET result_kind = 'external_link_attachment',
                result_external_link_id = $2,
                result_external_object_key = 'comment-7a2'
          WHERE pass_id = $1",
    )
    .bind(publish_pass.pass().into_uuid())
    .bind(link.into_uuid())
    .execute(&mut *attachment_only)
    .await?;
    let missing_attachment = attachment_only
        .commit()
        .await
        .expect_err("attachment result cannot commit without its child row");
    assert_sqlstate(&missing_attachment, "23514");

    fixture
        .store
        .attach_external_link(
            link,
            attachment(link, evidence[1].clone(), key("comment-7a2")),
        )
        .await?;
    let mut observation_only = pool.begin().await?;
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
    .execute(&mut *observation_only)
    .await?;
    let missing_observation = observation_only
        .commit()
        .await
        .expect_err("observation result cannot commit without its child row");
    assert_sqlstate(&missing_observation, "23514");

    let mut finding_event_only = pool.begin().await?;
    sqlx::query(
        "UPDATE review_pass
            SET result_kind = 'finding_event',
                result_finding_id = $2,
                result_finding_run_id = $3,
                result_finding_pass_id = $4,
                result_event_ordinal = 1,
                result_event_kind = 'accepted'
          WHERE pass_id = $1",
    )
    .bind(judge_pass.pass().into_uuid())
    .bind(finding_ref.finding().into_uuid())
    .bind(finding_ref.run().run().into_uuid())
    .bind(finding_ref.pass().pass().into_uuid())
    .execute(&mut *finding_event_only)
    .await?;
    let missing_finding_event = finding_event_only
        .commit()
        .await
        .expect_err("finding-event result cannot commit without its child row");
    assert_sqlstate(&missing_finding_event, "23514");
    Ok(())
}
