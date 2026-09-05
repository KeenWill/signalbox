//! Tool round continuation reloads, interrupts, and durable closure of tool execution.

use crate::*;

/// Registers one verified replica so a fixture attachment names a catalogued blob.
///
/// The attachment part itself travels with the fixture's submit input rather
/// than being inserted afterwards, because content parts are immutable outside
/// the transaction that creates their parent.
async fn register_fixture_blob(
    pool: &PgPool,
    seed: u128,
    digest: BlobDigest,
) -> Result<(), Box<dyn Error>> {
    let store = BlobStoreName::try_new(format!("fixture-{seed:x}"))?;
    let expected = ExpectedBlob::try_new(digest, 1)?;
    let binding = BlobStoreBindingRecord::new(store.clone(), Uuid::from_u128(seed + 0x90));
    BlobCatalogRepository::new(pool.clone())
        .register_verified_replica(
            expected,
            binding,
            BlobReplicaRecord::new(store, BlobObjectKey::for_digest(digest)),
        )
        .await?;
    Ok(())
}

async fn prepare_confirmed_tool_attempt(
    pool: &PgPool,
    seed: u128,
    arguments: &str,
    attachment: Option<BlobDigest>,
) -> Result<(RestartModelCallFixture, ToolAttemptId), Box<dyn Error>> {
    let (fixture, _, _, request) = checkpoint_confirmed_tool_round_with_attachment(
        pool,
        seed,
        "blob_read",
        arguments,
        attachment,
    )
    .await?;
    let repository = PostgresToolLoopRepository::new(pool.clone());
    repository
        .decide(
            decide_tool_request(
                DurableCommandId::from_uuid(Uuid::from_u128(seed + 0xa0)),
                request,
                ToolApprovalDecision::Approve,
            ),
            || TurnAttemptId::from_uuid(Uuid::from_u128(seed + 0xa1)),
        )
        .await?;
    let attempt = ToolAttemptId::from_uuid(Uuid::from_u128(seed + 0xa2));
    repository
        .prepare_next_attempt(
            fixture.session,
            fixture.turn,
            attempt,
            ToolEffectClass::ExternalEffect,
        )
        .await?;
    Ok((fixture, attempt))
}

/// One durable `blob_read_tool_charge` projection with its labels preserved.
#[derive(Debug, sqlx::FromRow)]
struct StoredBlobReadCharge {
    blob_digest: Vec<u8>,
    decoded_byte_count: Decimal,
    admission: bool,
}

/// Whether a recorded charge granted the request its decoded bytes.
#[derive(Debug, Eq, PartialEq)]
enum BlobReadChargeAdmission {
    Admitted,
    Rejected,
}

impl StoredBlobReadCharge {
    fn admission(&self) -> BlobReadChargeAdmission {
        if self.admission {
            BlobReadChargeAdmission::Admitted
        } else {
            BlobReadChargeAdmission::Rejected
        }
    }
}

/// blob-read visibility and decoded-byte charges commit before
/// dispatch authority.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn blob_read_preauthorization_is_visible_bounded_and_durable() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let visible_seed = 0xd000;
    let visible_digest = BlobDigest::digest(b"visible");
    let visible_decoded_bytes = 524_288_u64;
    let visible_arguments = format!(
        r#"{{"digest":"{visible_digest}","offset_bytes":"0","length_bytes":"{visible_decoded_bytes}"}}"#
    );
    register_fixture_blob(&pool, visible_seed, visible_digest).await?;
    let (visible_fixture, visible_attempt) = prepare_confirmed_tool_attempt(
        &pool,
        visible_seed,
        &visible_arguments,
        Some(visible_digest),
    )
    .await?;
    let repository = PostgresToolLoopRepository::new(pool.clone());
    let visible = repository
        .authorize_attempt_with_preauthorization(
            visible_fixture.session,
            visible_fixture.turn,
            visible_attempt,
            ToolPreauthorization::BlobRead {
                digest: visible_digest,
                decoded_bytes: NonZeroU64::new(visible_decoded_bytes)
                    .expect("the fixture bound is positive"),
            },
        )
        .await?;
    assert!(matches!(
        visible,
        ToolAttemptAuthorizationOutcome::Authorized(_)
    ));
    let charge: StoredBlobReadCharge = sqlx::query_as(
        "SELECT blob_digest, decoded_byte_count, admitted AS admission
           FROM blob_read_tool_charge
          WHERE request_id = (
                SELECT request_id FROM tool_attempt WHERE attempt_id = $1)",
    )
    .bind(visible_attempt.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(charge.blob_digest, visible_digest.as_bytes().as_slice());
    assert_eq!(
        charge.decoded_byte_count,
        Decimal::from(visible_decoded_bytes)
    );
    assert_eq!(charge.admission(), BlobReadChargeAdmission::Admitted);

    pool.close().await;
    drop(container);
    Ok(())
}

/// an unattached blob-read digest is rejected before dispatch and
/// leaves the durable attempt Prepared.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn unattached_blob_read_is_rejected_before_dispatch() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let repository = PostgresToolLoopRepository::new(pool.clone());

    let hidden_seed = 0xd200;
    let hidden_digest = BlobDigest::digest(b"hidden");
    let hidden_decoded_bytes = 1_u64;
    let hidden_arguments = format!(
        r#"{{"digest":"{hidden_digest}","offset_bytes":"0","length_bytes":"{hidden_decoded_bytes}"}}"#
    );
    let (hidden_fixture, hidden_attempt) =
        prepare_confirmed_tool_attempt(&pool, hidden_seed, &hidden_arguments, None).await?;
    let hidden = repository
        .authorize_attempt_with_preauthorization(
            hidden_fixture.session,
            hidden_fixture.turn,
            hidden_attempt,
            ToolPreauthorization::BlobRead {
                digest: hidden_digest,
                decoded_bytes: NonZeroU64::new(hidden_decoded_bytes)
                    .expect("the fixture byte is positive"),
            },
        )
        .await?;
    assert_eq!(
        hidden,
        ToolAttemptAuthorizationOutcome::PreauthorizationRejected {
            detail: ToolExecutionErrorDetail::try_new(String::from("blob_not_visible"))
                .expect("the fixed rejection detail is valid"),
        }
    );
    let hidden_state: String =
        sqlx::query_scalar("SELECT state_kind FROM tool_attempt WHERE attempt_id = $1")
            .bind(hidden_attempt.into_uuid())
            .fetch_one(&pool)
            .await?;
    assert_eq!(hidden_state, "prepared");

    pool.close().await;
    drop(container);
    Ok(())
}

async fn lock_tool_continuation_outbox_allocator(
    pool: &sqlx::PgPool,
) -> Result<sqlx::Transaction<'_, sqlx::Postgres>, Box<dyn Error>> {
    let mut transaction = pool.begin().await?;
    sqlx::query("SELECT singleton FROM outbox_sequence_state WHERE singleton FOR UPDATE")
        .execute(&mut *transaction)
        .await?;
    Ok(transaction)
}

async fn lock_tool_continuation_result_writes(
    pool: &sqlx::PgPool,
) -> Result<sqlx::Transaction<'_, sqlx::Postgres>, Box<dyn Error>> {
    let mut transaction = pool.begin().await?;
    sqlx::query("LOCK TABLE semantic_transcript_entry IN SHARE MODE")
        .execute(&mut *transaction)
        .await?;
    Ok(transaction)
}

async fn tool_continuation_order_guard_is_available(
    pool: &sqlx::PgPool,
) -> Result<bool, Box<dyn Error>> {
    let mut transaction = pool.begin().await?;
    let available: bool =
        sqlx::query_scalar("SELECT pg_try_advisory_xact_lock(hashtextextended($1, 0))")
            .bind("model_call_outbox_order_guard:v1")
            .fetch_one(&mut *transaction)
            .await?;
    transaction.rollback().await?;
    Ok(available)
}

#[derive(Debug, sqlx::FromRow)]
struct StoredReclassifiedSteeringFacts {
    disposition_kind: String,
    origin_turn_id: Option<Uuid>,
}

#[derive(Debug, PartialEq)]
enum TurnOriginPresence {
    Absent,
    Present,
}

impl From<Option<Uuid>> for TurnOriginPresence {
    fn from(origin_turn_id: Option<Uuid>) -> Self {
        match origin_turn_id {
            Some(_) => Self::Present,
            None => Self::Absent,
        }
    }
}

#[derive(Debug, PartialEq)]
struct ReclassifiedSteeringFacts {
    disposition_kind: String,
    origin: TurnOriginPresence,
}

