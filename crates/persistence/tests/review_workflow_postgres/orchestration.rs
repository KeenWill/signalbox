//! Orchestration coverage.

use super::*;

/// Starts PostgreSQL with `pg_stat_statements` loaded so a test can count the
/// statements one call issues.
///
/// The count is taken server-side rather than by instrumenting a call site,
/// because the cost this guards against is spread across two crates: the
/// orchestration loaders and the workflow store they delegate to. A server-side
/// counter sees every statement either one issues, including any added later.
async fn migrated_postgres_counting_statements()
-> Result<(ContainerAsync<Postgres>, PgPool), Box<dyn Error>> {
    let container = Postgres::default()
        .with_db_name(DATABASE_NAME)
        .with_user(DATABASE_USER)
        .with_password(DATABASE_PASSWORD)
        // A later `with_cmd` replaces the whole command, so the statement
        // counter's settings extend the shared ephemeral-durability arguments
        // rather than following them in a second call.
        .with_cmd(disposable_postgres_server_args().into_iter().chain([
            "-c",
            "shared_preload_libraries=pg_stat_statements",
            "-c",
            "pg_stat_statements.track=all",
        ]))
        .with_mount(disposable_postgres_state_tmpfs_from_example()?)
        .with_tag(POSTGRES_IMAGE_TAG)
        .with_labels(disposable_test_container_labels())
        .start()
        .await?;
    let host = container.get_host().await?;
    let port = container.get_host_port_ipv4(5432).await?;
    let database_url =
        format!("postgres://{DATABASE_USER}:{DATABASE_PASSWORD}@{host}:{port}/{DATABASE_NAME}");
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect_with(local_test_connection_options(&database_url)?)
        .await?;
    sqlx::query("CREATE EXTENSION IF NOT EXISTS pg_stat_statements")
        .execute(&pool)
        .await?;
    migrate(&pool).await?;
    Ok((container, pool))
}

/// Runs `work` and reports how many statements PostgreSQL executed for it.
///
/// The reset and the reading query are both excluded by name, and the reading
/// query has not yet been recorded when it computes its own sum.
async fn statements_executed<Work: Future<Output = Output>, Output>(
    pool: &PgPool,
    work: Work,
) -> Result<(Output, i64), Box<dyn Error>> {
    sqlx::query("SELECT pg_stat_statements_reset()")
        .execute(pool)
        .await?;
    let output = work.await;
    let executed = sqlx::query_scalar::<_, i64>(
        "SELECT COALESCE(sum(calls), 0)::bigint
           FROM pg_stat_statements
          WHERE query NOT LIKE '%pg_stat_statements%'",
    )
    .fetch_one(pool)
    .await?;
    Ok((output, executed))
}

#[track_caller]
fn expect_new_orchestration_claim(
    claim: ReviewOrchestrationCommandClaim,
) -> ReviewOrchestrationCommandGuard {
    match claim {
        ReviewOrchestrationCommandClaim::New(guard) => {
            assert!(!guard.is_pending());
            guard
        }
        ReviewOrchestrationCommandClaim::ExistingRecorded(_) => {
            panic!("fresh orchestration command unexpectedly replayed")
        }
        ReviewOrchestrationCommandClaim::Conflicting => {
            panic!("fresh orchestration command unexpectedly conflicted")
        }
    }
}

#[track_caller]
fn expect_pending_orchestration_claim(
    claim: ReviewOrchestrationCommandClaim,
) -> ReviewOrchestrationCommandGuard {
    match claim {
        ReviewOrchestrationCommandClaim::New(guard) => {
            assert!(guard.is_pending());
            guard
        }
        ReviewOrchestrationCommandClaim::ExistingRecorded(_) => {
            panic!("pending orchestration command unexpectedly replayed")
        }
        ReviewOrchestrationCommandClaim::Conflicting => {
            panic!("pending orchestration command unexpectedly conflicted")
        }
    }
}

#[track_caller]
fn expect_recorded_orchestration_claim(
    claim: ReviewOrchestrationCommandClaim,
) -> ReviewOrchestrationCommandResult {
    match claim {
        ReviewOrchestrationCommandClaim::ExistingRecorded(result) => result,
        ReviewOrchestrationCommandClaim::New(_) => {
            panic!("recorded orchestration command unexpectedly acquired a new fence")
        }
        ReviewOrchestrationCommandClaim::Conflicting => {
            panic!("recorded orchestration command unexpectedly conflicted")
        }
    }
}

struct PreparedOrchestrationFixture {
    workflow: ReviewWorkflowStore,
    store: PostgresReviewOrchestrationStore,
    attempt_id: ReviewOrchestrationAttemptId,
    attempt: ReviewOrchestrationAttempt,
    import: ReviewImportOutcome,
    claim: ReviewConcernClaim,
    plan: ReviewJudgmentPlan,
    finding_ref: ReviewFindingRef,
    evidence: Vec<ReviewPassEvidence>,
}

