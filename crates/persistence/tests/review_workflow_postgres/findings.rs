//! Findings coverage.

use super::*;

fn finding_with_side(
    reference: ReviewFindingRef,
    producing_pass: ReviewPassEvidence,
    target: &ReviewTarget,
    diff_side: Option<ReviewFindingDiffSide>,
) -> ReviewFinding {
    finding_with_confidence_axes_and_side(
        reference,
        producing_pass,
        target,
        FindingConfidenceAxes {
            is_real: 9_000,
            severity_label: 8_500,
        },
        diff_side,
    )
}

#[track_caller]
fn assert_finding_reference_load_corruption(
    loaded: Result<Option<ReviewFinding>, ReviewWorkflowStoreError>,
    expected_aggregate: &str,
) {
    let error = loaded.expect_err("corrupt finding reference must fail loading closed");
    let ReviewWorkflowStoreError::Corruption(error) = error else {
        panic!("expected typed finding-reference corruption");
    };
    assert_eq!(
        error.aggregate(),
        expected_aggregate,
        "corruption detail: {}",
        error.detail(),
    );
}

/// once a produced-finding inventory is sealed, later canonical
/// finding inserts cannot expand the result—even when the inventory was empty.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn sealed_finding_inventory_cannot_expand() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let succeeded = succeed_fixture_passes(&pool, &fixture.store, &[fixture.pass]).await[0].clone();
    let evidence = pass_with_produced_findings(Vec::new(), succeeded);
    fixture.store.insert_findings(&evidence, &[]).await?;

    let expansion = sqlx::query(
        "INSERT INTO review_finding
             (finding_id, run_id, target_id, producing_pass_id, file_path,
             line_start, line_end, diff_side, title, body, severity,
             is_real_confidence, severity_label_confidence, category,
             recommended_fix)
         VALUES (
             $1, $2, $3, $4, 'src/lib.rs',
             1, 1, 'right', 'Late finding', 'Body', 'high',
             9000, 8500, 'correctness', NULL
         )",
    )
    .bind(uuid(0x329))
    .bind(fixture.run.run().into_uuid())
    .bind(fixture.target.into_uuid())
    .bind(fixture.pass.pass().into_uuid())
    .execute(&pool)
    .await
    .expect_err("sealed finding inventory cannot admit a late finding");
    assert_sqlstate(&expansion, "23514");
    Ok(())
}

/// pass loading authenticates both directions of the sealed
/// produced-finding inventory against canonical finding rows.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn pass_loader_rejects_incomplete_finding_inventory() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let succeeded = succeed_fixture_passes(&pool, &fixture.store, &[fixture.pass]).await[0].clone();
    let finding_ref = ReviewFindingRef::new(fixture.pass, ReviewFindingId::from_uuid(uuid(0x328)));
    let evidence = pass_with_produced_findings(vec![finding_ref], succeeded);
    fixture
        .store
        .insert_finding(&finding(finding_ref, evidence, &fixture.target_snapshot))
        .await?;
    sqlx::query(
        "ALTER TABLE review_pass_produced_finding
         DISABLE TRIGGER review_pass_produced_finding_is_append_only",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE review_pass_produced_finding
            SET result_ordinal = 2
          WHERE pass_id = $1",
    )
    .bind(fixture.pass.pass().into_uuid())
    .execute(&pool)
    .await?;
    let ordinal_error = fixture
        .store
        .load_pass(fixture.pass.pass())
        .await
        .expect_err("non-contiguous inventory ordinals must fail pass loading closed");
    let ReviewWorkflowStoreError::Corruption(ordinal_error) = ordinal_error else {
        panic!("expected typed produced-finding ordinal corruption");
    };
    assert_eq!(ordinal_error.aggregate(), "review_pass_produced_finding");
    assert!(ordinal_error.detail().contains("ordinals"));
    sqlx::query(
        "UPDATE review_pass_produced_finding
            SET result_ordinal = 1
          WHERE pass_id = $1",
    )
    .bind(fixture.pass.pass().into_uuid())
    .execute(&pool)
    .await?;
    sqlx::query(
        "DELETE FROM review_pass_produced_finding
          WHERE pass_id = $1",
    )
    .bind(fixture.pass.pass().into_uuid())
    .execute(&pool)
    .await?;

    let error = fixture
        .store
        .load_pass(fixture.pass.pass())
        .await
        .expect_err("missing inventory member must fail pass loading closed");
    let ReviewWorkflowStoreError::Corruption(error) = error else {
        panic!("expected typed produced-finding inventory corruption");
    };
    assert_eq!(error.aggregate(), "review_pass_produced_finding");
    assert!(error.detail().contains("sealed findings"));
    Ok(())
}

/// finding-event validation reuses its held transaction connection,
/// so a one-connection pool cannot self-deadlock while loading current history.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn finding_event_uses_held_transaction_connection() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres_with_max_connections(1).await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let judge_pass = insert_fixture_pass(&fixture, 0x32a, ReviewPassKind::Judge).await;
    let evidence = succeed_fixture_passes(&pool, &fixture.store, &[fixture.pass, judge_pass]).await;
    let finding_ref = ReviewFindingRef::new(fixture.pass, ReviewFindingId::from_uuid(uuid(0x32b)));
    let review_evidence = pass_with_produced_findings(vec![finding_ref], evidence[0].clone());
    fixture
        .store
        .insert_finding(&finding(
            finding_ref,
            review_evidence,
            &fixture.target_snapshot,
        ))
        .await?;
    let append = fixture.store.append_finding_event(
        finding_ref.finding(),
        finding_event(
            finding_ref,
            ReviewEventOrdinal::one(),
            evidence[1].clone(),
            ReviewFindingEventKind::Accepted,
        ),
    );
    let appended = tokio::time::timeout(std::time::Duration::from_secs(5), append)
        .await
        .expect("held-transaction loading must not wait for another pool connection")?;
    assert_eq!(
        appended.expect("finding remains present").status(),
        ReviewFindingStatus::Accepted
    );
    Ok(())
}

/// appending an event through another same-run finding fails before
/// persistence.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn finding_event_rejects_foreign_owner() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let review_evidence =
        succeed_fixture_passes(&pool, &fixture.store, &[fixture.pass]).await[0].clone();
    let first = ReviewFindingRef::new(fixture.pass, ReviewFindingId::from_uuid(uuid(0x330)));
    let second = ReviewFindingRef::new(fixture.pass, ReviewFindingId::from_uuid(uuid(0x331)));
    let review_evidence = pass_with_produced_findings(vec![first, second], review_evidence);
    fixture
        .store
        .insert_findings(
            &review_evidence,
            &[
                finding(first, review_evidence.clone(), &fixture.target_snapshot),
                finding(second, review_evidence.clone(), &fixture.target_snapshot),
            ],
        )
        .await?;

    let error = fixture
        .store
        .append_finding_event(
            first.finding(),
            finding_event(
                second,
                ReviewEventOrdinal::one(),
                succeeded_pass(fixture.pass, ReviewPassKind::Judge),
                ReviewFindingEventKind::Accepted,
            ),
        )
        .await
        .expect_err("event owner must equal the loaded finding");
    let ReviewWorkflowStoreError::InvalidTransition(ReviewWorkflowTransitionError::Finding(error)) =
        error
    else {
        panic!("expected a typed finding transition rejection");
    };
    assert_eq!(
        error.failure(),
        ReviewFindingTransitionFailure::ForeignEventFinding
    );

    Ok(())
}

