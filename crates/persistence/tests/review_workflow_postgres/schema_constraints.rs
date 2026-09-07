//! Schema constraints coverage.

use super::*;

async fn migrated_postgres_in_configured_schema()
-> Result<(ContainerAsync<Postgres>, PgPool), Box<dyn Error>> {
    let container = Postgres::default()
        .with_db_name(DATABASE_NAME)
        .with_user(DATABASE_USER)
        .with_password(DATABASE_PASSWORD)
        .with_cmd(disposable_postgres_server_args())
        .with_mount(disposable_postgres_state_tmpfs_from_example()?)
        .with_tag(POSTGRES_IMAGE_TAG)
        .with_labels(disposable_test_container_labels())
        .start()
        .await?;
    let host = container.get_host().await?;
    let port = container.get_host_port_ipv4(5432).await?;
    let database_url =
        format!("postgres://{DATABASE_USER}:{DATABASE_PASSWORD}@{host}:{port}/{DATABASE_NAME}");
    let bootstrap = PgPoolOptions::new()
        .max_connections(1)
        .connect_with(local_test_connection_options(&database_url)?)
        .await?;
    sqlx::query("CREATE SCHEMA configured_review_workflow AUTHORIZATION signalbox")
        .execute(&bootstrap)
        .await?;
    sqlx::query(
        "ALTER ROLE signalbox
         SET search_path TO configured_review_workflow",
    )
    .execute(&bootstrap)
    .await?;
    bootstrap.close().await;

    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect_with(local_test_connection_options(&database_url)?)
        .await?;
    migrate(&pool).await?;
    migrate(&pool).await?;
    Ok((container, pool))
}

#[test]
fn maximum_width_key_fixture_is_full_width_and_role_distinct() {
    const MAXIMUM_KEY_BYTES: usize = 1_024;

    let provider = maximum_width_key(MaximumWidthKeyRole::Provider);
    let repository = maximum_width_key(MaximumWidthKeyRole::Repository);
    let head = maximum_width_key(MaximumWidthKeyRole::HeadRevision);
    let base = maximum_width_key(MaximumWidthKeyRole::BaseRevision);

    assert_eq!(provider.as_str().len(), MAXIMUM_KEY_BYTES);
    assert_eq!(repository.as_str().len(), MAXIMUM_KEY_BYTES);
    assert_eq!(head.as_str().len(), MAXIMUM_KEY_BYTES);
    assert_eq!(base.as_str().len(), MAXIMUM_KEY_BYTES);
    assert_ne!(provider, repository);
    assert_ne!(provider, head);
    assert_ne!(provider, base);
    assert_ne!(repository, head);
    assert_ne!(repository, base);
    assert_ne!(head, base);
}

fn finding_with_is_real_confidence(
    reference: ReviewFindingRef,
    producing_pass: ReviewPassEvidence,
    target: &ReviewTarget,
    is_real_confidence: u16,
) -> ReviewFinding {
    finding_with_confidence_axes_and_side(
        reference,
        producing_pass,
        target,
        FindingConfidenceAxes {
            is_real: is_real_confidence,
            severity_label: 8_500,
        },
        Some(ReviewFindingDiffSide::Right),
    )
}

/// the event-head migration retains the connection-selected workflow
/// schema and pins trigger lookups ahead of temporary objects.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn event_head_retains_configured_workflow_schema() -> Result<(), Box<dyn Error>> {
    const WORKFLOW_SCHEMA: &str = "configured_review_workflow";
    const PINNED_SEARCH_PATH: &str = "search_path=configured_review_workflow, pg_catalog, pg_temp";

    let (_container, pool) = migrated_postgres_in_configured_schema().await?;
    let current_schema: String = sqlx::query_scalar("SELECT current_schema()")
        .fetch_one(&pool)
        .await?;
    let head_schema: String = sqlx::query_scalar(
        "SELECT table_schema
           FROM information_schema.tables
          WHERE table_name = 'review_finding_event_head'",
    )
    .fetch_one(&pool)
    .await?;
    let transition_paths_are_pinned: bool = sqlx::query_scalar(
        "SELECT count(*) = 2
                AND bool_and($2 = ANY(function.proconfig))
           FROM pg_proc AS function
           JOIN pg_namespace AS namespace
             ON namespace.oid = function.pronamespace
          WHERE namespace.nspname = $1
            AND function.proname IN (
                'authenticate_review_finding_event_head',
                'advance_review_finding_event_head'
            )",
    )
    .bind(WORKFLOW_SCHEMA)
    .bind(PINNED_SEARCH_PATH)
    .fetch_one(&pool)
    .await?;

    assert_eq!(current_schema, WORKFLOW_SCHEMA);
    assert_eq!(head_schema, WORKFLOW_SCHEMA);
    assert!(transition_paths_are_pinned);
    Ok(())
}

/// lifecycle-only transition APIs reject an effect result before
/// changing the pass or run projection.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn generic_transition_rejects_effect_result() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let (_, turn) = start_review_pass(&fixture.store, fixture.pass).await;
    let output_frontier = complete_review_turn(&pool, turn).await;
    let no_findings =
        ReviewProducedFindings::try_new(Vec::new()).expect("empty inventory is canonical");

    let error = fixture
        .store
        .transition_run_and_pass(
            fixture.run.run(),
            fixture.pass.pass(),
            ReviewRunState::Succeeded {
                concluding_pass: fixture.pass,
            },
            ReviewPassState::Succeeded {
                turn,
                output_frontier,
                result: Some(ReviewPassResult::ProducedFindings(no_findings)),
            },
        )
        .await
        .expect_err("effect result requires its effect-owning transaction");
    assert!(matches!(
        error,
        ReviewWorkflowStoreError::NonAtomicPassResult
    ));
    assert_eq!(
        fixture
            .store
            .load_pass(fixture.pass.pass())
            .await?
            .expect("rejected transition leaves the pass loadable")
            .state(),
        &ReviewPassState::Running { turn }
    );
    Ok(())
}

