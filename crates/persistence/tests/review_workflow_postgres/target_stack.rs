//! Target stack coverage.

use super::*;

/// a superseded event round-trips a successor from another sealed
/// producer run without rewriting either finding's ancestry.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn cross_run_superseded_retains_independent_ancestry() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let successor_pass =
        insert_fixture_pass(&fixture, 0x4332, ReviewPassKind::ReadOnlyReview).await;
    let dedupe_pass = insert_fixture_pass(&fixture, 0x4333, ReviewPassKind::Dedupe).await;
    let evidence = succeed_fixture_passes(
        &pool,
        &fixture.store,
        &[fixture.pass, successor_pass, dedupe_pass],
    )
    .await;
    let finding_ref = ReviewFindingRef::new(fixture.pass, ReviewFindingId::from_uuid(uuid(0x4330)));
    let successor_ref =
        ReviewFindingRef::new(successor_pass, ReviewFindingId::from_uuid(uuid(0x4331)));
    let review_evidence = pass_with_produced_findings(vec![finding_ref], evidence[0].clone());
    let successor_evidence = pass_with_produced_findings(vec![successor_ref], evidence[1].clone());
    let open = finding(
        finding_ref,
        review_evidence.clone(),
        &fixture.target_snapshot,
    );
    let successor = finding(
        successor_ref,
        successor_evidence.clone(),
        &fixture.target_snapshot,
    );
    fixture
        .store
        .insert_findings(&review_evidence, std::slice::from_ref(&open))
        .await?;
    fixture
        .store
        .insert_findings(&successor_evidence, std::slice::from_ref(&successor))
        .await?;
    let event = finding_event(
        finding_ref,
        ReviewEventOrdinal::one(),
        evidence[2].clone(),
        ReviewFindingEventKind::Superseded {
            successor: ReviewReferencedFindingEvidence::try_from_finding(&successor)
                .expect("open finding is eligible successor evidence"),
        },
    );
    let expected = open
        .apply(event.clone())
        .expect("dedupe pass may identify the successor finding");
    fixture
        .store
        .append_finding_event(finding_ref.finding(), event)
        .await?;

    assert_eq!(
        fixture.store.load_finding(finding_ref.finding()).await?,
        Some(expected)
    );
    Ok(())
}

/// raw reservation inserts cannot diverge from the canonical target
/// provider.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn reservation_requires_canonical_target_provider() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let forged = sqlx::query(
        "INSERT INTO review_external_link
            (external_link_id, target_id, association_kind, run_id,
             finding_id, finding_producing_pass_id, provider_key,
             object_kind)
         VALUES ($1, $2, 'target', NULL, NULL, NULL,
                 'another-code-host', 'review_comment')",
    )
    .bind(uuid(0x754))
    .bind(fixture.target.into_uuid())
    .execute(&pool)
    .await
    .expect_err("reservation provider must equal the canonical target provider");
    assert_sqlstate(&forged, "23514");
    Ok(())
}

/// PostgreSQL rejects a producing pass from another target/run edge.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn cross_wired_pass_ancestry_is_rejected() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let other_target = ReviewTargetId::from_uuid(uuid(0x401));
    fixture
        .store
        .insert_target(
            &ReviewTarget::try_new(
                other_target,
                key("example-code-host"),
                key("example/other-repository"),
                ReviewTargetSubject::Commit,
                key("abcdef0123456789"),
                Some(key("9876543210fedcba")),
                None,
            )
            .expect("other target topology is valid"),
        )
        .await
        .expect("other target persists");
    let other_run = ReviewRunRef::new(other_target, ReviewRunId::from_uuid(uuid(0x402)));
    fixture
        .store
        .insert_run(&ReviewRun::new(
            other_run,
            ReviewWorkflowKind::ReadOnlyReview,
            ReviewPolicy::version_one(),
        ))
        .await
        .expect("other run persists");

    let cross_wired = sqlx::query(
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
    .bind(uuid(0x403))
    .bind(other_run.run().into_uuid())
    .bind(other_target.into_uuid())
    .bind(fixture.pass.pass().into_uuid())
    .execute(&pool)
    .await
    .expect_err("producing pass must be canonical for the finding run and target");
    assert_sqlstate(&cross_wired, "23514");

    Ok(())
}

/// target loading reconstructs the complete stack ancestry, and the
/// schema rejects a logical change request repeated anywhere in that chain.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn target_stack_ancestry_is_complete_and_nonrepeating() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let store = ReviewWorkflowStore::new(pool.clone());
    let provider = key("example-code-host");
    let repository = key("example/repository");
    let root = ReviewTarget::try_new(
        ReviewTargetId::from_uuid(uuid(0x770)),
        provider.clone(),
        repository.clone(),
        ReviewTargetSubject::ChangeRequest(
            ReviewChangeRequestNumber::try_new(41).expect("positive change request"),
        ),
        key("root-head"),
        Some(key("root-base")),
        None,
    )
    .expect("root target topology is valid");
    let middle = ReviewTarget::try_new(
        ReviewTargetId::from_uuid(uuid(0x771)),
        provider.clone(),
        repository.clone(),
        ReviewTargetSubject::ChangeRequest(
            ReviewChangeRequestNumber::try_new(42).expect("positive change request"),
        ),
        key("middle-head"),
        Some(key("root-head")),
        Some(&root),
    )
    .expect("middle target topology is valid");
    let leaf = ReviewTarget::try_new(
        ReviewTargetId::from_uuid(uuid(0x772)),
        provider,
        repository,
        ReviewTargetSubject::ChangeRequest(
            ReviewChangeRequestNumber::try_new(43).expect("positive change request"),
        ),
        key("leaf-head"),
        Some(key("middle-head")),
        Some(&middle),
    )
    .expect("leaf target topology is valid");
    store.insert_target(&root).await?;
    store.insert_target(&middle).await?;
    store.insert_target(&leaf).await?;
    assert_eq!(store.load_target(leaf.id()).await?, Some(leaf.clone()));

    let repeated = sqlx::query(
        "INSERT INTO review_target
            (target_id, provider_key, repository_key, subject_kind,
             change_request_number, head_revision, base_revision,
             stack_parent_target_id)
         VALUES (
             $1, 'example-code-host', 'example/repository',
             'change_request', 41, 'repeat-head', 'leaf-head', $2
         )",
    )
    .bind(uuid(0x773))
    .bind(leaf.id().into_uuid())
    .execute(&pool)
    .await
    .expect_err("one logical change request cannot repeat in a stack chain");
    assert_sqlstate(&repeated, "23514");
    Ok(())
}

