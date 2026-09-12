//! Loaders coverage.

use super::*;

#[track_caller]
fn assert_read_only_success_requires_atomic_inventory(error: ReviewWorkflowStoreError) {
    let ReviewWorkflowStoreError::InvalidTransition(ReviewWorkflowTransitionError::Pass(error)) =
        error
    else {
        panic!("missing read-only inventory must be a typed pass-transition rejection");
    };
    assert_eq!(
        error.failure(),
        ReviewPassTransitionFailure::IncompatibleResult
    );
}

async fn load_review_aggregate(
    store: &ReviewWorkflowStore,
    reference: ReviewPassRef,
) -> (ReviewRun, ReviewPass) {
    let run = store
        .load_run(reference.run().run())
        .await
        .expect("review run loads without corruption")
        .expect("review run exists");
    let pass = store
        .load_pass(reference.pass())
        .await
        .expect("review pass loads without corruption")
        .expect("review pass exists");
    (run, pass)
}

/// the store reconstructs complete workflow evidence,
/// including the canonical reservation, attachment, and observation sequence.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn review_workflow_store_reconstructs_complete_evidence() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres_with_max_connections(1).await?;
    let store = ReviewWorkflowStore::new(pool.clone());
    let session = SessionId::from_uuid(uuid(0x201));
    let accepted_input = AcceptedInputId::from_uuid(uuid(0x202));
    let turn = TurnId::from_uuid(uuid(0x203));
    insert_active_turn(&pool, session, accepted_input, turn).await;

    let target_id = ReviewTargetId::from_uuid(uuid(0x301));
    let target = ReviewTarget::try_new(
        target_id,
        key("example-code-host"),
        key("example/repository"),
        ReviewTargetSubject::ChangeRequest(
            ReviewChangeRequestNumber::try_new(42).expect("positive change request"),
        ),
        key("0123456789abcdef"),
        Some(key("fedcba9876543210")),
        None,
    )
    .expect("fixture target topology is valid");
    store.insert_target(&target).await.expect("target persists");
    assert_eq!(
        store.load_target(target_id).await.expect("target loads"),
        Some(target.clone())
    );

    let run_ref = ReviewRunRef::new(target_id, ReviewRunId::from_uuid(uuid(0x302)));
    let pass_ref = ReviewPassRef::new(run_ref, ReviewPassId::from_uuid(uuid(0x303)));
    let mut run = ReviewRun::new(
        run_ref,
        ReviewWorkflowKind::ReadOnlyReview,
        ReviewPolicy::version_one(),
    );
    let pass = ReviewPass::try_new(
        pass_ref,
        ReviewPassKind::ReadOnlyReview,
        &mut run,
        session,
        ReviewPassAcceptedInputEvidence::new(accepted_input, session, Some(turn)),
    )
    .expect("accepted input belongs to the fixture session");
    store.insert_run(&run).await.expect("queued run persists");
    store
        .insert_pass(&pass)
        .await
        .expect("queued pass persists");
    let (judge_pass, judge_turn) =
        insert_isolated_pass_for_target(&pool, &store, target_id, 0x306, ReviewPassKind::Judge)
            .await;
    let (publish_pass, publish_turn) =
        insert_isolated_pass_for_target(&pool, &store, target_id, 0x307, ReviewPassKind::Publish)
            .await;
    let (import_pass, import_turn) = insert_isolated_pass_for_target(
        &pool,
        &store,
        target_id,
        0x308,
        ReviewPassKind::ImportExternalContext,
    )
    .await;
    let (unchanged_import_pass, unchanged_import_turn) = insert_isolated_pass_for_target(
        &pool,
        &store,
        target_id,
        0x309,
        ReviewPassKind::ImportExternalContext,
    )
    .await;
    let (running_review, _) = start_review_pass(&store, pass_ref).await;
    start_review_pass(&store, judge_pass).await;
    start_review_pass(&store, publish_pass).await;
    start_review_pass(&store, import_pass).await;
    start_review_pass(&store, unchanged_import_pass).await;
    let output_frontier = complete_review_turn(&pool, turn).await;
    let judge_output_frontier = complete_review_turn(&pool, judge_turn).await;
    let publish_output_frontier = complete_review_turn(&pool, publish_turn).await;
    let import_output_frontier = complete_review_turn(&pool, import_turn).await;
    let unchanged_import_output_frontier = complete_review_turn(&pool, unchanged_import_turn).await;
    let review_evidence = propose_read_only_success(&store, running_review, output_frontier).await;
    let judge_evidence = conclude_review_pass(
        &store,
        judge_pass,
        ReviewPassState::Succeeded {
            turn: judge_turn,
            output_frontier: judge_output_frontier,
            result: None,
        },
    )
    .await;
    let publish_evidence = conclude_review_pass(
        &store,
        publish_pass,
        ReviewPassState::Succeeded {
            turn: publish_turn,
            output_frontier: publish_output_frontier,
            result: None,
        },
    )
    .await;
    let import_evidence = conclude_review_pass(
        &store,
        import_pass,
        ReviewPassState::Succeeded {
            turn: import_turn,
            output_frontier: import_output_frontier,
            result: None,
        },
    )
    .await;
    let unchanged_import_evidence = conclude_review_pass(
        &store,
        unchanged_import_pass,
        ReviewPassState::Succeeded {
            turn: unchanged_import_turn,
            output_frontier: unchanged_import_output_frontier,
            result: None,
        },
    )
    .await;

    let finding_ref = ReviewFindingRef::new(pass_ref, ReviewFindingId::from_uuid(uuid(0x304)));
    let open_finding = finding(finding_ref, review_evidence, &target);
    store
        .insert_finding(&open_finding)
        .await
        .expect("open finding persists");
    let accepted_event = finding_event(
        finding_ref,
        ReviewEventOrdinal::one(),
        judge_evidence,
        ReviewFindingEventKind::Accepted {
            confidence: signalbox_domain::ReviewJudgeConfidence::try_new(5)
                .expect("judge confidence"),
        },
    );
    let accepted_finding = open_finding
        .clone()
        .apply(accepted_event.clone())
        .expect("open finding accepts judgment");
    assert_eq!(
        store
            .append_finding_event(finding_ref.finding(), accepted_event)
            .await
            .expect("finding event persists"),
        Some(accepted_finding.clone())
    );

    let link_id = ReviewExternalLinkId::from_uuid(uuid(0x305));
    let reservation = ReviewExternalLink::try_reserve(
        link_id,
        ReviewExternalLinkAssociation::Finding(finding_ref),
        key("example-code-host"),
        ReviewExternalObjectKind::ReviewComment,
        &target,
    )
    .expect("reservation matches the target");
    assert_eq!(
        store
            .reserve_external_link(reservation.clone())
            .await
            .expect("first reservation persists"),
        ReserveExternalLinkOutcome::Inserted(reservation.clone())
    );
    assert_eq!(
        store
            .reserve_external_link(reservation.clone())
            .await
            .expect("equal replay loads the canonical reservation"),
        ReserveExternalLinkOutcome::Existing(reservation.clone())
    );
    let conflicting = ReviewExternalLink::try_reserve(
        link_id,
        ReviewExternalLinkAssociation::Finding(finding_ref),
        key("example-code-host"),
        ReviewExternalObjectKind::ReviewThread,
        &target,
    )
    .expect("conflicting payload remains target-valid");
    assert!(matches!(
        store.reserve_external_link(conflicting).await,
        Err(ReviewWorkflowStoreError::ReservationConflict(_))
    ));

    let posted_ordinal = ReviewEventOrdinal::try_new(2).expect("positive ordinal");
    let attachment = posted_attachment(
        link_id,
        publish_evidence,
        key("comment-84"),
        finding_ref,
        posted_ordinal,
    );
    let attached = reservation
        .clone()
        .attach(attachment.clone())
        .expect("same-target pass may attach");
    assert_eq!(
        store
            .attach_external_link(link_id, attachment)
            .await
            .expect("attachment persists"),
        Some(attached.clone())
    );
    let attachment_evidence = attached
        .attachment()
        .expect("attached link carries the producing pass")
        .pass_evidence()
        .clone();
    let posted_event = ReviewFindingEvent::new(
        finding_ref,
        posted_ordinal,
        attachment_evidence.reference(),
        attachment_evidence.clone(),
        run_evidence_for_pass(attachment_evidence),
        ReviewFindingEventKind::Posted {
            link: Box::new(
                signalbox_domain::ReviewFindingExternalLinkRef::try_new(finding_ref, &attached)
                    .expect("attached canonical link belongs to the finding"),
            ),
        },
    );
    let posted_finding = accepted_finding
        .apply(posted_event)
        .expect("accepted finding may record an attached publication");
    assert_eq!(
        store
            .load_finding(finding_ref.finding())
            .await
            .expect("atomically posted finding loads"),
        Some(posted_finding.clone())
    );
    sqlx::query(
        "ALTER TABLE review_external_link
         DROP CONSTRAINT review_external_link_finding_fk",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE review_external_link
         DISABLE TRIGGER review_external_link_is_append_only",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE review_external_link
            SET finding_producing_pass_id = $1
          WHERE external_link_id = $2",
    )
    .bind(judge_pass.pass().into_uuid())
    .bind(link_id.into_uuid())
    .execute(&pool)
    .await?;
    let error = store
        .load_finding(finding_ref.finding())
        .await
        .expect_err("finding loading must authenticate the stored link producer");
    let ReviewWorkflowStoreError::Corruption(error) = error else {
        panic!("expected typed finding-history corruption");
    };
    assert_eq!(error.aggregate(), "review_external_link");
    assert!(
        error
            .detail()
            .contains("finding producing pass row is missing"),
        "unexpected corruption detail: {}",
        error.detail(),
    );
    sqlx::query(
        "UPDATE review_external_link
            SET finding_producing_pass_id = $1
          WHERE external_link_id = $2",
    )
    .bind(pass_ref.pass().into_uuid())
    .bind(link_id.into_uuid())
    .execute(&pool)
    .await?;
    let first_observation = observation(
        link_id,
        ReviewEventOrdinal::one(),
        import_evidence,
        ReviewExternalObjectState::Current,
    );
    let observed = attached
        .observe(first_observation.clone())
        .expect("first observation is contiguous");
    assert_eq!(
        store
            .append_external_observation(link_id, first_observation)
            .await
            .expect("observation persists"),
        Some(observed.clone())
    );
    assert_eq!(
        store
            .load_external_link(link_id)
            .await
            .expect("complete link loads"),
        Some(observed.clone())
    );
    assert_eq!(
        store
            .load_finding(finding_ref.finding())
            .await
            .expect("posted finding loads through observed link history"),
        Some(posted_finding.clone())
    );
    for (reported_link, ordinal) in [
        (ReviewExternalLinkId::from_uuid(Uuid::nil()), 2),
        (link_id, 3),
    ] {
        let invalid_report = observation(
            reported_link,
            ReviewEventOrdinal::try_new(ordinal).expect("positive ordinal"),
            unchanged_import_evidence.clone(),
            ReviewExternalObjectState::Current,
        );
        store
            .append_external_observation(link_id, invalid_report)
            .await
            .expect_err("unchanged reports must name the link and next ordinal");
    }
    let unchanged = observation(
        link_id,
        ReviewEventOrdinal::try_new(2).expect("positive ordinal"),
        unchanged_import_evidence,
        ReviewExternalObjectState::Current,
    );
    let unchanged_link = store
        .append_external_observation(link_id, unchanged)
        .await
        .expect("unchanged polling is a semantic no-op")
        .expect("the canonical link remains present");
    let unchanged_pass = store
        .load_pass(unchanged_import_pass.pass())
        .await?
        .expect("unchanged import pass remains durable");
    assert_external_link_no_change_result(
        unchanged_pass.state(),
        ReviewExternalLinkNoChangeResult::new(
            link_id,
            ReviewEventOrdinal::one(),
            ReviewExternalObjectState::Current,
        ),
    );
    let unchanged_evidence =
        ReviewPassEvidence::from_pass(&unchanged_pass, ReviewPolicy::version_one());
    let expected_unchanged = observed
        .confirm_unchanged(
            unchanged_evidence.clone(),
            run_evidence_for_pass(unchanged_evidence),
        )
        .expect("the durable no-change result authenticates its claim");
    assert_eq!(unchanged_link, expected_unchanged);
    assert_eq!(
        store
            .load_external_link(link_id)
            .await
            .expect("no-change claim reloads"),
        Some(expected_unchanged.clone())
    );
    assert_eq!(
        store
            .load_finding(finding_ref.finding())
            .await
            .expect("posted finding loads through a durable no-change claim"),
        Some(posted_finding.clone())
    );
    let (later_import_pass, later_import_turn) = insert_isolated_pass_for_target(
        &pool,
        &store,
        target_id,
        0x30a,
        ReviewPassKind::ImportExternalContext,
    )
    .await;
    start_review_pass(&store, later_import_pass).await;
    let later_output_frontier = complete_review_turn(&pool, later_import_turn).await;
    let later_import_evidence = conclude_review_pass(
        &store,
        later_import_pass,
        ReviewPassState::Succeeded {
            turn: later_import_turn,
            output_frontier: later_output_frontier,
            result: None,
        },
    )
    .await;
    let later_observation = observation(
        link_id,
        ReviewEventOrdinal::try_new(2).expect("positive ordinal"),
        later_import_evidence,
        ReviewExternalObjectState::Outdated,
    );
    let expected_advanced = expected_unchanged
        .observe(later_observation.clone())
        .expect("later changed state advances the observation frontier");
    assert_eq!(
        store
            .append_external_observation(link_id, later_observation)
            .await
            .expect("later changed state persists"),
        Some(expected_advanced.clone())
    );
    assert_eq!(
        store
            .load_external_link(link_id)
            .await
            .expect("historical no-change claim reloads after later state"),
        Some(expected_advanced)
    );
    assert_eq!(
        store
            .load_finding(finding_ref.finding())
            .await
            .expect("posted finding loads after later external observations"),
        Some(posted_finding)
    );
    let unrelated_target = ReviewTarget::try_new(
        ReviewTargetId::from_uuid(uuid(0x30b)),
        key("example-code-host"),
        key("example/repository"),
        ReviewTargetSubject::Commit,
        key("unrelated-head"),
        None,
        None,
    )
    .expect("unrelated target is structurally valid");
    store.insert_target(&unrelated_target).await?;
    sqlx::query(
        "ALTER TABLE review_external_object_identity
         DISABLE TRIGGER review_external_object_identity_is_append_only",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE review_external_object_identity
            SET logical_target_id = $1
          WHERE provider_key = 'example-code-host'
            AND object_kind = 'review_comment'
            AND external_object_key = 'comment-84'",
    )
    .bind(unrelated_target.id().into_uuid())
    .execute(&pool)
    .await?;
    let identity_error = store
        .load_finding(finding_ref.finding())
        .await
        .expect_err("finding history must authenticate the external-object registry");
    let ReviewWorkflowStoreError::Corruption(identity_error) = identity_error else {
        panic!("expected typed finding-history corruption");
    };
    assert_eq!(
        identity_error.aggregate(),
        "review_external_link_attachment"
    );
    assert!(identity_error.detail().contains("unrelated logical target"));
    sqlx::query(
        "UPDATE review_external_object_identity
            SET logical_target_id = $1
          WHERE provider_key = 'example-code-host'
            AND object_kind = 'review_comment'
            AND external_object_key = 'comment-84'",
    )
    .bind(target_id.into_uuid())
    .execute(&pool)
    .await?;

    sqlx::query(
        "ALTER TABLE review_pass
         DISABLE TRIGGER review_pass_change_is_guarded",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE review_pass
            SET result_event_ordinal = 2
          WHERE pass_id = $1",
    )
    .bind(unchanged_import_pass.pass().into_uuid())
    .execute(&pool)
    .await?;
    let error = store
        .load_external_link(link_id)
        .await
        .expect_err("a no-change claim cannot consume a stale observation frontier");
    let ReviewWorkflowStoreError::Corruption(error) = error else {
        panic!("expected typed external-link corruption");
    };
    assert_eq!(error.aggregate(), "review_external_link");
    assert!(
        error.detail().contains("IncompatibleObservationPass"),
        "unexpected corruption detail: {}",
        error.detail(),
    );

    Ok(())
}