/// a relational caller cannot forge an event head and then append a
/// later event while omitting the event that supposedly established the head.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn schema_rejects_forged_head_with_gapped_history() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let fix_pass = insert_fixture_pass(&fixture, 0x8d1, ReviewPassKind::Fix).await;
    let evidence = succeed_fixture_passes(&pool, &fixture.store, &[fixture.pass, fix_pass]).await;
    let finding_ref = ReviewFindingRef::new(fixture.pass, ReviewFindingId::from_uuid(uuid(0x8d2)));
    let review_evidence = pass_with_produced_findings(vec![finding_ref], evidence[0].clone());
    let open = finding(finding_ref, review_evidence, &fixture.target_snapshot);
    fixture.store.insert_finding(&open).await?;

    let attack: Result<(), sqlx::Error> = async {
        let mut transaction = pool.begin().await?;
        sqlx::query(
            "UPDATE review_finding_event_head
                SET event_ordinal = 1,
                    status = 'accepted',
                    event_pass_kind = 'judge',
                    external_link_id = NULL
              WHERE finding_id = $1",
        )
        .bind(finding_ref.finding().into_uuid())
        .execute(&mut *transaction)
        .await?;
        sqlx::query(
            "UPDATE review_pass
                SET result_kind = 'finding_event',
                    result_finding_id = $2,
                    result_finding_run_id = $3,
                    result_finding_pass_id = $4,
                    result_event_ordinal = 2,
                    result_event_kind = 'fixed'
              WHERE pass_id = $1",
        )
        .bind(fix_pass.pass().into_uuid())
        .bind(finding_ref.finding().into_uuid())
        .bind(finding_ref.run().run().into_uuid())
        .bind(finding_ref.pass().pass().into_uuid())
        .execute(&mut *transaction)
        .await?;
        sqlx::query(
            "INSERT INTO review_finding_event
                (finding_id, event_ordinal, finding_run_id, target_id,
                 event_pass_id, event_pass_run_id, event_kind, reason,
                 referenced_finding_id, referenced_finding_run_id,
                 referenced_finding_target_id, referenced_finding_pass_id,
                 referenced_finding_status, external_link_id,
                 external_link_association_kind)
             VALUES (
                 $1, 2, $2, $3, $4, $5, 'fixed', NULL,
                 NULL, NULL, NULL, NULL, NULL, NULL, NULL
             )",
        )
        .bind(finding_ref.finding().into_uuid())
        .bind(finding_ref.run().run().into_uuid())
        .bind(finding_ref.target().into_uuid())
        .bind(fix_pass.pass().into_uuid())
        .bind(fix_pass.run().run().into_uuid())
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await
    }
    .await;

    let error = attack.expect_err("a head cannot advance without its exact durable event");
    assert_sqlstate(&error, "23514");
    assert_eq!(
        fixture.store.load_finding(finding_ref.finding()).await?,
        Some(open)
    );
    Ok(())
}

/// a direct event insert that waits behind its uncommitted predecessor
/// authenticates the post-wait head and admits the next ordinal.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn event_sequence_admits_committed_predecessor_after_wait() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let judge_pass = insert_fixture_pass(&fixture, 0x8c1, ReviewPassKind::Judge).await;
    let fix_pass = insert_fixture_pass(&fixture, 0x8c2, ReviewPassKind::Fix).await;
    let evidence =
        succeed_fixture_passes(&pool, &fixture.store, &[fixture.pass, judge_pass, fix_pass]).await;
    let finding_ref = ReviewFindingRef::new(fixture.pass, ReviewFindingId::from_uuid(uuid(0x8c3)));
    let review_evidence = pass_with_produced_findings(vec![finding_ref], evidence[0].clone());
    let open = finding(
        finding_ref,
        review_evidence.clone(),
        &fixture.target_snapshot,
    );
    fixture
        .store
        .insert_finding(&open)
        .await
        .expect("open finding persists");
    let accepted_event = finding_event(
        finding_ref,
        ReviewEventOrdinal::one(),
        evidence[1].clone(),
        ReviewFindingEventKind::Accepted,
    );
    let accepted = open
        .clone()
        .apply(accepted_event.clone())
        .expect("judge accepts the open finding");
    let fixed_event = finding_event(
        finding_ref,
        ReviewEventOrdinal::try_new(2).expect("second ordinal is valid"),
        evidence[2].clone(),
        ReviewFindingEventKind::Fixed,
    );
    let fixed = accepted
        .apply(fixed_event.clone())
        .expect("fix pass closes the accepted finding");

    let mut first_appender = pool.begin().await?;
    sqlx::query(
        "UPDATE review_pass
            SET result_kind = 'finding_event',
                result_finding_id = $2,
                result_finding_run_id = $3,
                result_finding_pass_id = $4,
                result_event_ordinal = $5,
                result_event_kind = 'accepted'
          WHERE pass_id = $1",
    )
    .bind(accepted_event.pass().pass().into_uuid())
    .bind(accepted_event.finding().finding().into_uuid())
    .bind(accepted_event.finding().run().run().into_uuid())
    .bind(accepted_event.finding().pass().pass().into_uuid())
    .bind(i64::from(accepted_event.ordinal().get()))
    .execute(&mut *first_appender)
    .await?;
    sqlx::query(
        "INSERT INTO review_finding_event
            (finding_id, event_ordinal, finding_run_id, target_id,
             event_pass_id, event_pass_run_id, event_kind, reason,
             referenced_finding_id, referenced_finding_run_id,
             referenced_finding_target_id, referenced_finding_pass_id,
             referenced_finding_status, external_link_id,
             external_link_association_kind)
         VALUES (
             $1, $2, $3, $4, $5, $6, 'accepted', NULL,
             NULL, NULL, NULL, NULL, NULL, NULL, NULL
         )",
    )
    .bind(accepted_event.finding().finding().into_uuid())
    .bind(i64::from(accepted_event.ordinal().get()))
    .bind(accepted_event.finding().run().run().into_uuid())
    .bind(accepted_event.finding().target().into_uuid())
    .bind(accepted_event.pass().pass().into_uuid())
    .bind(accepted_event.pass().run().run().into_uuid())
    .execute(&mut *first_appender)
    .await?;

    let mut second_appender = pool.begin().await?;
    sqlx::query(
        "UPDATE review_pass
            SET result_kind = 'finding_event',
                result_finding_id = $2,
                result_finding_run_id = $3,
                result_finding_pass_id = $4,
                result_event_ordinal = $5,
                result_event_kind = 'fixed'
          WHERE pass_id = $1",
    )
    .bind(fixed_event.pass().pass().into_uuid())
    .bind(fixed_event.finding().finding().into_uuid())
    .bind(fixed_event.finding().run().run().into_uuid())
    .bind(fixed_event.finding().pass().pass().into_uuid())
    .bind(i64::from(fixed_event.ordinal().get()))
    .execute(&mut *second_appender)
    .await?;
    let waiting_append = tokio::spawn(async move {
        sqlx::query(
            "INSERT INTO review_finding_event
                (finding_id, event_ordinal, finding_run_id, target_id,
                 event_pass_id, event_pass_run_id, event_kind, reason,
                 referenced_finding_id, referenced_finding_run_id,
                 referenced_finding_target_id, referenced_finding_pass_id,
                 referenced_finding_status, external_link_id,
                 external_link_association_kind)
             VALUES (
                 $1, $2, $3, $4, $5, $6, 'fixed', NULL,
                 NULL, NULL, NULL, NULL, NULL, NULL, NULL
             )",
        )
        .bind(fixed_event.finding().finding().into_uuid())
        .bind(i64::from(fixed_event.ordinal().get()))
        .bind(fixed_event.finding().run().run().into_uuid())
        .bind(fixed_event.finding().target().into_uuid())
        .bind(fixed_event.pass().pass().into_uuid())
        .bind(fixed_event.pass().run().run().into_uuid())
        .execute(&mut *second_appender)
        .await?;
        second_appender.commit().await
    });
    assert!(
        blocked_backends_reached(&pool, 1).await?,
        "second ordinal waits for its predecessor's finding lock"
    );
    first_appender.commit().await?;
    waiting_append
        .await
        .expect("second event task remains live")
        .expect("second event observes and follows its committed predecessor");

    assert_eq!(
        fixture.store.load_finding(finding_ref.finding()).await?,
        Some(fixed)
    );
    Ok(())
}