/// a tool-result continuation materializes and
/// reconstructs its transaction-local results before taking the shared
/// model-call ordering guard, then takes that guard before its results-projected
/// outbox append can wait on the allocator. This excludes both long reads from
/// the global writer critical section and the allocator-to-guard edge that
/// would deadlock against counted activation.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn tool_continuation_guards_before_result_outbox() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0x7ef8;
    let (fixture, _, _, request) =
        checkpoint_confirmed_tool_round(&pool, seed, "current_time", "{}").await?;
    let tool_repository = PostgresToolLoopRepository::new(pool.clone());
    let continuation_attempt = TurnAttemptId::from_uuid(Uuid::from_u128(seed + 0x22));
    tool_repository
        .decide(
            decide_tool_request(
                DurableCommandId::from_uuid(Uuid::from_u128(seed + 0x21)),
                request,
                ToolApprovalDecision::Approve,
            ),
            || continuation_attempt,
        )
        .await?;
    let tool_attempt = ToolAttemptId::from_uuid(Uuid::from_u128(seed + 0x23));
    tool_repository
        .prepare_next_attempt(
            fixture.session,
            fixture.turn,
            tool_attempt,
            ToolEffectClass::EffectFree,
        )
        .await?;
    let authorized = tool_repository
        .authorize_attempt(fixture.session, fixture.turn, tool_attempt)
        .await?;
    tool_repository
        .commit_observation(
            authorized
                .executor_fence()
                .bind(ToolAttemptObservation::Completed {
                    result: ToolResultContent::Text(
                        ToolResultText::try_new(String::from("2026-08-22T04:00:00Z"))
                            .expect("bounded result"),
                    ),
                }),
        )
        .await?;

    let selection = DirectModelSelection::from_uuid(Uuid::from_u128(seed + 5));
    let target =
        ResolvedProviderTarget::naming(ProviderModelIdentity::from_uuid(Uuid::from_u128(seed + 6)));
    let targets =
        ModelTargetCatalog::try_from_definitions([ModelTargetDefinition::new(selection, target)])
            .expect("one continuation target forms a catalog");
    let continuing_repository = PostgresToolLoopRepository::with_model_calls(
        pool.clone(),
        targets,
        model_credential_reference(),
    );
    let session = fixture.session;
    let turn = fixture.turn;
    let producing_call = fixture.call;
    let continuation_call = ModelCallId::from_uuid(Uuid::from_u128(seed + 0x28));
    let result_holder = lock_tool_continuation_result_writes(&pool).await?;
    let allocator_holder = lock_tool_continuation_outbox_allocator(&pool).await?;
    let continuation = tokio::spawn(async move {
        continuing_repository
            .prepare_continuation(
                session,
                turn,
                producing_call,
                signalbox_application::ToolContinuationIdentities::new(
                    vec![SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(
                        seed + 0x26,
                    ))],
                    ContextFrontierId::from_uuid(Uuid::from_u128(seed + 0x27)),
                    continuation_call,
                    FailedModelCallTurnIdentities::new(
                        SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 0x29)),
                        ContextFrontierId::from_uuid(Uuid::from_u128(seed + 0x2a)),
                    ),
                    ContextFrontierId::from_uuid(Uuid::from_u128(seed + 0x2b)),
                ),
                |_| panic!("fixture has no pending steering"),
            )
            .await
    });
    assert!(blocked_backends_reached(&pool, 1).await?);
    assert!(tool_continuation_order_guard_is_available(&pool).await?);
    result_holder.rollback().await?;
    assert!(blocked_backends_reached(&pool, 1).await?);
    assert!(!tool_continuation_order_guard_is_available(&pool).await?);
    allocator_holder.rollback().await?;
    assert_eq!(
        continuation.await??,
        signalbox_application::PrepareToolContinuationOutcome::Checkpointed(continuation_call)
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// A turn-state check does not rescan immutable historical tool-round
/// frontiers; the round-specific validator still owns that exact evidence.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn deferred_turn_validation_skips_immutable_tool_round_frontiers()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0x7efa;
    let (fixture, _, _, _) =
        checkpoint_confirmed_tool_round(&pool, seed, "current_time", "{}").await?;

    let mut frontier_holder = pool.begin().await?;
    sqlx::query("LOCK TABLE context_frontier_member IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *frontier_holder)
        .await?;
    sqlx::query("SELECT assert_turn_lifecycle_final_state($1)")
        .bind(fixture.turn.into_uuid())
        .execute(&pool)
        .await?;

    let round_validation = tokio::spawn({
        let pool = pool.clone();
        async move {
            sqlx::query("SELECT assert_tool_round_final_state($1)")
                .bind(fixture.call.into_uuid())
                .execute(&pool)
                .await
        }
    });
    assert!(blocked_backends_reached(&pool, 1).await?);
    frontier_holder.rollback().await?;
    round_validation.await??;

    pool.close().await;
    drop(container);
    Ok(())
}

/// provider usage plus newly projected result content that exhausts
/// configured headroom preserves the results, closes the turn with typed
/// evidence, and prepares no oversized continuation call. The daemon-owned
/// closure is budget-neutral for goals.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn tool_continuation_headroom_closes_before_another_call() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0x7ef9;
    let (fixture, _, _, request) = checkpoint_confirmed_tool_round_with_usage(
        &pool,
        seed,
        "current_time",
        "{}",
        ProviderReportedTokenUsage::unreported()
            .with_input_tokens(Some(70))
            .with_output_tokens(Some(5)),
    )
    .await?;
    let tool_repository = PostgresToolLoopRepository::new(pool.clone());
    let continuation_attempt = TurnAttemptId::from_uuid(Uuid::from_u128(seed + 0x22));
    tool_repository
        .decide(
            decide_tool_request(
                DurableCommandId::from_uuid(Uuid::from_u128(seed + 0x21)),
                request,
                ToolApprovalDecision::Approve,
            ),
            || continuation_attempt,
        )
        .await?;
    let tool_attempt = ToolAttemptId::from_uuid(Uuid::from_u128(seed + 0x23));
    tool_repository
        .prepare_next_attempt(
            fixture.session,
            fixture.turn,
            tool_attempt,
            ToolEffectClass::EffectFree,
        )
        .await?;
    let authorized = tool_repository
        .authorize_attempt(fixture.session, fixture.turn, tool_attempt)
        .await?;
    let result_text = String::from("2026-08-22T04:00:00Z");
    let result_content_bytes = u64::try_from(result_text.len())?;
    tool_repository
        .commit_observation(
            authorized
                .executor_fence()
                .bind(ToolAttemptObservation::Completed {
                    result: ToolResultContent::Text(
                        ToolResultText::try_new(result_text).expect("bounded result"),
                    ),
                }),
        )
        .await?;
    let selection = DirectModelSelection::from_uuid(Uuid::from_u128(seed + 5));
    let target =
        ResolvedProviderTarget::naming(ProviderModelIdentity::from_uuid(Uuid::from_u128(seed + 6)));
    let targets =
        ModelTargetCatalog::try_from_definitions([ModelTargetDefinition::new(selection, target)])
            .expect("one continuation target forms a catalog");
    let continuation_call = ModelCallId::from_uuid(Uuid::from_u128(seed + 0x28));
    let model_repository =
        PostgresModelCallRepository::new(pool.clone(), targets, model_credential_reference())
            .with_continuation_usage_limits([ToolContinuationUsageLimit::new(
                target,
                FastMode::Disabled,
                10,
                100,
            )]);
    let continuing_repository = model_repository.tool_loop_repository();
    let result_frontier = ContextFrontierId::from_uuid(Uuid::from_u128(seed + 0x27));
    let outcome = continuing_repository
        .prepare_continuation(
            fixture.session,
            fixture.turn,
            fixture.call,
            signalbox_application::ToolContinuationIdentities::new(
                vec![SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(
                    seed + 0x26,
                ))],
                result_frontier,
                continuation_call,
                FailedModelCallTurnIdentities::new(
                    SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 0x29)),
                    ContextFrontierId::from_uuid(Uuid::from_u128(seed + 0x2a)),
                ),
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 0x2b)),
            ),
            |_| panic!("fixture has no pending steering"),
        )
        .await?;
    let signalbox_application::PrepareToolContinuationOutcome::ContextCompactionRequired(required) =
        outcome
    else {
        panic!("reported usage closes the continuation for compaction");
    };
    assert_eq!(required.producing_call(), fixture.call);
    assert_eq!(required.failed().turn(), fixture.turn);
    let reported = model_repository
        .latest_reported_usage(fixture.session, target, result_frontier)
        .await?
        .expect("the producing call reported input usage");
    assert_eq!(
        reported.projected_unreported_content_bytes(),
        result_content_bytes
    );
    let producing_frontier: Uuid = sqlx::query_scalar(
        "SELECT context_frontier_id
           FROM model_call
          WHERE model_call_id = $1",
    )
    .bind(fixture.call.into_uuid())
    .fetch_one(&pool)
    .await?;
    let successor_reported = model_repository
        .latest_reported_usage(
            fixture.session,
            target,
            ContextFrontierId::from_uuid(producing_frontier),
        )
        .await?
        .expect("the durable headroom proof remains authoritative for a successor frontier");
    assert_eq!(
        successor_reported.projected_unreported_content_bytes(),
        result_content_bytes
    );
    let disjoint_content = "successor content outside the proved tool-result batch";
    SubmitInputRepository::new(pool.clone())
        .handle(
            start_input(
                seed + 0x200,
                seed + 1,
                disjoint_content,
                1,
                ModelSelectionOverride::UseSessionDefault,
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(seed + 0x201)),
            Some(TurnId::from_uuid(Uuid::from_u128(seed + 0x202))),
        )
        .await?;
    let disjoint_entry = Uuid::from_u128(seed + 0x203);
    activate_earliest_queued_turn(
        &pool,
        EarliestQueuedTurnActivation {
            session: fixture.session.into_uuid(),
            origin_entry: disjoint_entry,
            starting_frontier: Uuid::from_u128(seed + 0x204),
            initial_attempt: Uuid::from_u128(seed + 0x205),
        },
    )
    .await?;
    let producing_member_count: i64 = sqlx::query_scalar(
        "SELECT member_count::bigint
           FROM context_frontier
          WHERE owning_session_id = $1
            AND context_frontier_id = $2",
    )
    .bind(fixture.session.into_uuid())
    .bind(producing_frontier)
    .fetch_one(&pool)
    .await?;
    let disjoint_frontier = Uuid::from_u128(seed + 0x206);
    let mut transaction = pool.begin().await?;
    sqlx::query(
        "INSERT INTO context_frontier
             (owning_session_id, context_frontier_id, member_count,
              prefix_context_frontier_id)
         VALUES ($1, $2, $3, $4)",
    )
    .bind(fixture.session.into_uuid())
    .bind(disjoint_frontier)
    .bind(producing_member_count + 1)
    .bind(producing_frontier)
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO context_frontier_delta
             (owning_session_id, context_frontier_id, member_position,
              source_session_id, semantic_entry_id)
         VALUES ($1, $2, $3, $1, $4)",
    )
    .bind(fixture.session.into_uuid())
    .bind(disjoint_frontier)
    .bind(producing_member_count + 1)
    .bind(disjoint_entry)
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await?;
    let disjoint_reported = model_repository
        .latest_reported_usage(
            fixture.session,
            target,
            ContextFrontierId::from_uuid(disjoint_frontier),
        )
        .await?
        .expect("durable proof and a disjoint successor suffix are both retained");
    assert_eq!(
        disjoint_reported.projected_unreported_content_bytes(),
        result_content_bytes + u64::try_from(disjoint_content.len())?
    );

    let stored: (String, Option<Uuid>, Uuid, Decimal, Decimal, Decimal, i64) = sqlx::query_as(
        "SELECT lifecycle.terminal_disposition_kind,
                lifecycle.terminal_model_call_id,
                headroom.producing_model_call_id,
                headroom.projected_result_content_bytes,
                headroom.max_output_tokens,
                headroom.context_window_tokens,
                (SELECT count(*) FROM model_call WHERE model_call_id = $3)
           FROM turn_lifecycle AS lifecycle
           JOIN tool_continuation_context_headroom AS headroom
             ON headroom.terminal_attempt_id = lifecycle.terminal_attempt_id
            AND headroom.turn_id = lifecycle.turn_id
            AND headroom.session_id = lifecycle.session_id
          WHERE lifecycle.session_id = $1
            AND lifecycle.turn_id = $2",
    )
    .bind(fixture.session.into_uuid())
    .bind(fixture.turn.into_uuid())
    .bind(continuation_call.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        stored,
        (
            String::from("failed"),
            None,
            fixture.call.into_uuid(),
            Decimal::from(result_content_bytes),
            Decimal::from(10_u64),
            Decimal::from(100_u64),
            0,
        )
    );
    assert_eq!(
        GoalRepository::new(pool.clone())
            .unchargeable_automatic_resume_turns(fixture.session, &[fixture.turn])
            .await?
            .as_ref(),
        &[fixture.turn]
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// a returning foreground delegation result is model-visible
/// continuation content. The same-turn headroom bound counts its delivered
/// child-result bytes alongside executed tool results, so a round whose child
/// returned a large result takes the compaction boundary instead of preparing
/// the oversized continuation.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn tool_continuation_headroom_counts_delegation_results() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0x8900;
    let (fixture, _, _, requests) = checkpoint_tool_batch_with_approval_and_usage(
        &pool,
        seed,
        &[("spawn_session", "{}"), ("await_session", "{}")],
        InitialToolApproval::Confirm,
        ProviderReportedTokenUsage::unreported()
            .with_input_tokens(Some(70))
            .with_output_tokens(Some(5)),
    )
    .await?;
    let [spawning_request, awaiting_request] = requests.as_slice() else {
        panic!("the foreground delegation fixture has spawn and await requests")
    };
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(prepared(seed + 0x100, seed + 0x101, direct(seed + 5)))
        .await?;
    let child = Uuid::from_u128(seed + 0x101);
    let tool_repository = PostgresToolLoopRepository::new(pool.clone());
    let issuing_attempt = TurnAttemptId::from_uuid(Uuid::from_u128(seed + 0xe0));
    tool_repository
        .decide(
            decide_tool_request(
                DurableCommandId::from_uuid(Uuid::from_u128(seed + 0xd0)),
                *spawning_request,
                ToolApprovalDecision::Approve,
            ),
            || panic!("the first approval does not start execution"),
        )
        .await?;
    tool_repository
        .decide(
            decide_tool_request(
                DurableCommandId::from_uuid(Uuid::from_u128(seed + 0xd1)),
                *awaiting_request,
                ToolApprovalDecision::Approve,
            ),
            || issuing_attempt,
        )
        .await?;
    let spawn_attempt = ToolAttemptId::from_uuid(Uuid::from_u128(seed + 0xe1));
    tool_repository
        .prepare_next_attempt(
            fixture.session,
            fixture.turn,
            spawn_attempt,
            ToolEffectClass::EffectFree,
        )
        .await?
        .expect("the approved spawn request prepares one attempt");
    let authorized_spawn = tool_repository
        .authorize_attempt(fixture.session, fixture.turn, spawn_attempt)
        .await?;
    // The spawn result names the child session: a hyphenated UUID is 36 bytes.
    let spawn_result = child.to_string();
    tool_repository
        .commit_observation(authorized_spawn.executor_fence().bind(
            ToolAttemptObservation::Completed {
                result: ToolResultContent::Text(
                    ToolResultText::try_new(spawn_result).expect("bounded child identity"),
                ),
            },
        ))
        .await?;
    let await_attempt = ToolAttemptId::from_uuid(Uuid::from_u128(seed + 0xe2));
    tool_repository
        .prepare_next_attempt(
            fixture.session,
            fixture.turn,
            await_attempt,
            ToolEffectClass::EffectFree,
        )
        .await?
        .expect("the approved await request prepares one attempt");

    // Forty-two ASCII characters and one two-byte "é": 44 UTF-8 bytes.
    let child_result = "delivered foreground child result content é";
    sqlx::raw_sql(
        "ALTER TABLE session_delegation DISABLE TRIGGER ALL;
         ALTER TABLE session_delegation_event DISABLE TRIGGER ALL;
         ALTER TABLE session_delegation_wait DISABLE TRIGGER ALL;
         ALTER TABLE session_child_result DISABLE TRIGGER ALL;
         ALTER TABLE session_child_result_delivery DISABLE TRIGGER ALL;
         ALTER TABLE tool_attempt DISABLE TRIGGER ALL;",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO session_delegation
            (spawning_tool_request_id, parent_session_id, parent_turn_id,
             child_session_id, policy_kind)
         VALUES ($1, $2, $3, $4, 'background')",
    )
    .bind(spawning_request.into_uuid())
    .bind(fixture.session.into_uuid())
    .bind(fixture.turn.into_uuid())
    .bind(child)
    .execute(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO session_delegation_event
            (spawning_tool_request_id, event_ordinal, event_kind,
             provenance_kind, provenance_session_id, provenance_turn_id,
             provenance_tool_request_id)
         VALUES ($1, 1, 'spawned', 'tool_request', $2, $3, $1)",
    )
    .bind(spawning_request.into_uuid())
    .bind(fixture.session.into_uuid())
    .bind(fixture.turn.into_uuid())
    .execute(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO session_delegation_event
            (spawning_tool_request_id, event_ordinal, event_kind,
             outcome_kind, reason_kind, provenance_kind,
             provenance_session_id, provenance_turn_id)
         VALUES ($1, 2, 'outcome_recorded', 'result_returned',
                 'child_completed', 'child_turn', $2, $3)",
    )
    .bind(spawning_request.into_uuid())
    .bind(child)
    .bind(Uuid::from_u128(seed + 0x102))
    .execute(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO session_child_result
            (spawning_tool_request_id, event_ordinal, event_kind,
             outcome_kind, content_text)
         VALUES ($1, 2, 'outcome_recorded', 'result_returned', $2)",
    )
    .bind(spawning_request.into_uuid())
    .bind(child_result)
    .execute(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO session_delegation_wait
            (awaiting_tool_request_id, spawning_tool_request_id,
             parent_session_id, parent_turn_id, child_session_id, wait_mode)
         VALUES ($1, $2, $3, $4, $5, 'foreground')",
    )
    .bind(awaiting_request.into_uuid())
    .bind(spawning_request.into_uuid())
    .bind(fixture.session.into_uuid())
    .bind(fixture.turn.into_uuid())
    .bind(child)
    .execute(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO session_child_result_delivery
            (awaiting_tool_request_id, spawning_tool_request_id,
             parent_session_id, delivery_sequence, delivery_kind)
         VALUES ($1, $2, $3, NULL, NULL)",
    )
    .bind(awaiting_request.into_uuid())
    .bind(spawning_request.into_uuid())
    .bind(fixture.session.into_uuid())
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE tool_attempt
            SET state_kind = 'terminal',
                terminal_disposition_kind = 'awaiting_child',
                wait_spawning_request_id = $1,
                wait_child_session_id = $2
          WHERE attempt_id = $3",
    )
    .bind(spawning_request.into_uuid())
    .bind(child)
    .bind(await_attempt.into_uuid())
    .execute(&pool)
    .await?;
    sqlx::raw_sql(
        "ALTER TABLE session_delegation ENABLE TRIGGER ALL;
         ALTER TABLE session_delegation_event ENABLE TRIGGER ALL;
         ALTER TABLE session_delegation_wait ENABLE TRIGGER ALL;
         ALTER TABLE session_child_result ENABLE TRIGGER ALL;
         ALTER TABLE session_child_result_delivery ENABLE TRIGGER ALL;
         ALTER TABLE tool_attempt ENABLE TRIGGER ALL;",
    )
    .execute(&pool)
    .await?;

    let selection = DirectModelSelection::from_uuid(Uuid::from_u128(seed + 5));
    let target =
        ResolvedProviderTarget::naming(ProviderModelIdentity::from_uuid(Uuid::from_u128(seed + 6)));
    let targets =
        ModelTargetCatalog::try_from_definitions([ModelTargetDefinition::new(selection, target)])
            .expect("one continuation target forms a catalog");
    let result_frontier = ContextFrontierId::from_uuid(Uuid::from_u128(seed + 0xe6));
    let continuation_call = ModelCallId::from_uuid(Uuid::from_u128(seed + 0xe8));
    let model_repository =
        PostgresModelCallRepository::new(pool.clone(), targets, model_credential_reference())
            .with_continuation_usage_limits([ToolContinuationUsageLimit::new(
                target,
                FastMode::Disabled,
                10,
                130,
            )]);
    let outcome = model_repository
        .tool_loop_repository()
        .prepare_continuation(
            fixture.session,
            fixture.turn,
            fixture.call,
            signalbox_application::ToolContinuationIdentities::new(
                vec![
                    SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 0xe4)),
                    SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 0xe5)),
                ],
                result_frontier,
                continuation_call,
                FailedModelCallTurnIdentities::new(
                    SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 0xe9)),
                    ContextFrontierId::from_uuid(Uuid::from_u128(seed + 0xea)),
                ),
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 0xeb)),
            ),
            |_| panic!("fixture has no pending steering"),
        )
        .await?;

    let signalbox_application::PrepareToolContinuationOutcome::ContextCompactionRequired(required) =
        outcome
    else {
        panic!("the delivered child result exhausts the configured continuation headroom");
    };
    assert_eq!(required.producing_call(), fixture.call);
    let stored_bytes: Decimal = sqlx::query_scalar(
        "SELECT projected_result_content_bytes
           FROM tool_continuation_context_headroom
          WHERE session_id = $1
            AND turn_id = $2
            AND producing_model_call_id = $3",
    )
    .bind(fixture.session.into_uuid())
    .bind(fixture.turn.into_uuid())
    .bind(fixture.call.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(stored_bytes, Decimal::from(36 + 44_u64));
    let reported = model_repository
        .latest_reported_usage(fixture.session, target, result_frontier)
        .await?
        .expect("the producing call reported input usage");
    assert_eq!(
        reported.projected_unreported_content_bytes(),
        36 + 44,
        "a proved delegation result present in the successor projection is counted once"
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// S02 / S08: a NextSafePoint input accepted while a tool
/// round executes is consumed by the same-turn continuation call, and the
/// committed continuation shape reloads through the scheduling projection —
/// the next submit is accepted and the startup scan classifies the prepared
/// call instead of leaving the session permanently unloadable.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s02_s08_steering_consumed_at_continuation_reloads_and_scans() -> Result<(), Box<dyn Error>>
{
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0x7f00;
    let (fixture, _, _, request) =
        checkpoint_confirmed_tool_round(&pool, seed, "current_time", "{}").await?;
    let tool_repository = PostgresToolLoopRepository::new(pool.clone());
    let continuation_attempt = TurnAttemptId::from_uuid(Uuid::from_u128(seed + 0x22));
    tool_repository
        .decide(
            decide_tool_request(
                DurableCommandId::from_uuid(Uuid::from_u128(seed + 0x21)),
                request,
                ToolApprovalDecision::Approve,
            ),
            || continuation_attempt,
        )
        .await?;
    let tool_attempt = ToolAttemptId::from_uuid(Uuid::from_u128(seed + 0x23));
    tool_repository
        .prepare_next_attempt(
            fixture.session,
            fixture.turn,
            tool_attempt,
            ToolEffectClass::EffectFree,
        )
        .await?;
    let authorized_attempt = tool_repository
        .authorize_attempt(fixture.session, fixture.turn, tool_attempt)
        .await?;

    let steering_outcome = SubmitInputRepository::new(pool.clone())
        .handle(
            input_with_delivery(
                seed + 0x24,
                seed + 1,
                "steer the executing tool round",
                DeliveryRequest::NextSafePoint {
                    expected_active_turn: fixture.turn,
                },
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(seed + 0x25)),
            None,
        )
        .await?;
    assert!(matches!(
        steering_outcome,
        SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Applied(
            SubmitInputAppliedResult::PendingSteering(_)
        ))
    ));

    tool_repository
        .commit_observation(
            authorized_attempt
                .executor_fence()
                .bind(ToolAttemptObservation::Completed {
                    result: ToolResultContent::Text(
                        ToolResultText::try_new(String::from("2026-07-26T12:00:00Z"))
                            .expect("bounded result"),
                    ),
                }),
        )
        .await?;

    let selection = signalbox_domain::DirectModelSelection::from_uuid(Uuid::from_u128(seed + 5));
    let provider = ProviderModelIdentity::from_uuid(Uuid::from_u128(seed + 6));
    let targets = ModelTargetCatalog::try_from_definitions([ModelTargetDefinition::new(
        selection,
        ResolvedProviderTarget::naming(provider),
    )])
    .expect("one continuation target forms a catalog");
    let continuation_call = ModelCallId::from_uuid(Uuid::from_u128(seed + 0x28));
    let steering_entry = SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 0x2c));
    let continuation = PostgresToolLoopRepository::with_model_calls(
        pool.clone(),
        targets,
        model_credential_reference(),
    )
    .prepare_continuation(
        fixture.session,
        fixture.turn,
        fixture.call,
        signalbox_application::ToolContinuationIdentities::new(
            vec![SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(
                seed + 0x26,
            ))],
            ContextFrontierId::from_uuid(Uuid::from_u128(seed + 0x27)),
            continuation_call,
            FailedModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 0x29)),
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 0x2a)),
            ),
            ContextFrontierId::from_uuid(Uuid::from_u128(seed + 0x2b)),
        ),
        |_| {
            (
                steering_entry,
                TurnId::from_uuid(Uuid::from_u128(seed + 0x2d)),
            )
        },
    )
    .await?;
    assert_eq!(
        continuation,
        signalbox_application::PrepareToolContinuationOutcome::Checkpointed(continuation_call)
    );
    let consumed_shape: (String, Option<Uuid>, i64) = sqlx::query_as(
        "SELECT accepted.disposition_kind,
                accepted.consuming_model_call_id,
                (SELECT count(*) FROM context_frontier_delta AS delta
                  WHERE delta.owning_session_id = $2
                    AND delta.semantic_entry_id = $3)
           FROM accepted_input AS accepted
          WHERE accepted.session_id = $2
            AND accepted.accepted_input_id = $1",
    )
    .bind(Uuid::from_u128(seed + 0x25))
    .bind(fixture.session.into_uuid())
    .bind(steering_entry.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        consumed_shape,
        (
            String::from("consumed_as_steering"),
            Some(continuation_call.into_uuid()),
            1,
        ),
        "the continuation call durably consumed the steering input"
    );
    let receipt: (String, Option<Uuid>) = sqlx::query_as(
        "SELECT outcome_kind, delivered_turn_id
           FROM injection_settled_outbox_event
          WHERE command_id = $1",
    )
    .bind(Uuid::from_u128(seed + 0x24))
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        receipt,
        (String::from("delivered"), Some(fixture.turn.into_uuid())),
        "consumption settles the steering delivered to its source turn"
    );

    let queued_follow_up = SubmitInputRepository::new(pool.clone())
        .handle(
            input_with_delivery(
                seed + 0x2e,
                seed + 1,
                "queued work behind the continuation",
                DeliveryRequest::AfterCurrentTurn {
                    expected_active_turn: fixture.turn,
                    configuration: input_choices(1, ModelSelectionOverride::UseSessionDefault),
                },
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(seed + 0x2f)),
            Some(TurnId::from_uuid(Uuid::from_u128(seed + 0x30))),
        )
        .await?;
    assert!(
        matches!(
            queued_follow_up,
            SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Applied(
                SubmitInputAppliedResult::TurnOrigin(_)
            ))
        ),
        "writer-produced consumed steering must reconstitute before the next submit"
    );

    let mut recovery_ids = FixedStartupScanIds::new([], []);
    let scan = PostgresStartupScanRepository::new(pool.clone())
        .recover(
            fixture.session,
            signalbox_domain::AcceptedInputTurnFailureIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 0x31)),
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 0x32)),
            ),
            &mut recovery_ids,
        )
        .await?;
    assert_eq!(
        scan,
        StartupScanSessionOutcome::ResumablePreparedModelCall { turn: fixture.turn },
        "the startup scan preserves the prepared continuation for resumption"
    );
    let recovered_shape: (String, Option<String>, Uuid, String, Option<String>) = sqlx::query_as(
        "SELECT lifecycle.state_kind,
                lifecycle.terminal_disposition_kind,
                lifecycle.current_attempt_id,
                continuation.state_kind,
                continuation.terminal_disposition_kind
           FROM turn_lifecycle AS lifecycle
           JOIN model_call AS continuation
             ON continuation.session_id = lifecycle.session_id
            AND continuation.turn_id = lifecycle.turn_id
            AND continuation.model_call_id = $3
          WHERE lifecycle.session_id = $1
            AND lifecycle.turn_id = $2",
    )
    .bind(fixture.session.into_uuid())
    .bind(fixture.turn.into_uuid())
    .bind(continuation_call.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        recovered_shape,
        (
            String::from("active"),
            None,
            continuation_attempt.into_uuid(),
            String::from("prepared"),
            None,
        ),
        "restart recovery leaves the steering-consuming continuation unchanged"
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// S02 / S07 / S10: an interrupt applied while the
/// prepared continuation call of a completed tool round awaits send cancels
/// the turn naming that call, and the committed terminal shape reloads
/// through the scheduling projection — the interrupt successor activates
/// instead of leaving the session permanently unloadable.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s02_s07_s10_interrupted_continuation_call_reloads_and_activates_successor()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0x8100;
    let (fixture, _, _, request) =
        checkpoint_confirmed_tool_round(&pool, seed, "current_time", "{}").await?;
    let tool_repository = PostgresToolLoopRepository::new(pool.clone());
    let continuation_attempt = TurnAttemptId::from_uuid(Uuid::from_u128(seed + 0x22));
    tool_repository
        .decide(
            decide_tool_request(
                DurableCommandId::from_uuid(Uuid::from_u128(seed + 0x21)),
                request,
                ToolApprovalDecision::Approve,
            ),
            || continuation_attempt,
        )
        .await?;
    let tool_attempt = ToolAttemptId::from_uuid(Uuid::from_u128(seed + 0x23));
    tool_repository
        .prepare_next_attempt(
            fixture.session,
            fixture.turn,
            tool_attempt,
            ToolEffectClass::EffectFree,
        )
        .await?;
    let authorized_attempt = tool_repository
        .authorize_attempt(fixture.session, fixture.turn, tool_attempt)
        .await?;
    tool_repository
        .commit_observation(
            authorized_attempt
                .executor_fence()
                .bind(ToolAttemptObservation::Completed {
                    result: ToolResultContent::Text(
                        ToolResultText::try_new(String::from("2026-07-26T12:00:00Z"))
                            .expect("bounded result"),
                    ),
                }),
        )
        .await?;

    let selection = signalbox_domain::DirectModelSelection::from_uuid(Uuid::from_u128(seed + 5));
    let provider = ProviderModelIdentity::from_uuid(Uuid::from_u128(seed + 6));
    let targets = ModelTargetCatalog::try_from_definitions([ModelTargetDefinition::new(
        selection,
        ResolvedProviderTarget::naming(provider),
    )])
    .expect("one continuation target forms a catalog");
    let continuation_call = ModelCallId::from_uuid(Uuid::from_u128(seed + 0x28));
    let continuation = PostgresToolLoopRepository::with_model_calls(
        pool.clone(),
        targets,
        model_credential_reference(),
    )
    .prepare_continuation(
        fixture.session,
        fixture.turn,
        fixture.call,
        signalbox_application::ToolContinuationIdentities::new(
            vec![SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(
                seed + 0x26,
            ))],
            ContextFrontierId::from_uuid(Uuid::from_u128(seed + 0x27)),
            continuation_call,
            FailedModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 0x29)),
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 0x2a)),
            ),
            ContextFrontierId::from_uuid(Uuid::from_u128(seed + 0x2b)),
        ),
        |_| panic!("the fixture has no pending steering"),
    )
    .await?;
    assert_eq!(
        continuation,
        signalbox_application::PrepareToolContinuationOutcome::Checkpointed(continuation_call)
    );

    let successor = TurnId::from_uuid(Uuid::from_u128(seed + 0x30));
    let interrupt_outcome = SubmitInputRepository::new(pool.clone())
        .handle(
            input_with_delivery(
                seed + 0x24,
                seed + 1,
                "stop the prepared continuation",
                DeliveryRequest::Interrupt {
                    expected_active_turn: fixture.turn,
                    descendant_scope: DescendantTerminationScope::ParentAlone,
                    configuration: input_choices(1, ModelSelectionOverride::UseSessionDefault),
                },
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(seed + 0x25)),
            Some(successor),
        )
        .await?;
    assert!(matches!(
        interrupt_outcome,
        SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Applied(_))
    ));
    #[derive(Debug, PartialEq, sqlx::FromRow)]
    struct CancelledContinuationShape {
        turn_disposition: String,
        terminal_model_call_id: Option<Uuid>,
        call_disposition: String,
    }
    let cancelled_shape: CancelledContinuationShape = sqlx::query_as(
        "SELECT lifecycle.terminal_disposition_kind AS turn_disposition,
                lifecycle.terminal_model_call_id,
                continuation.terminal_disposition_kind AS call_disposition
           FROM turn_lifecycle AS lifecycle
           JOIN model_call AS continuation
             ON continuation.session_id = lifecycle.session_id
            AND continuation.model_call_id = lifecycle.terminal_model_call_id
          WHERE lifecycle.session_id = $1
            AND lifecycle.turn_id = $2",
    )
    .bind(fixture.session.into_uuid())
    .bind(fixture.turn.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        cancelled_shape,
        CancelledContinuationShape {
            turn_disposition: String::from("cancelled"),
            terminal_model_call_id: Some(continuation_call.into_uuid()),
            call_disposition: String::from("cancelled"),
        },
        "the interrupt terminalizes the turn naming its unsent continuation call"
    );

    let mut recovery_ids = FixedStartupScanIds::new([], []);
    let scan = PostgresStartupScanRepository::new(pool.clone())
        .recover(
            fixture.session,
            signalbox_domain::AcceptedInputTurnFailureIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 0x31)),
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 0x32)),
            ),
            &mut recovery_ids,
        )
        .await?;
    assert_eq!(
        scan,
        StartupScanSessionOutcome::NoActiveTurn,
        "writer-produced cancelled continuation history must reconstitute at startup"
    );

    let mut scheduling_probe = StartEligibleTurnService::new(
        FixedStartEligibleTurnIds::new(
            [SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(
                seed + 0x2c,
            ))],
            [ContextFrontierId::from_uuid(Uuid::from_u128(seed + 0x2d))],
            [TurnAttemptId::from_uuid(Uuid::from_u128(seed + 0x2e))],
        ),
        StartEligibleTurnRepository::new(pool.clone()),
    );
    let StartEligibleTurnOutcome::Activated(activated) =
        scheduling_probe.execute(fixture.session).await?
    else {
        panic!("the interrupt successor activates behind the cancelled continuation call");
    };
    assert_eq!(activated.turn(), successor);

    pool.close().await;
    drop(container);
    Ok(())
}