/// a referenced finding reconstitutes with its exact producing pass.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn referenced_finding_retains_producing_pass() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let canonical_pass = insert_fixture_pass(&fixture, 0x332, ReviewPassKind::ReadOnlyReview).await;
    let dedupe_pass = insert_fixture_pass(&fixture, 0x333, ReviewPassKind::Dedupe).await;
    let evidence = succeed_fixture_passes(
        &pool,
        &fixture.store,
        &[fixture.pass, canonical_pass, dedupe_pass],
    )
    .await;
    let finding_ref = ReviewFindingRef::new(fixture.pass, ReviewFindingId::from_uuid(uuid(0x330)));
    let canonical_ref =
        ReviewFindingRef::new(canonical_pass, ReviewFindingId::from_uuid(uuid(0x331)));
    let review_evidence = pass_with_produced_findings(vec![finding_ref], evidence[0].clone());
    let canonical_evidence = pass_with_produced_findings(vec![canonical_ref], evidence[1].clone());
    let dedupe_evidence = evidence[2].clone();
    let open = finding(
        finding_ref,
        review_evidence.clone(),
        &fixture.target_snapshot,
    );
    let canonical = finding(
        canonical_ref,
        canonical_evidence.clone(),
        &fixture.target_snapshot,
    );
    fixture
        .store
        .insert_findings(&review_evidence, std::slice::from_ref(&open))
        .await?;
    fixture
        .store
        .insert_findings(&canonical_evidence, std::slice::from_ref(&canonical))
        .await?;
    let event = finding_event(
        finding_ref,
        ReviewEventOrdinal::one(),
        dedupe_evidence,
        ReviewFindingEventKind::Duplicate {
            canonical: ReviewReferencedFindingEvidence::try_from_finding(&canonical)
                .expect("open finding is eligible reference evidence"),
        },
    );
    let expected = open
        .apply(event.clone())
        .expect("dedupe pass may identify the canonical finding");
    fixture
        .store
        .append_finding_event(finding_ref.finding(), event)
        .await?;

    let retry = fixture
        .store
        .transition_run_and_pass(
            dedupe_pass.run().run(),
            dedupe_pass.pass(),
            ReviewRunState::Queued,
            ReviewPassState::Queued,
        )
        .await
        .expect_err("terminal referenced result rejects a later pass transition cleanly");

    assert!(matches!(
        retry,
        ReviewWorkflowStoreError::InvalidTransition(ReviewWorkflowTransitionError::Pass(_))
    ));
    assert_eq!(
        fixture.store.load_finding(finding_ref.finding()).await?,
        Some(expected)
    );
    Ok(())
}

/// reference admission and reconstitution observe terminalization
/// that commits while waiting for the relational transition barrier.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn reference_refreshes_after_terminalization_wait() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let canonical_pass = insert_fixture_pass(&fixture, 0x8a1, ReviewPassKind::ReadOnlyReview).await;
    let rejection_pass = insert_fixture_pass(&fixture, 0x8a2, ReviewPassKind::Judge).await;
    let dedupe_pass = insert_fixture_pass(&fixture, 0x8a3, ReviewPassKind::Dedupe).await;
    let evidence = succeed_fixture_passes(
        &pool,
        &fixture.store,
        &[fixture.pass, canonical_pass, rejection_pass, dedupe_pass],
    )
    .await;
    let subject_ref = ReviewFindingRef::new(fixture.pass, ReviewFindingId::from_uuid(uuid(0x8a4)));
    let canonical_ref =
        ReviewFindingRef::new(canonical_pass, ReviewFindingId::from_uuid(uuid(0x8a5)));
    let subject_evidence = pass_with_produced_findings(vec![subject_ref], evidence[0].clone());
    let canonical_evidence = pass_with_produced_findings(vec![canonical_ref], evidence[1].clone());
    let subject = finding(
        subject_ref,
        subject_evidence.clone(),
        &fixture.target_snapshot,
    );
    let canonical = finding(
        canonical_ref,
        canonical_evidence.clone(),
        &fixture.target_snapshot,
    );
    fixture
        .store
        .insert_findings(&subject_evidence, std::slice::from_ref(&subject))
        .await?;
    fixture
        .store
        .insert_findings(&canonical_evidence, std::slice::from_ref(&canonical))
        .await?;
    let reason = text("the canonical finding is no longer actionable");
    let rejection = finding_event(
        canonical_ref,
        ReviewEventOrdinal::one(),
        evidence[2].clone(),
        ReviewFindingEventKind::Rejected {
            reason: reason.clone(),
        },
    );
    let duplicate = finding_event(
        subject_ref,
        ReviewEventOrdinal::one(),
        evidence[3].clone(),
        ReviewFindingEventKind::Duplicate {
            canonical: ReviewReferencedFindingEvidence::try_from_finding(&canonical)
                .expect("open canonical finding is reference evidence"),
        },
    );
    let rejected = canonical
        .apply(rejection)
        .expect("the judge may reject the canonical finding");

    let mut terminalizing = pool.begin().await?;
    sqlx::query(
        "SELECT finding_id
           FROM review_finding
          WHERE finding_id = $1
          FOR NO KEY UPDATE",
    )
    .bind(canonical_ref.finding().into_uuid())
    .fetch_one(&mut *terminalizing)
    .await?;
    sqlx::query(
        "UPDATE review_pass
            SET result_kind = 'finding_event',
                result_finding_id = $2,
                result_finding_run_id = $3,
                result_finding_pass_id = $4,
                result_event_ordinal = 1,
                result_event_kind = 'rejected',
                result_reason = $5
          WHERE pass_id = $1",
    )
    .bind(rejection_pass.pass().into_uuid())
    .bind(canonical_ref.finding().into_uuid())
    .bind(canonical_ref.run().run().into_uuid())
    .bind(canonical_ref.pass().pass().into_uuid())
    .bind(reason.as_str())
    .execute(&mut *terminalizing)
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
             $1, 1, $2, $3, $4, $5, 'rejected', $6,
             NULL, NULL, NULL, NULL, NULL, NULL, NULL
         )",
    )
    .bind(canonical_ref.finding().into_uuid())
    .bind(canonical_ref.run().run().into_uuid())
    .bind(canonical_ref.target().into_uuid())
    .bind(rejection_pass.pass().into_uuid())
    .bind(rejection_pass.run().run().into_uuid())
    .bind(reason.as_str())
    .execute(&mut *terminalizing)
    .await?;

    let mut appending_transaction = pool.begin().await?;
    sqlx::query(
        "UPDATE review_pass
            SET result_kind = 'finding_event',
                result_finding_id = $2,
                result_finding_run_id = $3,
                result_finding_pass_id = $4,
                result_event_ordinal = $5,
                result_event_kind = 'duplicate',
                result_referenced_finding_id = $6,
                result_referenced_finding_run_id = $7,
                result_referenced_finding_target_id = $8,
                result_referenced_finding_pass_id = $9,
                result_referenced_finding_status = 'open'
          WHERE pass_id = $1",
    )
    .bind(duplicate.pass().pass().into_uuid())
    .bind(duplicate.finding().finding().into_uuid())
    .bind(duplicate.finding().run().run().into_uuid())
    .bind(duplicate.finding().pass().pass().into_uuid())
    .bind(i64::from(duplicate.ordinal().get()))
    .bind(canonical_ref.finding().into_uuid())
    .bind(canonical_ref.run().run().into_uuid())
    .bind(canonical_ref.target().into_uuid())
    .bind(canonical_ref.pass().pass().into_uuid())
    .execute(&mut *appending_transaction)
    .await?;
    let appending = tokio::spawn(async move {
        sqlx::query(
            "INSERT INTO review_finding_event
                (finding_id, event_ordinal, finding_run_id, target_id,
                 event_pass_id, event_pass_run_id, event_kind, reason,
                 referenced_finding_id, referenced_finding_run_id,
                 referenced_finding_target_id, referenced_finding_pass_id,
                 referenced_finding_status, external_link_id,
                 external_link_association_kind)
             VALUES (
                 $1, $2, $3, $4, $5, $6, 'duplicate', NULL,
                 $7, $8, $9, $10, 'open', NULL, NULL
             )",
        )
        .bind(duplicate.finding().finding().into_uuid())
        .bind(i64::from(duplicate.ordinal().get()))
        .bind(duplicate.finding().run().run().into_uuid())
        .bind(duplicate.finding().target().into_uuid())
        .bind(duplicate.pass().pass().into_uuid())
        .bind(duplicate.pass().run().run().into_uuid())
        .bind(canonical_ref.finding().into_uuid())
        .bind(canonical_ref.run().run().into_uuid())
        .bind(canonical_ref.target().into_uuid())
        .bind(canonical_ref.pass().pass().into_uuid())
        .execute(&mut *appending_transaction)
        .await?;
        appending_transaction.commit().await
    });
    assert!(
        blocked_backends_reached(&pool, 1).await?,
        "reference admission waits for the canonical finding lock"
    );
    terminalizing.commit().await?;
    let error = appending
        .await
        .expect("reference admission task remains live")
        .expect_err("terminal canonical status must reject the stale reference");
    assert_sqlstate(&error, "23514");
    assert_eq!(
        fixture.store.load_finding(canonical_ref.finding()).await?,
        Some(rejected)
    );
    assert_eq!(
        fixture.store.load_finding(subject_ref.finding()).await?,
        Some(subject)
    );
    Ok(())
}