/// direct SQL cannot admit a judgment below the finding producer's
/// frozen minimum confidence threshold.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn schema_enforces_judge_confidence_threshold() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let judge_pass = insert_fixture_pass(&fixture, 0x33e, ReviewPassKind::Judge).await;
    let evidence = succeed_fixture_passes(&pool, &fixture.store, &[fixture.pass, judge_pass]).await;
    let finding_ref = ReviewFindingRef::new(fixture.pass, ReviewFindingId::from_uuid(uuid(0x33f)));
    let review_evidence = pass_with_produced_findings(vec![finding_ref], evidence[0].clone());
    fixture
        .store
        .insert_finding(&finding_with_is_real_confidence(
            finding_ref,
            review_evidence,
            &fixture.target_snapshot,
            6_999,
        ))
        .await?;
    let mut transaction = pool.begin().await?;
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
    .execute(&mut *transaction)
    .await?;
    let event = sqlx::query(
        "INSERT INTO review_finding_event
            (finding_id, event_ordinal, finding_run_id, target_id,
             event_pass_id, event_pass_run_id, event_kind, reason,
             referenced_finding_id, referenced_finding_status,
             external_link_id, external_link_association_kind)
         VALUES ($1, 1, $2, $3, $4, $5, 'accepted',
                 NULL, NULL, NULL, NULL, NULL)",
    )
    .bind(finding_ref.finding().into_uuid())
    .bind(finding_ref.run().run().into_uuid())
    .bind(finding_ref.target().into_uuid())
    .bind(judge_pass.pass().into_uuid())
    .bind(judge_pass.run().run().into_uuid())
    .execute(&mut *transaction)
    .await
    .expect_err("below-threshold judgment cannot bypass the domain through SQL");
    assert_sqlstate(&event, "23514");
    transaction.rollback().await?;
    Ok(())
}