/// pass loading validates the accepted input's canonical session.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn pass_loader_rejects_cross_wired_accepted_input() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let pass = fixture.pass.pass();
    let other_session = SessionId::from_uuid(uuid(0x211));
    let other_input = AcceptedInputId::from_uuid(uuid(0x212));
    let other_turn = TurnId::from_uuid(uuid(0x213));
    insert_active_turn_with_offset(&pool, other_session, other_input, other_turn, 0x1_000).await;

    sqlx::query(
        "ALTER TABLE review_pass
         DISABLE TRIGGER review_pass_change_is_guarded",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE review_pass
         DISABLE TRIGGER review_pass_run_projection_is_guarded",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE review_pass
         DROP CONSTRAINT review_pass_accepted_input_fk",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE review_pass
         DROP CONSTRAINT review_pass_origin_turn_fk",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE review_pass
            SET session_id = $2
          WHERE pass_id = $1",
    )
    .bind(pass.into_uuid())
    .bind(other_session.into_uuid())
    .execute(&pool)
    .await?;
    let session_error = fixture
        .store
        .load_pass(pass)
        .await
        .expect_err("canonical accepted-input session must reject cross-wiring");
    let ReviewWorkflowStoreError::Corruption(session_error) = session_error else {
        panic!("expected typed review-pass corruption");
    };
    assert_eq!(session_error.aggregate(), "review_pass");
    assert!(
        session_error
            .detail()
            .contains("AcceptedInputSessionMismatch")
    );
    sqlx::query(
        "UPDATE review_pass
            SET session_id = $2
          WHERE pass_id = $1",
    )
    .bind(pass.into_uuid())
    .bind(uuid(0x201))
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE accepted_input
         DISABLE TRIGGER ALL",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE accepted_input
            SET origin_turn_id = $2
          WHERE accepted_input_id = $1",
    )
    .bind(uuid(0x202))
    .bind(uuid(0x214))
    .execute(&pool)
    .await?;
    let origin_error = fixture
        .store
        .load_pass(pass)
        .await
        .expect_err("canonical accepted-input origin must reject cross-wiring");
    let ReviewWorkflowStoreError::Corruption(origin_error) = origin_error else {
        panic!("expected typed review-pass corruption");
    };
    assert_eq!(origin_error.aggregate(), "review_pass");
    assert!(
        origin_error
            .detail()
            .contains("accepted input origin turn differs")
    );

    Ok(())
}