/// The projection above decides both ambiguity verdicts, so its classification
/// is pinned directly rather than trusted: a batch transition for the turn
/// under test, a definitive outcome for it, one for an unrelated turn, and a
/// kind that bears on neither.
#[test]
fn announcement_for_classifies_each_outcome() {
    // Named by role. Only distinctness and which identity plays which part
    // matter here — no assertion depends on a particular hexadecimal value,
    // and spelling the numbers inline would present each as load-bearing.
    let mut ids = ClassifierFixtureIds::default();
    let turn = ids.next_turn();
    let other_turn = ids.next_turn();
    let call = ids.next_call();
    let attempt = ids.next_tool_attempt();
    let frontier = ids.next_frontier();
    let entry = ids.next_entry();

    assert_eq!(
        announcement_for(
            &DispatchedOutboxEventKind::ToolBatchTransition {
                turn,
                producing_call: call,
                state: DispatchedToolBatchState::RecoveryRequired { attempt },
            },
            turn,
            call,
        ),
        AmbiguityAnnouncement::BatchTransition(DispatchedToolBatchState::RecoveryRequired {
            attempt
        })
    );
    assert_eq!(
        announcement_for(
            &DispatchedOutboxEventKind::TurnTerminal {
                turn,
                disposition: DispatchedTurnTerminalDisposition::Failed {
                    failure_entry: entry,
                    terminal_frontier: frontier,
                },
            },
            turn,
            call,
        ),
        AmbiguityAnnouncement::DefinitiveTurnOutcome
    );
    assert_eq!(
        announcement_for(
            &DispatchedOutboxEventKind::TurnTerminal {
                turn: other_turn,
                disposition: DispatchedTurnTerminalDisposition::Failed {
                    failure_entry: entry,
                    terminal_frontier: frontier,
                },
            },
            turn,
            call,
        ),
        AmbiguityAnnouncement::Unrelated
    );
    assert_eq!(
        announcement_for(
            &DispatchedOutboxEventKind::SessionCreated(DispatchedSessionCreation {
                cause: SessionCreationCause::Interactive,
                ownership: SessionOwnership::Unmonitored,
            }),
            turn,
            call,
        ),
        AmbiguityAnnouncement::Unrelated
    );
}