/// direct SQL cannot publish a finding below the producer's
/// frozen publication threshold, even with matching attachment evidence.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn schema_enforces_publication_confidence_threshold() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let judge_pass = insert_fixture_pass(&fixture, 0x34a, ReviewPassKind::Judge).await;
    let publish_pass = insert_fixture_pass(&fixture, 0x34b, ReviewPassKind::Publish).await;
    let evidence = succeed_fixture_passes(
        &pool,
        &fixture.store,
        &[fixture.pass, judge_pass, publish_pass],
    )
    .await;
    let finding_ref = ReviewFindingRef::new(fixture.pass, ReviewFindingId::from_uuid(uuid(0x34c)));
    let review_evidence = pass_with_produced_findings(vec![finding_ref], evidence[0].clone());
    fixture
        .store
        .insert_finding(&finding_with_is_real_confidence(
            finding_ref,
            review_evidence,
            &fixture.target_snapshot,
            7_999,
        ))
        .await?;
    fixture
        .store
        .append_finding_event(
            finding_ref.finding(),
            finding_event(
                finding_ref,
                ReviewEventOrdinal::one(),
                evidence[1].clone(),
                ReviewFindingEventKind::Accepted,
            ),
        )
        .await?;
    let link = ReviewExternalLinkId::from_uuid(uuid(0x34d));
    fixture
        .store
        .reserve_external_link(
            ReviewExternalLink::try_reserve(
                link,
                ReviewExternalLinkAssociation::Finding(finding_ref),
                key("example-code-host"),
                ReviewExternalObjectKind::ReviewComment,
                &fixture.target_snapshot,
            )
            .expect("reservation matches the target"),
        )
        .await?;

    sqlx::query(
        "ALTER TABLE review_finding_event
         DISABLE TRIGGER USER",
    )
    .execute(&pool)
    .await?;
    let missing_association = sqlx::query(
        "INSERT INTO review_finding_event
            (finding_id, event_ordinal, finding_run_id, target_id,
             event_pass_id, event_pass_run_id, event_kind, reason,
             referenced_finding_id, referenced_finding_status,
             external_link_id, external_link_association_kind)
         VALUES ($1, 2, $2, $3, $4, $5, 'posted',
                 NULL, NULL, NULL, $6, NULL)",
    )
    .bind(finding_ref.finding().into_uuid())
    .bind(finding_ref.run().run().into_uuid())
    .bind(finding_ref.target().into_uuid())
    .bind(publish_pass.pass().into_uuid())
    .bind(publish_pass.run().run().into_uuid())
    .bind(link.into_uuid())
    .execute(&pool)
    .await
    .expect_err("linked finding events require their association discriminator");
    sqlx::query(
        "ALTER TABLE review_finding_event
         ENABLE TRIGGER USER",
    )
    .execute(&pool)
    .await?;
    assert_eq!(
        missing_association
            .as_database_error()
            .and_then(|error| error.constraint()),
        Some("review_finding_event_shape")
    );

    let missing_discriminator = sqlx::query(
        "UPDATE review_pass
            SET result_kind = 'external_link_attachment',
                result_finding_id = $2,
                result_finding_run_id = $3,
                result_finding_pass_id = $4,
                result_event_ordinal = 2,
                result_external_link_id = $5,
                result_external_object_key = 'comment-34d'
          WHERE pass_id = $1",
    )
    .bind(publish_pass.pass().into_uuid())
    .bind(finding_ref.finding().into_uuid())
    .bind(finding_ref.run().run().into_uuid())
    .bind(finding_ref.pass().pass().into_uuid())
    .bind(link.into_uuid())
    .execute(&pool)
    .await
    .expect_err("posted attachment evidence requires its event discriminator");
    assert_sqlstate(&missing_discriminator, "23514");

    let mut transaction = pool.begin().await?;
    sqlx::query(
        "UPDATE review_pass
            SET result_kind = 'external_link_attachment',
                result_finding_id = $2,
                result_finding_run_id = $3,
                result_finding_pass_id = $4,
                result_event_ordinal = 2,
                result_event_kind = 'posted',
                result_external_link_id = $5,
                result_external_object_key = 'comment-34d'
          WHERE pass_id = $1",
    )
    .bind(publish_pass.pass().into_uuid())
    .bind(finding_ref.finding().into_uuid())
    .bind(finding_ref.run().run().into_uuid())
    .bind(finding_ref.pass().pass().into_uuid())
    .bind(link.into_uuid())
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO review_external_link_attachment
            (external_link_id, target_id, pass_run_id, pass_id,
             provider_key, object_kind, external_object_key)
         VALUES ($1, $2, $3, $4, 'example-code-host',
                 'review_comment', 'comment-34d')",
    )
    .bind(link.into_uuid())
    .bind(fixture.target.into_uuid())
    .bind(publish_pass.run().run().into_uuid())
    .bind(publish_pass.pass().into_uuid())
    .execute(&mut *transaction)
    .await?;
    let posted = sqlx::query(
        "INSERT INTO review_finding_event
            (finding_id, event_ordinal, finding_run_id, target_id,
             event_pass_id, event_pass_run_id, event_kind, reason,
             referenced_finding_id, referenced_finding_status,
             external_link_id, external_link_association_kind)
         VALUES ($1, 2, $2, $3, $4, $5, 'posted',
                 NULL, NULL, NULL, $6, 'finding')",
    )
    .bind(finding_ref.finding().into_uuid())
    .bind(finding_ref.run().run().into_uuid())
    .bind(finding_ref.target().into_uuid())
    .bind(publish_pass.pass().into_uuid())
    .bind(publish_pass.run().run().into_uuid())
    .bind(link.into_uuid())
    .execute(&mut *transaction)
    .await
    .expect_err("below-threshold publication cannot bypass the domain through SQL");
    assert_sqlstate(&posted, "23514");
    transaction.rollback().await?;
    Ok(())
}

/// severity-label uncertainty cannot suppress a finding
/// whose is-real confidence clears both frozen policy thresholds.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn schema_thresholds_ignore_severity_label_confidence() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let judge_pass = insert_fixture_pass(&fixture, 0x34e, ReviewPassKind::Judge).await;
    let publish_pass = insert_fixture_pass(&fixture, 0x34f, ReviewPassKind::Publish).await;
    let evidence = succeed_fixture_passes(
        &pool,
        &fixture.store,
        &[fixture.pass, judge_pass, publish_pass],
    )
    .await;
    let finding_ref = ReviewFindingRef::new(fixture.pass, ReviewFindingId::from_uuid(uuid(0x350)));
    let review_evidence = pass_with_produced_findings(vec![finding_ref], evidence[0].clone());
    let finding = finding_with_confidence_axes_and_side(
        finding_ref,
        review_evidence,
        &fixture.target_snapshot,
        FindingConfidenceAxes {
            is_real: 9_500,
            severity_label: 0,
        },
        Some(ReviewFindingDiffSide::Right),
    );
    fixture.store.insert_finding(&finding).await?;
    fixture
        .store
        .append_finding_event(
            finding_ref.finding(),
            finding_event(
                finding_ref,
                ReviewEventOrdinal::one(),
                evidence[1].clone(),
                ReviewFindingEventKind::Accepted,
            ),
        )
        .await?;

    let link = ReviewExternalLinkId::from_uuid(uuid(0x351));
    let reservation = ReviewExternalLink::try_reserve(
        link,
        ReviewExternalLinkAssociation::Finding(finding_ref),
        key("example-code-host"),
        ReviewExternalObjectKind::ReviewComment,
        &fixture.target_snapshot,
    )
    .expect("reservation matches the target");
    fixture.store.reserve_external_link(reservation).await?;
    fixture
        .store
        .attach_external_link(
            link,
            posted_attachment(
                link,
                evidence[2].clone(),
                key("comment-351"),
                finding_ref,
                ReviewEventOrdinal::try_new(2).expect("positive ordinal"),
            ),
        )
        .await?;

    let posted = fixture
        .store
        .load_finding(finding_ref.finding())
        .await?
        .expect("posted finding loads");
    assert_eq!(posted.status(), ReviewFindingStatus::Posted);
    Ok(())
}