/// finding reconstitution rejects a mutable head that does not name
/// the exact latest append-only event.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn finding_load_rejects_mismatched_event_head() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let judge_pass = insert_fixture_pass(&fixture, 0x8b1, ReviewPassKind::Judge).await;
    let evidence = succeed_fixture_passes(&pool, &fixture.store, &[fixture.pass, judge_pass]).await;
    let finding_ref = ReviewFindingRef::new(fixture.pass, ReviewFindingId::from_uuid(uuid(0x8b2)));
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
    fixture
        .store
        .append_finding_event(
            finding_ref.finding(),
            finding_event(
                finding_ref,
                ReviewEventOrdinal::one(),
                evidence[1].clone(),
                ReviewFindingEventKind::Rejected {
                    reason: text("the finding is not actionable"),
                },
            ),
        )
        .await
        .expect("rejected finding event persists");

    sqlx::query(
        "ALTER TABLE review_finding_event_head
         DISABLE TRIGGER ALL",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE review_finding_event_head
            SET status = $2
          WHERE finding_id = $1",
    )
    .bind(finding_ref.finding().into_uuid())
    .bind("accepted")
    .execute(&pool)
    .await?;

    assert_finding_reference_load_corruption(
        fixture.store.load_finding(finding_ref.finding()).await,
        "review_finding_event_head",
    );
    Ok(())
}

/// the persistence boundary rejects a complete reference whose
/// authenticated producer belongs to another immutable target.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn store_rejects_cross_target_finding_reference() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let foreign_target = ReviewTargetId::from_uuid(uuid(0x6330));
    let foreign_snapshot = ReviewTarget::try_new(
        foreign_target,
        key("example-code-host"),
        key("example/repository"),
        ReviewTargetSubject::Commit,
        key("foreign-head"),
        Some(key("foreign-base")),
        None,
    )
    .expect("foreign target is valid");
    fixture.store.insert_target(&foreign_snapshot).await?;
    let foreign_producer = insert_isolated_pass_for_target(
        &pool,
        &fixture.store,
        foreign_target,
        0x6331,
        ReviewPassKind::ReadOnlyReview,
    )
    .await
    .0;
    let dedupe_pass = insert_fixture_pass(&fixture, 0x6332, ReviewPassKind::Dedupe).await;
    let evidence = succeed_fixture_passes(
        &pool,
        &fixture.store,
        &[fixture.pass, foreign_producer, dedupe_pass],
    )
    .await;
    let subject_ref = ReviewFindingRef::new(fixture.pass, ReviewFindingId::from_uuid(uuid(0x6333)));
    let foreign_ref =
        ReviewFindingRef::new(foreign_producer, ReviewFindingId::from_uuid(uuid(0x6334)));
    let subject_evidence = pass_with_produced_findings(vec![subject_ref], evidence[0].clone());
    let foreign_evidence = pass_with_produced_findings(vec![foreign_ref], evidence[1].clone());
    let subject = finding(
        subject_ref,
        subject_evidence.clone(),
        &fixture.target_snapshot,
    );
    let foreign = finding(foreign_ref, foreign_evidence.clone(), &foreign_snapshot);
    fixture
        .store
        .insert_findings(&subject_evidence, std::slice::from_ref(&subject))
        .await?;
    fixture
        .store
        .insert_findings(&foreign_evidence, std::slice::from_ref(&foreign))
        .await?;
    let error = sqlx::query(
        "UPDATE review_pass
            SET result_kind = 'finding_event',
                result_finding_id = $2,
                result_finding_run_id = $3,
                result_finding_pass_id = $4,
                result_event_ordinal = 1,
                result_event_kind = 'duplicate',
                result_referenced_finding_id = $5,
                result_referenced_finding_run_id = $6,
                result_referenced_finding_target_id = $7,
                result_referenced_finding_pass_id = $8,
                result_referenced_finding_status = 'open'
          WHERE pass_id = $1",
    )
    .bind(dedupe_pass.pass().into_uuid())
    .bind(subject_ref.finding().into_uuid())
    .bind(subject_ref.run().run().into_uuid())
    .bind(subject_ref.pass().pass().into_uuid())
    .bind(foreign_ref.finding().into_uuid())
    .bind(foreign_ref.run().run().into_uuid())
    .bind(foreign_ref.target().into_uuid())
    .bind(foreign_ref.pass().pass().into_uuid())
    .execute(&pool)
    .await
    .expect_err("cross-target reference must fail relational admission");
    assert_sqlstate(&error, "23514");
    Ok(())
}

/// reconstitution rejects a referenced producer whose durable frozen
/// policy differs or whose canonical pass is no longer read-only review.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn loader_rejects_reference_policy_or_producer_mismatch() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let canonical_pass =
        insert_fixture_pass(&fixture, 0x6431, ReviewPassKind::ReadOnlyReview).await;
    let dedupe_pass = insert_fixture_pass(&fixture, 0x6432, ReviewPassKind::Dedupe).await;
    let evidence = succeed_fixture_passes(
        &pool,
        &fixture.store,
        &[fixture.pass, canonical_pass, dedupe_pass],
    )
    .await;
    let subject_ref = ReviewFindingRef::new(fixture.pass, ReviewFindingId::from_uuid(uuid(0x6433)));
    let canonical_ref =
        ReviewFindingRef::new(canonical_pass, ReviewFindingId::from_uuid(uuid(0x6434)));
    let subject_evidence = pass_with_produced_findings(vec![subject_ref], evidence[0].clone());
    let canonical_evidence = pass_with_produced_findings(vec![canonical_ref], evidence[1].clone());
    let subject = finding(
        subject_ref,
        subject_evidence.clone(),
        &fixture.target_snapshot,
    );
    let canonical = finding(
        canonical_ref,
        canonical_evidence.clone(),
        &fixture.target_snapshot,
    );
    fixture
        .store
        .insert_findings(&subject_evidence, std::slice::from_ref(&subject))
        .await?;
    fixture
        .store
        .insert_findings(&canonical_evidence, std::slice::from_ref(&canonical))
        .await?;
    fixture
        .store
        .append_finding_event(
            subject_ref.finding(),
            finding_event(
                subject_ref,
                ReviewEventOrdinal::one(),
                evidence[2].clone(),
                ReviewFindingEventKind::Duplicate {
                    canonical: ReviewReferencedFindingEvidence::try_from_finding(&canonical)
                        .expect("open canonical finding is reference evidence"),
                },
            ),
        )
        .await?;

    sqlx::query(
        "ALTER TABLE review_run
         DROP CONSTRAINT review_run_confidence_bounds",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE review_run
         DISABLE TRIGGER review_run_change_is_guarded",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE review_run
            SET minimum_judge_confidence = 7001
          WHERE run_id = $1",
    )
    .bind(canonical_ref.run().run().into_uuid())
    .execute(&pool)
    .await?;

    let error = fixture
        .store
        .load_finding(subject_ref.finding())
        .await
        .expect_err("different frozen producer policy must fail loading closed");
    let ReviewWorkflowStoreError::Corruption(error) = error else {
        panic!("expected typed referenced-policy corruption");
    };
    assert_finding_reference_load_corruption(
        Err(ReviewWorkflowStoreError::Corruption(error)),
        "review_pass",
    );
    sqlx::query(
        "UPDATE review_run
            SET minimum_judge_confidence = 7000
          WHERE run_id = $1",
    )
    .bind(canonical_ref.run().run().into_uuid())
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
            SET pass_kind = 'judge'
          WHERE pass_id = $1",
    )
    .bind(canonical_ref.pass().pass().into_uuid())
    .execute(&pool)
    .await?;
    assert_finding_reference_load_corruption(
        fixture.store.load_finding(subject_ref.finding()).await,
        "review_pass",
    );
    Ok(())
}