async fn prepare_orchestration_fixture(
    pool: &PgPool,
) -> Result<PreparedOrchestrationFixture, Box<dyn Error>> {
    prepare_orchestration_fixture_with_findings(pool, 1).await
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn schema_requires_scored_categorical_judgments() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = prepare_orchestration_fixture(&pool).await?;
    let original: serde_json::Value = sqlx::query_scalar(
        "SELECT judgment FROM review_orchestration_judgment_member WHERE attempt_id = $1",
    )
    .bind(fixture.attempt_id.as_uuid())
    .fetch_one(&pool)
    .await?;
    let mut missing_confidence = original.clone();
    missing_confidence
        .as_object_mut()
        .expect("judgment object")
        .remove("confidence");
    let mut out_of_range = original.clone();
    out_of_range["confidence"] = serde_json::json!(6);
    let mut missing_category = original.clone();
    missing_category["bar_category"] = serde_json::Value::Null;
    let mut contradictory = original.clone();
    contradictory["bar_category"] = serde_json::json!("none");
    contradictory["decline_class"] = serde_json::json!("duplicate");
    sqlx::query("ALTER TABLE review_orchestration_judgment_member DISABLE TRIGGER USER")
        .execute(&pool)
        .await?;
    for invalid in [
        serde_json::Value::Null,
        missing_confidence,
        out_of_range,
        missing_category,
        contradictory,
    ] {
        let error = sqlx::query(
            "UPDATE review_orchestration_judgment_member SET judgment = $2 WHERE attempt_id = $1",
        )
        .bind(fixture.attempt_id.as_uuid())
        .bind(sqlx::types::Json(invalid))
        .execute(&pool)
        .await
        .expect_err("the required categorical shape cannot be bypassed through SQL");
        assert_eq!(
            error
                .as_database_error()
                .and_then(|error| error.constraint()),
            Some("review_orchestration_judgment_shape")
        );
    }
    sqlx::query("ALTER TABLE review_orchestration_judgment_member ENABLE TRIGGER USER")
        .execute(&pool)
        .await?;
    assert_eq!(
        fixture.store.load_judgment_plan(fixture.attempt_id).await?,
        Some(fixture.plan)
    );
    Ok(())
}