/// S06: an executor that cannot establish whether its
/// external effect happened terminalizes the attempt ambiguous and parks the
/// turn on a durable recovery wait naming that exact attempt, so the effect is
/// never silently repeated and the batch is never reported definitively
/// failed.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s06_ambiguous_external_effect_parks_a_durable_recovery_wait() -> Result<(), Box<dyn Error>>
{
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0x8180;
    // The proposal must name an external-effect tool. An effect-free request
    // would never reach this path in production: the catalog fixes its effect
    // class, the application prepares an attempt of that class, and the domain
    // rejects an ambiguous observation against an effect-free attempt outright.
    // The class therefore comes from the catalog declaration rather than from
    // this test, so the fixture cannot claim a pairing no catalog prepares.
    let catalog = ambiguity_fixture_catalog();
    let declared_effect_class = catalog
        .definition(
            &ToolName::try_new(String::from(AMBIGUITY_FIXTURE_TOOL))
                .expect("the fixture tool name is admitted"),
        )
        .expect("the fixture catalog declares the proposed tool")
        .effect_class();
    let (fixture, _, _, request) =
        checkpoint_confirmed_tool_round(&pool, seed, AMBIGUITY_FIXTURE_TOOL, "{}").await?;
    let tool_repository = PostgresToolLoopRepository::new(pool.clone());
    let continuation_attempt = TurnAttemptId::from_uuid(Uuid::from_u128(seed + 0x22));
    tool_repository
        .decide(
            decide_tool_request(
                DurableCommandId::from_uuid(Uuid::from_u128(seed + 0x21)),
                request,
                ToolApprovalDecision::Approve,
            ),
            || continuation_attempt,
        )
        .await?;
    let tool_attempt = ToolAttemptId::from_uuid(Uuid::from_u128(seed + 0x23));
    tool_repository
        .prepare_next_attempt(
            fixture.session,
            fixture.turn,
            tool_attempt,
            declared_effect_class,
        )
        .await?;
    let authorized_attempt = tool_repository
        .authorize_attempt(fixture.session, fixture.turn, tool_attempt)
        .await?;

    let ended = tool_repository
        .commit_observation(
            authorized_attempt
                .executor_fence()
                .bind(ToolAttemptObservation::Ambiguous),
        )
        .await?;
    let parked_attempt: Uuid = sqlx::query_scalar(
        "SELECT recovery_tool_attempt_id
           FROM turn_lifecycle
          WHERE turn_id = $1
            AND session_id = $2
            AND active_phase_kind = 'awaiting_tool_recovery'",
    )
    .bind(fixture.turn.into_uuid())
    .bind(fixture.session.into_uuid())
    .fetch_one(&pool)
    .await?;
    let mut dispatched = Vec::new();
    drain_outbox(&pool, |event| dispatched.push(event.kind().clone())).await?;

    assert_eq!(ended.attempt(), tool_attempt);
    assert_eq!(ended.end(), &ToolAttemptEnd::Ambiguous);
    assert_eq!(parked_attempt, tool_attempt.into_uuid());
    // The claim is that ambiguous work is *never* announced definitively, so
    // membership is not enough: an outbox that carried both this recovery and a
    // `ResultsProjected` for the same batch would satisfy `contains` while
    // reporting the effect resolved. Pin the whole ordered transition set for
    // this exact turn and producing call — the fixture's proposal, then the
    // recovery — so any additional terminal announcement breaks the shape.
    let batch_transitions = announced_batch_states(&dispatched, fixture.turn, fixture.call);
    let [
        DispatchedToolBatchState::Proposed { .. },
        DispatchedToolBatchState::RecoveryRequired {
            attempt: recovering,
        },
    ] = batch_transitions.as_slice()
    else {
        panic!(
            "an ambiguous external effect announces exactly its proposal and its recovery, \
             got {batch_transitions:?}"
        )
    };
    assert_eq!(
        *recovering, tool_attempt,
        "the recovery names the exact ambiguous attempt the gate waits on"
    );
    // The turn itself must not be reported resolved either: a definitive turn
    // outcome is the other way this batch could be announced as settled, and
    // the projection above forces every outbox kind to be classified as
    // definitive or not rather than assumed harmless.
    assert!(
        !announces_a_definitive_turn_outcome(&dispatched, fixture.turn, fixture.call),
        "an ambiguous external effect must not also report its turn definitively resolved, \
         got {dispatched:?}"
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// S02 / S10: a provider refusal on the continuation model call of
/// a completed tool round terminalizes the turn naming that call, and the
/// committed refused terminal shape reloads through the scheduling
/// projection — the startup scan completes and the next submit is accepted
/// instead of the session becoming permanently unloadable.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s02_s10_refused_continuation_call_reloads_and_scans() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0x8300;
    let (fixture, model_repository, continuation_call, authorized) =
        authorize_continuation_after_completed_round(&pool, seed).await?;
    let refused_observation = authorized
        .observation_correlation()
        .bind_terminal_observation(ModelCallTerminalObservation::Refused);
    let refused_outcome = model_repository
        .apply_terminal_observation(
            fixture.session,
            refused_observation,
            ModelCallTerminalIdentities::Refused(RefusedModelCallTurnIdentities::new(
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 0x36)),
            )),
            |_| panic!("the fixture has no pending steering to reclassify"),
        )
        .await?;
    assert!(
        matches!(refused_outcome, ModelCallTerminalOutcome::Refused(_)),
        "the provider refusal terminalizes the continuation call's turn"
    );
    #[derive(Debug, PartialEq, sqlx::FromRow)]
    struct RefusedContinuationShape {
        turn_disposition: String,
        terminal_model_call_id: Option<Uuid>,
        call_disposition: String,
    }
    let refused_shape: RefusedContinuationShape = sqlx::query_as(
        "SELECT lifecycle.terminal_disposition_kind AS turn_disposition,
                lifecycle.terminal_model_call_id,
                continuation.terminal_disposition_kind AS call_disposition
           FROM turn_lifecycle AS lifecycle
           JOIN model_call AS continuation
             ON continuation.session_id = lifecycle.session_id
            AND continuation.model_call_id = lifecycle.terminal_model_call_id
          WHERE lifecycle.session_id = $1
            AND lifecycle.turn_id = $2",
    )
    .bind(fixture.session.into_uuid())
    .bind(fixture.turn.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        refused_shape,
        RefusedContinuationShape {
            turn_disposition: String::from("refused"),
            terminal_model_call_id: Some(continuation_call.into_uuid()),
            call_disposition: String::from("refused"),
        },
        "the refusal names the round's own continuation call"
    );

    let mut recovery_ids = FixedStartupScanIds::new([], []);
    let scan = PostgresStartupScanRepository::new(pool.clone())
        .recover(
            fixture.session,
            signalbox_domain::AcceptedInputTurnFailureIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 0x31)),
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 0x32)),
            ),
            &mut recovery_ids,
        )
        .await?;
    assert_eq!(
        scan,
        StartupScanSessionOutcome::NoActiveTurn,
        "writer-produced refused continuation history must reconstitute at startup"
    );

    let post_refusal = SubmitInputRepository::new(pool.clone())
        .handle(
            start_input(
                seed + 0x33,
                seed + 1,
                "work after refused continuation",
                1,
                ModelSelectionOverride::UseSessionDefault,
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(seed + 0x34)),
            Some(TurnId::from_uuid(Uuid::from_u128(seed + 0x35))),
        )
        .await?;
    assert!(
        matches!(
            post_refusal,
            SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Applied(
                SubmitInputAppliedResult::TurnOrigin(_)
            ))
        ),
        "the refused terminal continuation shape must reconstitute before the next submit"
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// S04: a daemon restart with the continuation model call
/// of a completed tool round in flight classifies the call as ambiguous and
/// parks the turn awaiting a user recovery decision — the committed
/// recovery wait reloads through the scheduling projection, the reconcile
/// verb's precondition still names the parked turn, and the reconciling
/// interrupt terminalizes the turn naming that call.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s04_in_flight_continuation_call_restart_parks_recovery() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0x8500;
    let (fixture, _, continuation_call, _) =
        authorize_continuation_after_completed_round(&pool, seed).await?;

    let mut recovery_ids = FixedStartupScanIds::new([], []);
    let scan = PostgresStartupScanRepository::new(pool.clone())
        .recover(
            fixture.session,
            signalbox_domain::AcceptedInputTurnFailureIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 0x31)),
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 0x32)),
            ),
            &mut recovery_ids,
        )
        .await?;
    let StartupScanSessionOutcome::RecoveredModelCall(recovered) = scan else {
        panic!("the startup scan classifies the in-flight continuation call instead of aborting");
    };
    assert!(
        matches!(*recovered, ModelCallTerminalOutcome::AwaitingRecovery(_)),
        "the lost in-flight continuation call parks awaiting a user decision"
    );

    let mut second_scan_ids = FixedStartupScanIds::new([], []);
    let second_scan = PostgresStartupScanRepository::new(pool.clone())
        .recover(
            fixture.session,
            signalbox_domain::AcceptedInputTurnFailureIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 0x33)),
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 0x34)),
            ),
            &mut second_scan_ids,
        )
        .await?;
    assert_eq!(
        second_scan,
        StartupScanSessionOutcome::AwaitingRecoveryDecision { turn: fixture.turn },
        "the committed continuation recovery wait must reconstitute at the next startup"
    );

    assert_eq!(
        ProcessReadRepository::new(pool.clone())
            .model_call_recovery_precondition(fixture.session)
            .await?,
        ProcessModelCallRecoveryPrecondition::Parked { turn: fixture.turn },
        "the reconcile verb's precondition names the parked continuation turn"
    );

    let successor = TurnId::from_uuid(Uuid::from_u128(seed + 0x40));
    let reconcile_outcome = SubmitInputRepository::new(pool.clone())
        .handle(
            input_with_delivery(
                seed + 0x41,
                seed + 1,
                "reconcile the parked continuation call",
                DeliveryRequest::Interrupt {
                    expected_active_turn: fixture.turn,
                    descendant_scope: DescendantTerminationScope::ParentAlone,
                    configuration: input_choices(1, ModelSelectionOverride::UseSessionDefault),
                },
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(seed + 0x42)),
            Some(successor),
        )
        .await?;
    assert!(
        matches!(
            reconcile_outcome,
            SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Applied(_))
        ),
        "the reconcile verb's interrupt applies against the parked continuation turn"
    );
    let reconciled_shape: (String, Option<Uuid>) = sqlx::query_as(
        "SELECT terminal_disposition_kind, terminal_model_call_id
           FROM turn_lifecycle
          WHERE session_id = $1
            AND turn_id = $2",
    )
    .bind(fixture.session.into_uuid())
    .bind(fixture.turn.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        reconciled_shape,
        (
            String::from("reconciliation_required"),
            Some(continuation_call.into_uuid()),
        ),
        "reconciliation retains the exact ambiguous continuation call"
    );

    let mut third_scan_ids = FixedStartupScanIds::new([], []);
    let third_scan = PostgresStartupScanRepository::new(pool.clone())
        .recover(
            fixture.session,
            signalbox_domain::AcceptedInputTurnFailureIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 0x43)),
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 0x44)),
            ),
            &mut third_scan_ids,
        )
        .await?;
    assert_eq!(
        third_scan,
        StartupScanSessionOutcome::NoActiveTurn,
        "the reconciliation-required continuation terminal must reconstitute at startup"
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// S04 / S07: a daemon restart with a stop-requested
/// continuation call classifies it as ambiguous under its applied interrupt
/// and terminalizes the turn as reconciliation-required naming that call —
/// the committed terminal shape reloads through the scheduling projection
/// instead of leaving the session permanently unloadable.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s04_s07_stop_requested_continuation_call_restart_reconciles() -> Result<(), Box<dyn Error>>
{
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0x8700;
    let (fixture, _, continuation_call, _) =
        authorize_continuation_after_completed_round(&pool, seed).await?;

    let successor = TurnId::from_uuid(Uuid::from_u128(seed + 0x30));
    let interrupt_outcome = SubmitInputRepository::new(pool.clone())
        .handle(
            input_with_delivery(
                seed + 0x24,
                seed + 1,
                "stop the in-flight continuation",
                DeliveryRequest::Interrupt {
                    expected_active_turn: fixture.turn,
                    descendant_scope: DescendantTerminationScope::ParentAlone,
                    configuration: input_choices(1, ModelSelectionOverride::UseSessionDefault),
                },
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(seed + 0x25)),
            Some(successor),
        )
        .await?;
    assert!(
        matches!(
            interrupt_outcome,
            SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Applied(_))
        ),
        "the interrupt records a stop request against the in-flight continuation call"
    );

    let mut recovery_ids = FixedStartupScanIds::new([], []);
    let scan = PostgresStartupScanRepository::new(pool.clone())
        .recover(
            fixture.session,
            signalbox_domain::AcceptedInputTurnFailureIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 0x31)),
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 0x32)),
            ),
            &mut recovery_ids,
        )
        .await?;
    let StartupScanSessionOutcome::RecoveredModelCall(recovered) = scan else {
        panic!("the startup scan classifies the stop-requested continuation call");
    };
    assert!(
        matches!(
            *recovered,
            ModelCallTerminalOutcome::ReconciliationRequired(_)
        ),
        "the lost stop-requested continuation call requires reconciliation"
    );
    let reconciled_shape: (String, Option<Uuid>) = sqlx::query_as(
        "SELECT terminal_disposition_kind, terminal_model_call_id
           FROM turn_lifecycle
          WHERE session_id = $1
            AND turn_id = $2",
    )
    .bind(fixture.session.into_uuid())
    .bind(fixture.turn.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        reconciled_shape,
        (
            String::from("reconciliation_required"),
            Some(continuation_call.into_uuid()),
        ),
        "restart reconciliation names the stop-requested continuation call"
    );

    let mut second_scan_ids = FixedStartupScanIds::new([], []);
    let second_scan = PostgresStartupScanRepository::new(pool.clone())
        .recover(
            fixture.session,
            signalbox_domain::AcceptedInputTurnFailureIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 0x33)),
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 0x34)),
            ),
            &mut second_scan_ids,
        )
        .await?;
    assert_eq!(
        second_scan,
        StartupScanSessionOutcome::NoActiveTurn,
        "the reconciliation-required continuation terminal must reconstitute at startup"
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// an immediate interrupt after an approved
/// attempt checkpoint classifies the unsent attempt, closes its logical
/// request, and terminalizes through the applied interrupt atomically.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn interrupt_closes_checkpointed_tool_execution() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0x7480;
    let (fixture, _, _, request) =
        checkpoint_confirmed_tool_round(&pool, seed, "current_time", "{}").await?;
    let tool_repository = PostgresToolLoopRepository::new(pool.clone());
    tool_repository
        .decide(
            decide_tool_request(
                DurableCommandId::from_uuid(Uuid::from_u128(seed + 23)),
                request,
                ToolApprovalDecision::Approve,
            ),
            || TurnAttemptId::from_uuid(Uuid::from_u128(seed + 24)),
        )
        .await?;
    let tool_attempt = ToolAttemptId::from_uuid(Uuid::from_u128(seed + 25));
    tool_repository
        .prepare_next_attempt(
            fixture.session,
            fixture.turn,
            tool_attempt,
            ToolEffectClass::EffectFree,
        )
        .await?;

    let outcome = SubmitInputRepository::new(pool.clone())
        .handle(
            input_with_delivery(
                seed + 26,
                seed + 1,
                "stop checkpointed tool",
                DeliveryRequest::Interrupt {
                    expected_active_turn: fixture.turn,
                    descendant_scope: DescendantTerminationScope::ParentAlone,
                    configuration: input_choices(1, ModelSelectionOverride::UseSessionDefault),
                },
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(seed + 27)),
            Some(TurnId::from_uuid(Uuid::from_u128(seed + 28))),
        )
        .await?;
    assert!(matches!(
        outcome,
        SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Applied(_))
    ));
    assert!(
        tool_repository
            .prepare_next_attempt(
                fixture.session,
                fixture.turn,
                ToolAttemptId::from_uuid(Uuid::from_u128(seed + 29)),
                ToolEffectClass::EffectFree,
            )
            .await?
            .is_none(),
        "a winning interrupt makes stale attempt preparation a clean no-op"
    );

    let rows: Vec<String> = sqlx::query_scalar(
        "SELECT entry.payload_kind
           FROM semantic_transcript_entry AS entry
          WHERE entry.source_session_id = $1
            AND (
                entry.tool_result_attempt_id = $2
                OR entry.cancelled_turn_id = $3
            )",
    )
    .bind(fixture.session.into_uuid())
    .bind(tool_attempt.into_uuid())
    .bind(fixture.turn.into_uuid())
    .fetch_all(&pool)
    .await?;
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().any(|row| row == "tool_execution_result"));
    assert!(rows.iter().any(|row| row == "turn_cancelled"));
    let attempt_end: (String, String) = sqlx::query_as(
        "SELECT terminal_disposition_kind, error_kind
           FROM tool_attempt
          WHERE attempt_id = $1",
    )
    .bind(tool_attempt.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        attempt_end,
        (String::from("known_failed"), String::from("crash_lost"))
    );

    let (disposition, terminal_frontier_id): (String, Uuid) = sqlx::query_as(
        "SELECT terminal_disposition_kind, terminal_frontier_id
           FROM turn_lifecycle
          WHERE session_id = $1
            AND turn_id = $2",
    )
    .bind(fixture.session.into_uuid())
    .bind(fixture.turn.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(disposition, "cancelled");

    let selection = signalbox_domain::DirectModelSelection::from_uuid(Uuid::from_u128(seed + 5));
    let provider = ProviderModelIdentity::from_uuid(Uuid::from_u128(seed + 6));
    let targets = ModelTargetCatalog::try_from_definitions([ModelTargetDefinition::new(
        selection,
        ResolvedProviderTarget::naming(provider),
    )])
    .expect("one continuation target forms a catalog");
    let stale_continuation = PostgresToolLoopRepository::with_model_calls(
        pool.clone(),
        targets,
        model_credential_reference(),
    )
    .prepare_continuation(
        fixture.session,
        fixture.turn,
        fixture.call,
        signalbox_application::ToolContinuationIdentities::new(
            vec![SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(
                seed + 29,
            ))],
            ContextFrontierId::from_uuid(Uuid::from_u128(seed + 30)),
            ModelCallId::from_uuid(Uuid::from_u128(seed + 31)),
            FailedModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 32)),
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 33)),
            ),
            ContextFrontierId::from_uuid(Uuid::from_u128(seed + 34)),
        ),
        |_| panic!("an interrupted batch cannot consume steering"),
    )
    .await?;
    assert_eq!(
        stale_continuation,
        signalbox_application::PrepareToolContinuationOutcome::NoWork,
        "an interrupt that consumed the batch makes a stale continuation hint no work"
    );

    let cancellation_entry_id: Uuid = sqlx::query_scalar(
        "SELECT semantic_entry_id
           FROM semantic_transcript_entry
          WHERE cancelled_turn_id = $1
            AND payload_kind = 'turn_cancelled'",
    )
    .bind(fixture.turn.into_uuid())
    .fetch_one(&pool)
    .await?;

    let cancellation_events = drain_cancellation_dispatches(&pool).await?;
    assert_eq!(
        cancellation_events,
        vec![(
            fixture.session,
            fixture.turn,
            SemanticTranscriptEntryId::from_uuid(cancellation_entry_id),
            ContextFrontierId::from_uuid(terminal_frontier_id),
        )]
    );

    let follow_up = SubmitInputRepository::new(pool.clone())
        .handle(
            start_input(
                seed + 40,
                seed + 1,
                "work after cancelled tool round",
                1,
                ModelSelectionOverride::UseSessionDefault,
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(seed + 41)),
            Some(TurnId::from_uuid(Uuid::from_u128(seed + 42))),
        )
        .await?;
    assert!(
        matches!(
            follow_up,
            SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Applied(
                SubmitInputAppliedResult::TurnOrigin(_)
            ))
        ),
        "writer-produced cancelled tool history must reconstitute before the next submit"
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// S07 / S10: an interrupt against a parked approval wait
/// records the authoritative typed rejection instead of failing the submit
/// transaction, the wait remains durably parked with no accepted input, and
/// equal replay returns the recorded rejection.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s07_s10_parked_approval_interrupt_records_typed_rejection() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0x7e00;
    let (fixture, _, _, request) =
        checkpoint_confirmed_tool_round(&pool, seed, "current_time", "{}").await?;
    let interrupt_command = Uuid::from_u128(seed + 23);
    let parked_before: (String, Option<Uuid>) = sqlx::query_as(
        "SELECT active_phase_kind, approval_tool_request_id
           FROM turn_lifecycle
          WHERE session_id = $1
            AND turn_id = $2",
    )
    .bind(fixture.session.into_uuid())
    .bind(fixture.turn.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        parked_before,
        (
            String::from("awaiting_tool_approval"),
            Some(request.into_uuid()),
        ),
        "the confirmed tool round must be parked before the interrupt"
    );
    assert_eq!(
        signalbox_persistence::turn_liveness::PostgresTurnLivenessRepository::new(
            pool.clone(),
            signalbox_persistence::turn_liveness::TurnLivenessPersistenceBounds::new(
                Some(std::time::Duration::from_millis(7)),
                Some(std::time::Duration::from_millis(11)),
                Some(std::time::Duration::from_millis(13)),
            ),
        )
        .slot_held_active_turns(None)
        .await?
        .candidates(),
        [],
        "the slot-held watchdog never treats an approval wait as daemon-owned work"
    );

    let interrupt = input_with_delivery(
        seed + 23,
        seed + 1,
        "stop while confirm is pending",
        DeliveryRequest::Interrupt {
            expected_active_turn: fixture.turn,
            descendant_scope: DescendantTerminationScope::ParentAndDescendants,
            configuration: input_choices(1, ModelSelectionOverride::UseSessionDefault),
        },
    );
    let outcome = SubmitInputRepository::new(pool.clone())
        .handle(
            interrupt.clone(),
            AcceptedInputId::from_uuid(Uuid::from_u128(seed + 24)),
            Some(TurnId::from_uuid(Uuid::from_u128(seed + 25))),
        )
        .await?;
    assert_eq!(
        outcome,
        SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Rejected(
            SubmitInputRejectedResult::InterruptUnavailableWhileAwaitingApproval {
                session: fixture.session,
                active_turn: fixture.turn,
            },
        )),
        "an interrupt alone must not bypass the decision command",
    );

    let parked_after: (String, Option<Uuid>, i64) = sqlx::query_as(
        "SELECT active_phase_kind, approval_tool_request_id,
                (SELECT count(*) FROM accepted_input
                  WHERE accepting_command_id = $3)
           FROM turn_lifecycle
          WHERE session_id = $1
            AND turn_id = $2",
    )
    .bind(fixture.session.into_uuid())
    .bind(fixture.turn.into_uuid())
    .bind(interrupt_command)
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        parked_after,
        (
            String::from("awaiting_tool_approval"),
            Some(request.into_uuid()),
            0,
        ),
        "the approval wait must remain parked and the rejection must accept no input"
    );

    let replayed = SubmitInputRepository::new(pool.clone())
        .handle(
            interrupt,
            AcceptedInputId::from_uuid(Uuid::from_u128(seed + 26)),
            Some(TurnId::from_uuid(Uuid::from_u128(seed + 27))),
        )
        .await?;
    assert_eq!(
        replayed,
        SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Rejected(
            SubmitInputRejectedResult::InterruptUnavailableWhileAwaitingApproval {
                session: fixture.session,
                active_turn: fixture.turn,
            },
        )),
        "equal replay must return the recorded parked-approval rejection",
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// S07 / S10: a parked-approval interrupt rejection is
/// authoritative only against a turn the database still records as active on
/// its approval wait. The row shape proves only that the receipt names the
/// turn the command expected, so the deferred correlation trigger proves the
/// phase: a directly inserted receipt naming a running or a terminal turn
/// cannot commit and therefore never replays as authoritative.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s07_s10_parked_approval_rejection_requires_a_recorded_approval_wait()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;

    let running_seed = 0x7f00;
    let running = checkpoint_restart_model_call(&pool, running_seed, false).await?;
    let running_phase: (String, Option<String>) = sqlx::query_as(
        "SELECT state_kind, active_phase_kind
           FROM turn_lifecycle
          WHERE session_id = $1
            AND turn_id = $2",
    )
    .bind(running.session.into_uuid())
    .bind(running.turn.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        running_phase,
        (String::from("active"), Some(String::from("running"))),
        "the fixture turn must be running before the forged receipt names it"
    );
    let running_error = insert_parked_approval_interrupt_rejection(
        &pool,
        Uuid::from_u128(running_seed + 0x30),
        Uuid::from_u128(running_seed + 8),
        running.turn.into_uuid(),
    )
    .await
    .expect_err("a parked-approval rejection naming a running turn is corruption");
    assert_eq!(
        running_error
            .as_database_error()
            .and_then(|error| error.code()),
        Some("23503".into())
    );
    assert!(
        running_error
            .to_string()
            .contains("incomplete or cross-wired effect"),
        "the correlation trigger must refuse the running turn: {running_error}"
    );

    let terminal_seed = 0x7f80;
    let terminal = checkpoint_restart_model_call(&pool, terminal_seed, false).await?;
    let selection =
        signalbox_domain::DirectModelSelection::from_uuid(Uuid::from_u128(terminal_seed + 5));
    let provider = ProviderModelIdentity::from_uuid(Uuid::from_u128(terminal_seed + 6));
    let targets = ModelTargetCatalog::try_from_definitions([ModelTargetDefinition::new(
        selection,
        ResolvedProviderTarget::naming(provider),
    )])
    .expect("one restart fixture target forms a catalog");
    PostgresModelCallRepository::new(pool.clone(), targets, model_credential_reference())
        .fail_prepared_call(
            terminal.session,
            terminal.call,
            PreparedModelCallFailureCause::ToolRoundLimitReached,
            None,
            FailedModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(terminal_seed + 14)),
                ContextFrontierId::from_uuid(Uuid::from_u128(terminal_seed + 15)),
            ),
            |_| panic!("the fixture has no pending steering to reclassify"),
        )
        .await?;
    let terminal_phase: (String, Option<String>) = sqlx::query_as(
        "SELECT state_kind, active_phase_kind
           FROM turn_lifecycle
          WHERE session_id = $1
            AND turn_id = $2",
    )
    .bind(terminal.session.into_uuid())
    .bind(terminal.turn.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        terminal_phase,
        (String::from("terminal"), None),
        "the fixture turn must be terminal before the forged receipt names it"
    );
    let terminal_error = insert_parked_approval_interrupt_rejection(
        &pool,
        Uuid::from_u128(terminal_seed + 0x30),
        Uuid::from_u128(terminal_seed + 8),
        terminal.turn.into_uuid(),
    )
    .await
    .expect_err("a parked-approval rejection naming a terminal turn is corruption");
    assert_eq!(
        terminal_error
            .as_database_error()
            .and_then(|error| error.code()),
        Some("23503".into())
    );
    assert!(
        terminal_error
            .to_string()
            .contains("incomplete or cross-wired effect"),
        "the correlation trigger must refuse the terminal turn: {terminal_error}"
    );

    let forged: i64 = sqlx::query_scalar(
        "SELECT count(*)
           FROM submit_input_command
          WHERE rejection_kind = 'interrupt_unavailable_while_awaiting_approval'",
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(forged, 0, "no forged parked-approval receipt may survive");

    pool.close().await;
    drop(container);
    Ok(())
}