/// target/run/pass/finding legs are independently retained; corrupting
/// any one leg cannot be normalized into a plausible reference during load.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn loader_rejects_each_cross_wired_reference_ancestry_leg() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let canonical_pass =
        insert_fixture_pass(&fixture, 0x6531, ReviewPassKind::ReadOnlyReview).await;
    let dedupe_pass = insert_fixture_pass(&fixture, 0x6532, ReviewPassKind::Dedupe).await;
    let evidence = succeed_fixture_passes(
        &pool,
        &fixture.store,
        &[fixture.pass, canonical_pass, dedupe_pass],
    )
    .await;
    let subject_ref = ReviewFindingRef::new(fixture.pass, ReviewFindingId::from_uuid(uuid(0x6533)));
    let canonical_ref =
        ReviewFindingRef::new(canonical_pass, ReviewFindingId::from_uuid(uuid(0x6534)));
    let subject_evidence = pass_with_produced_findings(vec![subject_ref], evidence[0].clone());
    let canonical_evidence = pass_with_produced_findings(vec![canonical_ref], evidence[1].clone());
    let subject = finding(
        subject_ref,
        subject_evidence.clone(),
        &fixture.target_snapshot,
    );
    let canonical = finding(
        canonical_ref,
        canonical_evidence.clone(),
        &fixture.target_snapshot,
    );
    fixture
        .store
        .insert_findings(&subject_evidence, std::slice::from_ref(&subject))
        .await?;
    fixture
        .store
        .insert_findings(&canonical_evidence, std::slice::from_ref(&canonical))
        .await?;
    fixture
        .store
        .append_finding_event(
            subject_ref.finding(),
            finding_event(
                subject_ref,
                ReviewEventOrdinal::one(),
                evidence[2].clone(),
                ReviewFindingEventKind::Duplicate {
                    canonical: ReviewReferencedFindingEvidence::try_from_finding(&canonical)
                        .expect("open canonical finding is reference evidence"),
                },
            ),
        )
        .await?;

    sqlx::query(
        "ALTER TABLE review_finding_event
         DROP CONSTRAINT review_finding_event_referenced_finding_fk,
         DROP CONSTRAINT review_finding_event_referenced_inventory_fk,
         DROP CONSTRAINT review_finding_event_referenced_ancestry_shape,
         DISABLE TRIGGER review_finding_event_is_append_only",
    )
    .execute(&pool)
    .await?;

    sqlx::query(
        "UPDATE review_finding_event
            SET referenced_finding_target_id = $2
          WHERE finding_id = $1",
    )
    .bind(subject_ref.finding().into_uuid())
    .bind(uuid(0x65f1))
    .execute(&pool)
    .await?;
    assert_finding_reference_load_corruption(
        fixture.store.load_finding(subject_ref.finding()).await,
        "review_finding_event",
    );
    sqlx::query(
        "UPDATE review_finding_event
            SET referenced_finding_target_id = $2,
                referenced_finding_run_id = $3
          WHERE finding_id = $1",
    )
    .bind(subject_ref.finding().into_uuid())
    .bind(canonical_ref.target().into_uuid())
    .bind(uuid(0x65f2))
    .execute(&pool)
    .await?;
    assert_finding_reference_load_corruption(
        fixture.store.load_finding(subject_ref.finding()).await,
        "review_finding_event",
    );
    sqlx::query(
        "UPDATE review_finding_event
            SET referenced_finding_run_id = $2,
                referenced_finding_pass_id = $3
          WHERE finding_id = $1",
    )
    .bind(subject_ref.finding().into_uuid())
    .bind(canonical_ref.run().run().into_uuid())
    .bind(uuid(0x65f3))
    .execute(&pool)
    .await?;
    assert_finding_reference_load_corruption(
        fixture.store.load_finding(subject_ref.finding()).await,
        "review_finding_event",
    );
    sqlx::query(
        "UPDATE review_finding_event
            SET referenced_finding_pass_id = $2,
                referenced_finding_id = $3
          WHERE finding_id = $1",
    )
    .bind(subject_ref.finding().into_uuid())
    .bind(canonical_ref.pass().pass().into_uuid())
    .bind(uuid(0x65f4))
    .execute(&pool)
    .await?;
    assert_finding_reference_load_corruption(
        fixture.store.load_finding(subject_ref.finding()).await,
        "review_finding_event",
    );
    Ok(())
}

/// referenced producer reconstitution requires both its immutable
/// inventory seal and the exact referenced finding member.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn loader_rejects_unsealed_or_nonmember_referenced_producer() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let canonical_pass =
        insert_fixture_pass(&fixture, 0x6631, ReviewPassKind::ReadOnlyReview).await;
    let dedupe_pass = insert_fixture_pass(&fixture, 0x6632, ReviewPassKind::Dedupe).await;
    let evidence = succeed_fixture_passes(
        &pool,
        &fixture.store,
        &[fixture.pass, canonical_pass, dedupe_pass],
    )
    .await;
    let subject_ref = ReviewFindingRef::new(fixture.pass, ReviewFindingId::from_uuid(uuid(0x6633)));
    let canonical_ref =
        ReviewFindingRef::new(canonical_pass, ReviewFindingId::from_uuid(uuid(0x6634)));
    let subject_evidence = pass_with_produced_findings(vec![subject_ref], evidence[0].clone());
    let canonical_evidence = pass_with_produced_findings(vec![canonical_ref], evidence[1].clone());
    let subject = finding(
        subject_ref,
        subject_evidence.clone(),
        &fixture.target_snapshot,
    );
    let canonical = finding(
        canonical_ref,
        canonical_evidence.clone(),
        &fixture.target_snapshot,
    );
    fixture
        .store
        .insert_findings(&subject_evidence, std::slice::from_ref(&subject))
        .await?;
    fixture
        .store
        .insert_findings(&canonical_evidence, std::slice::from_ref(&canonical))
        .await?;
    fixture
        .store
        .append_finding_event(
            subject_ref.finding(),
            finding_event(
                subject_ref,
                ReviewEventOrdinal::one(),
                evidence[2].clone(),
                ReviewFindingEventKind::Duplicate {
                    canonical: ReviewReferencedFindingEvidence::try_from_finding(&canonical)
                        .expect("open canonical finding is reference evidence"),
                },
            ),
        )
        .await?;

    sqlx::query(
        "ALTER TABLE review_pass_finding_inventory_seal
         DISABLE TRIGGER review_pass_finding_inventory_seal_is_append_only",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "DELETE FROM review_pass_finding_inventory_seal
          WHERE pass_id = $1",
    )
    .bind(canonical_pass.pass().into_uuid())
    .execute(&pool)
    .await?;
    assert_finding_reference_load_corruption(
        fixture.store.load_finding(subject_ref.finding()).await,
        "review_pass_produced_finding",
    );
    sqlx::query(
        "INSERT INTO review_pass_finding_inventory_seal
            (pass_id, finding_count)
         VALUES ($1, 1)",
    )
    .bind(canonical_pass.pass().into_uuid())
    .execute(&pool)
    .await?;

    sqlx::query(
        "ALTER TABLE review_pass
         DROP CONSTRAINT review_pass_result_referenced_inventory_fk",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE review_finding_event
         DROP CONSTRAINT review_finding_event_referenced_inventory_fk",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE review_pass_produced_finding
         DISABLE TRIGGER review_pass_produced_finding_is_append_only",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "DELETE FROM review_pass_produced_finding
          WHERE pass_id = $1
            AND finding_id = $2",
    )
    .bind(canonical_pass.pass().into_uuid())
    .bind(canonical_ref.finding().into_uuid())
    .execute(&pool)
    .await?;
    assert_finding_reference_load_corruption(
        fixture.store.load_finding(subject_ref.finding()).await,
        "review_pass_produced_finding",
    );
    Ok(())
}