/// Prepares one sealed attempt carrying `findings` findings on a single target.
///
/// Every finding shares the fixture's target, which is what makes the count a
/// meaningful load parameter: the workflow store reconstructs findings by whole
/// target graph, so a loader that reaches for one finding at a time scales with
/// the product of the claim's members and the target's.
async fn prepare_orchestration_fixture_with_findings(
    pool: &PgPool,
    findings: usize,
) -> Result<PreparedOrchestrationFixture, Box<dyn Error>> {
    const IMPORT_PASS_IDENTITY: u128 = 0x7a0;
    const ANALYSIS_PASS_IDENTITY: u128 = 0x7a1;
    const EFFECT_PASS_IDENTITY: u128 = 0x7a2;
    const FIX_PASS_IDENTITY: u128 = 0x7a3;
    const FINDING_IDENTITY: u128 = 0x7a4;
    const ATTEMPT_IDENTITY: u128 = 0x7a5;
    const ADDITIONAL_FINDING_IDENTITY_BASE: u128 = 0x7c0;

    let fixture = insert_review_pass_fixture(pool).await;
    let import_pass = insert_fixture_pass(
        &fixture,
        IMPORT_PASS_IDENTITY,
        ReviewPassKind::ImportExternalContext,
    )
    .await;
    let analysis_pass =
        insert_fixture_pass(&fixture, ANALYSIS_PASS_IDENTITY, ReviewPassKind::Judge).await;
    let effect_pass =
        insert_fixture_pass(&fixture, EFFECT_PASS_IDENTITY, ReviewPassKind::Judge).await;
    let fix_pass = insert_fixture_pass(&fixture, FIX_PASS_IDENTITY, ReviewPassKind::Fix).await;
    let evidence = succeed_fixture_passes(
        pool,
        &fixture.store,
        &[
            fixture.pass,
            import_pass,
            analysis_pass,
            effect_pass,
            fix_pass,
        ],
    )
    .await;
    let finding_refs = (0..findings)
        .map(|index| {
            let identity = if index == 0 {
                FINDING_IDENTITY
            } else {
                ADDITIONAL_FINDING_IDENTITY_BASE + u128::try_from(index).unwrap_or_default()
            };
            ReviewFindingRef::new(fixture.pass, ReviewFindingId::from_uuid(uuid(identity)))
        })
        .collect::<Vec<_>>();
    let finding_ref = *finding_refs
        .first()
        .expect("the fixture carries at least one finding");
    let producer = pass_with_produced_findings(finding_refs.clone(), evidence[0].clone());
    let proposed = finding_refs
        .iter()
        .map(|reference| finding(*reference, producer.clone(), &fixture.target_snapshot))
        .collect::<Vec<_>>();
    fixture.store.insert_findings(&producer, &proposed).await?;
    let mut canonical_findings = Vec::with_capacity(finding_refs.len());
    for reference in &finding_refs {
        canonical_findings.push(
            fixture
                .store
                .load_finding(reference.finding())
                .await?
                .expect("canonical finding exists"),
        );
    }

    let attempt_id = ReviewOrchestrationAttemptId::from_uuid(uuid(ATTEMPT_IDENTITY));
    let attempt = ReviewOrchestrationAttempt::try_new(
        attempt_id,
        fixture.target,
        ReviewPolicy::version_one(),
        key("initial-five-v1"),
        ReviewStageTemplateDigests::new(
            ReviewTemplateDigest::new([1; 32]),
            ReviewTemplateDigest::new([2; 32]),
            ReviewTemplateDigest::new([3; 32]),
            ReviewTemplateDigest::new([4; 32]),
        ),
        vec![ReviewConcernSpec::new(
            key("correctness"),
            ReviewTemplateDigest::new([5; 32]),
        )],
    )?;
    let import = ReviewImportOutcome::Succeeded {
        pass: Box::new(evidence[1].clone()),
        run: run_evidence_for_pass(evidence[1].clone()),
        external_link: None,
        template_digest: ReviewTemplateDigest::new([1; 32]),
        context: ReviewImportedContextEvidence::new(import_pass, [6; 32]),
    };
    let claim = ReviewConcernClaim::new(
        key("correctness"),
        ReviewTemplateDigest::new([5; 32]),
        ReviewConcernOutcome::Succeeded(Box::new(ReviewConcernSuccess::new(
            producer,
            run_evidence_for_pass(evidence[0].clone()),
            ReviewTemplateDigest::new([5; 32]),
            canonical_findings,
        ))),
    );
    let plan = ReviewJudgmentPlan::new(
        evidence[2].clone(),
        run_evidence_for_pass(evidence[2].clone()),
        ReviewTemplateDigest::new([2; 32]),
        finding_refs
            .iter()
            .map(|reference| {
                ReviewJudgmentPlanMember::new(
                    *reference,
                    ReviewPlannedDisposition::Accepted,
                    signalbox_domain::ReviewJudgment::new(
                        signalbox_domain::ReviewBarVerdict::Accept(
                            signalbox_domain::ReviewBarCategory::OwnBehaviorDefect,
                        ),
                        signalbox_domain::ReviewJudgeConfidence::try_new(5)
                            .expect("judge confidence"),
                        text("The fixture supplies concrete evidence."),
                    ),
                )
            })
            .collect(),
    );
    let mut store = PostgresReviewOrchestrationStore::new(pool.clone());
    assert_eq!(
        store.record_attempt(attempt.clone()).await?,
        ReviewDurableSealOutcome::Recorded
    );
    assert_eq!(
        store.record_import(attempt_id, import.clone()).await?,
        ReviewDurableSealOutcome::Recorded
    );
    assert_eq!(
        store
            .record_concern_claim(attempt_id, claim.clone())
            .await?,
        ReviewDurableSealOutcome::Recorded
    );
    assert_eq!(
        store
            .seal_complete_fanout(attempt_id, vec![claim.clone()])
            .await?,
        ReviewDurableSealOutcome::Recorded
    );
    assert_eq!(
        store.seal_judgment_plan(attempt_id, plan.clone()).await?,
        ReviewDurableSealOutcome::Recorded
    );
    Ok(PreparedOrchestrationFixture {
        workflow: fixture.store,
        store,
        attempt_id,
        attempt,
        import,
        claim,
        plan,
        finding_ref,
        evidence,
    })
}

fn orchestration_command(
    fixture: &PreparedOrchestrationFixture,
    identity: u128,
    digest: [u8; 32],
) -> ReviewOrchestrationCommand {
    ReviewOrchestrationCommand {
        command_id: DurableCommandId::from_uuid(uuid(identity)),
        semantic_digest: digest,
        attempt: fixture.attempt_id,
        kind: ReviewOrchestrationCommandKind::JudgmentEffect,
    }
}

async fn incomplete_judgment_result(
    fixture: &PreparedOrchestrationFixture,
) -> Result<ReviewOrchestrationCommandResult, ReviewOrchestrationStoreError> {
    Ok(ReviewOrchestrationCommandResult {
        attempt: fixture.attempt_id,
        stage: ReviewOrchestrationStage::JudgmentIncomplete,
        progress: fixture
            .store
            .load_progress(fixture.attempt_id)
            .await?
            .expect("planned judgment has durable progress"),
    })
}