/// event compatibility is checked against the canonical persisted
/// pass kind, not only the kind carried by the in-memory event.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn canonical_pass_kind_rejects_misclassified_event() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let publish_pass = insert_fixture_pass(&fixture, 0x334, ReviewPassKind::Publish).await;
    let evidence =
        succeed_fixture_passes(&pool, &fixture.store, &[fixture.pass, publish_pass]).await;
    let finding_ref = ReviewFindingRef::new(fixture.pass, ReviewFindingId::from_uuid(uuid(0x335)));
    fixture
        .store
        .insert_finding(&finding(
            finding_ref,
            evidence[0].clone(),
            &fixture.target_snapshot,
        ))
        .await?;

    let error = fixture
        .store
        .append_finding_event(
            finding_ref.finding(),
            finding_event(
                finding_ref,
                ReviewEventOrdinal::one(),
                pass_evidence(
                    publish_pass,
                    ReviewPassKind::Judge,
                    evidence[1].clone().policy(),
                    evidence[1].state().clone(),
                ),
                ReviewFindingEventKind::Accepted,
            ),
        )
        .await
        .expect_err("canonical publication pass cannot accept a finding");
    let ReviewWorkflowStoreError::Corruption(error) = error else {
        panic!("canonical pass-kind mismatch must fail closed as pass corruption");
    };
    assert_eq!(error.aggregate(), "review_pass");
    assert!(
        error
            .detail()
            .contains("differs from canonical execution facts")
    );
    Ok(())
}

/// an effect result cannot be changed after its first atomic binding.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn bound_pass_result_is_immutable() -> Result<(), Box<dyn Error>> {
    const JUDGE_PASS_IDENTITY: u128 = 0x3342;
    const FINDING_IDENTITY: u128 = 0x3343;

    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let judge_pass =
        insert_fixture_pass(&fixture, JUDGE_PASS_IDENTITY, ReviewPassKind::Judge).await;
    let evidence = succeed_fixture_passes(&pool, &fixture.store, &[fixture.pass, judge_pass]).await;
    let finding_ref = ReviewFindingRef::new(
        fixture.pass,
        ReviewFindingId::from_uuid(uuid(FINDING_IDENTITY)),
    );
    fixture
        .store
        .insert_finding(&finding(
            finding_ref,
            evidence[0].clone(),
            &fixture.target_snapshot,
        ))
        .await?;
    fixture
        .store
        .append_finding_event(
            finding_ref.finding(),
            finding_event(
                finding_ref,
                ReviewEventOrdinal::one(),
                evidence[1].clone(),
                ReviewFindingEventKind::Accepted,
            ),
        )
        .await?;

    let mutation = sqlx::query(
        "UPDATE review_pass
            SET result_event_kind = 'rejected',
                result_reason = 'changed after binding'
          WHERE pass_id = $1",
    )
    .bind(judge_pass.pass().into_uuid())
    .execute(&pool)
    .await
    .expect_err("bound result payload is immutable");
    assert_sqlstate(&mutation, "23514");
    Ok(())
}

/// persistence rejects review policy versions that the domain cannot
/// reconstitute.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn schema_rejects_unsupported_policy_version() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let unsupported_policy = sqlx::query(
        "INSERT INTO review_run
            (run_id, target_id, workflow_kind, policy_version,
             minimum_judge_confidence, minimum_publication_confidence,
             state_kind, state_pass_id)
         VALUES ($1, $2, 'read_only_review', 2, 7500, 8500, 'queued', NULL)",
    )
    .bind(uuid(0x3340))
    .bind(fixture.target.into_uuid())
    .execute(&pool)
    .await
    .expect_err("unsupported policy versions must fail before domain loading");
    assert_sqlstate(&unsupported_policy, "23514");
    Ok(())
}

/// the store refuses to insert a run projection after transition.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn run_insert_requires_queued_state() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let running = fixture
        .store
        .load_run(fixture.run.run())
        .await?
        .expect("fixture run exists")
        .transition(
            ReviewRunState::Running {
                active_pass: fixture.pass,
            },
            Some(pass_evidence(
                fixture.pass,
                ReviewPassKind::ReadOnlyReview,
                ReviewPolicy::version_one(),
                ReviewPassState::Running {
                    turn: TurnId::from_uuid(uuid(0x203)),
                },
            )),
        )
        .expect("queued run activates with matching pass evidence");
    assert!(matches!(
        fixture.store.insert_run(&running).await,
        Err(ReviewWorkflowStoreError::InvalidInsertion(
            ReviewWorkflowInsertionError::RunNotQueued { .. }
        ))
    ));
    Ok(())
}

/// the store refuses to insert a pass projection after transition.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn pass_insert_requires_queued_state() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let turn = TurnId::from_uuid(uuid(0x203));
    let running = fixture
        .store
        .load_pass(fixture.pass.pass())
        .await?
        .expect("fixture pass exists")
        .transition(
            ReviewPassState::Running { turn },
            Some(ReviewPassTurnEvidence::new(
                turn,
                SessionId::from_uuid(uuid(0x201)),
                AcceptedInputId::from_uuid(uuid(0x202)),
                ReviewPassTurnOutcome::Active,
                None,
            )),
        )
        .expect("queued pass activates with matching turn evidence");
    assert!(matches!(
        fixture.store.insert_pass(&running).await,
        Err(ReviewWorkflowStoreError::InvalidInsertion(
            ReviewWorkflowInsertionError::PassNotQueued { .. }
        ))
    ));
    Ok(())
}