/// stack parents are confined to the target's provider and
/// repository.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn stack_parent_requires_same_repository() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let foreign_repository_parent = sqlx::query(
        "INSERT INTO review_target
            (target_id, provider_key, repository_key, subject_kind,
             change_request_number, head_revision, base_revision,
             stack_parent_target_id)
         VALUES (
             $1, 'example-code-host', 'example/other-repository',
             'commit', NULL, '1122334455667788',
             '0123456789abcdef', $2
         )",
    )
    .bind(uuid(0x701))
    .bind(fixture.target.into_uuid())
    .execute(&pool)
    .await
    .expect_err("stack parent must be in the target repository");
    assert_sqlstate(&foreign_repository_parent, "23514");
    Ok(())
}

/// a stack edge joins the child base to the canonical parent head.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn stack_parent_requires_canonical_revision() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let child = ReviewTarget::try_new(
        ReviewTargetId::from_uuid(uuid(0x801)),
        key("example-code-host"),
        key("example/repository"),
        ReviewTargetSubject::Commit,
        key("child-head"),
        Some(key("0123456789abcdef")),
        Some(&fixture.target_snapshot),
    )
    .expect("child base equals its canonical parent head");
    fixture.store.insert_target(&child).await?;
    assert_eq!(
        fixture.store.load_target(child.id()).await?,
        Some(child.clone())
    );

    let base_less = sqlx::query(
        "INSERT INTO review_target
            (target_id, provider_key, repository_key, subject_kind,
             change_request_number, head_revision, base_revision,
             stack_parent_target_id)
         VALUES (
             $1, 'example-code-host', 'example/repository',
             'commit', NULL, 'base-less-child', NULL, $2
         )",
    )
    .bind(uuid(0x802))
    .bind(fixture.target.into_uuid())
    .execute(&pool)
    .await
    .expect_err("a parented target must freeze an exact base revision");
    assert_sqlstate(&base_less, "23514");

    let disconnected = sqlx::query(
        "INSERT INTO review_target
            (target_id, provider_key, repository_key, subject_kind,
             change_request_number, head_revision, base_revision,
             stack_parent_target_id)
         VALUES (
             $1, 'example-code-host', 'example/repository',
             'commit', NULL, 'disconnected-child', 'unrelated-base', $2
         )",
    )
    .bind(uuid(0x803))
    .bind(fixture.target.into_uuid())
    .execute(&pool)
    .await
    .expect_err("a child base must equal its canonical parent head");
    assert_sqlstate(&disconnected, "23514");
    Ok(())
}

/// a frozen review target rejects in-place revision mutation.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn target_evidence_is_append_only() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let store = ReviewWorkflowStore::new(pool.clone());
    let target_id = ReviewTargetId::from_uuid(uuid(0x301));
    let target = ReviewTarget::try_new(
        target_id,
        key("example-code-host"),
        key("example/repository"),
        ReviewTargetSubject::Commit,
        key("0123456789abcdef"),
        None,
        None,
    )
    .expect("fixture target topology is valid");
    store.insert_target(&target).await.expect("target persists");

    let mutation = sqlx::query(
        "UPDATE review_target
            SET head_revision = 'different'
          WHERE target_id = $1",
    )
    .bind(target_id.into_uuid())
    .execute(&pool)
    .await
    .expect_err("target evidence is append-only");
    assert_sqlstate(&mutation, "23514");

    Ok(())
}

/// maximum-size target keys remain persistable without a wide-index
/// size failure.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn maximum_target_keys_do_not_overflow_indexes() -> Result<(), Box<dyn Error>> {
    const TARGET_IDENTITY: u128 = 0x750;

    let (_container, pool) = migrated_postgres().await?;
    let store = ReviewWorkflowStore::new(pool);
    let target = ReviewTarget::try_new(
        ReviewTargetId::from_uuid(uuid(TARGET_IDENTITY)),
        maximum_width_key(MaximumWidthKeyRole::Provider),
        maximum_width_key(MaximumWidthKeyRole::Repository),
        ReviewTargetSubject::Commit,
        maximum_width_key(MaximumWidthKeyRole::HeadRevision),
        Some(maximum_width_key(MaximumWidthKeyRole::BaseRevision)),
        None,
    )
    .expect("maximum-size target keys are admitted");

    store.insert_target(&target).await?;
    assert_eq!(store.load_target(target.id()).await?, Some(target));
    Ok(())
}