/// A snapshot reports one database snapshot, not a seam between several.
///
/// The whole projection is reconstructed inside a single read-only
/// `REPEATABLE READ` transaction, so its facts are all drawn from the instant
/// the transaction took its snapshot. This forces the interleave that a torn
/// read needs and shows it cannot happen: the snapshot is stopped after its
/// first read has fixed its MVCC snapshot, a writer then commits an effect, and
/// the snapshot is released to run every remaining loader. Those later loaders
/// must still report the pre-write state.
///
/// The stop is a table lock on `review_orchestration_import`, which the stage
/// ladder reads immediately after the attempt row. It is deliberately not a
/// timing delay: the writer commits only once `pg_stat_activity` shows the
/// reader blocked, so the ordering is observed rather than assumed.
///
/// Before the read became one transaction, each loader opened its own — so the
/// effect written here landed between them and the snapshot reported it.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn review_orchestration_snapshot_excludes_a_write_committed_after_it_began()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres_with_max_connections(6).await?;
    let fixture = prepare_orchestration_fixture(&pool).await?;

    let mut blocker = pool.begin().await?;
    sqlx::query("LOCK TABLE review_orchestration_import IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *blocker)
        .await?;

    let reader = fixture.store.clone();
    let attempt_id = fixture.attempt_id;
    let reading = tokio::spawn(async move { reader.load_snapshot(attempt_id).await });
    assert!(
        blocked_backends_reached(&pool, 1).await?,
        "the snapshot must be waiting on the held import lock before the writer commits"
    );

    let mut writer = PostgresReviewOrchestrationStore::new(pool.clone());
    assert_eq!(
        writer
            .record_applied_judgment_effect(ReviewJudgmentEffectId::new(
                attempt_id,
                fixture.finding_ref,
            ))
            .await?,
        ReviewDurableSealOutcome::Recorded,
        "the concurrent writer must commit while the snapshot is still blocked"
    );

    blocker.rollback().await?;
    let facts = tokio::time::timeout(std::time::Duration::from_secs(60), reading)
        .await???
        .expect("the sealed attempt has a snapshot");

    assert!(
        facts.applied_judgment_effects.is_empty(),
        "the snapshot reported an effect committed after it began: {:?}",
        facts.applied_judgment_effects
    );
    assert_eq!(
        facts.current_stage,
        ReviewOrchestrationCurrentStage::AwaitingJudgmentEffects,
        "the reported stage must agree with the effects the same snapshot reports"
    );

    // The write is durable; only this snapshot's view of it was fixed earlier.
    assert_eq!(
        fixture.store.current_stage(attempt_id).await?,
        Some(ReviewOrchestrationCurrentStage::AwaitingRepair)
    );
    Ok(())
}

/// A snapshot under construction blocks no writer.
///
/// An import-table lock pauses `load_snapshot` mid-construction while
/// `insert_active_turn_with_offset` writes `accepted_input` and `turn_lifecycle`.
/// Completing that write before the timeout proves the snapshot blocks no writer.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn review_orchestration_snapshot_does_not_block_input_or_turns() -> Result<(), Box<dyn Error>>
{
    const WRITER_SESSION: u128 = 0x7e0;
    const WRITER_INPUT: u128 = 0x7e1;
    const WRITER_TURN: u128 = 0x7e2;
    const WRITER_OFFSET: u128 = 0x7e3;

    let (_container, pool) = migrated_postgres_with_max_connections(6).await?;
    let fixture = prepare_orchestration_fixture(&pool).await?;

    let mut blocker = pool.begin().await?;
    sqlx::query("LOCK TABLE review_orchestration_import IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *blocker)
        .await?;

    let reader = fixture.store.clone();
    let attempt_id = fixture.attempt_id;
    let reading = tokio::spawn(async move { reader.load_snapshot(attempt_id).await });
    assert!(
        blocked_backends_reached(&pool, 1).await?,
        "the snapshot must be mid-construction before the writer runs"
    );

    tokio::time::timeout(
        std::time::Duration::from_secs(20),
        insert_active_turn_with_offset(
            &pool,
            SessionId::from_uuid(uuid(WRITER_SESSION)),
            AcceptedInputId::from_uuid(uuid(WRITER_INPUT)),
            TurnId::from_uuid(uuid(WRITER_TURN)),
            WRITER_OFFSET,
        ),
    )
    .await
    .expect("a snapshot under construction must not delay input submission or turn start");

    blocker.rollback().await?;
    tokio::time::timeout(std::time::Duration::from_secs(60), reading)
        .await???
        .expect("the sealed attempt has a snapshot");
    Ok(())
}

/// The snapshot holds exactly one pooled connection for its whole construction.
///
/// A single connection is a single transaction, which is what makes the read
/// coherent under MVCC rather than under locks. Pinning it against a
/// one-connection pool states that structurally: a construction that reached
/// for a second connection — one holding a guard transaction while its loaders
/// ran elsewhere — could not finish here at all.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn review_orchestration_snapshot_completes_on_a_single_connection()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres_with_max_connections(1).await?;
    let fixture = prepare_orchestration_fixture(&pool).await?;

    let facts = tokio::time::timeout(
        std::time::Duration::from_secs(60),
        fixture.store.load_snapshot(fixture.attempt_id),
    )
    .await
    .expect("a one-connection pool must satisfy the snapshot")?
    .expect("the sealed attempt has a snapshot");

    assert_eq!(
        facts.current_stage,
        ReviewOrchestrationCurrentStage::AwaitingJudgmentEffects
    );
    Ok(())
}