/// an interrupt against an external
/// tool recovery wait releases the slot as reconciliation-required while
/// retaining the exact ambiguous tool attempt and closing its logical request.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn interrupt_preserves_tool_recovery_ambiguity() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0x74c0;
    let (fixture, _, _, request) =
        checkpoint_confirmed_tool_round(&pool, seed, "external-tool", "{}").await?;
    let repository = PostgresToolLoopRepository::new(pool.clone());
    let issuing_attempt = TurnAttemptId::from_uuid(Uuid::from_u128(seed + 23));
    repository
        .decide(
            decide_tool_request(
                DurableCommandId::from_uuid(Uuid::from_u128(seed + 24)),
                request,
                ToolApprovalDecision::Approve,
            ),
            || issuing_attempt,
        )
        .await?;
    let tool_attempt = ToolAttemptId::from_uuid(Uuid::from_u128(seed + 25));
    repository
        .prepare_next_attempt(
            fixture.session,
            fixture.turn,
            tool_attempt,
            ToolEffectClass::ExternalEffect,
        )
        .await?;
    repository
        .authorize_attempt(fixture.session, fixture.turn, tool_attempt)
        .await?;
    let mut recovery_ids = FixedStartupScanIds::new([], []);
    assert_ambiguous_tool_recovery(
        PostgresStartupScanRepository::new(pool.clone())
            .recover(
                fixture.session,
                signalbox_domain::AcceptedInputTurnFailureIdentities::new(
                    SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 26)),
                    ContextFrontierId::from_uuid(Uuid::from_u128(seed + 27)),
                ),
                &mut recovery_ids,
            )
            .await?,
    );

    let successor = TurnId::from_uuid(Uuid::from_u128(seed + 30));
    let outcome = SubmitInputRepository::new(pool.clone())
        .handle(
            input_with_delivery(
                seed + 28,
                seed + 1,
                "stop ambiguous tool",
                DeliveryRequest::Interrupt {
                    expected_active_turn: fixture.turn,
                    descendant_scope: DescendantTerminationScope::ParentAlone,
                    configuration: input_choices(1, ModelSelectionOverride::UseSessionDefault),
                },
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(seed + 29)),
            Some(successor),
        )
        .await?;
    assert!(matches!(
        outcome,
        SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Applied(_))
    ));

    #[derive(Debug, PartialEq, sqlx::FromRow)]
    struct DurableToolReconciliationFacts {
        terminal_disposition_kind: String,
        terminal_model_call_id: Option<Uuid>,
        terminal_tool_attempt_id: Option<Uuid>,
        outbox_event_count: i64,
    }
    let durable: DurableToolReconciliationFacts = sqlx::query_as(
        "SELECT lifecycle.terminal_disposition_kind,
                lifecycle.terminal_model_call_id,
                lifecycle.terminal_tool_attempt_id,
                (SELECT count(*)
                   FROM turn_terminal_outbox_event AS event
                  WHERE event.disposition_kind = 'reconciliation_required'
                  AND event.session_id = lifecycle.session_id
                    AND event.turn_id = lifecycle.turn_id
                    AND event.model_call_id IS NULL
                    AND event.tool_attempt_id = $3
                    AND event.terminal_frontier_id =
                        lifecycle.terminal_frontier_id) AS outbox_event_count
           FROM turn_lifecycle AS lifecycle
          WHERE lifecycle.session_id = $1
            AND lifecycle.turn_id = $2",
    )
    .bind(fixture.session.into_uuid())
    .bind(fixture.turn.into_uuid())
    .bind(tool_attempt.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        durable,
        DurableToolReconciliationFacts {
            terminal_disposition_kind: String::from("reconciliation_required"),
            terminal_model_call_id: None,
            terminal_tool_attempt_id: Some(tool_attempt.into_uuid()),
            outbox_event_count: 1,
        }
    );
    let snapshot = ProcessReadRepository::new(pool.clone())
        .read_transcript(fixture.session)
        .await?
        .expect("tool reconciliation remains process-readable");
    assert_eq!(
        process_tool_reconciliation_operation(snapshot.turns()[0].state()),
        (issuing_attempt, tool_attempt)
    );
    assert_eq!(assistant_tool_request(snapshot.entries()), request);
    assert_eq!(closed_tool_request(snapshot.entries()), request);
    assert!(
        dispatched_tool_reconciliation(&pool, fixture.turn, tool_attempt).await?,
        "the tool reconciliation event must not block dispatch"
    );

    assert_eq!(
        activated_turn(
            StartEligibleTurnRepository::new(pool.clone())
                .handle(
                    fixture.session,
                    AcceptedInputTurnActivationIdentities::new(
                        SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 34)),
                        SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 31)),
                        ContextFrontierId::from_uuid(Uuid::from_u128(seed + 32)),
                        TurnAttemptId::from_uuid(Uuid::from_u128(seed + 33)),
                    ),
                )
                .await?,
        ),
        successor
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// the daemon's bounded recovery
/// ledger terminalizes an exact tool ambiguity without inventing a user
/// interrupt or erasing the physical outcome.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn automatic_tool_reconciliation_releases_the_slot() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0x74d0;
    let (fixture, _, _, request) =
        checkpoint_confirmed_tool_round(&pool, seed, "external-tool", "{}").await?;
    let tool_repository = PostgresToolLoopRepository::new(pool.clone());
    let issuing_attempt = TurnAttemptId::from_uuid(Uuid::from_u128(seed + 23));
    tool_repository
        .decide(
            decide_tool_request(
                DurableCommandId::from_uuid(Uuid::from_u128(seed + 24)),
                request,
                ToolApprovalDecision::Approve,
            ),
            || issuing_attempt,
        )
        .await?;
    let tool_attempt = ToolAttemptId::from_uuid(Uuid::from_u128(seed + 25));
    tool_repository
        .prepare_next_attempt(
            fixture.session,
            fixture.turn,
            tool_attempt,
            ToolEffectClass::ExternalEffect,
        )
        .await?;
    tool_repository
        .authorize_attempt(fixture.session, fixture.turn, tool_attempt)
        .await?;
    let mut recovery_ids = FixedStartupScanIds::new([], []);
    assert_ambiguous_tool_recovery(
        PostgresStartupScanRepository::new(pool.clone())
            .recover(
                fixture.session,
                signalbox_domain::AcceptedInputTurnFailureIdentities::new(
                    SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 26)),
                    ContextFrontierId::from_uuid(Uuid::from_u128(seed + 27)),
                ),
                &mut recovery_ids,
            )
            .await?,
    );

    let repository = PostgresAutomaticReconciliationRepository::new(pool.clone());
    let batch = repository.claim_due().await?;
    assert_eq!(batch.claimed().len(), 1);
    assert_eq!(
        batch.claimed()[0].operation(),
        AutomaticReconciliationOperation::ToolAttempt(tool_attempt)
    );
    assert_eq!(
        repository.reconcile(batch.claimed()[0]).await?,
        AutomaticReconciliationOutcome::Reconciled
    );
    let durable: (String, String, i32, i64) = sqlx::query_as(
        "SELECT lifecycle.terminal_disposition_kind,
                recovery.state_kind,
                recovery.attempt_count,
                (SELECT count(*)
                   FROM automatic_reconciliation_attempt AS attempt
                  WHERE attempt.turn_id = recovery.turn_id
                    AND attempt.outcome_kind = 'reconciled')
           FROM turn_lifecycle AS lifecycle
           JOIN automatic_reconciliation AS recovery
             ON recovery.turn_id = lifecycle.turn_id
          WHERE lifecycle.turn_id = $1",
    )
    .bind(fixture.turn.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        durable,
        (
            String::from("reconciliation_required"),
            String::from("reconciled"),
            1,
            1
        )
    );
    let snapshot = ProcessReadRepository::new(pool.clone())
        .read_transcript(fixture.session)
        .await?
        .expect("automatically reconciled tool ambiguity remains readable");
    assert_eq!(
        process_tool_reconciliation_operation(snapshot.turns()[0].state()),
        (issuing_attempt, tool_attempt)
    );
    assert_eq!(closed_tool_request(snapshot.entries()), request);

    pool.close().await;
    drop(container);
    Ok(())
}

