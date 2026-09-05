//! Model credential pinning, tool decision receipts, and tool batch reload across restart.

use crate::*;

/// The creation pin is event 1, equal replay never rereads a changed pin, and
/// current credentials are selected only by append-and-head advancement.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn session_model_credentials_are_an_append_only_creation_snapshot()
-> Result<(), Box<dyn Error>> {
    const ANTHROPIC_FAMILY: &str = "anthropic";
    const CODEX_FAMILY: &str = "codex";
    const FIRST_ANTHROPIC: &str = "anthropic-first";
    const FIRST_CODEX: &str = "codex-first";
    const SECOND_CODEX: &str = "codex-second";

    let (container, pool, _database_url) = migrated_postgres().await?;
    let command_id = DurableCommandId::from_uuid(Uuid::from_u128(0xce01));
    let session = SessionId::from_uuid(Uuid::from_u128(0xce02));
    let replay_candidate = SessionId::from_uuid(Uuid::from_u128(0xce03));
    let request = CreateSessionRequest::try_new(
        command_id,
        SessionConfigurationDefaults::new(direct(0xce04)),
    )?;
    let first_pin = SessionCredentialPin::try_new(vec![
        SessionModelCredential::new(ANTHROPIC_FAMILY, FIRST_ANTHROPIC),
        SessionModelCredential::new(CODEX_FAMILY, FIRST_CODEX),
    ])
    .expect("fixture credential snapshot is valid");
    let mut first = CreateSessionService::new(
        FixedSessionIds::new([session]),
        CreateSessionRepository::new(pool.clone(), first_pin),
    );

    let CreateSessionOutcome::Applied(first_result) = first.execute(request.clone()).await? else {
        panic!("first handling applies the fixture creation");
    };
    assert_eq!(first_result.session(), session);
    let first_snapshot: Vec<(String, String)> = sqlx::query_as(
        "SELECT model_family, credential_reference
           FROM session_model_credential_entry
          WHERE session_id = $1 AND event_ordinal = 1
          ORDER BY model_family",
    )
    .bind(session.into_uuid())
    .fetch_all(&pool)
    .await?;
    assert_eq!(
        first_snapshot,
        vec![
            (ANTHROPIC_FAMILY.to_owned(), FIRST_ANTHROPIC.to_owned()),
            (CODEX_FAMILY.to_owned(), FIRST_CODEX.to_owned()),
        ]
    );

    let changed_pin = SessionCredentialPin::try_new(vec![SessionModelCredential::new(
        CODEX_FAMILY,
        SECOND_CODEX,
    )])
    .expect("changed fixture credential snapshot is valid");
    let mut replay = CreateSessionService::new(
        FixedSessionIds::new([replay_candidate]),
        CreateSessionRepository::new(pool.clone(), changed_pin),
    );
    let CreateSessionOutcome::Applied(replay_result) = replay.execute(request).await? else {
        panic!("equal replay returns the applied fixture creation");
    };
    assert_eq!(replay_result.session(), session);
    let replay_counts: (i64, i64, i64) = sqlx::query_as(
        "SELECT
            (SELECT count(*) FROM session_model_credential_record WHERE session_id = $1),
            (SELECT count(*) FROM session_model_credential_entry WHERE session_id = $1),
            (SELECT current_event_ordinal::bigint
               FROM session_current_model_credentials WHERE session_id = $1)",
    )
    .bind(session.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(replay_counts, (1, 2, 1));
    let late_entry_error = sqlx::query(
        "INSERT INTO session_model_credential_entry
            (session_id, event_ordinal, model_family, credential_reference)
         VALUES ($1, 1, 'late-family', 'late-reference')",
    )
    .bind(session.into_uuid())
    .execute(&pool)
    .await
    .expect_err("a published credential snapshot rejects late entries");
    let late_entry_database_error = late_entry_error
        .as_database_error()
        .expect("the snapshot guard returns a database error");
    assert_eq!(late_entry_database_error.code(), Some("P0001".into()));
    assert_eq!(
        late_entry_database_error.message(),
        "published session model credential snapshots are immutable"
    );

    sqlx::query(
        "INSERT INTO session_model_credential_record
            (session_id, event_ordinal, event_kind, provenance_kind,
             provenance_command_id, recorded_at)
         VALUES ($1, 2, 'updated', 'credential_update', $2, transaction_timestamp())",
    )
    .bind(session.into_uuid())
    .bind(command_id.into_uuid())
    .execute(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO session_model_credential_entry
            (session_id, event_ordinal, model_family, credential_reference)
         VALUES ($1, 2, $2, $3)",
    )
    .bind(session.into_uuid())
    .bind(CODEX_FAMILY)
    .bind(SECOND_CODEX)
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE session_current_model_credentials
            SET current_event_ordinal = 2
          WHERE session_id = $1",
    )
    .bind(session.into_uuid())
    .execute(&pool)
    .await?;

    assert_eq!(
        current_session_credential(&pool, session, CODEX_FAMILY)
            .await?
            .as_str(),
        SECOND_CODEX
    );
    let rewrite_error = sqlx::query(
        "UPDATE session_model_credential_entry
            SET credential_reference = 'rewrite'
          WHERE session_id = $1 AND event_ordinal = 1 AND model_family = $2",
    )
    .bind(session.into_uuid())
    .bind(CODEX_FAMILY)
    .execute(&pool)
    .await
    .expect_err("historical credential entries reject rewrites");
    let rewrite_database_error = rewrite_error
        .as_database_error()
        .expect("the history guard returns a database error");
    assert_eq!(rewrite_database_error.code(), Some("P0001".into()));
    assert_eq!(
        rewrite_database_error.message(),
        "session model credential history is append-only"
    );
    let delete_head_error = sqlx::query(
        "DELETE FROM session_current_model_credentials
          WHERE session_id = $1",
    )
    .bind(session.into_uuid())
    .execute(&pool)
    .await
    .expect_err("the current credential projection rejects deletion");
    let delete_head_database_error = delete_head_error
        .as_database_error()
        .expect("the current projection guard returns a database error");
    assert_eq!(delete_head_database_error.code(), Some("P0001".into()));
    assert_eq!(
        delete_head_database_error.message(),
        "session model credential head is not deletable"
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// A call keeps the credential profile selected from its creation-time event
/// after a later credential event advances the session head.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn model_call_usage_keeps_credential_pin_after_update_event() -> Result<(), Box<dyn Error>> {
    const FAMILY: &str = "cost-proof-family";
    const SUBSCRIPTION_PROFILE: &str = "cost-proof-subscription";
    const API_PROFILE: &str = "cost-proof-api";

    let (container, pool, _database_url) = migrated_postgres().await?;
    let command = DurableCommandId::from_uuid(Uuid::from_u128(0xcf01));
    let session = SessionId::from_uuid(Uuid::from_u128(0xcf02));
    let selection = DirectModelSelection::from_uuid(Uuid::from_u128(0xcf03));
    let turn = TurnId::from_uuid(Uuid::from_u128(0xcf04));
    let attempt = TurnAttemptId::from_uuid(Uuid::from_u128(0xcf05));
    let call = ModelCallId::from_uuid(Uuid::from_u128(0xcf06));
    let target =
        ResolvedProviderTarget::naming(ProviderModelIdentity::from_uuid(Uuid::from_u128(0xcf07)));
    let pin = SessionCredentialPin::try_new(vec![SessionModelCredential::new(
        FAMILY,
        SUBSCRIPTION_PROFILE,
    )])
    .expect("fixture credential snapshot is valid");
    CreateSessionRepository::new(pool.clone(), pin)
        .handle(prepared(
            command.into_uuid().as_u128(),
            session.into_uuid().as_u128(),
            ModelSelectionRequest::Direct(selection),
        ))
        .await?;
    SubmitInputRepository::new(pool.clone())
        .handle(
            start_input(
                0xcf08,
                session.into_uuid().as_u128(),
                "credential pin cost proof",
                1,
                ModelSelectionOverride::UseSessionDefault,
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(0xcf09)),
            Some(turn),
        )
        .await?;
    activate_earliest_queued_turn(
        &pool,
        EarliestQueuedTurnActivation {
            session: session.into_uuid(),
            origin_entry: Uuid::from_u128(0xcf0a),
            starting_frontier: Uuid::from_u128(0xcf0b),
            initial_attempt: attempt.into_uuid(),
        },
    )
    .await?;
    let targets =
        ModelTargetCatalog::try_from_definitions([ModelTargetDefinition::new(selection, target)])
            .expect("one credential proof target forms a catalog");
    let credential_families =
        ModelCredentialFamilyCatalog::try_new([(target, Arc::<str>::from(FAMILY), None)])
            .expect("one target-to-family route forms a catalog");
    let repository = PostgresModelCallRepository::new(
        pool.clone(),
        targets,
        ModelCallCredentialReference::new("unused-fallback"),
    )
    .with_session_credentials(credential_families);
    let prepared_call = repository
        .prepare_initial_call(
            session,
            call,
            FailedModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xcf0c)),
                ContextFrontierId::from_uuid(Uuid::from_u128(0xcf0d)),
            ),
            ContextFrontierId::from_uuid(Uuid::from_u128(0xcf0e)),
            |_| {
                (
                    SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xcf0f)),
                    TurnId::from_uuid(Uuid::from_u128(0xcf10)),
                )
            },
        )
        .await?;
    assert_eq!(
        prepared_call,
        PrepareInitialModelCallOutcome::Checkpointed(call)
    );
    repository
        .fail_prepared_call(
            session,
            call,
            PreparedModelCallFailureCause::CapabilityKnownFailure,
            None,
            FailedModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xcf11)),
                ContextFrontierId::from_uuid(Uuid::from_u128(0xcf12)),
            ),
            |_| TurnId::from_uuid(Uuid::from_u128(0xcf13)),
        )
        .await?;

    sqlx::query(
        "INSERT INTO session_model_credential_record
            (session_id, event_ordinal, event_kind, provenance_kind,
             provenance_command_id, recorded_at)
         VALUES ($1, 2, 'updated', 'credential_update', $2, transaction_timestamp())",
    )
    .bind(session.into_uuid())
    .bind(command.into_uuid())
    .execute(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO session_model_credential_entry
            (session_id, event_ordinal, model_family, credential_reference)
         VALUES ($1, 2, $2, $3)",
    )
    .bind(session.into_uuid())
    .bind(FAMILY)
    .bind(API_PROFILE)
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE session_current_model_credentials
            SET current_event_ordinal = 2
          WHERE session_id = $1",
    )
    .bind(session.into_uuid())
    .execute(&pool)
    .await?;

    assert_eq!(
        current_session_credential(&pool, session, FAMILY)
            .await?
            .as_str(),
        API_PROFILE
    );
    let snapshot = ProcessReadRepository::new(pool.clone())
        .read_transcript(session)
        .await?
        .expect("the terminal call has a transcript projection");
    assert_eq!(snapshot.model_call_usage().len(), 1);
    let usage = &snapshot.model_call_usage()[0];
    assert_eq!(usage.call(), call);
    assert_eq!(usage.target(), target);
    assert_eq!(usage.credential_profile(), SUBSCRIPTION_PROFILE);
    assert_eq!(
        usage.provenance(),
        ProcessModelCallUsageProvenance::Reported
    );
    assert_eq!(
        usage.input_token_semantics(),
        Some(ProcessModelCallInputTokenSemantics::CacheExclusive)
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// Snapshot publication serializes with entry insertion, so a concurrent late
/// family cannot pass an earlier MVCC visibility check and mutate the new head.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn session_model_credential_publication_rejects_a_concurrent_late_entry()
-> Result<(), Box<dyn Error>> {
    const FIRST_FAMILY: &str = "first-family";
    const FIRST_REFERENCE: &str = "first-reference";
    const CURRENT_FAMILY: &str = "current-family";
    const CURRENT_REFERENCE: &str = "current-reference";
    const LATE_FAMILY: &str = "late-family";
    const LATE_REFERENCE: &str = "late-reference";

    let (container, pool, _database_url) = migrated_postgres().await?;
    let command_id = DurableCommandId::from_uuid(Uuid::from_u128(0xce11));
    let session = SessionId::from_uuid(Uuid::from_u128(0xce12));
    let request = CreateSessionRequest::try_new(
        command_id,
        SessionConfigurationDefaults::new(direct(0xce13)),
    )?;
    let pin = SessionCredentialPin::try_new(vec![SessionModelCredential::new(
        FIRST_FAMILY,
        FIRST_REFERENCE,
    )])
    .expect("fixture credential snapshot is valid");
    let mut service = CreateSessionService::new(
        FixedSessionIds::new([session]),
        CreateSessionRepository::new(pool.clone(), pin),
    );
    let CreateSessionOutcome::Applied(created) = service.execute(request).await? else {
        panic!("fixture session creation applies");
    };
    assert_eq!(created.session(), session);

    sqlx::query(
        "INSERT INTO session_model_credential_record
            (session_id, event_ordinal, event_kind, provenance_kind,
             provenance_command_id, recorded_at)
         VALUES ($1, 2, 'updated', 'credential_update', $2, transaction_timestamp())",
    )
    .bind(session.into_uuid())
    .bind(command_id.into_uuid())
    .execute(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO session_model_credential_entry
            (session_id, event_ordinal, model_family, credential_reference)
         VALUES ($1, 2, $2, $3)",
    )
    .bind(session.into_uuid())
    .bind(CURRENT_FAMILY)
    .bind(CURRENT_REFERENCE)
    .execute(&pool)
    .await?;

    let mut publication = pool.begin().await?;
    sqlx::query(
        "UPDATE session_current_model_credentials
            SET current_event_ordinal = 2
          WHERE session_id = $1",
    )
    .bind(session.into_uuid())
    .execute(&mut *publication)
    .await?;
    let late_pool = pool.clone();
    let late_insert = tokio::spawn(async move {
        sqlx::query(
            "INSERT INTO session_model_credential_entry
                (session_id, event_ordinal, model_family, credential_reference)
             VALUES ($1, 2, $2, $3)",
        )
        .bind(session.into_uuid())
        .bind(LATE_FAMILY)
        .bind(LATE_REFERENCE)
        .execute(&late_pool)
        .await
    });
    assert!(
        blocked_backends_reached(&pool, 1).await?,
        "the late entry must block on the publication's session-row lock"
    );
    publication.commit().await?;
    late_insert
        .await?
        .expect_err("publication makes the concurrent late entry invalid");
    let current_snapshot: Vec<(String, String)> = sqlx::query_as(
        "SELECT model_family, credential_reference
           FROM session_model_credential_entry
          WHERE session_id = $1 AND event_ordinal = 2
          ORDER BY model_family",
    )
    .bind(session.into_uuid())
    .fetch_all(&pool)
    .await?;
    assert_eq!(
        current_snapshot,
        vec![(CURRENT_FAMILY.to_owned(), CURRENT_REFERENCE.to_owned())]
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// S10: stored tool arguments use the same exact canonical JSON or
/// undecodable representation admitted by the domain.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s10_tool_argument_representation_is_database_checked() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0x7380;
    let canonical = r#"{"exponent":1e+400,"wide":18446744073709551617}"#;
    let (fixture, _, _, request) =
        checkpoint_confirmed_tool_round(&pool, seed, "current_time", canonical).await?;
    let stored: (String, String) = sqlx::query_as(
        "SELECT arguments_kind, arguments_text
           FROM tool_request
          WHERE request_id = $1",
    )
    .bind(request.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(stored, (String::from("json"), String::from(canonical)));

    let depth = 512;
    let deep = format!("{}null{}", "[".repeat(depth), "]".repeat(depth));
    let (_, _, _, deep_request) =
        checkpoint_confirmed_tool_round(&pool, seed + 0x1000, "current_time", &deep).await?;
    let stored_deep: (String, String) = sqlx::query_as(
        "SELECT arguments_kind, arguments_text
           FROM tool_request
          WHERE request_id = $1",
    )
    .bind(deep_request.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(stored_deep, (String::from("json"), deep));

    let escaped_nul = r#"{"x":"\u0000"}"#;
    let (_, _, _, escaped_nul_request) =
        checkpoint_confirmed_tool_round(&pool, seed + 0x2000, "current_time", escaped_nul).await?;
    let stored_escaped_nul: (String, String) = sqlx::query_as(
        "SELECT arguments_kind, arguments_text
           FROM tool_request
          WHERE request_id = $1",
    )
    .bind(escaped_nul_request.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        stored_escaped_nul,
        (String::from("json"), String::from(escaped_nul))
    );

    for (offset, kind, text) in [
        (0_u128, "json", "{broken"),
        (1, "json", r#"{"b":2,"a":1}"#),
        (2, "undecodable", "{}"),
        (3, "undecodable", r#"{ "a": 1 }"#),
    ] {
        let error = sqlx::query(
            "INSERT INTO tool_request
                (request_id, session_id, turn_id, producing_model_call_id,
                 request_ordinal, tool_name, arguments_kind, arguments_text)
             VALUES ($1, $2, $3, $4, $5, 'invalid_fixture', $6, $7)",
        )
        .bind(Uuid::from_u128(seed + 100 + offset))
        .bind(fixture.session.into_uuid())
        .bind(fixture.turn.into_uuid())
        .bind(fixture.call.into_uuid())
        .bind(Decimal::from(1_u64 + u64::try_from(offset)?))
        .bind(kind)
        .bind(text)
        .execute(&pool)
        .await
        .expect_err("kind and stored argument representation must agree");
        assert_eq!(
            error
                .as_database_error()
                .and_then(|error| error.constraint()),
            Some("tool_request_arguments_representation")
        );
    }

    pool.close().await;
    drop(container);
    Ok(())
}

/// S10: user-decision receipts for one batch reconstitute from one identity-set
/// load instead of one query per approval row.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s10_user_decision_receipts_batch_reconstitute() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0x73a0;
    let (fixture, _, _, requests) = checkpoint_confirmed_tool_batch(
        &pool,
        seed,
        &[
            ("first-dangerous-tool", "{}"),
            ("second-dangerous-tool", "{}"),
        ],
    )
    .await?;
    let repository = PostgresToolLoopRepository::new(pool.clone());
    let [first_request, second_request] = requests.as_slice() else {
        panic!("the fixture proposes exactly two dangerous tools");
    };
    repository
        .decide(
            decide_tool_request(
                DurableCommandId::from_uuid(Uuid::from_u128(seed + 0xd0)),
                *first_request,
                ToolApprovalDecision::Deny { reason: None },
            ),
            || TurnAttemptId::from_uuid(Uuid::from_u128(seed + 0xe0)),
        )
        .await?;
    repository
        .decide(
            decide_tool_request(
                DurableCommandId::from_uuid(Uuid::from_u128(seed + 0xd1)),
                *second_request,
                ToolApprovalDecision::Deny { reason: None },
            ),
            || TurnAttemptId::from_uuid(Uuid::from_u128(seed + 0xe1)),
        )
        .await?;

    let reconstituted = repository
        .load_active_batch(fixture.session, fixture.turn)
        .await?
        .expect("the fully denied batch remains available for result projection");
    let first_approval = reconstituted
        .approval(*first_request)
        .expect("the first user decision reconstitutes");
    assert!(matches!(
        first_approval.decision(),
        ToolApprovalDecision::Deny { .. }
    ));
    assert_eq!(
        first_approval.source(),
        signalbox_domain::ToolDecisionSource::UserCommand
    );
    let second_approval = reconstituted
        .approval(*second_request)
        .expect("the second user decision reconstitutes");
    assert!(matches!(
        second_approval.decision(),
        ToolApprovalDecision::Deny { .. }
    ));
    assert_eq!(
        second_approval.source(),
        signalbox_domain::ToolDecisionSource::UserCommand
    );
    assert!(
        ProcessReadRepository::new(pool.clone())
            .session_has_tool_history(fixture.session)
            .await?
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// S10: a replayed not-earliest receipt can name only an earlier
/// request from the same producing round.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s10_decision_receipt_rejects_cross_round_earliest_request() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0x73c0;
    let (_, _, _, requested) =
        checkpoint_confirmed_tool_round(&pool, seed, "current_time", "{}").await?;
    let (_, _, _, foreign_earlier) =
        checkpoint_confirmed_tool_round(&pool, seed + 0x100, "current_time", "{}").await?;
    let mut transaction = pool.begin().await?;
    let command = Uuid::from_u128(seed + 0x200);
    sqlx::query(
        "INSERT INTO durable_command
            (command_id, command_kind, storage_version, claimed_at, issuer_kind)
         VALUES ($1, 'decide_tool_request', 1, transaction_timestamp(), 'operator')",
    )
    .bind(command)
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO decide_tool_request_command
            (command_id, command_kind, storage_version, request_id,
             decision_kind, denial_reason, result_kind, rejection_kind,
             result_earliest_undecided_request_id)
         VALUES ($1, 'decide_tool_request', 1, $2,
                 'approve', NULL, 'rejected',
                 'not_earliest_undecided', $3)",
    )
    .bind(command)
    .bind(requested.into_uuid())
    .bind(foreign_earlier.into_uuid())
    .execute(&mut *transaction)
    .await?;
    let error =
        sqlx::query("SET CONSTRAINTS decide_tool_request_command_requires_effect IMMEDIATE")
            .execute(&mut *transaction)
            .await
            .expect_err("a recorded blocker from another round is corruption");
    assert_eq!(
        error
            .as_database_error()
            .and_then(|error| error.constraint()),
        Some("decide_tool_request_command_earliest_correlation")
    );
    transaction.rollback().await?;

    pool.close().await;
    drop(container);
    Ok(())
}

/// S31: durable runner lease binding keeps restart reconstitution from
/// issuing a second runner capability for the same physical attempt.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s31_active_batch_reload_restores_consumed_runner_issuance() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0x73f0;
    let (fixture, _, _, request) =
        checkpoint_confirmed_tool_round(&pool, seed, "current_time", "{}").await?;
    let repository = PostgresToolLoopRepository::new(pool.clone());
    let turn_attempt = TurnAttemptId::from_uuid(Uuid::from_u128(seed + 0xe0));
    repository
        .decide(
            decide_tool_request(
                DurableCommandId::from_uuid(Uuid::from_u128(seed + 0xd0)),
                request,
                ToolApprovalDecision::Approve,
            ),
            || turn_attempt,
        )
        .await?;
    let attempt = ToolAttemptId::from_uuid(Uuid::from_u128(seed + 0xe1));
    repository
        .prepare_next_attempt(
            fixture.session,
            fixture.turn,
            attempt,
            ToolEffectClass::EffectFree,
        )
        .await?
        .expect("the approved request prepares its physical attempt");
    repository
        .authorize_attempt(fixture.session, fixture.turn, attempt)
        .await?;
    sqlx::query("ALTER TABLE runner_lease_generation DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query(
        "INSERT INTO runner_lease_generation
            (lease_id, generation, attempt_id, session_id, runner_id,
             tool_name, effect_class, placement_event_ordinal,
             registration_enrollment_id, registration_revision,
             predecessor_generation)
         VALUES ($1, 1, $2, $3, $4, $5, 'pure', 1, $6, 1, NULL)",
    )
    .bind(Uuid::from_u128(seed + 0xe2))
    .bind(attempt.into_uuid())
    .bind(fixture.session.into_uuid())
    .bind(Uuid::from_u128(seed + 0xe3))
    .bind("current_time")
    .bind(Uuid::from_u128(seed + 0xe4))
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE runner_lease_generation ENABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    let reloaded = repository
        .load_active_batch(fixture.session, fixture.turn)
        .await?
        .expect("the active batch reloads with its durable lease binding");
    let duplicate = reloaded
        .resume_runner_attempt(attempt)
        .expect_err("durably issued runner authority cannot be minted again after restart");

    assert_eq!(
        duplicate.failure(),
        ToolBatchExecutionFailure::AttemptStageMismatch
    );
    pool.close().await;
    drop(container);
    Ok(())
}

/// S31: a stored retryable claimed loss leaves its source
/// attempt in flight, so a restarted process reloads an active batch that
/// still carries the exact live source the checked claimed replacement
/// requires, and its retired inventory stays empty until the atomic
/// replacement commit retires the predecessor.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s31_batch_reload_preserves_lost_claimed_source_attempt() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0x7460;
    let (fixture, _, _, requests) = checkpoint_confirmed_tool_batch(
        &pool,
        seed,
        &[("current_time", "{}"), ("current_time", "{}")],
    )
    .await?;
    let repository = PostgresToolLoopRepository::new(pool.clone());
    let turn_attempt = TurnAttemptId::from_uuid(Uuid::from_u128(seed + 0xe0));
    let [first_request, second_request] = requests.as_slice() else {
        panic!("the two-proposal fixture returns two requests")
    };
    repository
        .decide(
            decide_tool_request(
                DurableCommandId::from_uuid(Uuid::from_u128(seed + 0xd0)),
                *first_request,
                ToolApprovalDecision::Approve,
            ),
            || turn_attempt,
        )
        .await?;
    repository
        .decide(
            decide_tool_request(
                DurableCommandId::from_uuid(Uuid::from_u128(seed + 0xd1)),
                *second_request,
                ToolApprovalDecision::Approve,
            ),
            || turn_attempt,
        )
        .await?;
    let source = ToolAttemptId::from_uuid(Uuid::from_u128(seed + 0xe1));
    repository
        .prepare_next_attempt(
            fixture.session,
            fixture.turn,
            source,
            ToolEffectClass::EffectFree,
        )
        .await?
        .expect("the first approved request prepares its physical attempt");
    repository
        .authorize_attempt(fixture.session, fixture.turn, source)
        .await?;
    // The exact durable shape a stored retryable claimed loss leaves before
    // any replacement is reserved: a lost-claimed lease head over the still
    // in-flight source attempt.
    sqlx::query("ALTER TABLE runner_lease_generation DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query(
        "INSERT INTO runner_lease_generation
            (lease_id, generation, attempt_id, session_id, runner_id,
             tool_name, effect_class, placement_event_ordinal,
             registration_enrollment_id, registration_revision,
             predecessor_generation)
         VALUES ($1, 1, $2, $3, $4, $5, 'pure', 1, $6, 1, NULL)",
    )
    .bind(Uuid::from_u128(seed + 0xe2))
    .bind(source.into_uuid())
    .bind(fixture.session.into_uuid())
    .bind(Uuid::from_u128(seed + 0xe3))
    .bind("current_time")
    .bind(Uuid::from_u128(seed + 0xe4))
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE runner_lease_generation ENABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query("ALTER TABLE runner_lease_event DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query(
        "INSERT INTO runner_lease_event
            (lease_id, generation, event_ordinal, state_kind)
         VALUES ($1, 1, 1, 'offered'), ($1, 1, 2, 'claimed'),
                ($1, 1, 3, 'lost_claimed')",
    )
    .bind(Uuid::from_u128(seed + 0xe2))
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE runner_lease_event ENABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query("ALTER TABLE runner_current_lease_event DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query(
        "INSERT INTO runner_current_lease_event
            (lease_id, generation, event_ordinal)
         VALUES ($1, 1, 3)",
    )
    .bind(Uuid::from_u128(seed + 0xe2))
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE runner_current_lease_event ENABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    let current_source: (Uuid, String) = sqlx::query_as(
        "SELECT attempt_id, state_kind
           FROM runner_current_tool_attempt
          WHERE request_id = $1",
    )
    .bind(first_request.into_uuid())
    .fetch_one(&pool)
    .await?;
    let reloaded = repository
        .load_active_batch(fixture.session, fixture.turn)
        .await?
        .expect("the active batch reloads with its live lost-claimed source");
    let live_source = reloaded
        .prepare_next_attempt(
            ToolAttemptId::from_uuid(Uuid::from_u128(seed + 0xe5)),
            ToolEffectClass::EffectFree,
        )
        .expect_err("the lost-claimed source survives reload as the live attempt");

    assert_eq!(current_source, (source.into_uuid(), "in_flight".to_owned()));
    assert_eq!(reloaded.retired_attempts().count(), 0);
    assert_eq!(
        live_source.failure(),
        ToolBatchExecutionFailure::LiveAttemptPresent
    );
    pool.close().await;
    drop(container);
    Ok(())
}

/// S31: active-batch reload restores the durable
/// retired-identity inventory a claimed runner retry leaves behind, so a
/// restarted process rejects reuse of the retired physical-attempt identity in
/// the domain instead of failing on the retained database row's key.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s31_batch_reload_restores_retired_attempt_identities() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0x7440;
    let (fixture, _, _, requests) = checkpoint_confirmed_tool_batch(
        &pool,
        seed,
        &[("current_time", "{}"), ("current_time", "{}")],
    )
    .await?;
    let repository = PostgresToolLoopRepository::new(pool.clone());
    let turn_attempt = TurnAttemptId::from_uuid(Uuid::from_u128(seed + 0xe0));
    let [first_request, second_request] = requests.as_slice() else {
        panic!("the two-proposal fixture returns two requests")
    };
    repository
        .decide(
            decide_tool_request(
                DurableCommandId::from_uuid(Uuid::from_u128(seed + 0xd0)),
                *first_request,
                ToolApprovalDecision::Approve,
            ),
            || turn_attempt,
        )
        .await?;
    repository
        .decide(
            decide_tool_request(
                DurableCommandId::from_uuid(Uuid::from_u128(seed + 0xd1)),
                *second_request,
                ToolApprovalDecision::Approve,
            ),
            || turn_attempt,
        )
        .await?;
    let retired = ToolAttemptId::from_uuid(Uuid::from_u128(seed + 0xe1));
    repository
        .prepare_next_attempt(
            fixture.session,
            fixture.turn,
            retired,
            ToolEffectClass::EffectFree,
        )
        .await?
        .expect("the first approved request prepares its physical attempt");
    repository
        .authorize_attempt(fixture.session, fixture.turn, retired)
        .await?;
    let issuing_attempt: Uuid = sqlx::query_scalar(
        "SELECT issuing_turn_attempt_id
           FROM tool_attempt
          WHERE attempt_id = $1",
    )
    .bind(retired.into_uuid())
    .fetch_one(&pool)
    .await?;
    // The exact durable shape a persisted claimed pure retry leaves behind:
    // a lost-claimed lease head over the retired terminal predecessor and the
    // committed replacement attempt that completed after the restart.
    sqlx::query("ALTER TABLE runner_lease_generation DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query(
        "INSERT INTO runner_lease_generation
            (lease_id, generation, attempt_id, session_id, runner_id,
             tool_name, effect_class, placement_event_ordinal,
             registration_enrollment_id, registration_revision,
             predecessor_generation)
         VALUES ($1, 1, $2, $3, $4, $5, 'pure', 1, $6, 1, NULL)",
    )
    .bind(Uuid::from_u128(seed + 0xe2))
    .bind(retired.into_uuid())
    .bind(fixture.session.into_uuid())
    .bind(Uuid::from_u128(seed + 0xe3))
    .bind("current_time")
    .bind(Uuid::from_u128(seed + 0xe4))
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE runner_lease_generation ENABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query("ALTER TABLE runner_lease_event DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query(
        "INSERT INTO runner_lease_event
            (lease_id, generation, event_ordinal, state_kind)
         VALUES ($1, 1, 1, 'offered'), ($1, 1, 2, 'claimed'),
                ($1, 1, 3, 'lost_claimed')",
    )
    .bind(Uuid::from_u128(seed + 0xe2))
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE runner_lease_event ENABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query("ALTER TABLE runner_current_lease_event DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query(
        "INSERT INTO runner_current_lease_event
            (lease_id, generation, event_ordinal)
         VALUES ($1, 1, 3)",
    )
    .bind(Uuid::from_u128(seed + 0xe2))
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE runner_current_lease_event ENABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query("ALTER TABLE tool_attempt DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query(
        "UPDATE tool_attempt
            SET state_kind = 'terminal',
                terminal_disposition_kind = 'known_failed',
                error_kind = 'crash_lost'
          WHERE attempt_id = $1",
    )
    .bind(retired.into_uuid())
    .execute(&pool)
    .await?;
    let replacement = ToolAttemptId::from_uuid(Uuid::from_u128(seed + 0xe5));
    sqlx::query(
        "INSERT INTO tool_attempt
            (attempt_id, request_id, session_id, turn_id,
             issuing_turn_attempt_id, effect_class, dispatch_generation,
             state_kind, terminal_disposition_kind, result_content_kind,
             result_text)
         VALUES ($1, $2, $3, $4, $5, 'effect_free', 1,
                 'terminal', 'completed', 'text', 'replacement completed')",
    )
    .bind(replacement.into_uuid())
    .bind(first_request.into_uuid())
    .bind(fixture.session.into_uuid())
    .bind(fixture.turn.into_uuid())
    .bind(issuing_attempt)
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE tool_attempt ENABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    let reloaded = repository
        .load_active_batch(fixture.session, fixture.turn)
        .await?
        .expect("the active batch reloads with its retired-identity inventory");
    let reuse = reloaded
        .prepare_next_attempt(retired, ToolEffectClass::EffectFree)
        .expect_err("a durably retired identity cannot be reused after restart");

    assert_eq!(
        reloaded.retired_attempts().collect::<Vec<_>>(),
        vec![retired]
    );
    assert_eq!(
        reuse.failure(),
        ToolBatchExecutionFailure::AttemptIdentityReuse
    );
    pool.close().await;
    drop(container);
    Ok(())
}

/// The frontiers the outbox reports for one turn's tool-batch transitions.
///
/// Draining the outbox and matching its events is plumbing irrelevant to the
/// behavior under test, so it lives here rather than in a test body
/// (`docs/agents/testing-style.md` rules 2 and 3). Reporting the frontiers
/// themselves rather than booleans also lets a caller assert the value it
/// expects, which a boolean cannot distinguish from a missing event (rule 6).
struct ObservedToolBatchFrontiers {
    proposed: Option<ContextFrontierId>,
    results_projected: Option<ContextFrontierId>,
}

async fn observed_tool_batch_frontiers(
    pool: &PgPool,
    turn: TurnId,
    producing_call: ModelCallId,
) -> Result<ObservedToolBatchFrontiers, OutboxDispatchError> {
    let mut proposed = None;
    let mut results_projected = None;
    drain_outbox(pool, |event| match event.kind() {
        DispatchedOutboxEventKind::ToolBatchTransition {
            turn: event_turn,
            producing_call: event_call,
            state: DispatchedToolBatchState::Proposed { frontier },
        } if *event_turn == turn && *event_call == producing_call => {
            proposed = Some(*frontier);
        }
        DispatchedOutboxEventKind::ToolBatchTransition {
            turn: event_turn,
            producing_call: event_call,
            state: DispatchedToolBatchState::ResultsProjected { frontier },
        } if *event_turn == turn && *event_call == producing_call => {
            results_projected = Some(*frontier);
        }
        _ => {}
    })
    .await?;
    Ok(ObservedToolBatchFrontiers {
        proposed,
        results_projected,
    })
}

/// S02 / S10 / S11: one confirmed
/// proposal survives a repository restart, records a replay-safe user
/// decision, executes through an exact durable fence, and projects one
/// reference-only result atomically with the same-turn continuation call.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s02_s10_s11_tool_round_survives_restart_and_projects_result() -> Result<(), Box<dyn Error>>
{
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0x7400;
    let (fixture, model_repository, observation, request) =
        checkpoint_confirmed_tool_round(&pool, seed, "current_time", "{}").await?;
    let mut scheduling_probe = StartEligibleTurnService::new(
        FixedStartEligibleTurnIds::new(
            [SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(
                seed + 30,
            ))],
            [ContextFrontierId::from_uuid(Uuid::from_u128(seed + 31))],
            [TurnAttemptId::from_uuid(Uuid::from_u128(seed + 32))],
        ),
        StartEligibleTurnRepository::new(pool.clone()),
    );
    assert_eq!(
        scheduling_probe.execute(fixture.session).await?,
        StartEligibleTurnOutcome::NoEligibleTurn,
        "the scheduler reloads the parked tool round without inventing work"
    );
    assert_eq!(
        model_repository
            .reread_terminal_observation(fixture.session, &observation)
            .await?,
        RetainedModelCallObservationStatus::AlreadyCommitted
    );

    let tool_repository = PostgresToolLoopRepository::new(pool.clone());
    let restarted_repository = PostgresToolLoopRepository::new(pool.clone());
    let parked = restarted_repository
        .load_active_batch(fixture.session, fixture.turn)
        .await?
        .expect("the active logical batch reloads after repository restart");
    assert_eq!(parked.producing_call(), fixture.call);
    assert_eq!(parked.requests()[0].id(), request);
    assert!(parked.awaiting_approval().is_some());

    let continuation_attempt = TurnAttemptId::from_uuid(Uuid::from_u128(seed + 23));
    let command_id = DurableCommandId::from_uuid(Uuid::from_u128(seed + 24));
    let approve = decide_tool_request(command_id, request, ToolApprovalDecision::Approve);
    let decision = tool_repository
        .decide(approve.clone(), || continuation_attempt)
        .await?;
    assert!(matches!(
        decision.result(),
        DecideToolRequestResult::Applied(_)
    ));
    assert_eq!(
        tool_repository
            .decide(approve, || panic!("replay consumes no identity"))
            .await?,
        decision,
        "same command identity and payload replay the terminal receipt"
    );
    let running_tool_snapshot = ProcessReadRepository::new(pool.clone())
        .read_transcript(fixture.session)
        .await?
        .expect("an approved running tool round remains process-readable");
    assert_eq!(
        running_tool_snapshot.turns()[0].state(),
        &ProcessTurnState::ActiveRunning {
            current_attempt: continuation_attempt,
            current_model_call: None,
        }
    );
    assert!(matches!(
        tool_repository
            .decide(
                decide_tool_request(
                    command_id,
                    request,
                    ToolApprovalDecision::Deny { reason: None },
                ),
                || panic!("conflicting replay consumes no identity"),
            )
            .await,
        Err(ToolLoopRepositoryError::ConflictingCommandReuse)
    ));

    let tool_attempt = ToolAttemptId::from_uuid(Uuid::from_u128(seed + 25));
    let prepared_attempt = tool_repository
        .prepare_next_attempt(
            fixture.session,
            fixture.turn,
            tool_attempt,
            ToolEffectClass::EffectFree,
        )
        .await?
        .expect("the revalidated batch still has work");
    assert_eq!(prepared_attempt.state(), CurrentToolAttemptState::Prepared);
    let prepared_reread = tool_repository
        .reread_ambiguous_authorization(fixture.session, fixture.turn, tool_attempt)
        .await?;
    let ToolAttemptAuthorizationStatus::Prepared(prepared_reread) = prepared_reread else {
        panic!("an unauthorized attempt must reread as prepared");
    };
    assert_eq!(prepared_reread.attempt(), tool_attempt);
    let authorized_attempt = tool_repository
        .authorize_attempt(fixture.session, fixture.turn, tool_attempt)
        .await?;
    let in_flight_reread = tool_repository
        .reread_ambiguous_authorization(fixture.session, fixture.turn, tool_attempt)
        .await?;
    let ToolAttemptAuthorizationStatus::InFlight(in_flight_reread) = in_flight_reread else {
        panic!("an authorized attempt must reread as in flight");
    };
    assert_eq!(in_flight_reread, authorized_attempt);
    let impossible_preflight_error = sqlx::query(
        "UPDATE tool_attempt
            SET state_kind = 'terminal',
                terminal_disposition_kind = 'known_failed',
                error_kind = 'unknown_tool'
          WHERE attempt_id = $1",
    )
    .bind(tool_attempt.into_uuid())
    .execute(&pool)
    .await
    .expect_err("in-flight work cannot acquire preflight-only terminal evidence");
    assert_eq!(
        impossible_preflight_error
            .as_database_error()
            .and_then(|error| error.code()),
        Some("23514".into())
    );
    let ended = tool_repository
        .commit_observation(
            authorized_attempt
                .executor_fence()
                .bind(ToolAttemptObservation::Completed {
                    result: ToolResultContent::Text(
                        ToolResultText::try_new(String::from("2026-07-23T12:00:00Z"))
                            .expect("bounded result"),
                    ),
                }),
        )
        .await?;
    assert!(matches!(ended.end(), ToolAttemptEnd::Completed { .. }));

    let unrelated_session = Uuid::from_u128(seed + 80);
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(prepared(seed + 81, seed + 80, direct(seed + 82)))
        .await?;
    // Unrolled rather than looped so each rejected reference reads on its own
    // (`docs/agents/testing-style.md` rules 2 and 3). The two cases differ in
    // which reference column is populated, which is the behavior under test.
    let cross_session_closed_error = sqlx::query(
        "INSERT INTO semantic_transcript_entry
                (source_session_id, semantic_entry_id, payload_kind,
                 tool_result_request_id, tool_result_attempt_id)
             VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(unrelated_session)
    .bind(Uuid::from_u128(seed + 83))
    .bind("tool_closed_by_turn_end")
    .bind(Some(request.into_uuid()))
    .bind(None::<Uuid>)
    .execute(&pool)
    .await
    .expect_err("tool-result references must belong to the entry's source session");
    assert_eq!(
        cross_session_closed_error
            .as_database_error()
            .and_then(|error| error.code()),
        Some("23503".into())
    );
    let cross_session_result_error = sqlx::query(
        "INSERT INTO semantic_transcript_entry
                (source_session_id, semantic_entry_id, payload_kind,
                 tool_result_request_id, tool_result_attempt_id)
             VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(unrelated_session)
    .bind(Uuid::from_u128(seed + 84))
    .bind("tool_execution_result")
    .bind(None::<Uuid>)
    .bind(Some(tool_attempt.into_uuid()))
    .execute(&pool)
    .await
    .expect_err("tool-result references must belong to the entry's source session");
    assert_eq!(
        cross_session_result_error
            .as_database_error()
            .and_then(|error| error.code()),
        Some("23503".into())
    );

    let resolved = restarted_repository
        .load_active_batch(fixture.session, fixture.turn)
        .await?
        .expect("the ended attempt remains part of the active batch");
    assert!(
        resolved
            .prepare_result_projection(
                vec![SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(
                    seed + 26
                ))],
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 27)),
            )
            .is_ok()
    );
    let selection = signalbox_domain::DirectModelSelection::from_uuid(Uuid::from_u128(seed + 5));
    let provider = ProviderModelIdentity::from_uuid(Uuid::from_u128(seed + 6));
    let targets = ModelTargetCatalog::try_from_definitions([ModelTargetDefinition::new(
        selection,
        ResolvedProviderTarget::naming(provider),
    )])
    .expect("one continuation target forms a catalog");
    let continuing_repository = PostgresToolLoopRepository::with_model_calls(
        pool.clone(),
        targets,
        model_credential_reference(),
    );
    let result_entry = SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 26));
    let continuation_frontier = ContextFrontierId::from_uuid(Uuid::from_u128(seed + 27));
    let continuation_call = ModelCallId::from_uuid(Uuid::from_u128(seed + 28));
    let continuation = continuing_repository
        .prepare_continuation(
            fixture.session,
            fixture.turn,
            fixture.call,
            signalbox_application::ToolContinuationIdentities::new(
                vec![result_entry],
                continuation_frontier,
                continuation_call,
                FailedModelCallTurnIdentities::new(
                    SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 29)),
                    ContextFrontierId::from_uuid(Uuid::from_u128(seed + 30)),
                ),
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 31)),
            ),
            |_| panic!("fixture has no pending steering"),
        )
        .await?;
    assert_eq!(
        continuation,
        signalbox_application::PrepareToolContinuationOutcome::Checkpointed(continuation_call)
    );

    let durable_shape: (i64, i64, i64, i64, i64, i64, i64, i64, i64, i64) = sqlx::query_as(
        "SELECT
            (SELECT count(*) FROM tool_round WHERE producing_model_call_id = $1),
            (SELECT count(*) FROM tool_request WHERE request_id = $2),
            (SELECT count(*) FROM tool_approval_decision
              WHERE request_id = $2
                AND decision_kind = 'approve'
                AND decision_source = 'user_command'),
            (SELECT count(*) FROM tool_attempt
              WHERE attempt_id = $3
                AND state_kind = 'terminal'
                AND terminal_disposition_kind = 'completed'),
            (SELECT count(*) FROM semantic_transcript_entry
              WHERE semantic_entry_id = $8
                AND payload_kind = 'tool_execution_result'
                AND tool_result_attempt_id = $3),
            (SELECT count(*) FROM turn_attempt
              WHERE turn_attempt_id = $4
                AND state_kind = 'running'),
            (SELECT count(*) FROM turn_lifecycle
              WHERE session_id = $5
                AND turn_id = $6
                AND state_kind = 'active'
                AND active_phase_kind = 'running'
                AND current_attempt_id = $4
                AND active_tool_round_call_id IS NULL),
            (SELECT count(*) FROM model_call
              WHERE model_call_id = $7
                AND session_id = $5
                AND turn_id = $6
                AND turn_attempt_id = $4
                AND context_frontier_id = $9
                AND state_kind = 'prepared'),
            (SELECT count(*) FROM tool_batch_transition_outbox_event
              WHERE producing_model_call_id = $1
                AND transition_kind = 'proposed'
                AND frontier_id = (
                    SELECT boundary_frontier_id
                      FROM tool_round
                     WHERE producing_model_call_id = $1
                )),
            (SELECT count(*) FROM tool_batch_transition_outbox_event
              WHERE producing_model_call_id = $1
                AND transition_kind = 'results_projected'
                AND frontier_id = $9)",
    )
    .bind(fixture.call.into_uuid())
    .bind(request.into_uuid())
    .bind(tool_attempt.into_uuid())
    .bind(continuation_attempt.into_uuid())
    .bind(fixture.session.into_uuid())
    .bind(fixture.turn.into_uuid())
    .bind(continuation_call.into_uuid())
    .bind(result_entry.into_uuid())
    .bind(continuation_frontier.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(durable_shape, (1, 1, 1, 1, 1, 1, 1, 1, 1, 1));

    let duplicate_result_error = sqlx::query(
        "INSERT INTO semantic_transcript_entry
            (source_session_id, semantic_entry_id, payload_kind,
             tool_result_request_id)
         VALUES ($1, $2, 'tool_closed_by_turn_end', $3)",
    )
    .bind(fixture.session.into_uuid())
    .bind(Uuid::from_u128(seed + 90))
    .bind(request.into_uuid())
    .execute(&pool)
    .await
    .expect_err("one request cannot have attempt- and request-referenced results");
    assert_eq!(
        duplicate_result_error
            .as_database_error()
            .and_then(|error| error.code()),
        Some("23505".into())
    );
    assert!(matches!(
        ToolLoopRepositoryError::from(duplicate_result_error),
        ToolLoopRepositoryError::Corruption(_)
    ));

    let mut missing_current_result = pool.begin().await?;
    sqlx::query(
        "ALTER TABLE context_frontier_delta
         DISABLE TRIGGER context_frontier_member_is_append_only",
    )
    .execute(&mut *missing_current_result)
    .await?;
    sqlx::query(
        "DELETE FROM context_frontier_delta
          WHERE owning_session_id = $1
            AND context_frontier_id = $2
            AND source_session_id = $1
            AND semantic_entry_id = $3",
    )
    .bind(fixture.session.into_uuid())
    .bind(continuation_frontier.into_uuid())
    .bind(result_entry.into_uuid())
    .execute(&mut *missing_current_result)
    .await?;
    let missing_result_error = sqlx::query("SELECT assert_model_call_final_state_without_stop($1)")
        .bind(continuation_call.into_uuid())
        .execute(&mut *missing_current_result)
        .await
        .expect_err("a continuation call requires every current-round result");
    assert_eq!(
        missing_result_error
            .as_database_error()
            .and_then(|error| error.code()),
        Some("23514".into())
    );
    missing_current_result.rollback().await?;

    assert!(
        restarted_repository
            .load_active_batch(fixture.session, fixture.turn)
            .await?
            .is_none(),
        "the atomic continuation no longer exposes the completed batch"
    );
    let observed = observed_tool_batch_frontiers(&pool, fixture.turn, fixture.call).await?;
    assert_eq!(
        observed.proposed,
        Some(parked.yielded_snapshot().frontier().snapshot())
    );
    assert_eq!(observed.results_projected, Some(continuation_frontier));

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn delegated_denial_reloads_in_a_continuation_model_frontier() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0x7ef0;
    let (fixture, model_repository, _, requests) = checkpoint_tool_batch_with_approval(
        &pool,
        seed,
        APPROVAL_PROPOSAL,
        InitialToolApproval::Delegated,
    )
    .await?;
    let [request] = requests.as_slice() else {
        panic!("the fixture has one delegated request")
    };
    let continuation_attempt = TurnAttemptId::from_uuid(Uuid::from_u128(seed + 0xd1));
    let mut transaction = pool.begin().await?;
    let _judge_call = persist_delegated_denial_fixture(
        &mut transaction,
        &fixture,
        *request,
        seed + 0xe0,
        continuation_attempt,
        None,
        None,
    )
    .await?;
    transaction.commit().await?;

    let result_entry = SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 0xd2));
    let result_frontier = ContextFrontierId::from_uuid(Uuid::from_u128(seed + 0xd3));
    let continuation_call = ModelCallId::from_uuid(Uuid::from_u128(seed + 0xd4));
    let continuation = model_repository
        .tool_loop_repository()
        .prepare_continuation(
            fixture.session,
            fixture.turn,
            fixture.call,
            signalbox_application::ToolContinuationIdentities::new(
                vec![result_entry],
                result_frontier,
                continuation_call,
                FailedModelCallTurnIdentities::new(
                    SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 0xd5)),
                    ContextFrontierId::from_uuid(Uuid::from_u128(seed + 0xd6)),
                ),
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 0xd7)),
            ),
            |_| panic!("the fixture has no pending steering"),
        )
        .await?;
    assert_eq!(
        continuation,
        signalbox_application::PrepareToolContinuationOutcome::Checkpointed(continuation_call)
    );
    assert!(matches!(
        model_repository
            .authorize_send(fixture.session, continuation_call)
            .await?,
        AuthorizeModelCallOutcome::Authorized(_)
    ));

    pool.close().await;
    drop(container);
    Ok(())
}