/// pass loading authenticates the queued pass's exact origin turn,
/// independently of its accepted-input snapshot.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn pass_loader_rejects_missing_origin_turn() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let missing_turn = uuid(0x21f);

    sqlx::query(
        "ALTER TABLE review_pass
         DISABLE TRIGGER review_pass_change_is_guarded",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE review_pass
         DROP CONSTRAINT review_pass_origin_turn_fk",
    )
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE accepted_input DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query(
        "UPDATE accepted_input
            SET origin_turn_id = $2
          WHERE accepted_input_id = $1",
    )
    .bind(uuid(0x202))
    .bind(missing_turn)
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE review_pass
            SET origin_turn_id = $2
          WHERE pass_id = $1",
    )
    .bind(fixture.pass.pass().into_uuid())
    .bind(missing_turn)
    .execute(&pool)
    .await?;

    let error = fixture
        .store
        .load_pass(fixture.pass.pass())
        .await
        .expect_err("missing canonical origin turn must fail pass loading closed");
    let ReviewWorkflowStoreError::Corruption(error) = error else {
        panic!("expected typed review-pass corruption");
    };
    assert_eq!(error.aggregate(), "review_pass");
    assert!(error.detail().contains("origin turn row is missing"));
    Ok(())
}

/// a pass whose canonical target row is missing is corruption, even
/// when its run row remains present.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn pass_loader_rejects_missing_target() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    sqlx::query(
        "ALTER TABLE review_run
         DROP CONSTRAINT review_run_target_fk",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE review_target
         DISABLE TRIGGER review_target_is_append_only",
    )
    .execute(&pool)
    .await?;
    sqlx::query("DELETE FROM review_target WHERE target_id = $1")
        .bind(fixture.target.into_uuid())
        .execute(&pool)
        .await?;

    let error = fixture
        .store
        .load_pass(fixture.pass.pass())
        .await
        .expect_err("missing canonical target must fail pass loading closed");
    let ReviewWorkflowStoreError::Corruption(error) = error else {
        panic!("expected typed review-pass corruption");
    };
    assert_eq!(error.aggregate(), "review_pass");
    assert!(error.detail().contains("target row is missing"));
    Ok(())
}