/// reservation insertion refuses post-effect attachment evidence.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn reservation_insert_requires_pending_state() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let link = ReviewExternalLinkId::from_uuid(uuid(0x307));
    let attached = ReviewExternalLink::try_reserve(
        link,
        ReviewExternalLinkAssociation::Target(fixture.target),
        key("example-code-host"),
        ReviewExternalObjectKind::ReviewComment,
        &fixture.target_snapshot,
    )
    .expect("reservation matches the target")
    .attach(attachment(
        link,
        succeeded_pass(fixture.pass, ReviewPassKind::Publish),
        key("comment-85"),
    ))
    .expect("same-target pass may attach");
    assert!(matches!(
        fixture.store.reserve_external_link(attached).await,
        Err(ReviewWorkflowStoreError::InvalidInsertion(
            ReviewWorkflowInsertionError::ExternalLinkNotPending
        ))
    ));
    let claimed_link = ReviewExternalLinkId::from_uuid(uuid(0x308));
    let claimed_pass = pass_evidence(
        fixture.pass,
        ReviewPassKind::Publish,
        ReviewPolicy::version_one(),
        ReviewPassState::Blocked {
            turn: TurnId::from_uuid(uuid(0x203)),
            result: Some(ReviewPassResult::ExternalLinkPublicationBlocked(
                ReviewExternalLinkPublicationBlockedResult::new(
                    claimed_link,
                    text("provider acknowledgement requires reconciliation"),
                ),
            )),
        },
    );
    let claimed = ReviewExternalLink::try_reserve(
        claimed_link,
        ReviewExternalLinkAssociation::Target(fixture.target),
        key("example-code-host"),
        ReviewExternalObjectKind::ReviewComment,
        &fixture.target_snapshot,
    )
    .expect("reservation matches the target")
    .block_publication(claimed_pass.clone(), run_evidence_for_pass(claimed_pass))
    .expect("blocked publication claim belongs to the reservation");
    assert!(matches!(
        fixture.store.reserve_external_link(claimed).await,
        Err(ReviewWorkflowStoreError::InvalidInsertion(
            ReviewWorkflowInsertionError::ExternalLinkNotPending
        ))
    ));
    Ok(())
}

/// the canonical pass/finding and external-claim lookup
/// paths remain indexed by their leading filter columns.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn review_lookup_indexes_are_pinned() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let external_link_index: String = sqlx::query_scalar(
        "SELECT indexdef
           FROM pg_indexes
          WHERE schemaname = 'public'
            AND indexname = 'review_pass_external_link_result_index'",
    )
    .fetch_one(&pool)
    .await?;
    assert!(external_link_index.contains("(result_external_link_id, pass_id)"));
    assert!(external_link_index.contains("result_external_link_id IS NOT NULL"));

    let producing_pass_index: String = sqlx::query_scalar(
        "SELECT indexdef
           FROM pg_indexes
          WHERE schemaname = 'public'
            AND indexname = 'review_finding_producing_pass_index'",
    )
    .fetch_one(&pool)
    .await?;
    assert!(producing_pass_index.contains("(producing_pass_id, target_id, run_id, finding_id)"));

    let attachment_identity_index: String = sqlx::query_scalar(
        "SELECT indexdef
           FROM pg_indexes
          WHERE schemaname = 'public'
            AND indexname = 'review_external_link_attachment_identity_index'",
    )
    .fetch_one(&pool)
    .await?;
    assert!(attachment_identity_index.contains("(identity_digest, target_id)"));

    let blocked_link_index: String = sqlx::query_scalar(
        "SELECT indexdef
           FROM pg_indexes
          WHERE schemaname = 'public'
            AND indexname = 'review_finding_event_blocked_link_index'",
    )
    .fetch_one(&pool)
    .await?;
    assert!(blocked_link_index.contains("(external_link_id)"));
    assert!(blocked_link_index.contains("event_kind = 'blocked_with_reason'"));
    Ok(())
}

/// raw run rows must begin queued.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn schema_requires_new_run_to_be_queued() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let direct_cancelled_run = sqlx::query(
        "INSERT INTO review_run
            (run_id, target_id, workflow_kind, policy_version,
             minimum_judge_confidence, minimum_publication_confidence,
             state_kind, state_pass_id)
         VALUES (
             $1, $2, 'read_only_review', 1, 7000, 8000,
             'cancelled', NULL
         )",
    )
    .bind(uuid(0x607))
    .bind(fixture.target.into_uuid())
    .execute(&pool)
    .await
    .expect_err("raw review runs must begin queued");
    assert_sqlstate(&direct_cancelled_run, "23514");
    Ok(())
}

/// raw pass rows must begin queued.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn schema_requires_new_pass_to_be_queued() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let direct_failed_pass = sqlx::query(
        "INSERT INTO review_pass
            (pass_id, run_id, target_id, pass_kind, session_id,
             accepted_input_id, origin_turn_id, state_kind, turn_id,
             output_frontier_id)
         VALUES (
             $1, $2, $3, 'read_only_review', $4,
             $5, $6, 'failed', $6, NULL
         )",
    )
    .bind(uuid(0x608))
    .bind(fixture.run.run().into_uuid())
    .bind(fixture.target.into_uuid())
    .bind(uuid(0x201))
    .bind(uuid(0x202))
    .bind(uuid(0x203))
    .execute(&pool)
    .await
    .expect_err("raw review passes must begin queued");
    assert_sqlstate(&direct_failed_pass, "23514");
    Ok(())
}