/// Snapshot cost grows linearly, not quadratically, in an attempt's findings.
///
/// Two independent defects once made it quadratic. The stage ladder
/// independently re-ran nearly every loader the snapshot ran again on the next
/// line, and each concern finding was fetched with a loader that reconstructs
/// the whole target finding graph in order to return a single row — so the
/// claim's members multiplied the target's. The wire contract admits 1,024
/// findings, where a quadratic is not a constant factor.
///
/// The marginal assertion is the load-bearing one: an absolute ceiling can be
/// met by a quadratic on a small fixture, but a per-finding bound cannot. Under
/// either defect the marginal cost is itself proportional to the finding count,
/// so it fails whichever one returns.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn review_orchestration_snapshot_cost_is_linear_in_findings() -> Result<(), Box<dyn Error>> {
    /// Statements one additional finding on the attempt may add.
    ///
    /// A finding costs one projection plus its event and external-link reads.
    /// Measured marginal cost is 8; the budget carries headroom over that and
    /// stays far below what a per-finding target-graph reconstruction needs.
    /// For scale, the same two fixtures measured 77 and 125 statements here
    /// against 172 and 1,180 before this shape was fixed.
    const STATEMENTS_PER_ADDITIONAL_FINDING: i64 = 10;
    const FEW_FINDINGS: usize = 2;
    const MANY_FINDINGS: usize = 8;

    let (_small_container, small_pool) = migrated_postgres_counting_statements().await?;
    let small = prepare_orchestration_fixture_with_findings(&small_pool, FEW_FINDINGS).await?;
    let (small_snapshot, small_statements) =
        statements_executed(&small_pool, small.store.load_snapshot(small.attempt_id)).await?;
    assert!(
        small_snapshot?.is_some(),
        "the sealed attempt must produce a snapshot"
    );

    let (_large_container, large_pool) = migrated_postgres_counting_statements().await?;
    let large = prepare_orchestration_fixture_with_findings(&large_pool, MANY_FINDINGS).await?;
    let (large_snapshot, large_statements) =
        statements_executed(&large_pool, large.store.load_snapshot(large.attempt_id)).await?;
    assert!(
        large_snapshot?.is_some(),
        "the sealed attempt must produce a snapshot"
    );

    let additional_findings = i64::try_from(MANY_FINDINGS - FEW_FINDINGS)?;
    let budget = small_statements + additional_findings * STATEMENTS_PER_ADDITIONAL_FINDING;
    assert!(
        large_statements <= budget,
        "a {MANY_FINDINGS}-finding snapshot executed {large_statements} statements against a \
         {FEW_FINDINGS}-finding baseline of {small_statements}, over the linear budget of \
         {budget}; the snapshot is scaling faster than its findings"
    );
    Ok(())
}

/// Complete stage seals reconstruct one coherent orchestration snapshot.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn review_orchestration_store_reconstructs_complete_attempt() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let mut fixture = prepare_orchestration_fixture(&pool).await?;
    let accepted = fixture
        .workflow
        .append_finding_event(
            fixture.finding_ref.finding(),
            finding_event(
                fixture.finding_ref,
                ReviewEventOrdinal::one(),
                fixture.evidence[3].clone(),
                ReviewFindingEventKind::Accepted {
                    confidence: signalbox_domain::ReviewJudgeConfidence::try_new(5)
                        .expect("judge confidence"),
                },
            ),
        )
        .await?
        .expect("accepted finding exists");
    let fixed = fixture
        .workflow
        .append_finding_event(
            fixture.finding_ref.finding(),
            finding_event(
                fixture.finding_ref,
                ReviewEventOrdinal::try_new(2).expect("two is positive"),
                fixture.evidence[4].clone(),
                ReviewFindingEventKind::Fixed,
            ),
        )
        .await?
        .expect("fixed finding exists");
    assert_eq!(accepted.status(), ReviewFindingStatus::Accepted);
    let repair = ReviewRepairMemberOutcome::Fixed(Box::new(ReviewRepairSuccess::new(
        fixed.events()[1].clone(),
        ReviewTemplateDigest::new([3; 32]),
    )));
    let applied_effect = ReviewJudgmentEffectId::new(fixture.attempt_id, fixture.finding_ref);
    assert_eq!(
        fixture
            .store
            .record_applied_judgment_effect(applied_effect)
            .await?,
        ReviewDurableSealOutcome::Recorded
    );
    assert_eq!(
        fixture.store.current_stage(fixture.attempt_id).await?,
        Some(ReviewOrchestrationCurrentStage::AwaitingRepair)
    );
    assert_eq!(
        fixture
            .store
            .seal_repair_inventory(fixture.attempt_id, vec![fixture.finding_ref])
            .await?,
        ReviewDurableSealOutcome::Recorded
    );
    assert_eq!(
        fixture
            .store
            .record_repair_outcomes(fixture.attempt_id, vec![repair.clone()])
            .await?,
        ReviewDurableSealOutcome::Recorded
    );
    assert_eq!(
        fixture
            .store
            .seal_publication_inventory(fixture.attempt_id, Vec::new())
            .await?,
        ReviewDurableSealOutcome::Recorded
    );
    assert_eq!(
        fixture
            .store
            .record_publication_outcomes(fixture.attempt_id, Vec::new())
            .await?,
        ReviewDurableSealOutcome::Recorded
    );
    let snapshot = fixture
        .store
        .load_snapshot(fixture.attempt_id)
        .await?
        .expect("completed attempt has a coherent snapshot");

    assert_eq!(snapshot.attempt, fixture.attempt);
    assert_eq!(
        snapshot.current_stage,
        ReviewOrchestrationCurrentStage::Complete
    );
    assert_eq!(snapshot.concern_claims, vec![fixture.claim]);
    assert_eq!(snapshot.judgment_plan, Some(fixture.plan));
    assert_eq!(snapshot.applied_judgment_effects, vec![applied_effect]);
    assert_eq!(snapshot.repair_outcomes, Some(vec![repair]));
    assert_eq!(snapshot.publication_outcomes, Some(Vec::new()));
    Ok(())
}