/// pass loading validates the referenced turn's canonical session.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn pass_loader_rejects_cross_wired_turn() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let pass = fixture.pass.pass();
    let other_session = SessionId::from_uuid(uuid(0x211));
    let other_input = AcceptedInputId::from_uuid(uuid(0x212));
    let other_turn = TurnId::from_uuid(uuid(0x213));
    insert_active_turn_with_offset(&pool, other_session, other_input, other_turn, 0x1_000).await;

    sqlx::query(
        "ALTER TABLE review_pass
         DISABLE TRIGGER review_pass_change_is_guarded",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE review_pass
         DISABLE TRIGGER review_pass_run_projection_is_guarded",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE review_pass
         DROP CONSTRAINT review_pass_turn_fk",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE review_pass
            SET state_kind = 'running',
                turn_id = $2
          WHERE pass_id = $1",
    )
    .bind(pass.into_uuid())
    .bind(other_turn.into_uuid())
    .execute(&pool)
    .await?;
    let turn_error = fixture
        .store
        .load_pass(pass)
        .await
        .expect_err("canonical turn ownership must reject cross-wiring");
    let ReviewWorkflowStoreError::Corruption(turn_error) = turn_error else {
        panic!("expected typed review-pass corruption");
    };
    assert_eq!(turn_error.aggregate(), "review_pass");
    assert!(
        turn_error.detail().contains("TurnOriginMismatch"),
        "unexpected corruption detail: {}",
        turn_error.detail()
    );

    Ok(())
}