/// eligibility replays pending
/// steering reclassified behind an interrupted ambiguous tool attempt without
/// requiring that reconciliation predecessor to own a model call.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn replays_reclassified_tool_reconciliation_predecessor() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0x74e0;
    let (fixture, _, _, request) =
        checkpoint_confirmed_tool_round(&pool, seed, "external-tool", "{}").await?;
    let repository = PostgresToolLoopRepository::new(pool.clone());
    let issuing_attempt = TurnAttemptId::from_uuid(Uuid::from_u128(seed + 23));
    repository
        .decide(
            decide_tool_request(
                DurableCommandId::from_uuid(Uuid::from_u128(seed + 24)),
                request,
                ToolApprovalDecision::Approve,
            ),
            || issuing_attempt,
        )
        .await?;
    let tool_attempt = ToolAttemptId::from_uuid(Uuid::from_u128(seed + 25));
    repository
        .prepare_next_attempt(
            fixture.session,
            fixture.turn,
            tool_attempt,
            ToolEffectClass::ExternalEffect,
        )
        .await?;
    repository
        .authorize_attempt(fixture.session, fixture.turn, tool_attempt)
        .await?;

    let pending_steering = AcceptedInputId::from_uuid(Uuid::from_u128(seed + 35));
    let steering_command = DurableCommandId::from_uuid(Uuid::from_u128(seed + 36));
    let steering_session = fixture.session;
    let steering_input = SubmitInput::new(
        steering_command,
        steering_session,
        UserContent::try_text(String::from("steer while the tool attempt is ambiguous"))
            .expect("test steering content is admitted"),
        DeliveryRequest::NextSafePoint {
            expected_active_turn: fixture.turn,
        },
    );
    assert!(matches!(
        SubmitInputRepository::new(pool.clone())
            .handle(steering_input, pending_steering, None)
            .await?,
        SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Applied(
            SubmitInputAppliedResult::PendingSteering(_)
        ))
    ));

    let mut recovery_ids = FixedStartupScanIds::new([], []);
    assert_ambiguous_tool_recovery(
        PostgresStartupScanRepository::new(pool.clone())
            .recover(
                fixture.session,
                signalbox_domain::AcceptedInputTurnFailureIdentities::new(
                    SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 26)),
                    ContextFrontierId::from_uuid(Uuid::from_u128(seed + 27)),
                ),
                &mut recovery_ids,
            )
            .await?,
    );

    let successor = TurnId::from_uuid(Uuid::from_u128(seed + 30));
    let interrupt_command = DurableCommandId::from_uuid(Uuid::from_u128(seed + 28));
    let interrupt_session = fixture.session;
    let interrupt_input = SubmitInput::new(
        interrupt_command,
        interrupt_session,
        UserContent::try_text(String::from("stop ambiguous tool"))
            .expect("test interrupt content is admitted"),
        DeliveryRequest::Interrupt {
            expected_active_turn: fixture.turn,
            descendant_scope: DescendantTerminationScope::ParentAlone,
            configuration: input_choices(1, ModelSelectionOverride::UseSessionDefault),
        },
    );
    let interrupt = SubmitInputRepository::new(pool.clone())
        .handle(
            interrupt_input,
            AcceptedInputId::from_uuid(Uuid::from_u128(seed + 29)),
            Some(successor),
        )
        .await?;
    assert!(matches!(
        interrupt,
        SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Applied(_))
    ));

    let stored_steering: StoredReclassifiedSteeringFacts = sqlx::query_as(
        "SELECT disposition_kind, origin_turn_id
           FROM accepted_input
          WHERE accepted_input_id = $1",
    )
    .bind(pending_steering.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        ReclassifiedSteeringFacts {
            disposition_kind: stored_steering.disposition_kind,
            origin: stored_steering.origin_turn_id.into(),
        },
        ReclassifiedSteeringFacts {
            disposition_kind: String::from("reclassified_as_turn_origin"),
            origin: TurnOriginPresence::Present,
        },
    );
    assert_eq!(
        activated_turn(
            StartEligibleTurnRepository::new(pool.clone())
                .handle(
                    fixture.session,
                    AcceptedInputTurnActivationIdentities::new(
                        SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 34)),
                        SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 31)),
                        ContextFrontierId::from_uuid(Uuid::from_u128(seed + 32)),
                        TurnAttemptId::from_uuid(Uuid::from_u128(seed + 33)),
                    ),
                )
                .await?,
        ),
        successor
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// Every session an eligibility sweep reports, following its continuations.
///
/// Paging the sweep is plumbing rather than the behavior under test, so the
/// iteration lives here (`docs/agents/testing-style.md` rules 2 and 3).
async fn swept_sessions(pool: &PgPool) -> Result<Vec<SessionId>, Box<dyn Error>> {
    let mut sweep = PostgresEligibilitySweep::new(pool.clone());
    let mut sessions = Vec::new();
    loop {
        let (page, _dispatch_starts, continuation) = sweep.find_sessions().await?.into_parts();
        sessions.extend(page);
        if !continuation {
            break;
        }
    }
    Ok(sessions)
}

/// Moves one frontier-delta member to `position`.
///
/// The reordering cases differ only in which entry moves where, so the
/// statement lives here and each case states its two knobs at the call site
/// (`docs/agents/testing-style.md` rules 2 and 4).
async fn move_delta_member(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session: Uuid,
    frontier: Uuid,
    entry: Uuid,
    position: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE context_frontier_delta
                SET member_position = $1
              WHERE owning_session_id = $2
                AND context_frontier_id = $3
                AND semantic_entry_id = $4",
    )
    .bind(position)
    .bind(session)
    .bind(frontier)
    .bind(entry)
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