/// Equal stage seals replay without changing immutable attempt facts.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn review_orchestration_store_replays_equal_stage_seals() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let mut fixture = prepare_orchestration_fixture(&pool).await?;

    assert_eq!(
        fixture.store.record_attempt(fixture.attempt).await?,
        ReviewDurableSealOutcome::EqualReplay
    );
    assert_eq!(
        fixture
            .store
            .record_import(fixture.attempt_id, fixture.import)
            .await?,
        ReviewDurableSealOutcome::EqualReplay
    );
    assert_eq!(
        fixture
            .store
            .seal_complete_fanout(fixture.attempt_id, vec![fixture.claim])
            .await?,
        ReviewDurableSealOutcome::EqualReplay
    );
    assert_eq!(
        fixture
            .store
            .seal_judgment_plan(fixture.attempt_id, fixture.plan)
            .await?,
        ReviewDurableSealOutcome::EqualReplay
    );
    Ok(())
}

/// A recovery-only interrupted judgment result remains visible to current-stage
/// and coherent-snapshot reconstruction.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn review_orchestration_recovery_preserves_interrupted_judgment_state()
-> Result<(), Box<dyn Error>> {
    const COMMAND_IDENTITY: u128 = 0x7a9;

    let (_container, pool) = migrated_postgres().await?;
    let fixture = prepare_orchestration_fixture(&pool).await?;
    let command = orchestration_command(&fixture, COMMAND_IDENTITY, [10; 32]);
    let result = incomplete_judgment_result(&fixture).await?;
    let guard = expect_new_orchestration_claim(fixture.store.begin_command(command).await?);
    assert_eq!(
        fixture
            .store
            .record_command_recovery(command, result.clone())
            .await?,
        result
    );
    drop(guard);

    assert_eq!(
        fixture.store.current_stage(fixture.attempt_id).await?,
        Some(ReviewOrchestrationCurrentStage::JudgmentIncomplete)
    );
    assert_eq!(
        fixture
            .store
            .load_snapshot(fixture.attempt_id)
            .await?
            .expect("interrupted attempt has a coherent snapshot")
            .current_stage,
        ReviewOrchestrationCurrentStage::JudgmentIncomplete
    );
    Ok(())
}

/// A recovery-only orchestration command reserves its user-global identity and
/// its exact retry materializes the typed receipt.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn review_orchestration_recovery_reserves_global_command_identity()
-> Result<(), Box<dyn Error>> {
    const COMMAND_IDENTITY: u128 = 0x7aa;
    const FOREIGN_TARGET_IDENTITY: u128 = 0x7ab;

    let (_container, pool) = migrated_postgres().await?;
    let fixture = prepare_orchestration_fixture(&pool).await?;
    let command = orchestration_command(&fixture, COMMAND_IDENTITY, [11; 32]);
    let result = incomplete_judgment_result(&fixture).await?;
    let guard = expect_new_orchestration_claim(fixture.store.begin_command(command).await?);
    assert_eq!(
        fixture
            .store
            .record_command_recovery(command, result.clone())
            .await?,
        result
    );
    drop(guard);
    let foreign_target = ReviewTarget::try_new(
        ReviewTargetId::from_uuid(uuid(FOREIGN_TARGET_IDENTITY)),
        key("provider"),
        key("foreign-repository"),
        ReviewTargetSubject::Commit,
        key("foreign-head"),
        None,
        None,
    )
    .expect("foreign target fixture is valid");
    let foreign_command = ReviewWorkflowCommand::new(
        command.command_id,
        [12; 32],
        ReviewWorkflowOperation::CreateTarget(foreign_target.clone()),
    );
    let mut workflow_commands = ReviewWorkflowCommandService::new(fixture.workflow.clone());

    assert_eq!(
        workflow_commands.execute(foreign_command).await?,
        ReviewWorkflowCommandOutcome::ConflictingReuse {
            command_id: command.command_id
        }
    );
    assert_eq!(
        fixture.workflow.load_target(foreign_target.id()).await?,
        None
    );
    assert_eq!(
        expect_recorded_orchestration_claim(fixture.store.begin_command(command).await?),
        result
    );
    Ok(())
}