/// a run projection may report only the canonical outcome of its
/// referenced pass.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn run_projection_rejects_noncanonical_pass_outcome() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    start_review_pass(&fixture.store, fixture.pass).await;

    let guarded = sqlx::query(
        "UPDATE review_run
            SET state_kind = 'succeeded'
          WHERE run_id = $1",
    )
    .bind(fixture.run.run().into_uuid())
    .execute(&pool)
    .await
    .expect_err("run success requires a canonically succeeded pass");
    assert_sqlstate(&guarded, "23514");

    sqlx::query(
        "ALTER TABLE review_run
         DISABLE TRIGGER review_run_change_is_guarded",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE review_run
         DISABLE TRIGGER review_run_pass_projection_is_guarded",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE review_run
            SET state_kind = 'succeeded'
          WHERE run_id = $1",
    )
    .bind(fixture.run.run().into_uuid())
    .execute(&pool)
    .await?;
    let error = fixture
        .store
        .load_run(fixture.run.run())
        .await
        .expect_err("loader must reject a run/pass outcome contradiction");
    let ReviewWorkflowStoreError::Corruption(error) = error else {
        panic!("expected typed review-run corruption");
    };
    assert_eq!(error.aggregate(), "review_run");
    assert!(error.detail().contains("PassStateMismatch"));

    Ok(())
}