/// S05 / S10 / S11: denial never dispatches,
/// schema failure is durable result evidence, external-effect crash loss parks
/// on exact recovery authority, and effect-free loss closes every request
/// before the turn fails.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s05_s10_s11_tool_failures_close_durably() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let repository = PostgresToolLoopRepository::new(pool.clone());

    let deny_seed = 0x7500;
    let (denied_fixture, _, _, denied_request) =
        checkpoint_confirmed_tool_round(&pool, deny_seed, "dangerous-tool", "{}").await?;
    let approval_snapshot = ProcessReadRepository::new(pool.clone())
        .read_transcript(denied_fixture.session)
        .await?
        .expect("approval waits remain process-readable");
    assert!(matches!(
        approval_snapshot.turns()[0].state(),
        ProcessTurnState::ActiveAwaitingToolApproval { request }
            if *request == denied_request
    ));
    let mut forged_blanket = pool.begin().await?;
    sqlx::query(
        "INSERT INTO tool_approval_decision
            (request_id, decision_kind, decision_source, denial_reason,
             user_command_id)
         VALUES ($1, 'approve', 'session_blanket', NULL, NULL)",
    )
    .bind(denied_request.into_uuid())
    .execute(&mut *forged_blanket)
    .await?;
    let forged_blanket_error =
        sqlx::query("SET CONSTRAINTS tool_approval_session_blanket_provenance IMMEDIATE")
            .execute(&mut *forged_blanket)
            .await
            .expect_err("disabled frozen configuration cannot authorize a blanket approval");
    assert_eq!(
        forged_blanket_error
            .as_database_error()
            .and_then(|error| error.constraint()),
        Some("tool_approval_session_blanket_requires_frozen_approve_all")
    );
    forged_blanket.rollback().await?;
    let malformed_command_error = sqlx::query(
        "INSERT INTO decide_tool_request_command
            (command_id, command_kind, storage_version, request_id,
             decision_kind, denial_reason, result_kind, rejection_kind,
             result_earliest_undecided_request_id)
         VALUES ($1, 'decide_tool_request', 1, $2,
                 'deny', E'unsafe\\nreason', 'applied', NULL, NULL)",
    )
    .bind(Uuid::from_u128(deny_seed + 89))
    .bind(denied_request.into_uuid())
    .execute(&pool)
    .await
    .expect_err("stored decision command reason must reject control characters");
    assert_eq!(
        malformed_command_error
            .as_database_error()
            .and_then(|error| error.constraint()),
        Some("decide_tool_request_command_decision_shape")
    );
    let mut malformed_denial = pool.begin().await?;
    let malformed_command = Uuid::from_u128(deny_seed + 90);
    sqlx::query(
        "INSERT INTO durable_command
            (command_id, command_kind, storage_version, claimed_at, issuer_kind)
         VALUES ($1, 'decide_tool_request', 1, transaction_timestamp(), 'operator')",
    )
    .bind(malformed_command)
    .execute(&mut *malformed_denial)
    .await?;
    sqlx::query(
        "INSERT INTO decide_tool_request_command
            (command_id, command_kind, storage_version, request_id,
             decision_kind, denial_reason, result_kind, rejection_kind,
             result_earliest_undecided_request_id)
         VALUES ($1, 'decide_tool_request', 1, $2,
                 'deny', 'safe', 'applied', NULL, NULL)",
    )
    .bind(malformed_command)
    .bind(denied_request.into_uuid())
    .execute(&mut *malformed_denial)
    .await?;
    let malformed_denial_error = sqlx::query(
        "INSERT INTO tool_approval_decision
            (request_id, decision_kind, decision_source, denial_reason,
             user_command_id)
         VALUES ($1, 'deny', 'user_command', E'unsafe\\nreason', $2)",
    )
    .bind(denied_request.into_uuid())
    .bind(malformed_command)
    .execute(&mut *malformed_denial)
    .await
    .expect_err("stored denial reason must reject control characters");
    assert_eq!(
        malformed_denial_error
            .as_database_error()
            .and_then(|error| error.constraint()),
        Some("tool_approval_decision_shape")
    );
    malformed_denial.rollback().await?;

    let denied_continuation = TurnAttemptId::from_uuid(Uuid::from_u128(deny_seed + 23));
    let denial = repository
        .decide(
            decide_tool_request(
                DurableCommandId::from_uuid(Uuid::from_u128(deny_seed + 24)),
                denied_request,
                ToolApprovalDecision::Deny { reason: None },
            ),
            || denied_continuation,
        )
        .await?;
    assert!(matches!(
        denial.result(),
        DecideToolRequestResult::Applied(applied)
            if matches!(
                applied.resolution().decision(),
                ToolApprovalDecision::Deny { .. }
            )
    ));
    assert!(matches!(
        repository
            .decide(
                decide_tool_request(
                    DurableCommandId::from_uuid(Uuid::from_u128(deny_seed + 25)),
                    denied_request,
                    ToolApprovalDecision::Approve,
                ),
                || panic!("resolved request consumes no identity"),
            )
            .await?
            .result(),
        DecideToolRequestResult::Rejected(
            signalbox_domain::DecideToolRequestRejectedResult::AlreadyResolved { request }
        ) if *request == denied_request
    ));
    let denied_batch = repository
        .load_active_batch(denied_fixture.session, denied_fixture.turn)
        .await?
        .expect("denied batch remains available for reference-only projection");
    assert!(matches!(
        repository
            .prepare_next_attempt(
                denied_fixture.session,
                denied_fixture.turn,
                ToolAttemptId::from_uuid(Uuid::from_u128(deny_seed + 26)),
                ToolEffectClass::ExternalEffect,
            )
            .await,
        Err(ToolLoopRepositoryError::InvalidTransition(
            "batch has no next serialized attempt"
        ))
    ));
    let denied_entry = SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(deny_seed + 27));
    let denied_projection = denied_batch
        .prepare_result_projection(
            vec![denied_entry],
            ContextFrontierId::from_uuid(Uuid::from_u128(deny_seed + 28)),
        )
        .expect("denial is a complete logical result");
    assert_eq!(denied_projection.entries().len(), 1);

    let schema_seed = 0x7600;
    let (schema_fixture, _, _, schema_request) =
        checkpoint_confirmed_tool_round(&pool, schema_seed, "current_time", "{broken").await?;
    let schema_continuation = TurnAttemptId::from_uuid(Uuid::from_u128(schema_seed + 23));
    repository
        .decide(
            decide_tool_request(
                DurableCommandId::from_uuid(Uuid::from_u128(schema_seed + 24)),
                schema_request,
                ToolApprovalDecision::Approve,
            ),
            || schema_continuation,
        )
        .await?;
    let schema_attempt = ToolAttemptId::from_uuid(Uuid::from_u128(schema_seed + 25));
    repository
        .prepare_next_attempt(
            schema_fixture.session,
            schema_fixture.turn,
            schema_attempt,
            ToolEffectClass::EffectFree,
        )
        .await?;
    let mut malformed_error_detail = pool.begin().await?;
    let malformed_detail_error = sqlx::query(
        "UPDATE tool_attempt
            SET state_kind = 'terminal',
                terminal_disposition_kind = 'known_failed',
                error_kind = 'invalid_arguments',
                error_detail = E'unsafe\\ndetail'
          WHERE attempt_id = $1",
    )
    .bind(schema_attempt.into_uuid())
    .execute(&mut *malformed_error_detail)
    .await
    .expect_err("stored execution detail must reject control characters");
    assert_eq!(
        malformed_detail_error
            .as_database_error()
            .and_then(|error| error.constraint()),
        Some("tool_attempt_error_detail_bounded")
    );
    malformed_error_detail.rollback().await?;
    let schema_failure = repository
        .commit_preflight_error(
            schema_fixture.session,
            schema_fixture.turn,
            schema_attempt,
            ToolExecutionError::new(ToolExecutionErrorKind::InvalidArguments, None),
        )
        .await?;
    assert!(matches!(
        schema_failure.end(),
        ToolAttemptEnd::KnownFailed { error }
            if error.kind() == ToolExecutionErrorKind::InvalidArguments
    ));
    let issuing_attempt_state: String = sqlx::query_scalar(
        "SELECT state_kind
           FROM turn_attempt
          WHERE turn_attempt_id = $1",
    )
    .bind(schema_continuation.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        issuing_attempt_state, "running",
        "preflight terminal evidence makes the result projection continuation-eligible"
    );
    let mut completed_attempt_recovery_ids = FixedStartupScanIds::new([], []);
    assert!(matches!(
        PostgresStartupScanRepository::new(pool.clone())
            .recover(
                schema_fixture.session,
                signalbox_domain::AcceptedInputTurnFailureIdentities::new(
                    SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(schema_seed + 90)),
                    ContextFrontierId::from_uuid(Uuid::from_u128(schema_seed + 91)),
                ),
                &mut completed_attempt_recovery_ids,
            )
            .await?,
        StartupScanSessionOutcome::ResumableToolBatch { turn }
            if turn == schema_fixture.turn
    ));
    let recovery_sessions = swept_sessions(&pool).await?;
    assert!(
        recovery_sessions.contains(&schema_fixture.session),
        "the durable sweep must reschedule a resumable active tool batch"
    );
    let schema_batch = repository
        .load_active_batch(schema_fixture.session, schema_fixture.turn)
        .await?
        .expect("schema failure remains exact terminal attempt evidence");
    let schema_entry = SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(schema_seed + 26));
    let schema_projection = schema_batch
        .prepare_result_projection(
            vec![schema_entry],
            ContextFrontierId::from_uuid(Uuid::from_u128(schema_seed + 27)),
        )
        .expect("definitive preflight failure projects as a tool result");
    assert_eq!(schema_projection.entries().len(), 1);

    let crash_seed = 0x7700;
    let (crash_fixture, _, _, crash_request) =
        checkpoint_confirmed_tool_round(&pool, crash_seed, "external-tool", "{}").await?;
    let crash_continuation = TurnAttemptId::from_uuid(Uuid::from_u128(crash_seed + 23));
    repository
        .decide(
            decide_tool_request(
                DurableCommandId::from_uuid(Uuid::from_u128(crash_seed + 24)),
                crash_request,
                ToolApprovalDecision::Approve,
            ),
            || crash_continuation,
        )
        .await?;
    let crash_attempt = ToolAttemptId::from_uuid(Uuid::from_u128(crash_seed + 25));
    repository
        .prepare_next_attempt(
            crash_fixture.session,
            crash_fixture.turn,
            crash_attempt,
            ToolEffectClass::ExternalEffect,
        )
        .await?;
    repository
        .authorize_attempt(crash_fixture.session, crash_fixture.turn, crash_attempt)
        .await?;
    let pending_ambiguous_input = AcceptedInputId::from_uuid(Uuid::from_u128(crash_seed + 29));
    assert!(matches!(
        SubmitInputRepository::new(pool.clone())
            .handle(
                input_with_delivery(
                    crash_seed + 28,
                    crash_seed + 1,
                    "steer while external work is in flight",
                    DeliveryRequest::NextSafePoint {
                        expected_active_turn: crash_fixture.turn,
                    },
                ),
                pending_ambiguous_input,
                None,
            )
            .await?,
        SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Applied(
            SubmitInputAppliedResult::PendingSteering(_)
        ))
    ));
    let mut crash_recovery_ids = FixedStartupScanIds::new([], []);
    assert!(matches!(
        PostgresStartupScanRepository::new(pool.clone())
            .recover(
                crash_fixture.session,
                signalbox_domain::AcceptedInputTurnFailureIdentities::new(
                    SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(crash_seed + 26)),
                    ContextFrontierId::from_uuid(Uuid::from_u128(crash_seed + 27)),
                ),
                &mut crash_recovery_ids,
            )
            .await?,
        StartupScanSessionOutcome::RecoveredToolAttempt(outcome)
            if matches!(*outcome, ToolAttemptCrashOutcome::Ambiguous(_))
    ));
    let restarted = PostgresToolLoopRepository::new(pool.clone())
        .load_active_batch(crash_fixture.session, crash_fixture.turn)
        .await?
        .expect("external-effect ambiguity reloads after restart");
    assert!(matches!(
        restarted.awaiting_recovery(),
        Some(waiting) if waiting.attempt() == crash_attempt
    ));
    let recovery_snapshot = ProcessReadRepository::new(pool.clone())
        .read_transcript(crash_fixture.session)
        .await?
        .expect("tool recovery waits remain process-readable");
    assert!(matches!(
        recovery_snapshot.turns()[0].state(),
        ProcessTurnState::ActiveAwaitingToolRecovery {
            ended_attempt,
            recovery_attempt,
            automatic_reconciliation_attempts,
            operator_action_required,
        } if *ended_attempt == crash_continuation && *recovery_attempt == crash_attempt
            && *automatic_reconciliation_attempts == 0
            && !operator_action_required
    ));
    let pending_ambiguous_disposition: String = sqlx::query_scalar(
        "SELECT disposition_kind
           FROM accepted_input
          WHERE accepted_input_id = $1",
    )
    .bind(pending_ambiguous_input.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(pending_ambiguous_disposition, "pending_steering");

    let durable_shape: (i64, i64, i64) = sqlx::query_as(
        "SELECT
            (SELECT count(*) FROM tool_attempt
              WHERE request_id = $1),
            (SELECT count(*) FROM tool_attempt
              WHERE attempt_id = $2
                AND state_kind = 'terminal'
                AND terminal_disposition_kind = 'known_failed'
                AND error_kind = 'invalid_arguments'),
            (SELECT count(*) FROM turn_lifecycle
              WHERE session_id = $3
                AND turn_id = $4
                AND state_kind = 'active'
                AND active_phase_kind = 'awaiting_tool_recovery'
                AND active_tool_round_call_id = $5
                AND recovery_tool_attempt_id = $6)",
    )
    .bind(denied_request.into_uuid())
    .bind(schema_attempt.into_uuid())
    .bind(crash_fixture.session.into_uuid())
    .bind(crash_fixture.turn.into_uuid())
    .bind(crash_fixture.call.into_uuid())
    .bind(crash_attempt.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(durable_shape, (0, 1, 1));

    let effect_free_seed = 0x7800;
    let (effect_free_fixture, _, _, effect_free_request) =
        checkpoint_confirmed_tool_round(&pool, effect_free_seed, "current_time", "{}").await?;
    repository
        .decide(
            decide_tool_request(
                DurableCommandId::from_uuid(Uuid::from_u128(effect_free_seed + 24)),
                effect_free_request,
                ToolApprovalDecision::Approve,
            ),
            || TurnAttemptId::from_uuid(Uuid::from_u128(effect_free_seed + 23)),
        )
        .await?;
    let effect_free_attempt = ToolAttemptId::from_uuid(Uuid::from_u128(effect_free_seed + 25));
    repository
        .prepare_next_attempt(
            effect_free_fixture.session,
            effect_free_fixture.turn,
            effect_free_attempt,
            ToolEffectClass::EffectFree,
        )
        .await?;
    repository
        .authorize_attempt(
            effect_free_fixture.session,
            effect_free_fixture.turn,
            effect_free_attempt,
        )
        .await?;
    let pending_effect_free_input =
        AcceptedInputId::from_uuid(Uuid::from_u128(effect_free_seed + 29));
    assert!(matches!(
        SubmitInputRepository::new(pool.clone())
            .handle(
                input_with_delivery(
                    effect_free_seed + 28,
                    effect_free_seed + 1,
                    "steer after effect-free dispatch",
                    DeliveryRequest::NextSafePoint {
                        expected_active_turn: effect_free_fixture.turn,
                    },
                ),
                pending_effect_free_input,
                None,
            )
            .await?,
        SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Applied(
            SubmitInputAppliedResult::PendingSteering(_)
        ))
    ));
    let recovered_effect_free_turn = TurnId::from_uuid(Uuid::from_u128(effect_free_seed + 30));
    let mut effect_free_recovery_ids = FixedStartupScanIds::new(
        [SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(
            effect_free_seed + 31,
        ))],
        [ContextFrontierId::from_uuid(Uuid::from_u128(
            effect_free_seed + 32,
        ))],
    )
    .with_reclassified_turns([recovered_effect_free_turn]);
    assert!(matches!(
        PostgresStartupScanRepository::new(pool.clone())
            .recover(
                effect_free_fixture.session,
                signalbox_domain::AcceptedInputTurnFailureIdentities::new(
                    SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(effect_free_seed + 26)),
                    ContextFrontierId::from_uuid(Uuid::from_u128(effect_free_seed + 27)),
                ),
                &mut effect_free_recovery_ids,
            )
            .await?,
        StartupScanSessionOutcome::RecoveredToolAttempt(outcome)
            if matches!(*outcome, ToolAttemptCrashOutcome::KnownFailed(_))
    ));
    let effect_free_shape: (String, String, String, String, Uuid) = sqlx::query_as(
        "SELECT
            (SELECT state_kind FROM turn_lifecycle WHERE turn_id = $1),
            (SELECT terminal_disposition_kind FROM turn_lifecycle WHERE turn_id = $1),
            (SELECT error_kind FROM tool_attempt WHERE attempt_id = $2),
            (SELECT disposition_kind FROM accepted_input
              WHERE accepted_input_id = $3),
            (SELECT origin_turn_id FROM accepted_input
              WHERE accepted_input_id = $3)",
    )
    .bind(effect_free_fixture.turn.into_uuid())
    .bind(effect_free_attempt.into_uuid())
    .bind(pending_effect_free_input.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        effect_free_shape,
        (
            "terminal".to_owned(),
            "failed".to_owned(),
            "crash_lost".to_owned(),
            "reclassified_as_turn_origin".to_owned(),
            recovered_effect_free_turn.into_uuid(),
        )
    );
    let terminal_kinds: Vec<String> = sqlx::query_scalar(
        "SELECT array_agg(entry.payload_kind ORDER BY member.member_position)
           FROM context_frontier_member AS member
           JOIN semantic_transcript_entry AS entry
             ON entry.source_session_id = member.source_session_id
            AND entry.semantic_entry_id = member.semantic_entry_id
          WHERE member.owning_session_id = $1
            AND member.context_frontier_id = $2",
    )
    .bind(effect_free_fixture.session.into_uuid())
    .bind(Uuid::from_u128(effect_free_seed + 27))
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        terminal_kinds,
        [
            "origin_accepted_input",
            "assistant_tool_use",
            "tool_execution_result",
            "turn_failed",
        ]
    );
    let mut reordered_terminal = pool.begin().await?;
    sqlx::query(
        "ALTER TABLE context_frontier_delta
         DISABLE TRIGGER context_frontier_member_is_append_only",
    )
    .execute(&mut *reordered_terminal)
    .await?;
    let reordered_frontier = Uuid::from_u128(effect_free_seed + 27);
    let reordered_session = effect_free_fixture.session.into_uuid();
    move_delta_member(
        &mut reordered_terminal,
        reordered_session,
        reordered_frontier,
        Uuid::from_u128(effect_free_seed + 31),
        99,
    )
    .await?;
    move_delta_member(
        &mut reordered_terminal,
        reordered_session,
        reordered_frontier,
        Uuid::from_u128(effect_free_seed + 26),
        3,
    )
    .await?;
    move_delta_member(
        &mut reordered_terminal,
        reordered_session,
        reordered_frontier,
        Uuid::from_u128(effect_free_seed + 31),
        4,
    )
    .await?;
    let reordered_terminal_error = sqlx::query("SELECT assert_tool_loop_turn_final_state($1)")
        .bind(effect_free_fixture.turn.into_uuid())
        .execute(&mut *reordered_terminal)
        .await
        .expect_err("failure requires proposal-ordered tool results before its marker");
    assert_eq!(
        reordered_terminal_error
            .as_database_error()
            .and_then(|error| error.constraint()),
        Some("tool_loop_terminal_result_suffix_exact")
    );
    reordered_terminal.rollback().await?;
    let effect_free_snapshot = ProcessReadRepository::new(pool.clone())
        .read_transcript(effect_free_fixture.session)
        .await?
        .expect("known tool-crash failure remains process-readable");
    assert!(effect_free_snapshot.entries().iter().any(|entry| matches!(
        entry,
        ProcessTranscriptEntry::ToolExecutionResult {
            request,
            attempt,
            ..
        } if *request == effect_free_request && *attempt == effect_free_attempt
    )));
    let mut effect_free_dispatched = Vec::new();
    drain_outbox(&pool, |event| {
        effect_free_dispatched.push(event.kind().clone());
    })
    .await?;
    assert!(
        announced_failed_turns(&effect_free_dispatched).contains(&effect_free_fixture.turn),
        "known tool-crash failure must not be rejected for earlier call history"
    );

    let follow_up = SubmitInputRepository::new(pool.clone())
        .handle(
            start_input(
                effect_free_seed + 40,
                effect_free_seed + 1,
                "work after failed tool round",
                1,
                ModelSelectionOverride::UseSessionDefault,
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(effect_free_seed + 41)),
            Some(TurnId::from_uuid(Uuid::from_u128(effect_free_seed + 42))),
        )
        .await?;
    assert!(
        matches!(
            follow_up,
            SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Applied(
                SubmitInputAppliedResult::TurnOrigin(_)
            ))
        ),
        "writer-produced failed tool history must reconstitute before the next submit"
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// concurrent user-global command claims serialize before either
/// request-local decision can commit.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn tool_decision_command_race_has_one_global_winner() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let first_seed = 0x7900;
    let second_seed = 0x7a00;
    let (first, _, _, first_request) =
        checkpoint_confirmed_tool_round(&pool, first_seed, "current_time", "{}").await?;
    let (second, _, _, second_request) =
        checkpoint_confirmed_tool_round(&pool, second_seed, "current_time", "{}").await?;
    let command_id = DurableCommandId::from_uuid(Uuid::from_u128(0x7b00));
    let repository = PostgresToolLoopRepository::new(pool.clone());
    let first_decision = repository.decide(
        decide_tool_request(command_id, first_request, ToolApprovalDecision::Approve),
        || TurnAttemptId::from_uuid(Uuid::from_u128(first_seed + 23)),
    );
    let second_decision = repository.decide(
        decide_tool_request(command_id, second_request, ToolApprovalDecision::Approve),
        || TurnAttemptId::from_uuid(Uuid::from_u128(second_seed + 23)),
    );
    let (first_result, second_result) = tokio::join!(first_decision, second_decision);
    assert!(
        matches!(
            (&first_result, &second_result),
            (Ok(_), Err(ToolLoopRepositoryError::ConflictingCommandReuse))
                | (Err(ToolLoopRepositoryError::ConflictingCommandReuse), Ok(_))
        ),
        "exactly one request-local decision wins the user-global identity"
    );
    let winner_count: i64 = sqlx::query_scalar(
        "SELECT count(*)
           FROM tool_approval_decision
          WHERE request_id IN ($1, $2)
            AND user_command_id = $3",
    )
    .bind(first_request.into_uuid())
    .bind(second_request.into_uuid())
    .bind(command_id.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(winner_count, 1);

    assert_ne!(first.session, second.session);
    pool.close().await;
    drop(container);
    Ok(())
}