/// duplicate/superseded references cannot close a cycle by
/// referencing a finding whose current status is already terminal.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn finding_references_reject_cycles() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let second_producer =
        insert_fixture_pass(&fixture, 0x335, ReviewPassKind::ReadOnlyReview).await;
    let first_dedupe = insert_fixture_pass(&fixture, 0x336, ReviewPassKind::Dedupe).await;
    let second_dedupe = insert_fixture_pass(&fixture, 0x337, ReviewPassKind::Dedupe).await;
    let evidence = succeed_fixture_passes(
        &pool,
        &fixture.store,
        &[fixture.pass, second_producer, first_dedupe, second_dedupe],
    )
    .await;
    let first = ReviewFindingRef::new(fixture.pass, ReviewFindingId::from_uuid(uuid(0x338)));
    let second = ReviewFindingRef::new(second_producer, ReviewFindingId::from_uuid(uuid(0x339)));
    let review_evidence = pass_with_produced_findings(vec![first], evidence[0].clone());
    let second_evidence = pass_with_produced_findings(vec![second], evidence[1].clone());
    let first_finding = finding(first, review_evidence.clone(), &fixture.target_snapshot);
    let second_finding = finding(second, second_evidence.clone(), &fixture.target_snapshot);
    fixture
        .store
        .insert_findings(&review_evidence, std::slice::from_ref(&first_finding))
        .await?;
    fixture
        .store
        .insert_findings(&second_evidence, std::slice::from_ref(&second_finding))
        .await?;
    fixture
        .store
        .append_finding_event(
            first.finding(),
            finding_event(
                first,
                ReviewEventOrdinal::one(),
                evidence[2].clone(),
                ReviewFindingEventKind::Duplicate {
                    canonical: ReviewReferencedFindingEvidence::try_from_finding(&second_finding)
                        .expect("open finding is eligible reference evidence"),
                },
            ),
        )
        .await?
        .expect("first reference persists");

    let cycle = fixture
        .store
        .append_finding_event(
            second.finding(),
            finding_event(
                second,
                ReviewEventOrdinal::one(),
                evidence[3].clone(),
                ReviewFindingEventKind::Duplicate {
                    canonical: ReviewReferencedFindingEvidence::try_from_finding(&first_finding)
                        .expect("open finding is eligible reference evidence"),
                },
            ),
        )
        .await
        .expect_err("terminal reference targets cannot close a cycle");
    let ReviewWorkflowStoreError::Database(cycle) = cycle else {
        panic!("cycle prevention must be a database rejection");
    };
    assert_sqlstate(&cycle, "23514");
    Ok(())
}

/// complete-target loading rejects a transitive cross-run cycle even
/// when corrupt SQL bypassed the admission trigger that prevents it.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn loader_rejects_transitive_cross_run_reference_cycle() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let second_producer =
        insert_fixture_pass(&fixture, 0x5331, ReviewPassKind::ReadOnlyReview).await;
    let third_producer =
        insert_fixture_pass(&fixture, 0x5332, ReviewPassKind::ReadOnlyReview).await;
    let first_dedupe = insert_fixture_pass(&fixture, 0x5333, ReviewPassKind::Dedupe).await;
    let second_dedupe = insert_fixture_pass(&fixture, 0x5334, ReviewPassKind::Dedupe).await;
    let third_dedupe = insert_fixture_pass(&fixture, 0x5335, ReviewPassKind::Dedupe).await;
    let evidence = succeed_fixture_passes(
        &pool,
        &fixture.store,
        &[
            fixture.pass,
            second_producer,
            third_producer,
            first_dedupe,
            second_dedupe,
            third_dedupe,
        ],
    )
    .await;
    let first = ReviewFindingRef::new(fixture.pass, ReviewFindingId::from_uuid(uuid(0x5340)));
    let second = ReviewFindingRef::new(second_producer, ReviewFindingId::from_uuid(uuid(0x5341)));
    let third = ReviewFindingRef::new(third_producer, ReviewFindingId::from_uuid(uuid(0x5342)));
    let first_evidence = pass_with_produced_findings(vec![first], evidence[0].clone());
    let second_evidence = pass_with_produced_findings(vec![second], evidence[1].clone());
    let third_evidence = pass_with_produced_findings(vec![third], evidence[2].clone());
    let first_finding = finding(first, first_evidence.clone(), &fixture.target_snapshot);
    let second_finding = finding(second, second_evidence.clone(), &fixture.target_snapshot);
    let third_finding = finding(third, third_evidence.clone(), &fixture.target_snapshot);
    fixture
        .store
        .insert_findings(&first_evidence, std::slice::from_ref(&first_finding))
        .await?;
    fixture
        .store
        .insert_findings(&second_evidence, std::slice::from_ref(&second_finding))
        .await?;
    fixture
        .store
        .insert_findings(&third_evidence, std::slice::from_ref(&third_finding))
        .await?;
    fixture
        .store
        .append_finding_event(
            first.finding(),
            finding_event(
                first,
                ReviewEventOrdinal::one(),
                evidence[3].clone(),
                ReviewFindingEventKind::Duplicate {
                    canonical: ReviewReferencedFindingEvidence::try_from_finding(&second_finding)
                        .expect("open second finding is reference evidence"),
                },
            ),
        )
        .await?;
    fixture
        .store
        .append_finding_event(
            second.finding(),
            finding_event(
                second,
                ReviewEventOrdinal::one(),
                evidence[4].clone(),
                ReviewFindingEventKind::Duplicate {
                    canonical: ReviewReferencedFindingEvidence::try_from_finding(&third_finding)
                        .expect("open third finding is reference evidence"),
                },
            ),
        )
        .await?;

    sqlx::query(
        "ALTER TABLE review_finding_event
         DISABLE TRIGGER review_finding_event_sequence_is_guarded,
         DISABLE TRIGGER review_finding_event_transition_head_is_guarded,
         DISABLE TRIGGER review_finding_event_transition_head_is_advanced",
    )
    .execute(&pool)
    .await?;
    let mut transaction = pool.begin().await?;
    sqlx::query(
        "UPDATE review_pass
            SET result_kind = 'finding_event',
                result_finding_id = $2,
                result_finding_run_id = $3,
                result_finding_pass_id = $4,
                result_event_ordinal = 1,
                result_event_kind = 'duplicate',
                result_referenced_finding_id = $5,
                result_referenced_finding_run_id = $6,
                result_referenced_finding_target_id = $7,
                result_referenced_finding_pass_id = $8,
                result_referenced_finding_status = 'open'
          WHERE pass_id = $1",
    )
    .bind(third_dedupe.pass().into_uuid())
    .bind(third.finding().into_uuid())
    .bind(third.run().run().into_uuid())
    .bind(third.pass().pass().into_uuid())
    .bind(first.finding().into_uuid())
    .bind(first.run().run().into_uuid())
    .bind(first.target().into_uuid())
    .bind(first.pass().pass().into_uuid())
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
             $1, 1, $2, $3, $4, $5, 'duplicate', NULL,
             $6, $7, $8, $9, 'open', NULL, NULL
         )",
    )
    .bind(third.finding().into_uuid())
    .bind(third.run().run().into_uuid())
    .bind(third.target().into_uuid())
    .bind(third_dedupe.pass().into_uuid())
    .bind(third_dedupe.run().run().into_uuid())
    .bind(first.finding().into_uuid())
    .bind(first.run().run().into_uuid())
    .bind(first.target().into_uuid())
    .bind(first.pass().pass().into_uuid())
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "UPDATE review_finding_event_head
            SET event_ordinal = 1,
                status = $2,
                event_pass_kind = $3,
                external_link_id = NULL
          WHERE finding_id = $1",
    )
    .bind(third.finding().into_uuid())
    .bind("duplicate")
    .bind("dedupe")
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await?;
    sqlx::query(
        "ALTER TABLE review_finding_event
         ENABLE TRIGGER review_finding_event_sequence_is_guarded,
         ENABLE TRIGGER review_finding_event_transition_head_is_guarded,
         ENABLE TRIGGER review_finding_event_transition_head_is_advanced",
    )
    .execute(&pool)
    .await?;

    let error = fixture
        .store
        .load_finding(first.finding())
        .await
        .expect_err("transitive reference cycle must fail complete graph loading");
    let ReviewWorkflowStoreError::Corruption(error) = error else {
        panic!("expected typed finding reference-graph corruption");
    };
    assert_eq!(error.aggregate(), "review_finding");
    assert!(error.detail().contains("reference graph"));
    Ok(())
}