/// change-request targets require a frozen comparison revision.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn schema_requires_change_request_base() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let missing_change_request = sqlx::query(
        "INSERT INTO review_target
            (target_id, provider_key, repository_key, subject_kind,
             change_request_number, head_revision, base_revision,
             stack_parent_target_id)
         VALUES (
             $1, 'example-code-host', 'example/repository',
             'change_request', 42, '0123456789abcdef', NULL, NULL
         )",
    )
    .bind(uuid(0x601))
    .execute(&pool)
    .await
    .expect_err("change-request targets require their frozen comparison revision");
    assert_sqlstate(&missing_change_request, "23514");
    Ok(())
}

/// policy version one has one canonical threshold tuple.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn schema_requires_canonical_version_one_policy() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let noncanonical_policy = sqlx::query(
        "INSERT INTO review_run
            (run_id, target_id, workflow_kind, policy_version,
             minimum_judge_confidence, minimum_publication_confidence,
             state_kind, state_pass_id)
         VALUES ($1, $2, 'read_only_review', 1, 7001, 8000, 'queued', NULL)",
    )
    .bind(uuid(0x602))
    .bind(fixture.target.into_uuid())
    .execute(&pool)
    .await
    .expect_err("version one requires the exact 7000/8000 threshold tuple");
    assert_sqlstate(&noncanonical_policy, "23514");
    Ok(())
}

/// finding line ranges are absent or complete.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn schema_rejects_half_populated_line_range() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let half_populated_range = sqlx::query(
        "INSERT INTO review_finding
             (finding_id, run_id, target_id, producing_pass_id, file_path,
             line_start, line_end, diff_side, title, body, severity,
             is_real_confidence, severity_label_confidence, category,
             recommended_fix)
         VALUES (
             $1, $2, $3, $4, 'src/lib.rs',
             1, NULL, 'right', 'Finding', 'Body', 'high',
             9000, 8500, 'correctness', NULL
         )",
    )
    .bind(uuid(0x603))
    .bind(fixture.run.run().into_uuid())
    .bind(fixture.target.into_uuid())
    .bind(fixture.pass.pass().into_uuid())
    .execute(&pool)
    .await
    .expect_err("finding line ranges are absent or fully populated");
    assert_sqlstate(&half_populated_range, "23514");
    Ok(())
}

/// rejected finding events require their exact reason.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn schema_requires_rejection_reason() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let judge_pass = insert_fixture_pass(&fixture, 0x609, ReviewPassKind::Judge).await;
    let evidence = succeed_fixture_passes(&pool, &fixture.store, &[fixture.pass, judge_pass]).await;
    let reason_finding =
        ReviewFindingRef::new(fixture.pass, ReviewFindingId::from_uuid(uuid(0x604)));
    fixture
        .store
        .insert_finding(&finding(
            reason_finding,
            evidence[0].clone(),
            &fixture.target_snapshot,
        ))
        .await
        .expect("reason-shape fixture persists");
    let missing_reason = sqlx::query(
        "INSERT INTO review_finding_event
            (finding_id, event_ordinal, finding_run_id, target_id,
             event_pass_id, event_pass_run_id, event_kind, reason,
             referenced_finding_id, external_link_id,
             external_link_association_kind)
         VALUES ($1, 1, $2, $3, $4, $2, 'rejected', NULL, NULL, NULL, NULL)",
    )
    .bind(reason_finding.finding().into_uuid())
    .bind(fixture.run.run().into_uuid())
    .bind(fixture.target.into_uuid())
    .bind(judge_pass.pass().into_uuid())
    .execute(&pool)
    .await
    .expect_err("rejected events require a reason");
    assert_sqlstate(&missing_reason, "23514");
    Ok(())
}

/// cancelling a running run cannot erase its active pass.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn schema_running_run_cancellation_retains_pass() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    start_review_pass(&fixture.store, fixture.pass).await;

    let erased_run_pass = sqlx::query(
        "UPDATE review_run
            SET state_kind = 'cancelled', state_pass_id = NULL
          WHERE run_id = $1",
    )
    .bind(fixture.run.run().into_uuid())
    .execute(&pool)
    .await
    .expect_err("running cancellation retains the active pass");
    assert_sqlstate(&erased_run_pass, "23514");
    Ok(())
}

/// cancelling a running pass cannot erase its active turn.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn schema_running_pass_cancellation_retains_turn() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    start_review_pass(&fixture.store, fixture.pass).await;
    let erased_pass_turn = sqlx::query(
        "UPDATE review_pass
            SET state_kind = 'cancelled', turn_id = NULL
          WHERE pass_id = $1",
    )
    .bind(fixture.pass.pass().into_uuid())
    .execute(&pool)
    .await
    .expect_err("running cancellation retains the active turn");
    assert_sqlstate(&erased_pass_turn, "23514");

    Ok(())
}

/// a pass kind is the exact one-to-one projection of its run
/// workflow.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn pass_kind_requires_matching_run_workflow() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let run = ReviewRunRef::new(fixture.target, ReviewRunId::from_uuid(uuid(0x702)));
    fixture
        .store
        .insert_run(&ReviewRun::new(
            run,
            ReviewWorkflowKind::JudgeFindings,
            ReviewPolicy::version_one(),
        ))
        .await?;
    let mismatched = sqlx::query(
        "INSERT INTO review_pass
            (pass_id, run_id, target_id, pass_kind, session_id,
             accepted_input_id, origin_turn_id, state_kind, turn_id,
             output_frontier_id)
         VALUES (
             $1, $2, $3, 'read_only_review', $4, $5, $6,
             'queued', NULL, NULL
         )",
    )
    .bind(uuid(0x703))
    .bind(run.run().into_uuid())
    .bind(fixture.target.into_uuid())
    .bind(uuid(0x201))
    .bind(uuid(0x202))
    .bind(uuid(0x203))
    .execute(&pool)
    .await
    .expect_err("pass kind must match the canonical run workflow");
    assert_sqlstate(&mismatched, "23514");
    Ok(())
}