/// loading a pass validates the canonical state projection of its run.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn pass_loader_rejects_noncanonical_run_projection() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;

    sqlx::query(
        "ALTER TABLE review_run
         DISABLE TRIGGER review_run_change_is_guarded",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE review_run
         DISABLE TRIGGER review_run_pass_projection_is_guarded",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE review_run
            SET state_kind = 'cancelled',
                state_pass_id = NULL
          WHERE run_id = $1",
    )
    .bind(fixture.run.run().into_uuid())
    .execute(&pool)
    .await?;

    let error = fixture
        .store
        .load_pass(fixture.pass.pass())
        .await
        .expect_err("pass loading must reject a contradictory canonical run");
    let ReviewWorkflowStoreError::Corruption(error) = error else {
        panic!("expected typed review-run corruption");
    };
    assert_eq!(error.aggregate(), "review_run");
    assert!(error.detail().contains("UnexpectedPassEvidence"));
    Ok(())
}

/// a pass projection may report only the canonical outcome of its
/// referenced turn.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn pass_projection_rejects_noncanonical_turn_outcome() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    start_review_pass(&fixture.store, fixture.pass).await;

    let guarded = sqlx::query(
        "UPDATE review_pass
            SET state_kind = 'failed'
          WHERE pass_id = $1",
    )
    .bind(fixture.pass.pass().into_uuid())
    .execute(&pool)
    .await
    .expect_err("pass failure requires a canonical terminal turn");
    assert_sqlstate(&guarded, "23514");

    sqlx::query(
        "ALTER TABLE review_pass
         DISABLE TRIGGER review_pass_change_is_guarded",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE review_pass
         DISABLE TRIGGER review_pass_run_projection_is_guarded",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE review_pass
            SET state_kind = 'failed'
          WHERE pass_id = $1",
    )
    .bind(fixture.pass.pass().into_uuid())
    .execute(&pool)
    .await?;
    let error = fixture
        .store
        .load_pass(fixture.pass.pass())
        .await
        .expect_err("loader must reject a pass/turn outcome contradiction");
    let ReviewWorkflowStoreError::Corruption(error) = error else {
        panic!("expected typed review-pass corruption");
    };
    assert_eq!(error.aggregate(), "review_pass");
    assert!(error.detail().contains("TurnOutcomeMismatch"));

    Ok(())
}