/// a referenced finding's missing canonical producer is corruption,
/// even when the aggregate finding's own producer remains intact.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn finding_load_rejects_missing_referenced_producer() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let dedupe_pass = insert_fixture_pass(&fixture, 0x33a, ReviewPassKind::Dedupe).await;
    let evidence =
        succeed_fixture_passes(&pool, &fixture.store, &[fixture.pass, dedupe_pass]).await;
    let finding_ref = ReviewFindingRef::new(fixture.pass, ReviewFindingId::from_uuid(uuid(0x33b)));
    let canonical_ref =
        ReviewFindingRef::new(fixture.pass, ReviewFindingId::from_uuid(uuid(0x33c)));
    let review_evidence =
        pass_with_produced_findings(vec![finding_ref, canonical_ref], evidence[0].clone());
    let subject = finding(
        finding_ref,
        review_evidence.clone(),
        &fixture.target_snapshot,
    );
    let canonical = finding(
        canonical_ref,
        review_evidence.clone(),
        &fixture.target_snapshot,
    );
    fixture
        .store
        .insert_findings(&review_evidence, &[subject, canonical.clone()])
        .await?;
    fixture
        .store
        .append_finding_event(
            finding_ref.finding(),
            finding_event(
                finding_ref,
                ReviewEventOrdinal::one(),
                evidence[1].clone(),
                ReviewFindingEventKind::Duplicate {
                    canonical: ReviewReferencedFindingEvidence::try_from_finding(&canonical)
                        .expect("open finding is eligible reference evidence"),
                },
            ),
        )
        .await?;

    sqlx::query(
        "ALTER TABLE review_finding
         DROP CONSTRAINT review_finding_producing_pass_fk",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE review_pass
         DROP CONSTRAINT review_pass_result_referenced_finding_fk,
         DROP CONSTRAINT review_pass_result_referenced_inventory_fk",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE review_pass_produced_finding
         DROP CONSTRAINT review_pass_produced_finding_finding_fk",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE review_finding_event
         DROP CONSTRAINT review_finding_event_referenced_finding_fk,
         DROP CONSTRAINT review_finding_event_referenced_inventory_fk",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE review_finding
         DISABLE TRIGGER review_finding_is_append_only",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE review_finding
            SET producing_pass_id = $2
          WHERE finding_id = $1",
    )
    .bind(canonical_ref.finding().into_uuid())
    .bind(uuid(0x33d))
    .execute(&pool)
    .await?;

    assert_finding_reference_load_corruption(
        fixture.store.load_finding(finding_ref.finding()).await,
        "review_pass_produced_finding",
    );
    Ok(())
}

/// the event row must exactly match the finding result committed by
/// its terminal pass, including ordinal and event type.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn finding_event_requires_exact_pass_result() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let judge_pass = insert_fixture_pass(&fixture, 0x33e, ReviewPassKind::Judge).await;
    let evidence = succeed_fixture_passes(&pool, &fixture.store, &[fixture.pass, judge_pass]).await;
    let finding_ref = ReviewFindingRef::new(fixture.pass, ReviewFindingId::from_uuid(uuid(0x33f)));
    fixture
        .store
        .insert_finding(&finding(
            finding_ref,
            evidence[0].clone(),
            &fixture.target_snapshot,
        ))
        .await?;
    let mut transaction = pool.begin().await?;
    sqlx::query(
        "UPDATE review_pass
            SET result_kind = 'finding_event',
                result_finding_id = $1,
                result_finding_run_id = $2,
                result_finding_pass_id = $3,
                result_event_ordinal = 1,
                result_event_kind = 'accepted'
          WHERE pass_id = $4",
    )
    .bind(finding_ref.finding().into_uuid())
    .bind(finding_ref.run().run().into_uuid())
    .bind(finding_ref.pass().pass().into_uuid())
    .bind(judge_pass.pass().into_uuid())
    .execute(&mut *transaction)
    .await?;

    let mismatched = sqlx::query(
        "INSERT INTO review_finding_event
            (finding_id, event_ordinal, finding_run_id, target_id,
             event_pass_id, event_pass_run_id, event_kind, reason,
             referenced_finding_id, referenced_finding_status,
             external_link_id, external_link_association_kind)
         VALUES (
             $1, 1, $2, $3, $4, $5, 'rejected', 'not accepted',
             NULL, NULL, NULL, NULL
         )",
    )
    .bind(finding_ref.finding().into_uuid())
    .bind(finding_ref.run().run().into_uuid())
    .bind(finding_ref.target().into_uuid())
    .bind(judge_pass.pass().into_uuid())
    .bind(judge_pass.run().run().into_uuid())
    .execute(&mut *transaction)
    .await
    .expect_err("event kind must equal the terminal pass result");
    assert_sqlstate(&mismatched, "23514");
    transaction.rollback().await?;
    Ok(())
}

/// file-relative findings admit no diff side, while a diff-relative
/// location requires a canonical target base.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn finding_diff_side_requires_target_base() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let target = ReviewTarget::try_new(
        ReviewTargetId::from_uuid(uuid(0x340)),
        key("example-code-host"),
        key("example/base-free-repository"),
        ReviewTargetSubject::Commit,
        key("0123456789abcdef"),
        None,
        None,
    )
    .expect("a commit target need not name a comparison revision");
    fixture.store.insert_target(&target).await?;
    let (pass, _) = insert_isolated_pass_for_target(
        &pool,
        &fixture.store,
        target.id(),
        0x342,
        ReviewPassKind::ReadOnlyReview,
    )
    .await;
    let run = pass.run();
    let review_evidence = succeed_fixture_passes(&pool, &fixture.store, &[pass]).await[0].clone();
    let file_relative = ReviewFindingRef::new(pass, ReviewFindingId::from_uuid(uuid(0x343)));
    let file_relative = finding_with_side(file_relative, review_evidence, &target, None);
    fixture.store.insert_finding(&file_relative).await?;
    assert_eq!(
        fixture
            .store
            .load_finding(file_relative.proposal().reference().finding())
            .await?,
        Some(file_relative)
    );

    let diff_relative = sqlx::query(
        "INSERT INTO review_finding
             (finding_id, run_id, target_id, producing_pass_id, file_path,
             line_start, line_end, diff_side, title, body, severity,
             is_real_confidence, severity_label_confidence, category,
             recommended_fix)
         VALUES (
             $1, $2, $3, $4, 'src/lib.rs',
             1, 1, 'right', 'Finding', 'Body', 'high',
             9000, 8500, 'correctness', NULL
         )",
    )
    .bind(uuid(0x344))
    .bind(run.run().into_uuid())
    .bind(target.id().into_uuid())
    .bind(pass.pass().into_uuid())
    .execute(&pool)
    .await
    .expect_err("diff-relative finding requires target comparison evidence");
    assert_sqlstate(&diff_relative, "23514");

    Ok(())
}