/// one run owns at most one pass.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn run_rejects_second_pass() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let second = sqlx::query(
        "INSERT INTO review_pass
            (pass_id, run_id, target_id, pass_kind, session_id,
             accepted_input_id, origin_turn_id, state_kind, turn_id,
             output_frontier_id)
         VALUES (
             $1, $2, $3, 'read_only_review', $4, $5, $6,
             'queued', NULL, NULL
         )",
    )
    .bind(uuid(0x704))
    .bind(fixture.run.run().into_uuid())
    .bind(fixture.target.into_uuid())
    .bind(uuid(0x201))
    .bind(uuid(0x202))
    .bind(uuid(0x203))
    .execute(&pool)
    .await
    .expect_err("one run cannot own a second pass");
    assert_sqlstate(&second, "23505");
    Ok(())
}

/// append-only workflow evidence also rejects statement-
/// level truncation, which bypasses row delete triggers.
async fn assert_review_workflow_truncate_rejected(
    pool: &PgPool,
    table: &'static str,
    statement: &'static str,
) {
    let error = sqlx::query(statement)
        .execute(pool)
        .await
        .expect_err("every workflow table rejects truncate");
    assert_eq!(
        error
            .as_database_error()
            .and_then(|database| database.code())
            .as_deref(),
        Some("23514"),
        "{table}",
    );
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn review_workflow_tables_reject_truncate() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    assert_review_workflow_truncate_rejected(
        &pool,
        "review_target",
        "TRUNCATE TABLE review_target CASCADE",
    )
    .await;
    assert_review_workflow_truncate_rejected(
        &pool,
        "review_run",
        "TRUNCATE TABLE review_run CASCADE",
    )
    .await;
    assert_review_workflow_truncate_rejected(
        &pool,
        "review_pass",
        "TRUNCATE TABLE review_pass CASCADE",
    )
    .await;
    assert_review_workflow_truncate_rejected(
        &pool,
        "review_finding",
        "TRUNCATE TABLE review_finding CASCADE",
    )
    .await;
    assert_review_workflow_truncate_rejected(
        &pool,
        "review_pass_produced_finding",
        "TRUNCATE TABLE review_pass_produced_finding CASCADE",
    )
    .await;
    assert_review_workflow_truncate_rejected(
        &pool,
        "review_pass_finding_inventory_seal",
        "TRUNCATE TABLE review_pass_finding_inventory_seal CASCADE",
    )
    .await;
    assert_review_workflow_truncate_rejected(
        &pool,
        "review_external_link",
        "TRUNCATE TABLE review_external_link CASCADE",
    )
    .await;
    assert_review_workflow_truncate_rejected(
        &pool,
        "review_external_object_identity",
        "TRUNCATE TABLE review_external_object_identity CASCADE",
    )
    .await;
    assert_review_workflow_truncate_rejected(
        &pool,
        "review_external_link_attachment",
        "TRUNCATE TABLE review_external_link_attachment CASCADE",
    )
    .await;
    assert_review_workflow_truncate_rejected(
        &pool,
        "review_finding_event",
        "TRUNCATE TABLE review_finding_event CASCADE",
    )
    .await;
    assert_review_workflow_truncate_rejected(
        &pool,
        "review_finding_event_head",
        "TRUNCATE TABLE review_finding_event_head CASCADE",
    )
    .await;
    assert_review_workflow_truncate_rejected(
        &pool,
        "review_external_link_observation",
        "TRUNCATE TABLE review_external_link_observation CASCADE",
    )
    .await;
    assert_review_workflow_truncate_rejected(
        &pool,
        "review_workflow_command",
        "TRUNCATE TABLE review_workflow_command CASCADE",
    )
    .await;
    Ok(())
}

/// atomic admission rejects a pass owned by another run root.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn atomic_run_pass_admission_rejects_cross_wired_roots() -> Result<(), Box<dyn Error>> {
    const STORED_RUN_IDENTITY: u128 = 0x77b;
    const STORED_PASS_IDENTITY: u128 = 0x77c;

    let (_container, pool) = migrated_postgres().await?;
    let fixture = review_command_admission_fixture(&pool).await;
    let stored_run_reference = ReviewRunRef::new(
        fixture.target,
        ReviewRunId::from_uuid(uuid(STORED_RUN_IDENTITY)),
    );
    let stored_pass_reference = ReviewPassRef::new(
        stored_run_reference,
        ReviewPassId::from_uuid(uuid(STORED_PASS_IDENTITY)),
    );
    let mut stored_run = ReviewRun::new(
        stored_run_reference,
        ReviewWorkflowKind::ReadOnlyReview,
        ReviewPolicy::version_one(),
    );
    let stored_pass = ReviewPass::try_new(
        stored_pass_reference,
        ReviewPassKind::ReadOnlyReview,
        &mut stored_run,
        fixture.session,
        ReviewPassAcceptedInputEvidence::new(
            fixture.accepted_input,
            fixture.session,
            Some(fixture.origin_turn),
        ),
    )
    .expect("the stored-run pass fixture is domain-valid");
    fixture.store.insert_run(&stored_run).await?;

    let error = fixture
        .store
        .insert_run_and_pass(&fixture.run, &stored_pass)
        .await
        .expect_err("cross-wired roots must fail before insertion");

    assert!(matches!(
        error,
        ReviewWorkflowStoreError::InvalidInsertion(ReviewWorkflowInsertionError::RunPassMismatch)
    ));
    assert_eq!(
        fixture
            .store
            .load_run(fixture.run.reference().run())
            .await?,
        None
    );
    assert_eq!(
        fixture
            .store
            .load_pass(stored_pass.reference().pass())
            .await?,
        None
    );
    assert!(
        fixture
            .store
            .load_run(stored_run.reference().run())
            .await?
            .is_some()
    );
    Ok(())
}