/// canonical read-only success admission is atomic, so every committed
/// intermediate aggregate remains loadable rather than appearing corrupt.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn read_only_success_admission_is_atomic_and_always_loadable() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;

    let (queued_run, queued_pass) = load_review_aggregate(&fixture.store, fixture.pass).await;
    assert_eq!(queued_run.state(), ReviewRunState::Queued);
    assert_eq!(queued_pass.state(), &ReviewPassState::Queued);

    let (running, turn) = start_review_pass(&fixture.store, fixture.pass).await;
    let running_run_state = ReviewRunState::Running {
        active_pass: fixture.pass,
    };
    assert_eq!(running.state(), &ReviewPassState::Running { turn });
    let (loaded_run, loaded_pass) = load_review_aggregate(&fixture.store, fixture.pass).await;
    assert_eq!(loaded_run.state(), running_run_state);
    assert_eq!(loaded_pass.state(), running.state());

    let output_frontier = complete_review_turn(&pool, turn).await;
    let (loaded_run, loaded_pass) = load_review_aggregate(&fixture.store, fixture.pass).await;
    assert_eq!(loaded_run.state(), running_run_state);
    assert_eq!(loaded_pass.state(), running.state());

    let unbound_success = ReviewPassState::Succeeded {
        turn,
        output_frontier,
        result: None,
    };
    let error = fixture
        .store
        .transition_run_and_pass(
            fixture.run.run(),
            fixture.pass.pass(),
            ReviewRunState::Succeeded {
                concluding_pass: fixture.pass,
            },
            unbound_success,
        )
        .await
        .expect_err("read-only success without its inventory is rejected before commit");
    assert_read_only_success_requires_atomic_inventory(error);
    let (loaded_run, loaded_pass) = load_review_aggregate(&fixture.store, fixture.pass).await;
    assert_eq!(loaded_run.state(), running_run_state);
    assert_eq!(loaded_pass.state(), running.state());

    let no_finding_references = Vec::new();
    let no_findings = Vec::<ReviewFinding>::new();
    let produced_findings = ReviewPassResult::ProducedFindings(
        ReviewProducedFindings::try_new(no_finding_references)
            .expect("empty inventory is canonical"),
    );
    let completed_turn = ReviewPassTurnEvidence::new(
        turn,
        running.session(),
        running.accepted_input(),
        ReviewPassTurnOutcome::Completed,
        Some(output_frontier),
    );
    let succeeded = running
        .transition(
            ReviewPassState::Succeeded {
                turn,
                output_frontier,
                result: Some(produced_findings),
            },
            Some(completed_turn),
        )
        .expect("completed read-only pass proposes its exact inventory");
    let evidence = ReviewPassEvidence::from_pass(&succeeded, queued_run.policy());
    fixture
        .store
        .insert_findings(&evidence, &no_findings)
        .await?;

    let (loaded_run, loaded_pass) = load_review_aggregate(&fixture.store, fixture.pass).await;
    assert_eq!(loaded_pass.state(), evidence.state());
    assert_eq!(
        loaded_run.state(),
        ReviewRunState::Succeeded {
            concluding_pass: fixture.pass,
        }
    );
    Ok(())
}

/// a pass-only state change cannot commit without its run
/// projection.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn pass_only_projection_is_rejected() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let turn = fixture
        .store
        .load_pass(fixture.pass.pass())
        .await?
        .expect("fixture pass exists")
        .origin_turn();
    let mut transaction = pool.begin().await?;
    sqlx::query(
        "UPDATE review_pass
            SET state_kind = 'running',
                turn_id = $2
          WHERE pass_id = $1",
    )
    .bind(fixture.pass.pass().into_uuid())
    .bind(turn.into_uuid())
    .execute(&mut *transaction)
    .await?;
    let error = transaction
        .commit()
        .await
        .expect_err("pass-only activation cannot commit");
    assert_sqlstate(&error, "23514");
    Ok(())
}