/// the store refuses to insert a finding carrying event history.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn finding_insert_requires_open_state() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let judge_pass = insert_fixture_pass(&fixture, 0x3060, ReviewPassKind::Judge).await;
    let evidence = succeed_fixture_passes(&pool, &fixture.store, &[fixture.pass, judge_pass]).await;
    let finding_ref = ReviewFindingRef::new(fixture.pass, ReviewFindingId::from_uuid(uuid(0x306)));
    let accepted = finding(finding_ref, evidence[0].clone(), &fixture.target_snapshot)
        .apply(finding_event(
            finding_ref,
            ReviewEventOrdinal::one(),
            evidence[1].clone(),
            ReviewFindingEventKind::Accepted,
        ))
        .expect("open finding accepts judgment");
    assert!(matches!(
        fixture.store.insert_finding(&accepted).await,
        Err(ReviewWorkflowStoreError::InvalidInsertion(
            ReviewWorkflowInsertionError::FindingNotOpen { .. }
        ))
    ));
    Ok(())
}

/// a finding-associated reservation authenticates the finding's exact
/// canonical producing pass.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn reservation_rejects_forged_finding_producer() -> Result<(), Box<dyn Error>> {
    const FINDING_IDENTITY: u128 = 0x751;
    const FORGED_PASS_IDENTITY: u128 = 0x752;
    const LINK_IDENTITY: u128 = 0x753;

    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let evidence = succeed_fixture_passes(&pool, &fixture.store, &[fixture.pass]).await[0].clone();
    let finding_ref = ReviewFindingRef::new(
        fixture.pass,
        ReviewFindingId::from_uuid(uuid(FINDING_IDENTITY)),
    );
    fixture
        .store
        .insert_finding(&finding(finding_ref, evidence, &fixture.target_snapshot))
        .await?;
    let forged_finding = ReviewFindingRef::new(
        ReviewPassRef::new(
            fixture.run,
            ReviewPassId::from_uuid(uuid(FORGED_PASS_IDENTITY)),
        ),
        finding_ref.finding(),
    );
    let reservation = ReviewExternalLink::try_reserve(
        ReviewExternalLinkId::from_uuid(uuid(LINK_IDENTITY)),
        ReviewExternalLinkAssociation::Finding(forged_finding),
        key("example-code-host"),
        ReviewExternalObjectKind::ReviewComment,
        &fixture.target_snapshot,
    )
    .expect("forged producer remains target-valid before persistence authentication");

    let error = fixture
        .store
        .reserve_external_link(reservation)
        .await
        .expect_err("reservation must authenticate the complete finding reference");
    let ReviewWorkflowStoreError::Database(error) = error else {
        panic!("canonical finding authentication must be a database rejection");
    };
    assert_sqlstate(&error, "23503");
    Ok(())
}

/// finding-event serialization remains compatible with the key-share
/// lock PostgreSQL takes while checking a foreign finding reference.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn finding_event_serialization_is_fk_compatible() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let judge_pass = insert_fixture_pass(&fixture, 0x30a, ReviewPassKind::Judge).await;
    let evidence = succeed_fixture_passes(&pool, &fixture.store, &[fixture.pass, judge_pass]).await;
    let finding_ref = ReviewFindingRef::new(fixture.pass, ReviewFindingId::from_uuid(uuid(0x309)));
    fixture
        .store
        .insert_finding(&finding(
            finding_ref,
            evidence[0].clone(),
            &fixture.target_snapshot,
        ))
        .await
        .expect("open finding persists");

    let mut foreign_key_reader = pool.begin().await?;
    sqlx::query(
        "SELECT finding_id
           FROM review_finding
          WHERE finding_id = $1
          FOR KEY SHARE",
    )
    .bind(finding_ref.finding().into_uuid())
    .fetch_one(&mut *foreign_key_reader)
    .await?;

    let mut appender = pool.begin().await?;
    sqlx::query("SET LOCAL lock_timeout = '1s'")
        .execute(&mut *appender)
        .await?;
    sqlx::query(
        "UPDATE review_pass
            SET result_kind = 'finding_event',
                result_finding_id = $1,
                result_finding_run_id = $2,
                result_finding_pass_id = $3,
                result_event_ordinal = 1,
                result_event_kind = 'accepted'
          WHERE pass_id = $4",
    )
    .bind(finding_ref.finding().into_uuid())
    .bind(finding_ref.pass().run().run().into_uuid())
    .bind(finding_ref.pass().pass().into_uuid())
    .bind(judge_pass.pass().into_uuid())
    .execute(&mut *appender)
    .await?;
    sqlx::query(
        "INSERT INTO review_finding_event
            (finding_id, event_ordinal, finding_run_id, target_id,
             event_pass_id, event_pass_run_id, event_kind, reason,
             referenced_finding_id, external_link_id,
             external_link_association_kind)
         VALUES ($1, 1, $2, $3, $4, $5, 'accepted', NULL, NULL, NULL, NULL)",
    )
    .bind(finding_ref.finding().into_uuid())
    .bind(fixture.run.run().into_uuid())
    .bind(fixture.target.into_uuid())
    .bind(judge_pass.pass().into_uuid())
    .bind(judge_pass.run().run().into_uuid())
    .execute(&mut *appender)
    .await
    .expect("event root lock must remain compatible with foreign-key readers");
    appender.commit().await?;
    foreign_key_reader.rollback().await?;

    Ok(())
}

/// PostgreSQL rejects an event history that does not begin at one.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn gapped_finding_history_is_rejected() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let judge_pass = insert_fixture_pass(&fixture, 0x30b, ReviewPassKind::Judge).await;
    let evidence = succeed_fixture_passes(&pool, &fixture.store, &[fixture.pass, judge_pass]).await;
    let second_finding_ref =
        ReviewFindingRef::new(fixture.pass, ReviewFindingId::from_uuid(uuid(0x306)));
    fixture
        .store
        .insert_finding(&finding(
            second_finding_ref,
            evidence[0].clone(),
            &fixture.target_snapshot,
        ))
        .await
        .expect("second open finding persists");
    let gap = sqlx::query(
        "INSERT INTO review_finding_event
            (finding_id, event_ordinal, finding_run_id, target_id,
             event_pass_id, event_pass_run_id, event_kind, reason,
             referenced_finding_id, external_link_id,
             external_link_association_kind)
         VALUES ($1, 2, $2, $3, $4, $5, 'accepted', NULL, NULL, NULL, NULL)",
    )
    .bind(second_finding_ref.finding().into_uuid())
    .bind(fixture.run.run().into_uuid())
    .bind(fixture.target.into_uuid())
    .bind(judge_pass.pass().into_uuid())
    .bind(judge_pass.run().run().into_uuid())
    .execute(&pool)
    .await
    .expect_err("event history must start at ordinal one");
    assert_sqlstate(&gap, "23514");

    Ok(())
}

/// a missing immutable finding producer is corruption, not absence.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn finding_load_rejects_missing_producing_pass() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let review_evidence =
        succeed_fixture_passes(&pool, &fixture.store, &[fixture.pass]).await[0].clone();
    let finding_ref = ReviewFindingRef::new(fixture.pass, ReviewFindingId::from_uuid(uuid(0x740)));
    fixture
        .store
        .insert_finding(&finding(
            finding_ref,
            review_evidence,
            &fixture.target_snapshot,
        ))
        .await?;

    sqlx::query(
        "ALTER TABLE review_finding
         DROP CONSTRAINT review_finding_producing_pass_fk",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE review_pass_produced_finding
         DROP CONSTRAINT review_pass_produced_finding_pass_fk",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE review_pass_finding_inventory_seal
         DROP CONSTRAINT review_pass_finding_inventory_seal_pass_fk",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE review_run
         DROP CONSTRAINT review_run_state_pass_fk",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE review_pass
         DISABLE TRIGGER review_pass_reject_delete",
    )
    .execute(&pool)
    .await?;
    sqlx::query("DELETE FROM review_pass WHERE pass_id = $1")
        .bind(fixture.pass.pass().into_uuid())
        .execute(&pool)
        .await?;

    let error = fixture
        .store
        .load_finding(finding_ref.finding())
        .await
        .expect_err("missing producer must fail closed");
    let ReviewWorkflowStoreError::Corruption(error) = error else {
        panic!("expected typed review-finding corruption");
    };
    assert_eq!(error.aggregate(), "review_finding");
    assert!(error.detail().contains("producing pass row is missing"));
    Ok(())
}