/// A pending orchestration intent immediately rejects a cross-kind registry row.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn review_orchestration_intent_blocks_cross_kind_registry_insert()
-> Result<(), Box<dyn Error>> {
    const COMMAND_IDENTITY: u128 = 0x7af;

    let (_container, pool) = migrated_postgres().await?;
    let fixture = prepare_orchestration_fixture(&pool).await?;
    let command = orchestration_command(&fixture, COMMAND_IDENTITY, [18; 32]);
    let result = incomplete_judgment_result(&fixture).await?;
    let guard = expect_new_orchestration_claim(fixture.store.begin_command(command).await?);
    let mut transaction = pool.begin().await?;
    let error = sqlx::query(
        "INSERT INTO durable_command
            (command_id, command_kind, storage_version, claimed_at, issuer_kind)
         VALUES ($1, 'review_workflow', 1, transaction_timestamp(), 'operator')",
    )
    .bind(command.command_id.into_uuid())
    .execute(&mut *transaction)
    .await
    .expect_err("intent already reserves the user-global command identity");
    assert_sqlstate(&error, "23505");
    transaction.rollback().await?;
    drop(guard);

    assert_eq!(
        fixture
            .store
            .record_command_recovery(command, result.clone())
            .await?,
        result
    );
    assert_eq!(
        expect_recorded_orchestration_claim(fixture.store.begin_command(command).await?),
        result
    );
    Ok(())
}

/// A committed orchestration receipt replays exactly and conflicting semantic
/// reuse is rejected.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn review_orchestration_command_receipt_replays_and_conflicts() -> Result<(), Box<dyn Error>>
{
    const COMMAND_IDENTITY: u128 = 0x7ac;

    let (_container, pool) = migrated_postgres().await?;
    let fixture = prepare_orchestration_fixture(&pool).await?;
    let command = orchestration_command(&fixture, COMMAND_IDENTITY, [13; 32]);
    let result = incomplete_judgment_result(&fixture).await?;
    let guard = expect_new_orchestration_claim(fixture.store.begin_command(command).await?);
    assert_eq!(guard.record(result.clone()).await?, result);

    assert_eq!(
        expect_recorded_orchestration_claim(fixture.store.begin_command(command).await?),
        result
    );
    assert!(matches!(
        fixture
            .store
            .begin_command(ReviewOrchestrationCommand {
                semantic_digest: [14; 32],
                ..command
            })
            .await?,
        ReviewOrchestrationCommandClaim::Conflicting
    ));
    Ok(())
}

/// An equal concurrent orchestration claim observes the durable pending intent and replays.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn review_orchestration_equal_command_claim_resumes_pending_and_replays()
-> Result<(), Box<dyn Error>> {
    const COMMAND_IDENTITY: u128 = 0x7ad;

    let (_container, pool) = migrated_postgres().await?;
    let fixture = prepare_orchestration_fixture(&pool).await?;
    let command = orchestration_command(&fixture, COMMAND_IDENTITY, [15; 32]);
    let result = incomplete_judgment_result(&fixture).await?;
    let winner = expect_new_orchestration_claim(fixture.store.begin_command(command).await?);
    let contender = expect_pending_orchestration_claim(fixture.store.begin_command(command).await?);
    assert_eq!(winner.record(result.clone()).await?, result);
    assert_eq!(contender.record(result.clone()).await?, result);
    Ok(())
}

/// A conflicting command immediately rejects against the durable pending intent.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn review_orchestration_conflicting_command_rejects_pending_intent()
-> Result<(), Box<dyn Error>> {
    const COMMAND_IDENTITY: u128 = 0x7ae;

    let (_container, pool) = migrated_postgres().await?;
    let fixture = prepare_orchestration_fixture(&pool).await?;
    let command = orchestration_command(&fixture, COMMAND_IDENTITY, [16; 32]);
    let result = incomplete_judgment_result(&fixture).await?;
    let winner = expect_new_orchestration_claim(fixture.store.begin_command(command).await?);
    let conflicting = ReviewOrchestrationCommand {
        semantic_digest: [17; 32],
        ..command
    };
    assert!(matches!(
        fixture.store.begin_command(conflicting).await?,
        ReviewOrchestrationCommandClaim::Conflicting
    ));
    assert_eq!(winner.record(result).await?.attempt, fixture.attempt_id);
    Ok(())
}

/// An existing attempt identity rejects different immutable frozen input.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn review_orchestration_attempt_rejects_conflicting_frozen_input()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let mut fixture = prepare_orchestration_fixture(&pool).await?;
    let conflicting_attempt = ReviewOrchestrationAttempt::try_new(
        fixture.attempt_id,
        fixture.attempt.target(),
        ReviewPolicy::version_one(),
        key("different-version"),
        fixture.attempt.stage_templates(),
        fixture.attempt.concerns().to_vec(),
    )?;

    assert_eq!(
        fixture.store.record_attempt(conflicting_attempt).await?,
        ReviewDurableSealOutcome::Conflict
    );
    Ok(())
}