/// an applied interrupt racing a tool-using response closes
/// every request in proposal order, binds those facts into the terminal
/// frontier, and makes a later user decision canonically AlreadyResolved.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn stopped_tool_round_closes_requests_and_decision_replay() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0x7c00;
    let (fixture, model_repository, _prepared, authorized) =
        authorize_checkpointed_model_call_with_prepared(&pool, seed).await?;
    SubmitInputRepository::new(pool.clone())
        .handle(
            input_with_delivery(
                seed + 19,
                seed + 1,
                "stop tool response",
                DeliveryRequest::Interrupt {
                    expected_active_turn: fixture.turn,
                    descendant_scope: DescendantTerminationScope::ParentAlone,
                    configuration: input_choices(1, ModelSelectionOverride::UseSessionDefault),
                },
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(seed + 20)),
            Some(TurnId::from_uuid(Uuid::from_u128(seed + 21))),
        )
        .await?;

    let first_request = signalbox_domain::ToolRequestId::from_uuid(Uuid::from_u128(seed + 22));
    let second_request = signalbox_domain::ToolRequestId::from_uuid(Uuid::from_u128(seed + 23));
    let response = ToolUsingAssistantResponse::try_from_parts(vec![
        AssistantResponsePart::ToolCall(ToolCallProposal::new(
            ToolName::try_new(String::from("first_tool")).expect("valid fixture tool name"),
            NormalizedToolArguments::try_from_provider_text(String::from("{}"))
                .expect("bounded fixture arguments"),
        )),
        AssistantResponsePart::Text(
            AssistantText::try_new(String::from("between")).expect("valid fixture text"),
        ),
        AssistantResponsePart::ToolCall(ToolCallProposal::new(
            ToolName::try_new(String::from("second_tool")).expect("valid fixture tool name"),
            NormalizedToolArguments::try_from_provider_text(String::from("{}"))
                .expect("bounded fixture arguments"),
        )),
    ])
    .expect("the fixture contains tool proposals");
    let observation = authorized
        .observation_correlation()
        .bind_terminal_observation(ModelCallTerminalObservation::CompletedWithTools { response });
    let outcome = model_repository
        .apply_terminal_observation(
            fixture.session,
            observation,
            ModelCallTerminalIdentities::StoppedToolRound(
                StoppedToolRoundModelCallIdentities::new(
                    vec![
                        StoppedToolResponsePartIdentity::tool_call(
                            SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 24)),
                            first_request,
                            SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 25)),
                            InitialToolApproval::Confirm,
                        ),
                        StoppedToolResponsePartIdentity::text(
                            SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 26)),
                        ),
                        StoppedToolResponsePartIdentity::tool_call(
                            SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 27)),
                            second_request,
                            SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 28)),
                            InitialToolApproval::Confirm,
                        ),
                    ],
                    SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 29)),
                    ContextFrontierId::from_uuid(Uuid::from_u128(seed + 30)),
                ),
            ),
            |_| panic!("the fixture has no pending steering to reclassify"),
        )
        .await?;
    assert!(matches!(
        outcome,
        ModelCallTerminalOutcome::CancelledWithToolResponse(_)
    ));

    let rejection = PostgresToolLoopRepository::new(pool.clone())
        .decide(
            decide_tool_request(
                DurableCommandId::from_uuid(Uuid::from_u128(seed + 31)),
                first_request,
                ToolApprovalDecision::Approve,
            ),
            || panic!("turn-closed request consumes no continuation identity"),
        )
        .await?;
    assert_eq!(
        rejection.result(),
        &DecideToolRequestResult::Rejected(
            signalbox_domain::DecideToolRequestRejectedResult::AlreadyResolved {
                request: first_request,
            },
        )
    );
    let terminal_suffix: Vec<String> = sqlx::query_scalar(
        "SELECT entry.payload_kind
           FROM turn_lifecycle AS lifecycle
           JOIN context_frontier_member AS member
             ON member.owning_session_id = lifecycle.session_id
            AND member.context_frontier_id = lifecycle.terminal_frontier_id
           JOIN semantic_transcript_entry AS entry
             ON entry.source_session_id = member.source_session_id
            AND entry.semantic_entry_id = member.semantic_entry_id
          WHERE lifecycle.turn_id = $1
            AND entry.payload_kind IN (
                'tool_closed_by_turn_end',
                'turn_cancelled'
            )
          ORDER BY member.member_position",
    )
    .bind(fixture.turn.into_uuid())
    .fetch_all(&pool)
    .await?;
    assert_eq!(
        terminal_suffix,
        [
            "tool_closed_by_turn_end",
            "tool_closed_by_turn_end",
            "turn_cancelled"
        ]
    );

    let response_positions: Vec<(Uuid, Decimal)> = sqlx::query_as(
        "SELECT entry.semantic_entry_id, member.member_position
           FROM semantic_transcript_entry AS entry
           JOIN context_frontier_member AS member
             ON member.owning_session_id = entry.source_session_id
            AND member.context_frontier_id = $1
            AND member.semantic_entry_id = entry.semantic_entry_id
          WHERE entry.producing_model_call_id = $2
            AND entry.payload_kind IN ('assistant_text', 'assistant_tool_use')
          ORDER BY entry.assistant_response_part_ordinal",
    )
    .bind(Uuid::from_u128(seed + 30))
    .bind(fixture.call.into_uuid())
    .fetch_all(&pool)
    .await?;
    assert_eq!(response_positions.len(), 3);
    let mut swapped = pool.begin().await?;
    sqlx::query(
        "ALTER TABLE context_frontier_delta
         DISABLE TRIGGER context_frontier_member_is_append_only",
    )
    .execute(&mut *swapped)
    .await?;
    sqlx::query(
        "UPDATE context_frontier_delta
            SET member_position = member_position + 100
          WHERE owning_session_id = $1
            AND context_frontier_id = $2
            AND semantic_entry_id = $3",
    )
    .bind(fixture.session.into_uuid())
    .bind(Uuid::from_u128(seed + 30))
    .bind(response_positions[0].0)
    .execute(&mut *swapped)
    .await?;
    sqlx::query(
        "UPDATE context_frontier_delta
            SET member_position = $1
          WHERE owning_session_id = $2
            AND context_frontier_id = $3
            AND semantic_entry_id = $4",
    )
    .bind(response_positions[0].1)
    .bind(fixture.session.into_uuid())
    .bind(Uuid::from_u128(seed + 30))
    .bind(response_positions[1].0)
    .execute(&mut *swapped)
    .await?;
    sqlx::query(
        "UPDATE context_frontier_delta
            SET member_position = $1
          WHERE owning_session_id = $2
            AND context_frontier_id = $3
            AND semantic_entry_id = $4",
    )
    .bind(response_positions[1].1)
    .bind(fixture.session.into_uuid())
    .bind(Uuid::from_u128(seed + 30))
    .bind(response_positions[0].0)
    .execute(&mut *swapped)
    .await?;
    let swapped_error = sqlx::query("SELECT assert_tool_round_final_state($1)")
        .bind(fixture.call.into_uuid())
        .execute(&mut *swapped)
        .await
        .expect_err("swapped text/tool parts must fail complete response-order validation");
    assert_eq!(
        swapped_error
            .as_database_error()
            .and_then(|error| error.code()),
        Some("23514".into())
    );
    swapped.rollback().await?;

    let closed_entry: Uuid = sqlx::query_scalar(
        "SELECT entry.semantic_entry_id
           FROM semantic_transcript_entry AS entry
           JOIN tool_request AS request
             ON request.request_id = entry.tool_result_request_id
          WHERE request.producing_model_call_id = $1
            AND entry.payload_kind = 'tool_closed_by_turn_end'
          ORDER BY request.request_ordinal
          LIMIT 1",
    )
    .bind(fixture.call.into_uuid())
    .fetch_one(&pool)
    .await?;
    let mut omitted_closure = pool.begin().await?;
    sqlx::query(
        "ALTER TABLE context_frontier_delta
         DISABLE TRIGGER context_frontier_member_is_append_only",
    )
    .execute(&mut *omitted_closure)
    .await?;
    sqlx::query(
        "DELETE FROM context_frontier_delta
          WHERE owning_session_id = $1
            AND context_frontier_id = $2
            AND semantic_entry_id = $3",
    )
    .bind(fixture.session.into_uuid())
    .bind(Uuid::from_u128(seed + 30))
    .bind(closed_entry)
    .execute(&mut *omitted_closure)
    .await?;
    let omitted_error = sqlx::query("SELECT assert_tool_round_final_state($1)")
        .bind(fixture.call.into_uuid())
        .execute(&mut *omitted_closure)
        .await
        .expect_err("terminal frontier must include every closed result");
    assert_eq!(
        omitted_error
            .as_database_error()
            .and_then(|error| error.code()),
        Some("23514".into())
    );
    omitted_closure.rollback().await?;

    pool.close().await;
    drop(container);
    Ok(())
}

async fn commit_stopped_tool_round(
    pool: &PgPool,
    seed: u128,
) -> Result<
    (
        RestartModelCallFixture,
        SemanticTranscriptEntryId,
        ContextFrontierId,
        TurnId,
    ),
    Box<dyn Error>,
> {
    let successor = TurnId::from_uuid(Uuid::from_u128(seed + 21));
    let (fixture, model_repository, _prepared, authorized) =
        authorize_checkpointed_model_call_with_prepared(pool, seed).await?;
    SubmitInputRepository::new(pool.clone())
        .handle(
            input_with_delivery(
                seed + 19,
                seed + 1,
                "stop tool response",
                DeliveryRequest::Interrupt {
                    expected_active_turn: fixture.turn,
                    descendant_scope: DescendantTerminationScope::ParentAlone,
                    configuration: input_choices(1, ModelSelectionOverride::UseSessionDefault),
                },
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(seed + 20)),
            Some(successor),
        )
        .await?;

    let response =
        ToolUsingAssistantResponse::try_from_parts(vec![AssistantResponsePart::ToolCall(
            ToolCallProposal::new(
                ToolName::try_new(String::from("first_tool")).expect("valid fixture tool name"),
                NormalizedToolArguments::try_from_provider_text(String::from("{}"))
                    .expect("bounded fixture arguments"),
            ),
        )])
        .expect("the fixture contains one tool proposal");
    let cancellation_entry = SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 29));
    let terminal_frontier = ContextFrontierId::from_uuid(Uuid::from_u128(seed + 30));
    model_repository
        .apply_terminal_observation(
            fixture.session,
            authorized
                .observation_correlation()
                .bind_terminal_observation(ModelCallTerminalObservation::CompletedWithTools {
                    response,
                }),
            ModelCallTerminalIdentities::StoppedToolRound(
                StoppedToolRoundModelCallIdentities::new(
                    vec![StoppedToolResponsePartIdentity::tool_call(
                        SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 24)),
                        signalbox_domain::ToolRequestId::from_uuid(Uuid::from_u128(seed + 22)),
                        SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 25)),
                        InitialToolApproval::Confirm,
                    )],
                    cancellation_entry,
                    terminal_frontier,
                ),
            ),
            |_| panic!("the fixture has no pending steering to reclassify"),
        )
        .await?;

    Ok((fixture, cancellation_entry, terminal_frontier, successor))
}

/// S02 / S07 / S11: the terminal shape committed when a
/// stop request races a tool-using response reloads through the scheduling
/// projection, so the interrupt successor activates instead of leaving the
/// session permanently unloadable.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s02_s07_s11_stopped_tool_round_reloads_and_activates_successor()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0x7d00;
    let (fixture, _cancellation_entry, _terminal_frontier, successor) =
        commit_stopped_tool_round(&pool, seed).await?;

    let activation = StartEligibleTurnRepository::new(pool.clone())
        .handle(
            fixture.session,
            AcceptedInputTurnActivationIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 34)),
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 31)),
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 32)),
                TurnAttemptId::from_uuid(Uuid::from_u128(seed + 33)),
            ),
        )
        .await?;
    assert_eq!(
        activated_turn(activation),
        successor,
        "the committed stopped tool round must reload as a terminal predecessor"
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// S02 / S07 / S11: a stopped tool response's cancellation
/// remains dispatchable when its correlated producing call completed.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s02_s07_s11_stopped_tool_round_cancellation_dispatches() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0x7d80;
    let (fixture, cancellation_entry, terminal_frontier, _successor) =
        commit_stopped_tool_round(&pool, seed).await?;
    let terminal_call_disposition: String = sqlx::query_scalar(
        "SELECT terminal_disposition_kind
           FROM model_call
          WHERE model_call_id = $1",
    )
    .bind(fixture.call.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(terminal_call_disposition, "completed");
    let mut dispatched = Vec::new();
    drain_outbox(&pool, |event| dispatched.push(event.kind().clone())).await?;
    assert!(
        dispatched.contains(&DispatchedOutboxEventKind::TurnTerminal {
            turn: fixture.turn,
            disposition: DispatchedTurnTerminalDisposition::Cancelled {
                cancellation_entry,
                terminal_frontier,
            },
        }),
        "the cancelled turn with its completed producing call must dispatch"
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// S02 / S07 / S11: a cancellation naming a completed terminal call
/// is dispatchable only with the correlated closed-by-turn-end tool round.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s02_s07_s11_completed_cancellation_requires_closed_tool_round()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0x7e00;
    let (fixture, _cancellation_entry, _terminal_frontier, _successor) =
        commit_stopped_tool_round(&pool, seed).await?;
    let sequence = sqlx::query_scalar(
        "SELECT event_sequence
           FROM turn_terminal_outbox_event
          WHERE disposition_kind = 'cancelled'
          AND turn_id = $1",
    )
    .bind(fixture.turn.into_uuid())
    .fetch_one(&pool)
    .await?;
    sqlx::query("ALTER TABLE tool_round DISABLE TRIGGER USER")
        .execute(&pool)
        .await?;
    sqlx::query(
        "UPDATE tool_round
            SET boundary_kind = 'continuing'
          WHERE producing_model_call_id = $1",
    )
    .bind(fixture.call.into_uuid())
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE tool_round ENABLE TRIGGER USER")
        .execute(&pool)
        .await?;
    rewind_outbox_delivery_before(&pool, sequence).await?;

    assert!(matches!(
        OutboxDispatcher::new(pool.clone())
            .dispatch_next(|_| {
                panic!("a completed cancellation without its closed tool round must not dispatch")
            })
            .await,
        Err(OutboxDispatchError::Corruption(
            OutboxCorruption::InvalidTerminalEventCorrelation
        ))
    ));

    pool.close().await;
    drop(container);
    Ok(())
}