/// a missing finding-event pass is corruption, not a silently
/// shortened history.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn finding_load_rejects_missing_event_pass() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let judge_pass = insert_fixture_pass(&fixture, 0x741, ReviewPassKind::Judge).await;
    let evidence = succeed_fixture_passes(&pool, &fixture.store, &[fixture.pass, judge_pass]).await;
    let finding_ref = ReviewFindingRef::new(fixture.pass, ReviewFindingId::from_uuid(uuid(0x742)));
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

    sqlx::query(
        "ALTER TABLE review_finding_event
         DROP CONSTRAINT review_finding_event_pass_fk",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE review_run
         DROP CONSTRAINT review_run_state_pass_fk",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE review_pass
         DISABLE TRIGGER review_pass_reject_delete",
    )
    .execute(&pool)
    .await?;
    sqlx::query("DELETE FROM review_pass WHERE pass_id = $1")
        .bind(judge_pass.pass().into_uuid())
        .execute(&pool)
        .await?;

    let error = fixture
        .store
        .load_finding(finding_ref.finding())
        .await
        .expect_err("missing event pass must fail closed");
    let ReviewWorkflowStoreError::Corruption(error) = error else {
        panic!("expected typed review-event corruption");
    };
    assert_eq!(error.aggregate(), "review_finding_event");
    assert!(error.detail().contains("event pass row is missing"));
    Ok(())
}

/// finding insertion authenticates a succeeded read-only-review
/// producer.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn finding_rejects_queued_producer() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let unauthorized = sqlx::query(
        "INSERT INTO review_finding
             (finding_id, run_id, target_id, producing_pass_id, file_path,
             line_start, line_end, diff_side, title, body, severity,
             is_real_confidence, severity_label_confidence, category,
             recommended_fix)
         VALUES (
             $1, $2, $3, $4, 'src/lib.rs',
             NULL, NULL, NULL, 'Finding', 'Body', 'high',
             9000, 8500, 'correctness', NULL
         )",
    )
    .bind(uuid(0x705))
    .bind(fixture.run.run().into_uuid())
    .bind(fixture.target.into_uuid())
    .bind(fixture.pass.pass().into_uuid())
    .execute(&pool)
    .await
    .expect_err("queued pass cannot produce a finding");
    assert_sqlstate(&unauthorized, "23514");
    Ok(())
}

/// a finding event cannot claim a failed disposition pass.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn finding_event_rejects_failed_pass() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let other_session = SessionId::from_uuid(uuid(0x721));
    let other_input = AcceptedInputId::from_uuid(uuid(0x722));
    let other_turn = TurnId::from_uuid(uuid(0x723));
    insert_active_turn_with_offset(&pool, other_session, other_input, other_turn, 0x3_000).await;
    let judge_pass = insert_pass_for_target(
        &fixture.store,
        fixture.target,
        0x706,
        ReviewPassKind::Judge,
        other_session,
        other_input,
    )
    .await;
    let turn = TurnId::from_uuid(uuid(0x203));
    let (running_review, _) = start_review_pass(&fixture.store, fixture.pass).await;
    start_review_pass(&fixture.store, judge_pass).await;
    let output_frontier = complete_review_turn(&pool, turn).await;
    fail_review_turn(&pool, other_turn).await;
    let review_evidence =
        propose_read_only_success(&fixture.store, running_review, output_frontier).await;
    conclude_review_pass(
        &fixture.store,
        judge_pass,
        ReviewPassState::Failed { turn: other_turn },
    )
    .await;
    let finding_ref = ReviewFindingRef::new(fixture.pass, ReviewFindingId::from_uuid(uuid(0x707)));
    fixture
        .store
        .insert_finding(&finding(
            finding_ref,
            review_evidence,
            &fixture.target_snapshot,
        ))
        .await?;
    let failed_event = sqlx::query(
        "INSERT INTO review_finding_event
            (finding_id, event_ordinal, finding_run_id, target_id,
             event_pass_id, event_pass_run_id, event_kind, reason,
             referenced_finding_id, external_link_id,
             external_link_association_kind)
         VALUES ($1, 1, $2, $3, $4, $5, 'accepted', NULL, NULL, NULL, NULL)",
    )
    .bind(finding_ref.finding().into_uuid())
    .bind(fixture.run.run().into_uuid())
    .bind(fixture.target.into_uuid())
    .bind(judge_pass.pass().into_uuid())
    .bind(judge_pass.run().run().into_uuid())
    .execute(&pool)
    .await
    .expect_err("failed pass cannot author a completed event");
    assert_sqlstate(&failed_event, "23514");
    Ok(())
}

/// a findings receipt cannot omit its stable result count.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn findings_receipt_rejects_missing_count() -> Result<(), Box<dyn Error>> {
    const COMMAND_IDENTITY: u128 = 0x765;

    let (_container, pool) = migrated_postgres().await?;
    let error = sqlx::query(
        "INSERT INTO review_workflow_command
            (command_id, command_kind, storage_version, semantic_digest,
             operation_kind, result_kind, result_run_id, result_pass_id)
         VALUES ($1, 'review_workflow', 1, $2, 'record_findings',
                 'findings_recorded', $3, $4)",
    )
    .bind(uuid(COMMAND_IDENTITY))
    .bind([11_u8; 32].as_slice())
    .bind(uuid(0x766))
    .bind(uuid(0x767))
    .execute(&pool)
    .await
    .expect_err("the receipt shape requires a stable finding count");

    assert_sqlstate(&error, "23514");
    Ok(())
}

/// findings recovery compares immutable proposals after disposition.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn findings_receipt_recovers_after_later_disposition() -> Result<(), Box<dyn Error>> {
    const COMMAND_IDENTITY: u128 = 0x769;
    const JUDGE_PASS_IDENTITY: u128 = 0x76a;
    const FINDING_IDENTITY: u128 = 0x76b;

    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let judge_pass =
        insert_fixture_pass(&fixture, JUDGE_PASS_IDENTITY, ReviewPassKind::Judge).await;
    let evidence = succeed_fixture_passes(&pool, &fixture.store, &[fixture.pass, judge_pass]).await;
    let finding_ref = ReviewFindingRef::new(
        fixture.pass,
        ReviewFindingId::from_uuid(uuid(FINDING_IDENTITY)),
    );
    let producing_pass = pass_with_produced_findings(vec![finding_ref], evidence[0].clone());
    let recorded_finding = finding(
        finding_ref,
        producing_pass.clone(),
        &fixture.target_snapshot,
    );
    let recorded_findings = vec![recorded_finding];
    let finding_count = recorded_findings.len();
    fixture
        .store
        .insert_findings(&producing_pass, &recorded_findings)
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
        .await?
        .expect("the canonical finding accepts its disposition");
    let command = ReviewWorkflowCommand::new(
        DurableCommandId::from_uuid(uuid(COMMAND_IDENTITY)),
        [13; 32],
        ReviewWorkflowOperation::RecordFindings {
            pass: producing_pass,
            findings: recorded_findings,
        },
    );
    let expected =
        ReviewWorkflowCommandOutcome::Recorded(ReviewWorkflowCommandResult::FindingsRecorded {
            run: fixture.run.run(),
            pass: fixture.pass.pass(),
            finding_count,
        });

    let mut service = ReviewWorkflowCommandService::new(fixture.store);
    assert_eq!(service.execute(command.clone()).await?, expected);
    assert_eq!(service.execute(command).await?, expected);
    Ok(())
}