/// Concurrent coherent snapshots stay within a configured two-connection pool
/// and the database's matching hard connection limit.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn review_orchestration_snapshots_respect_configured_connection_capacity()
-> Result<(), Box<dyn Error>> {
    const ATTEMPT_IDENTITY: u128 = 0x7b0;

    let (_container, pool) = migrated_postgres_with_max_connections(2).await?;
    sqlx::query("ALTER ROLE signalbox CONNECTION LIMIT 2")
        .execute(&pool)
        .await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let attempt_id = ReviewOrchestrationAttemptId::from_uuid(uuid(ATTEMPT_IDENTITY));
    let attempt = ReviewOrchestrationAttempt::try_new(
        attempt_id,
        fixture.target,
        ReviewPolicy::version_one(),
        key("capacity-proof-v1"),
        ReviewStageTemplateDigests::new(
            ReviewTemplateDigest::new([11; 32]),
            ReviewTemplateDigest::new([12; 32]),
            ReviewTemplateDigest::new([13; 32]),
            ReviewTemplateDigest::new([14; 32]),
        ),
        vec![ReviewConcernSpec::new(
            key("correctness"),
            ReviewTemplateDigest::new([15; 32]),
        )],
    )?;
    let mut store = PostgresReviewOrchestrationStore::new(pool);
    assert_eq!(
        store.record_attempt(attempt.clone()).await?,
        ReviewDurableSealOutcome::Recorded
    );
    let first_store = store.clone();
    let second_store = store;
    let (first, second) = tokio::time::timeout(std::time::Duration::from_secs(10), async move {
        tokio::join!(
            first_store.load_snapshot(attempt_id),
            second_store.load_snapshot(attempt_id)
        )
    })
    .await
    .expect("two admitted snapshots finish within configured capacity");
    let expected_stage = ReviewOrchestrationCurrentStage::AwaitingImport;
    assert_eq!(first?.expect("first snapshot exists").attempt, attempt);
    assert_eq!(
        second?.expect("second snapshot exists").current_stage,
        expected_stage
    );
    Ok(())
}

/// Records an unsealed concern inventory for the supplied canonical target.
async fn unsealed_concern_attempt(
    pool: &PgPool,
    target: ReviewTargetId,
) -> Result<(PostgresReviewOrchestrationStore, ReviewOrchestrationAttempt), Box<dyn Error>> {
    const FIXTURE_TEMPLATE_DIGEST: [u8; 32] = [1; 32];
    const FIXTURE_CONCERN: &str = "terminal-evidence";
    const FIXTURE_CONCERN_SET: &str = "terminal-evidence-v1";
    let digest = ReviewTemplateDigest::new(FIXTURE_TEMPLATE_DIGEST);
    let attempt = ReviewOrchestrationAttempt::try_new(
        ReviewOrchestrationAttemptId::from_uuid(Uuid::now_v7()),
        target,
        ReviewPolicy::version_one(),
        key(FIXTURE_CONCERN_SET),
        ReviewStageTemplateDigests::new(digest, digest, digest, digest),
        vec![ReviewConcernSpec::new(key(FIXTURE_CONCERN), digest)],
    )?;
    let mut store = PostgresReviewOrchestrationStore::new(pool.clone());
    assert_eq!(
        store.record_attempt(attempt.clone()).await?,
        ReviewDurableSealOutcome::Recorded
    );
    Ok((store, attempt))
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn negative_concern_claims_reject_a_canonical_nonterminal_pass() -> Result<(), Box<dyn Error>>
{
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let (mut store, attempt) = unsealed_concern_attempt(&pool, fixture.target).await?;
    let concern = &attempt.concerns()[0];
    for outcome in [
        ReviewConcernOutcome::Failed { pass: fixture.pass },
        ReviewConcernOutcome::Blocked { pass: fixture.pass },
        ReviewConcernOutcome::Cancelled {
            pass: Some(fixture.pass),
        },
    ] {
        let claim =
            ReviewConcernClaim::new(concern.key().clone(), concern.template_digest(), outcome);
        let error = store
            .record_concern_claim(attempt.id(), claim.clone())
            .await
            .expect_err("the canonical pass is queued, so no terminal claim is authentic");
        assert!(
            matches!(error, ReviewOrchestrationStoreError::Corruption(_)),
            "claim {claim:?}: {error:?}"
        );
    }
    assert!(store.load_concern_claims(attempt.id()).await?.is_empty());
    Ok(())
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn failed_concern_claim_admits_matching_terminal_pass_and_run() -> Result<(), Box<dyn Error>>
{
    let (_container, pool) = migrated_postgres().await?;
    let fixture = insert_review_pass_fixture(&pool).await;
    let (_, turn) = start_review_pass(&fixture.store, fixture.pass).await;
    complete_review_turn(&pool, turn).await;
    conclude_review_pass(
        &fixture.store,
        fixture.pass,
        ReviewPassState::Failed { turn },
    )
    .await;
    let (mut store, attempt) = unsealed_concern_attempt(&pool, fixture.target).await?;
    let concern = &attempt.concerns()[0];
    let claim = ReviewConcernClaim::new(
        concern.key().clone(),
        concern.template_digest(),
        ReviewConcernOutcome::Failed { pass: fixture.pass },
    );
    assert_eq!(
        store
            .record_concern_claim(attempt.id(), claim.clone())
            .await?,
        ReviewDurableSealOutcome::Recorded
    );
    assert_eq!(
        store
            .record_concern_claim(attempt.id(), claim.clone())
            .await?,
        ReviewDurableSealOutcome::EqualReplay
    );
    assert_eq!(store.load_concern_claims(attempt.id()).await?, [claim]);
    Ok(())
}
