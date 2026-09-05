//! Model call execution transactions, startup scan classification, and steering reclassification after restart.

use std::{collections::HashMap, num::NonZeroU32, time::Duration};

use crate::*;

/// Binds a fixture membership priority under the non-zero schema constraint.
fn nonzero_priority(value: u32) -> NonZeroU32 {
    NonZeroU32::new(value).expect("fixture membership priority is non-zero")
}

async fn active_credential_pool_fixture(
    pool: &sqlx::PgPool,
    seed: u128,
    pool_name: &str,
    member_reference: &str,
) -> Result<(SessionId, TurnId, PostgresModelCallRepository), Box<dyn Error>> {
    let session = SessionId::from_uuid(Uuid::from_u128(seed + 1));
    let turn = TurnId::from_uuid(Uuid::from_u128(seed + 2));
    let selection = DirectModelSelection::from_uuid(Uuid::from_u128(seed + 3));
    let target =
        ResolvedProviderTarget::naming(ProviderModelIdentity::from_uuid(Uuid::from_u128(seed + 4)));
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(prepared(
            seed + 5,
            seed + 1,
            ModelSelectionRequest::Direct(selection),
        ))
        .await?;
    SubmitInputRepository::new(pool.clone())
        .handle(
            start_input(
                seed + 6,
                seed + 1,
                "serialize shared locks",
                1,
                ModelSelectionOverride::UseSessionDefault,
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(seed + 7)),
            Some(turn),
        )
        .await?;
    activate_earliest_queued_turn(
        pool,
        EarliestQueuedTurnActivation {
            session: session.into_uuid(),
            origin_entry: Uuid::from_u128(seed + 8),
            starting_frontier: Uuid::from_u128(seed + 9),
            initial_attempt: Uuid::from_u128(seed + 10),
        },
    )
    .await?;
    let targets =
        ModelTargetCatalog::try_from_definitions([ModelTargetDefinition::new(selection, target)])
            .expect("one pool fixture target forms a catalog");
    let policy = CredentialPoolRuntimePolicy::new(
        pool_name.to_owned(),
        vec![CredentialPoolRuntimeMember::new(
            member_reference.to_owned(),
            nonzero_priority(1),
        )],
        signalbox_persistence::model_execution::CredentialPoolRuntimeExhaustion::Fail,
        CredentialPoolRuntimeAction::SwitchNow,
        CredentialPoolRuntimeAction::SwitchNow,
        CredentialPoolRuntimeAction::SwitchNow,
        CredentialPoolRuntimeAction::Quarantine,
    );
    let repository = PostgresModelCallRepository::new(
        pool.clone(),
        targets,
        ModelCallCredentialReference::new("unused-default"),
    )
    .with_credential_pools(HashMap::from([(target, policy)]));
    Ok((session, turn, repository))
}

async fn lock_outbox_sequence_allocator(
    pool: &sqlx::PgPool,
) -> Result<sqlx::Transaction<'_, sqlx::Postgres>, Box<dyn Error>> {
    let mut transaction = pool.begin().await?;
    sqlx::query("SELECT singleton FROM outbox_sequence_state WHERE singleton FOR UPDATE")
        .execute(&mut *transaction)
        .await?;
    Ok(transaction)
}

async fn credential_action_head_is_available(
    pool: &sqlx::PgPool,
    member_reference: &str,
) -> Result<bool, Box<dyn Error>> {
    let mut transaction = pool.begin().await?;
    let available: bool =
        sqlx::query_scalar("SELECT pg_try_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(format!("credential_pool_action_head:{member_reference}"))
            .fetch_one(&mut *transaction)
            .await?;
    transaction.rollback().await?;
    Ok(available)
}

async fn model_call_outbox_order_guard_is_available(
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

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn deferred_final_state_validation_claims_are_typed_and_transaction_local()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0x7730_u128;
    let (_session, turn, _repository) =
        active_credential_pool_fixture(&pool, seed, "claim-pool", "claim-member").await?;

    let mut transaction = pool.begin().await?;
    sqlx::query("SELECT assert_turn_lifecycle_final_state($1)")
        .bind(turn.into_uuid())
        .execute(&mut *transaction)
        .await?;
    let duplicate_turn_claim: bool =
        sqlx::query_scalar("SELECT claim_deferred_final_state_validation('turn_lifecycle', $1)")
            .bind(turn.into_uuid())
            .fetch_one(&mut *transaction)
            .await?;
    let distinct_kind_claim: bool =
        sqlx::query_scalar("SELECT claim_deferred_final_state_validation('model_call', $1)")
            .bind(turn.into_uuid())
            .fetch_one(&mut *transaction)
            .await?;
    assert!(!duplicate_turn_claim);
    assert!(distinct_kind_claim);
    transaction.rollback().await?;

    let renewed_turn_claim: bool =
        sqlx::query_scalar("SELECT claim_deferred_final_state_validation('turn_lifecycle', $1)")
            .bind(turn.into_uuid())
            .fetch_one(&pool)
            .await?;
    assert!(renewed_turn_claim);

    pool.close().await;
    drop(container);
    Ok(())
}

/// INV-007 / INV-009 / INV-012: model-call writers acquire one ordering guard,
/// finish credential action locking, and only then wait for the shared outbox
/// allocator. Counted activation carries proof that it acquired the same guard
/// before its earlier activation event.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv007_inv009_inv012_model_call_writers_guard_credential_before_outbox()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0x7720_u128;
    let pool_name = "ordered-pool";
    let member_reference = "ordered-member";
    let (session, _turn, repository) =
        active_credential_pool_fixture(&pool, seed, pool_name, member_reference).await?;

    let allocator_holder = lock_outbox_sequence_allocator(&pool).await?;
    let preparation = tokio::spawn({
        let repository = repository.clone();
        async move {
            repository
                .prepare_initial_call(
                    session,
                    ModelCallId::from_uuid(Uuid::from_u128(seed + 11)),
                    FailedModelCallTurnIdentities::new(
                        SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 12)),
                        ContextFrontierId::from_uuid(Uuid::from_u128(seed + 13)),
                    ),
                    ContextFrontierId::from_uuid(Uuid::from_u128(seed + 14)),
                    |_| {
                        (
                            SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 15)),
                            TurnId::from_uuid(Uuid::from_u128(seed + 16)),
                        )
                    },
                )
                .await
        }
    });
    assert!(blocked_backends_reached(&pool, 1).await?);
    assert!(!model_call_outbox_order_guard_is_available(&pool).await?);
    assert!(!credential_action_head_is_available(&pool, member_reference).await?);
    allocator_holder.rollback().await?;
    let PrepareInitialModelCallOutcome::Checkpointed(prepared_call) = preparation.await?? else {
        panic!("the released preparation must checkpoint");
    };
    assert_eq!(
        prepared_call,
        ModelCallId::from_uuid(Uuid::from_u128(seed + 11))
    );

    let PrepareInitialModelCallOutcome::Ready { request, .. } = repository
        .prepare_initial_call(
            session,
            ModelCallId::from_uuid(Uuid::from_u128(seed + 17)),
            FailedModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 18)),
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 19)),
            ),
            ContextFrontierId::from_uuid(Uuid::from_u128(seed + 20)),
            |_| {
                (
                    SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 21)),
                    TurnId::from_uuid(Uuid::from_u128(seed + 22)),
                )
            },
        )
        .await?
    else {
        panic!("the committed call must reload");
    };
    let AuthorizeModelCallOutcome::Authorized(authorized) = repository
        .authorize_send(session, request.call().id())
        .await?
    else {
        panic!("the prepared call must authorize");
    };

    let allocator_holder = lock_outbox_sequence_allocator(&pool).await?;
    let observation = tokio::spawn({
        let mut repository = repository.clone();
        async move {
            repository
                .commit_observation(
                    session,
                    authorized
                        .observation_correlation()
                        .bind_provider_failure_observation_with_retry_after(
                            ProviderModelCallFailureCause::QuotaExhausted,
                            ProviderReportedTokenUsage::unreported(),
                            None,
                            true,
                        ),
                    signalbox_application::ModelCallTerminalIdentityCandidates::Availability {
                        failed: FailedModelCallTurnIdentities::new(
                            SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 23)),
                            ContextFrontierId::from_uuid(Uuid::from_u128(seed + 24)),
                        ),
                        successor_attempt: TurnAttemptId::from_uuid(Uuid::from_u128(seed + 25)),
                    },
                    |_| TurnId::from_uuid(Uuid::from_u128(seed + 26)),
                )
                .await
        }
    });
    assert!(blocked_backends_reached(&pool, 1).await?);
    assert!(!model_call_outbox_order_guard_is_available(&pool).await?);
    assert!(!credential_action_head_is_available(&pool, member_reference).await?);
    allocator_holder.rollback().await?;
    let Some(ModelCallObservationCommitOutcome::PoolExhausted(_)) = observation.await?? else {
        panic!("the sole unavailable member must exhaust its pool");
    };

    pool.close().await;
    drop(container);
    Ok(())
}

/// Reproduces #771: a quota failure on member A with `switch_now` must commit
/// a distinct successor on member B; exhausting B is a typed pool cause.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn configured_quota_failure_records_pool_rotation_action_end_to_end()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0x7710_u128;
    let session = SessionId::from_uuid(Uuid::from_u128(seed + 1));
    let turn = TurnId::from_uuid(Uuid::from_u128(seed + 2));
    let first_attempt = TurnAttemptId::from_uuid(Uuid::from_u128(seed + 3));
    let selection = DirectModelSelection::from_uuid(Uuid::from_u128(seed + 4));
    let target =
        ResolvedProviderTarget::naming(ProviderModelIdentity::from_uuid(Uuid::from_u128(seed + 5)));
    let pool_name_fixture = "codex-main";
    let first_member_fixture = "codex-a";
    let second_member_fixture = "codex-b";
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(prepared(
            seed + 6,
            seed + 1,
            ModelSelectionRequest::Direct(selection),
        ))
        .await?;
    SubmitInputRepository::new(pool.clone())
        .handle(
            start_input(
                seed + 7,
                seed + 1,
                "rotate credentials",
                1,
                ModelSelectionOverride::UseSessionDefault,
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(seed + 8)),
            Some(turn),
        )
        .await?;
    activate_earliest_queued_turn(
        &pool,
        EarliestQueuedTurnActivation {
            session: session.into_uuid(),
            origin_entry: Uuid::from_u128(seed + 9),
            starting_frontier: Uuid::from_u128(seed + 10),
            initial_attempt: first_attempt.into_uuid(),
        },
    )
    .await?;
    let targets =
        ModelTargetCatalog::try_from_definitions([ModelTargetDefinition::new(selection, target)])
            .expect("one pool fixture target forms a catalog");
    let policy = CredentialPoolRuntimePolicy::new(
        pool_name_fixture,
        vec![
            CredentialPoolRuntimeMember::new(first_member_fixture, nonzero_priority(1)),
            CredentialPoolRuntimeMember::new(second_member_fixture, nonzero_priority(2)),
        ],
        signalbox_persistence::model_execution::CredentialPoolRuntimeExhaustion::Fail,
        CredentialPoolRuntimeAction::SwitchNow,
        CredentialPoolRuntimeAction::SwitchNow,
        CredentialPoolRuntimeAction::SwitchNow,
        CredentialPoolRuntimeAction::Quarantine,
    );
    let mut repository = PostgresModelCallRepository::new(
        pool.clone(),
        targets,
        ModelCallCredentialReference::new("unused-default"),
    )
    .with_credential_pools(HashMap::from([(target, policy)]));
    let first_call = ModelCallId::from_uuid(Uuid::from_u128(seed + 11));
    let PrepareInitialModelCallOutcome::Checkpointed(first_checkpoint) = repository
        .prepare_initial_call(
            session,
            first_call,
            FailedModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 12)),
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 13)),
            ),
            ContextFrontierId::from_uuid(Uuid::from_u128(seed + 14)),
            |_| {
                (
                    SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 15)),
                    TurnId::from_uuid(Uuid::from_u128(seed + 16)),
                )
            },
        )
        .await?
    else {
        panic!("member A must checkpoint")
    };
    assert_eq!(first_checkpoint, first_call);
    let PrepareInitialModelCallOutcome::Ready {
        request: first_request,
        credential_reference: first_reference,
        ..
    } = repository
        .prepare_initial_call(
            session,
            ModelCallId::from_uuid(Uuid::from_u128(seed + 17)),
            FailedModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 18)),
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 19)),
            ),
            ContextFrontierId::from_uuid(Uuid::from_u128(seed + 20)),
            |_| {
                (
                    SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 21)),
                    TurnId::from_uuid(Uuid::from_u128(seed + 22)),
                )
            },
        )
        .await?
    else {
        panic!("member A must reload")
    };
    assert_eq!(first_reference.as_str(), first_member_fixture);
    let AuthorizeModelCallOutcome::Authorized(first_authorized) = repository
        .authorize_send(session, first_request.call().id())
        .await?
    else {
        panic!("member A must authorize")
    };
    let successor_attempt = TurnAttemptId::from_uuid(Uuid::from_u128(seed + 23));
    let first_commit = repository
        .commit_observation(
            session,
            first_authorized
                .observation_correlation()
                .bind_provider_failure_observation_with_retry_after(
                    ProviderModelCallFailureCause::QuotaExhausted,
                    ProviderReportedTokenUsage::unreported(),
                    None,
                    true,
                ),
            signalbox_application::ModelCallTerminalIdentityCandidates::Availability {
                failed: FailedModelCallTurnIdentities::new(
                    SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 24)),
                    ContextFrontierId::from_uuid(Uuid::from_u128(seed + 25)),
                ),
                successor_attempt,
            },
            |_| TurnId::from_uuid(Uuid::from_u128(seed + 26)),
        )
        .await?;
    let Some(ModelCallObservationCommitOutcome::AvailabilitySuccessor(successor)) = first_commit
    else {
        panic!("quota on A must create a successor")
    };
    assert_eq!(
        successor.successor().successor_attempt().id(),
        successor_attempt
    );
    assert_eq!(successor.backoff(), std::time::Duration::ZERO);
    let second_call = ModelCallId::from_uuid(Uuid::from_u128(seed + 27));
    let PrepareInitialModelCallOutcome::Checkpointed(second_checkpoint) = repository
        .prepare_initial_call(
            session,
            second_call,
            FailedModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 28)),
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 29)),
            ),
            ContextFrontierId::from_uuid(Uuid::from_u128(seed + 30)),
            |_| {
                (
                    SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 31)),
                    TurnId::from_uuid(Uuid::from_u128(seed + 32)),
                )
            },
        )
        .await?
    else {
        panic!("member B must checkpoint")
    };
    assert_eq!(second_checkpoint, second_call);
    let PrepareInitialModelCallOutcome::Ready {
        request: second_request,
        credential_reference: second_reference,
        ..
    } = repository
        .prepare_initial_call(
            session,
            ModelCallId::from_uuid(Uuid::from_u128(seed + 33)),
            FailedModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 34)),
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 35)),
            ),
            ContextFrontierId::from_uuid(Uuid::from_u128(seed + 36)),
            |_| {
                (
                    SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 37)),
                    TurnId::from_uuid(Uuid::from_u128(seed + 38)),
                )
            },
        )
        .await?
    else {
        panic!("member B must reload")
    };
    assert_eq!(second_reference.as_str(), second_member_fixture);
    let AuthorizeModelCallOutcome::Authorized(second_authorized) = repository
        .authorize_send(session, second_request.call().id())
        .await?
    else {
        panic!("member B must authorize")
    };
    let second_commit = repository
        .commit_observation(
            session,
            second_authorized
                .observation_correlation()
                .bind_provider_failure_observation_with_retry_after(
                    ProviderModelCallFailureCause::QuotaExhausted,
                    ProviderReportedTokenUsage::unreported(),
                    None,
                    true,
                ),
            signalbox_application::ModelCallTerminalIdentityCandidates::Availability {
                failed: FailedModelCallTurnIdentities::new(
                    SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 39)),
                    ContextFrontierId::from_uuid(Uuid::from_u128(seed + 40)),
                ),
                successor_attempt: TurnAttemptId::from_uuid(Uuid::from_u128(seed + 41)),
            },
            |_| TurnId::from_uuid(Uuid::from_u128(seed + 42)),
        )
        .await?;
    let Some(ModelCallObservationCommitOutcome::PoolExhausted(
        signalbox_application::CredentialPoolExhaustedOutcome::AfterCall {
            pool_name,
            terminal: _,
        },
    )) = second_commit
    else {
        panic!("the last unavailable member must exhaust the pool")
    };
    assert_eq!(pool_name.as_ref(), pool_name_fixture);
    let durable_rotation: (i64, i64) = sqlx::query_as(
        "SELECT
             (SELECT count(*) FROM credential_pool_chain_exclusion
               WHERE session_id = $1 AND turn_id = $2),
             (SELECT count(*) FROM credential_pool_terminal_exhaustion
               WHERE session_id = $1 AND turn_id = $2)",
    )
    .bind(session.into_uuid())
    .bind(turn.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(durable_rotation, (2, 1));
    pool.close().await;
    drop(container);
    Ok(())
}

/// S01 / S20 / S21 / INV-014 / INV-015 / INV-032 / INV-035: the production
/// persistence chain checkpoints Prepared with its credential and input-token
/// semantics pins, reloads them instead of changed deployment values,
/// separately authorizes send, and atomically commits exact assistant content,
/// provider compaction, completion, terminal frontier, lifecycle, call,
/// attempt, and typed outbox records.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s01_s20_s21_inv014_inv015_inv032_inv035_model_call_transactions_complete_first_reply()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let session = SessionId::from_uuid(Uuid::from_u128(0x8e1));
    let direct_selection =
        signalbox_domain::DirectModelSelection::from_uuid(Uuid::from_u128(0xce1));
    let mut create_service = CreateSessionService::new(
        FixedSessionIds::new([session]),
        CreateSessionRepository::new(pool.clone(), test_session_credential_pin()),
    );
    let CreateSessionOutcome::Applied(_) = create_service
        .execute(CreateSessionRequest::try_new(
            DurableCommandId::from_uuid(Uuid::from_u128(0x4e1)),
            SessionConfigurationDefaults::new(ModelSelectionRequest::Direct(direct_selection)),
        )?)
        .await?
    else {
        panic!("the model-call fixture session must be created");
    };

    let accepted_input = AcceptedInputId::from_uuid(Uuid::from_u128(0x9e1));
    let turn = TurnId::from_uuid(Uuid::from_u128(0xae1));
    let mut submit_service = SubmitInputService::new(
        FixedSubmitInputIds::new([accepted_input], [turn]),
        SubmitInputRepository::new(pool.clone()),
        AcceptingEligibilityNudge,
        signalbox_application::InProcessToolDispatchGate::default(),
    );
    let SubmitInputOutcome::Recorded(SubmitInputResult::Applied(
        SubmitInputAppliedResult::TurnOrigin(origin),
    )) = submit_service
        .execute(SubmitInputRequest::try_new(
            DurableCommandId::from_uuid(Uuid::from_u128(0x4e2)),
            session,
            UserContent::try_text("exact user request".to_owned())
                .expect("fixture user content is admitted"),
            DeliveryRequest::StartWhenNoActiveTurn {
                configuration: input_choices(1, ModelSelectionOverride::UseSessionDefault),
            },
        )?)
        .await?
    else {
        panic!("the model-call fixture input must be accepted");
    };
    assert_eq!(origin.accepted_input(), accepted_input);
    assert_eq!(origin.turn(), turn);

    let starting_frontier = ContextFrontierId::from_uuid(Uuid::from_u128(0xee1));
    let attempt = TurnAttemptId::from_uuid(Uuid::from_u128(0xbe1));
    let mut activation_service = StartEligibleTurnService::new(
        FixedStartEligibleTurnIds::new(
            [SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xde1))],
            [starting_frontier],
            [attempt],
        ),
        StartEligibleTurnRepository::new(pool.clone()),
    );
    let StartEligibleTurnOutcome::Activated(activated) =
        activation_service.execute(session).await?
    else {
        panic!("the model-call fixture turn must activate");
    };
    assert_eq!(activated.turn(), turn);
    record_empty_instruction_manifest(&pool, session).await?;

    let provider_identity = ProviderModelIdentity::from_uuid(Uuid::from_u128(0xfe1));
    let resolved_target = ResolvedProviderTarget::naming(provider_identity);
    let targets = ModelTargetCatalog::try_from_definitions([ModelTargetDefinition::new(
        direct_selection,
        resolved_target,
    )])
    .expect("one immutable direct target forms a catalog");
    let pinned_credential_reference = model_credential_reference();
    let repository = PostgresModelCallRepository::new(
        pool.clone(),
        targets.clone(),
        pinned_credential_reference.clone(),
    )
    .with_cache_inclusive_input_targets(HashSet::from([resolved_target]));
    let call = ModelCallId::from_uuid(Uuid::from_u128(0xce2));
    let PrepareInitialModelCallOutcome::Checkpointed(checkpointed_call) = repository
        .prepare_initial_call(
            session,
            call,
            FailedModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xde8)),
                ContextFrontierId::from_uuid(Uuid::from_u128(0xee8)),
            ),
            ContextFrontierId::from_uuid(Uuid::from_u128(0xfe8)),
            |_| {
                (
                    SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xdf8)),
                    TurnId::from_uuid(Uuid::from_u128(0xdf9)),
                )
            },
        )
        .await?
    else {
        panic!("a fresh call must stop at its Prepared checkpoint");
    };
    assert_eq!(checkpointed_call, call);

    sqlx::query(
        "ALTER TABLE turn_instruction_manifest
         DISABLE TRIGGER turn_instruction_manifest_is_append_only",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE turn_instruction_manifest
            SET manifest_hash = $1
          WHERE session_id = $2
            AND turn_id = $3",
    )
    .bind([0_u8; 32].as_slice())
    .bind(session.into_uuid())
    .bind(turn.into_uuid())
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE turn_instruction_manifest
          ENABLE TRIGGER turn_instruction_manifest_is_append_only",
    )
    .execute(&pool)
    .await?;
    let corrupted_manifest = repository
        .prepare_initial_call(
            session,
            ModelCallId::from_uuid(Uuid::from_u128(0xce3)),
            FailedModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xde9)),
                ContextFrontierId::from_uuid(Uuid::from_u128(0xee9)),
            ),
            ContextFrontierId::from_uuid(Uuid::from_u128(0xfe9)),
            |_| {
                (
                    SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xdf9)),
                    TurnId::from_uuid(Uuid::from_u128(0xdfa)),
                )
            },
        )
        .await
        .expect_err("reconstitution must authenticate the exact turn instruction manifest");
    assert!(matches!(
        corrupted_manifest,
        ModelCallRepositoryError::Corruption(_)
    ));
    sqlx::query(
        "ALTER TABLE turn_instruction_manifest
         DISABLE TRIGGER turn_instruction_manifest_is_append_only",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE turn_instruction_manifest
            SET manifest_hash = $1
          WHERE session_id = $2
            AND turn_id = $3",
    )
    .bind(
        signalbox_domain::TurnInstructionManifest::empty_turn_start(
            signalbox_domain::TurnInstructionManifestId::from_uuid(turn.into_uuid()),
            session,
            turn,
        )
        .manifest_hash()
        .as_bytes()
        .as_slice(),
    )
    .bind(session.into_uuid())
    .bind(turn.into_uuid())
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE turn_instruction_manifest
          ENABLE TRIGGER turn_instruction_manifest_is_append_only",
    )
    .execute(&pool)
    .await?;

    let repository = PostgresModelCallRepository::new(
        pool.clone(),
        targets,
        ModelCallCredentialReference::new("replacement-provider-reference"),
    );
    let unused_call_candidate = ModelCallId::from_uuid(Uuid::from_u128(0xce3));
    let PrepareInitialModelCallOutcome::Ready {
        request: prepared,
        credential_reference,
        ..
    } = repository
        .prepare_initial_call(
            session,
            unused_call_candidate,
            FailedModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xde9)),
                ContextFrontierId::from_uuid(Uuid::from_u128(0xee9)),
            ),
            ContextFrontierId::from_uuid(Uuid::from_u128(0xfe9)),
            |_| {
                (
                    SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xdf9)),
                    TurnId::from_uuid(Uuid::from_u128(0xdfa)),
                )
            },
        )
        .await?
    else {
        panic!("a later invocation must reload the committed Prepared call");
    };
    assert_eq!(credential_reference, pinned_credential_reference);
    assert_eq!(prepared.session(), session);
    assert_eq!(prepared.turn(), turn);
    assert_eq!(prepared.attempt(), attempt);
    assert_eq!(prepared.call().id(), call);
    assert_eq!(prepared.call().target().identity(), provider_identity);
    let input_includes_cache_tokens: bool = sqlx::query_scalar(
        "SELECT usage_input_includes_cache_tokens
           FROM model_call
          WHERE model_call_id = $1",
    )
    .bind(call.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert!(input_includes_cache_tokens);

    assert_eq!(prepared.frontier_entries().len(), 1);
    assert_eq!(
        prepared
            .origin_content(accepted_input)
            .expect("the frontier origin must carry its checked receipt content")
            .single_text()
            .expect("the fixture has exactly one text part")
            .as_str(),
        "exact user request"
    );
    assert_eq!(
        repository
            .reread_ambiguous_authorization(session, &prepared)
            .await?,
        ModelCallAuthorizationReread::Prepared
    );

    let AuthorizeModelCallOutcome::Authorized(authorized) =
        repository.authorize_send(session, call).await?
    else {
        panic!("the exact Prepared call must authorize")
    };
    let authorized = *authorized;
    assert_eq!(
        repository.authorize_send(session, call).await?,
        AuthorizeModelCallOutcome::NoSend
    );
    assert_eq!(
        repository
            .reread_ambiguous_authorization(session, &prepared)
            .await?,
        ModelCallAuthorizationReread::InFlight(Box::new(authorized.clone()))
    );
    let observation_correlation = authorized.observation_correlation();
    assert_eq!(authorized.call().id(), call);
    assert_eq!(
        authorized.call().state(),
        signalbox_domain::CurrentModelCallState::InFlight
    );
    assert_eq!(
        authorized.attempt().state(),
        &CurrentTurnAttemptState::Running
    );
    assert_eq!(
        repository
            .reread_ambiguous_authorization(session, &prepared)
            .await?,
        ModelCallAuthorizationReread::InFlight(Box::new(authorized.clone()))
    );
    assert_eq!(
        repository
            .prepare_initial_call(
                session,
                ModelCallId::from_uuid(Uuid::from_u128(0xce4)),
                FailedModelCallTurnIdentities::new(
                    SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xdea)),
                    ContextFrontierId::from_uuid(Uuid::from_u128(0xeea)),
                ),
                ContextFrontierId::from_uuid(Uuid::from_u128(0xfea)),
                |_| {
                    (
                        SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xdfa)),
                        TurnId::from_uuid(Uuid::from_u128(0xdfb)),
                    )
                },
            )
            .await?,
        PrepareInitialModelCallOutcome::NoWork
    );

    let provider_compaction_entry = SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xde2));
    let assistant_entry = SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xde3));
    let completion_entry = SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xde4));
    let terminal_frontier = ContextFrontierId::from_uuid(Uuid::from_u128(0xee2));
    let provider_compaction = ProviderCompactionBlock::try_new(String::from(
        r#"{"type":"compaction", "content":"exact summary", "encrypted_content":"opaque=="}"#,
    ))
    .expect("fixture provider compaction block is admitted");
    let assistant_text = AssistantText::try_new("exact assistant reply".to_owned())
        .expect("fixture assistant content is admitted");
    let observation = observation_correlation.bind_terminal_observation_with_usage(
        ModelCallTerminalObservation::CompletedWithProviderCompaction {
            response: vec![
                AssistantResponsePart::ProviderCompaction(provider_compaction.clone()),
                AssistantResponsePart::Text(assistant_text.clone()),
            ],
        },
        ProviderReportedTokenUsage::unreported()
            .with_input_tokens(Some(123))
            .with_output_tokens(Some(17)),
    );
    assert_eq!(
        repository
            .reread_terminal_observation(session, &observation)
            .await?,
        RetainedModelCallObservationStatus::Pending
    );
    let projected_text_bytes_before: Decimal = sqlx::query_scalar(
        "SELECT projected_text_bytes FROM session_timeline_fact WHERE session_id = $1",
    )
    .bind(session.into_uuid())
    .fetch_one(&pool)
    .await?;
    let outcome = repository
        .apply_terminal_observation(
            session,
            observation.clone(),
            ModelCallTerminalIdentities::Completed(CompletedModelCallIdentities::new(
                vec![provider_compaction_entry, assistant_entry],
                completion_entry,
                terminal_frontier,
            )),
            |_| panic!("the fixture has no pending steering to reclassify"),
        )
        .await?;
    assert_eq!(
        repository
            .reread_terminal_observation(session, &observation)
            .await?,
        RetainedModelCallObservationStatus::AlreadyCommitted
    );
    let ModelCallTerminalOutcome::Completed(completed) = outcome else {
        panic!("the definitive response must complete the turn");
    };
    assert_eq!(completed.turn(), turn);
    assert_eq!(completed.assistant_entries().len(), 2);
    assert_eq!(
        completed.assistant_entries()[0].payload(),
        &signalbox_domain::SemanticTranscriptEntryPayload::ProviderCompaction {
            producing_call: call,
            block: provider_compaction.clone(),
        }
    );
    assert_eq!(
        completed.assistant_entries()[1].payload(),
        &signalbox_domain::SemanticTranscriptEntryPayload::AssistantText {
            producing_call: call,
            value: assistant_text.clone(),
        }
    );

    let reported = repository
        .latest_reported_usage(session, resolved_target, terminal_frontier)
        .await?
        .expect("completed provider compaction reports retained-context semantics");
    assert!(!reported.input_is_retained());

    let durable_shape: (i64, i64, i64, i64, i64, i64, i64, i64, i64) = sqlx::query_as(
        "SELECT
            (SELECT count(*) FROM model_call
              WHERE model_call_id = $1
                AND state_kind = 'terminal'
                AND terminal_disposition_kind = 'completed'),
            (SELECT count(*) FROM turn_attempt
              WHERE turn_attempt_id = $2
                AND state_kind = 'ended'
                AND end_disposition = 'turn_completed'),
            (SELECT count(*) FROM semantic_transcript_entry
              WHERE semantic_entry_id = $3
                AND payload_kind = 'assistant_text'
                AND assistant_text_value = $8
                AND producing_model_call_id = $1),
            (SELECT count(*) FROM semantic_transcript_entry
              WHERE semantic_entry_id = $4
                AND payload_kind = 'turn_completed'
                AND completed_turn_id = $5),
            (SELECT count(*) FROM turn_lifecycle
              WHERE turn_id = $5
                AND state_kind = 'terminal'
                AND terminal_disposition_kind = 'completed'
                AND terminal_frontier_id = $6
                AND terminal_attempt_id = $2
                AND terminal_model_call_id = $1),
            (SELECT count(*) FROM model_call_transition_outbox_event
              WHERE model_call_id = $1),
            (SELECT count(*) FROM turn_terminal_outbox_event
              WHERE disposition_kind = 'completed'
              AND turn_id = $5
                AND model_call_id = $1
                AND completion_entry_id = $4
                AND terminal_frontier_id = $6),
            (SELECT count(*) FROM turn_lifecycle
              WHERE turn_id = $5
                AND pinned_provider_model_identity_id = $7),
            (SELECT count(*) FROM model_call
              WHERE model_call_id = $1
                AND credential_reference = $9)",
    )
    .bind(call.into_uuid())
    .bind(attempt.into_uuid())
    .bind(assistant_entry.into_uuid())
    .bind(completion_entry.into_uuid())
    .bind(turn.into_uuid())
    .bind(terminal_frontier.into_uuid())
    .bind(provider_identity.into_uuid())
    .bind(assistant_text.as_str())
    .bind(pinned_credential_reference.as_str())
    .fetch_one(&pool)
    .await?;
    assert_eq!(durable_shape, (1, 1, 1, 1, 1, 3, 1, 1, 1));
    let durable_provider_compaction: i64 = sqlx::query_scalar(
        "SELECT count(*)
           FROM semantic_transcript_entry
          WHERE semantic_entry_id = $1
            AND payload_kind = 'provider_compaction'
            AND assistant_text_value = $2
            AND producing_model_call_id = $3",
    )
    .bind(provider_compaction_entry.into_uuid())
    .bind(provider_compaction.as_json())
    .bind(call.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(durable_provider_compaction, 1);
    let projected_text_bytes_after: Decimal = sqlx::query_scalar(
        "SELECT projected_text_bytes FROM session_timeline_fact WHERE session_id = $1",
    )
    .bind(session.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        projected_text_bytes_after - projected_text_bytes_before,
        Decimal::from(u64::try_from(assistant_text.as_str().len())?),
        "opaque provider compaction bytes are excluded from transcript text accounting"
    );

    let completion_sequence: Decimal = sqlx::query_scalar(
        "SELECT event_sequence
           FROM turn_terminal_outbox_event
          WHERE disposition_kind = 'completed'
          AND turn_id = $1",
    )
    .bind(turn.into_uuid())
    .fetch_one(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE turn_lifecycle
            DROP CONSTRAINT turn_lifecycle_terminal_attempt_fk,
            DROP CONSTRAINT turn_lifecycle_terminal_call_fk",
    )
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE turn_lifecycle DISABLE TRIGGER USER")
        .execute(&pool)
        .await?;
    sqlx::query(
        "UPDATE turn_lifecycle
            SET terminal_attempt_id = $1,
                terminal_model_call_id = $2
          WHERE turn_id = $3",
    )
    .bind(Uuid::from_u128(0xbad1))
    .bind(Uuid::from_u128(0xbad2))
    .bind(turn.into_uuid())
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE turn_lifecycle ENABLE TRIGGER USER")
        .execute(&pool)
        .await?;
    sqlx::query(
        "ALTER TABLE outbox_delivery_state
         DISABLE TRIGGER outbox_delivery_advances_prefix",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE outbox_delivery_state
            SET delivered_through = $1 - 1,
                last_delivery_xid = pg_current_xact_id()
          WHERE singleton",
    )
    .bind(completion_sequence)
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE outbox_delivery_state
         ENABLE TRIGGER outbox_delivery_advances_prefix",
    )
    .execute(&pool)
    .await?;
    assert!(matches!(
        OutboxDispatcher::new(pool.clone())
            .dispatch_next(|_| panic!("cross-wired terminal ownership must not be offered"))
            .await,
        Err(OutboxDispatchError::Corruption(
            OutboxCorruption::InvalidTerminalEventCorrelation
        ))
    ));
    sqlx::query("ALTER TABLE turn_lifecycle DISABLE TRIGGER USER")
        .execute(&pool)
        .await?;
    sqlx::query(
        "UPDATE turn_lifecycle
            SET terminal_attempt_id = $1,
                terminal_model_call_id = $2
          WHERE turn_id = $3",
    )
    .bind(attempt.into_uuid())
    .bind(call.into_uuid())
    .bind(turn.into_uuid())
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE turn_lifecycle ENABLE TRIGGER USER")
        .execute(&pool)
        .await?;

    sqlx::query("ALTER TABLE turn_terminal_outbox_event DISABLE TRIGGER USER")
        .execute(&pool)
        .await?;
    sqlx::query("DELETE FROM turn_terminal_outbox_event WHERE disposition_kind = 'completed' AND turn_id = $1")
        .bind(turn.into_uuid())
        .execute(&pool)
        .await?;
    sqlx::query("ALTER TABLE turn_terminal_outbox_event ENABLE TRIGGER USER")
        .execute(&pool)
        .await?;
    assert!(matches!(
        repository
            .reread_terminal_observation(session, &observation)
            .await,
        Err(ModelCallRepositoryError::InvalidTransition(
            "retained observation terminal closure changed"
        ))
    ));

    pool.close().await;
    drop(container);
    Ok(())
}

/// INV-092: a durable Prepared model call is a reconciliation-sweep hint, so
/// temporary attachment unavailability can retry without process restart.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv092_prepared_model_call_remains_scheduler_eligible() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let session = SessionId::from_uuid(Uuid::from_u128(0x8e0_6201));
    let direct_selection =
        signalbox_domain::DirectModelSelection::from_uuid(Uuid::from_u128(0xce0_6201));
    let mut create_service = CreateSessionService::new(
        FixedSessionIds::new([session]),
        CreateSessionRepository::new(pool.clone(), test_session_credential_pin()),
    );
    let CreateSessionOutcome::Applied(_) = create_service
        .execute(CreateSessionRequest::try_new(
            DurableCommandId::from_uuid(Uuid::from_u128(0x4e0_6201)),
            SessionConfigurationDefaults::new(ModelSelectionRequest::Direct(direct_selection)),
        )?)
        .await?
    else {
        panic!("the prepared-call sweep fixture session is created")
    };

    let accepted_input = AcceptedInputId::from_uuid(Uuid::from_u128(0x9e0_6201));
    let turn = TurnId::from_uuid(Uuid::from_u128(0xae0_6201));
    let mut submit_service = SubmitInputService::new(
        FixedSubmitInputIds::new([accepted_input], [turn]),
        SubmitInputRepository::new(pool.clone()),
        AcceptingEligibilityNudge,
        signalbox_application::InProcessToolDispatchGate::default(),
    );
    let SubmitInputOutcome::Recorded(SubmitInputResult::Applied(
        SubmitInputAppliedResult::TurnOrigin(_),
    )) = submit_service
        .execute(SubmitInputRequest::try_new(
            DurableCommandId::from_uuid(Uuid::from_u128(0x4e0_6202)),
            session,
            UserContent::try_text("attachment retry fixture".to_owned())
                .expect("fixture user content is admitted"),
            DeliveryRequest::StartWhenNoActiveTurn {
                configuration: input_choices(1, ModelSelectionOverride::UseSessionDefault),
            },
        )?)
        .await?
    else {
        panic!("the prepared-call sweep fixture input is accepted")
    };

    let attempt = TurnAttemptId::from_uuid(Uuid::from_u128(0xbe0_6201));
    let mut activation_service = StartEligibleTurnService::new(
        FixedStartEligibleTurnIds::new(
            [SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(
                0xde0_6201,
            ))],
            [ContextFrontierId::from_uuid(Uuid::from_u128(0xee0_6201))],
            [attempt],
        ),
        StartEligibleTurnRepository::new(pool.clone()),
    );
    let StartEligibleTurnOutcome::Activated(_) = activation_service.execute(session).await? else {
        panic!("the prepared-call sweep fixture turn activates")
    };
    record_empty_instruction_manifest(&pool, session).await?;

    let provider_identity = ProviderModelIdentity::from_uuid(Uuid::from_u128(0xfe0_6201));
    let targets = ModelTargetCatalog::try_from_definitions([ModelTargetDefinition::new(
        direct_selection,
        ResolvedProviderTarget::naming(provider_identity),
    )])
    .expect("one immutable direct target forms a catalog");
    let repository =
        PostgresModelCallRepository::new(pool.clone(), targets, model_credential_reference());
    let call = ModelCallId::from_uuid(Uuid::from_u128(0xce0_6202));
    let PrepareInitialModelCallOutcome::Checkpointed(checkpointed) = repository
        .prepare_initial_call(
            session,
            call,
            FailedModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xde0_6202)),
                ContextFrontierId::from_uuid(Uuid::from_u128(0xee0_6202)),
            ),
            ContextFrontierId::from_uuid(Uuid::from_u128(0xfe0_6202)),
            |_| {
                (
                    SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xdf0_6201)),
                    TurnId::from_uuid(Uuid::from_u128(0xdf0_6202)),
                )
            },
        )
        .await?
    else {
        panic!("the fresh model call checkpoints Prepared")
    };
    let (eligible, _dispatch_starts, continuation) = PostgresEligibilitySweep::new(pool.clone())
        .find_sessions()
        .await?
        .into_parts();

    assert_eq!(checkpointed, call);
    assert!(!continuation);
    assert_eq!(eligible, vec![session]);

    pool.close().await;
    drop(container);
    Ok(())
}

/// S02 / S08 / INV-005 / INV-012 / INV-014 / INV-015 / INV-032 / INV-036: the scripted
/// application path consumes multiple steering inputs at preparation, renders
/// them immediately in the process projection and to the provider in acceptance
/// order, rejects noncontiguous stored snapshot ordinals before resume,
/// preserves the staged terminal commits, and replays each immutable
/// pending-steering receipt after consumption.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s02_inv014_inv015_application_service_completes_scripted_reply()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let session = SessionId::from_uuid(Uuid::from_u128(0x18e1));
    let selection = signalbox_domain::DirectModelSelection::from_uuid(Uuid::from_u128(0x1ce1));
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(prepared(
            0x14e1,
            0x18e1,
            ModelSelectionRequest::Direct(selection),
        ))
        .await?;

    let accepted_input = AcceptedInputId::from_uuid(Uuid::from_u128(0x19e1));
    let turn = TurnId::from_uuid(Uuid::from_u128(0x1ae1));
    let initial_content = "service user request";
    SubmitInputRepository::new(pool.clone())
        .handle(
            start_input(
                0x14e2,
                0x18e1,
                initial_content,
                1,
                ModelSelectionOverride::UseSessionDefault,
            ),
            accepted_input,
            Some(turn),
        )
        .await?;
    let attempt = TurnAttemptId::from_uuid(Uuid::from_u128(0x1be1));
    let mut activation = StartEligibleTurnService::new(
        FixedStartEligibleTurnIds::new(
            [SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(
                0x1de1,
            ))],
            [ContextFrontierId::from_uuid(Uuid::from_u128(0x1ee1))],
            [attempt],
        ),
        StartEligibleTurnRepository::new(pool.clone()),
    );
    assert!(matches!(
        activation.execute(session).await?,
        StartEligibleTurnOutcome::Activated(_)
    ));
    record_empty_instruction_manifest(&pool, session).await?;
    let steering_inputs = [
        AcceptedInputId::from_uuid(Uuid::from_u128(0x19e2)),
        AcceptedInputId::from_uuid(Uuid::from_u128(0x19e3)),
    ];
    let submit_repository = SubmitInputRepository::new(pool.clone());
    let first_steering_content = "first steering";
    let first_steering_command = SubmitInput::new(
        DurableCommandId::from_uuid(Uuid::from_u128(0x14e3)),
        session,
        UserContent::try_text(String::from(first_steering_content))
            .expect("fixture steering content is admitted"),
        DeliveryRequest::NextSafePoint {
            expected_active_turn: turn,
        },
    );
    let first_steering = submit_repository
        .handle(first_steering_command.clone(), steering_inputs[0], None)
        .await?;
    assert!(matches!(
        &first_steering,
        SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Applied(
            SubmitInputAppliedResult::PendingSteering(_)
        ))
    ));
    let second_steering_content = "second steering";
    let second_steering_command = SubmitInput::new(
        DurableCommandId::from_uuid(Uuid::from_u128(0x14e4)),
        session,
        UserContent::try_text(String::from(second_steering_content))
            .expect("fixture steering content is admitted"),
        DeliveryRequest::NextSafePoint {
            expected_active_turn: turn,
        },
    );
    let second_steering = submit_repository
        .handle(second_steering_command.clone(), steering_inputs[1], None)
        .await?;
    assert!(matches!(
        &second_steering,
        SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Applied(
            SubmitInputAppliedResult::PendingSteering(_)
        ))
    ));

    let provider_identity = ProviderModelIdentity::from_uuid(Uuid::from_u128(0x1fe1));
    let targets = ModelTargetCatalog::try_from_definitions([ModelTargetDefinition::new(
        selection,
        ResolvedProviderTarget::naming(provider_identity),
    )])
    .expect("one immutable direct target forms a catalog");
    let repository =
        PostgresModelCallRepository::new(pool.clone(), targets, model_credential_reference());
    let call = ModelCallId::from_uuid(Uuid::from_u128(0x1ce2));
    let corrupt_snapshot_unused_call = ModelCallId::from_uuid(Uuid::from_u128(0x1ce4));
    let unused_call = ModelCallId::from_uuid(Uuid::from_u128(0x1ce3));
    let assistant_entry = SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0x1de4));
    let completion_entry = SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0x1de5));
    let steering_entries = [
        SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0x1de6)),
        SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0x1de7)),
    ];
    let steering_frontier = ContextFrontierId::from_uuid(Uuid::from_u128(0x1ee2));
    let terminal_frontier = ContextFrontierId::from_uuid(Uuid::from_u128(0x1ee4));
    let assistant_text = AssistantText::try_new(String::from("service assistant reply"))
        .expect("fixture assistant content is admitted");
    let mut reused_frontier_entries = [
        SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0x1df8)),
        SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0x1df9)),
    ]
    .into_iter();
    let mut reused_frontier_turns = [
        TurnId::from_uuid(Uuid::from_u128(0x1af8)),
        TurnId::from_uuid(Uuid::from_u128(0x1af9)),
    ]
    .into_iter();
    let collision = repository
        .prepare_initial_call(
            session,
            call,
            FailedModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0x1dfa)),
                ContextFrontierId::from_uuid(Uuid::from_u128(0x1ef8)),
            ),
            ContextFrontierId::from_uuid(Uuid::from_u128(0x1ee1)),
            |_| {
                (
                    reused_frontier_entries
                        .next()
                        .expect("one entry candidate per pending steering input"),
                    reused_frontier_turns
                        .next()
                        .expect("one turn candidate per pending steering input"),
                )
            },
        )
        .await
        .expect_err("a reused steering-frontier identity must be retryable");
    assert!(
        matches!(
            collision,
            ModelCallRepositoryError::IdentityCollision(
                ModelCallIdentityCollision::TerminalFrontier
            )
        ),
        "unexpected reused-frontier result: {collision:?}"
    );
    let collision = repository
        .prepare_initial_call(
            session,
            call,
            FailedModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0x1df0)),
                ContextFrontierId::from_uuid(Uuid::from_u128(0x1ef0)),
            ),
            ContextFrontierId::from_uuid(Uuid::from_u128(0x1ef1)),
            |_| {
                (
                    SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0x1de1)),
                    TurnId::from_uuid(Uuid::from_u128(0x1af0)),
                )
            },
        )
        .await
        .expect_err("a steering identity already in the frontier must be retryable");
    assert!(matches!(
        collision,
        ModelCallRepositoryError::IdentityCollision(ModelCallIdentityCollision::SemanticEntry)
    ));
    let duplicate_candidate = SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0x1df1));
    let collision = repository
        .prepare_initial_call(
            session,
            call,
            FailedModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0x1df2)),
                ContextFrontierId::from_uuid(Uuid::from_u128(0x1ef2)),
            ),
            ContextFrontierId::from_uuid(Uuid::from_u128(0x1ef3)),
            |_| {
                (
                    duplicate_candidate,
                    TurnId::from_uuid(Uuid::from_u128(0x1af1)),
                )
            },
        )
        .await
        .expect_err("duplicate generated steering identities must be retryable");
    assert!(matches!(
        collision,
        ModelCallRepositoryError::IdentityCollision(ModelCallIdentityCollision::SemanticEntry)
    ));
    let mut service = ModelCallExecutionService::new(
        FixedModelCallExecutionIds::new(
            [call, corrupt_snapshot_unused_call, unused_call],
            [
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0x1de2)),
                steering_entries[0],
                steering_entries[1],
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0x1daa)),
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0x1de3)),
                assistant_entry,
                completion_entry,
            ],
            [
                ContextFrontierId::from_uuid(Uuid::from_u128(0x1ee5)),
                steering_frontier,
                ContextFrontierId::from_uuid(Uuid::from_u128(0x1eaa)),
                ContextFrontierId::from_uuid(Uuid::from_u128(0x1eab)),
                ContextFrontierId::from_uuid(Uuid::from_u128(0x1ee3)),
                ContextFrontierId::from_uuid(Uuid::from_u128(0x1ee6)),
                terminal_frontier,
            ],
            [
                TurnId::from_uuid(Uuid::from_u128(0x1ae2)),
                TurnId::from_uuid(Uuid::from_u128(0x1ae3)),
            ],
            [signalbox_domain::ToolRequestId::from_uuid(Uuid::from_u128(
                0x1ce1,
            ))],
            [TurnAttemptId::from_uuid(Uuid::from_u128(0x1ae4))],
        ),
        repository.clone(),
        repository.clone(),
        repository.clone(),
        repository,
        ScriptedModelCallProvider::new([ScriptedModelCallStep::Return(
            ModelCallTerminalObservation::Completed {
                assistant_text: vec![assistant_text.clone()],
            },
        )]),
        InProcessAttemptDispatchGate::default(),
        None,
    );

    assert_eq!(
        service.execute(session).await?,
        ModelCallExecutionOutcome::Checkpointed(call)
    );
    let prepared_snapshot = ProcessReadRepository::new(pool.clone())
        .read_transcript(session)
        .await?
        .expect("the prepared call has a transcript projection");
    assert_eq!(prepared_snapshot.entries().len(), 3);
    assert_projected_steering_entry(
        &prepared_snapshot.entries()[1],
        steering_inputs[0],
        turn,
        first_steering_content,
    );
    assert_projected_steering_entry(
        &prepared_snapshot.entries()[2],
        steering_inputs[1],
        turn,
        second_steering_content,
    );
    sqlx::query("ALTER TABLE context_frontier_delta DISABLE TRIGGER USER")
        .execute(&pool)
        .await?;
    sqlx::query(
        "UPDATE context_frontier_delta
            SET member_position = 4
          WHERE owning_session_id = $1
            AND context_frontier_id = $2
            AND member_position = 3",
    )
    .bind(session.into_uuid())
    .bind(steering_frontier.into_uuid())
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE context_frontier_delta ENABLE TRIGGER USER")
        .execute(&pool)
        .await?;
    let corrupt_snapshot = service
        .execute(session)
        .await
        .expect_err("the noncontiguous call snapshot must fail closed");
    assert!(
        matches!(
            corrupt_snapshot,
            ModelCallExecutionError::Prepare(ModelCallRepositoryError::Corruption(
                ModelCallCorruption::Scheduling(SubmitInputCorruption::Inconsistent(
                    "context frontier contiguous membership"
                ))
            ))
        ),
        "unexpected noncontiguous-snapshot result: {corrupt_snapshot:?}"
    );
    sqlx::query("ALTER TABLE context_frontier_delta DISABLE TRIGGER USER")
        .execute(&pool)
        .await?;
    sqlx::query(
        "UPDATE context_frontier_delta
            SET member_position = 3
          WHERE owning_session_id = $1
            AND context_frontier_id = $2
            AND member_position = 4",
    )
    .bind(session.into_uuid())
    .bind(steering_frontier.into_uuid())
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE context_frontier_delta ENABLE TRIGGER USER")
        .execute(&pool)
        .await?;
    let ModelCallExecutionOutcome::ObservationCommitted(outcome) = service.execute(session).await?
    else {
        panic!("the resumed prepared call must commit its scripted observation")
    };
    let ModelCallTerminalOutcome::Completed(completed) = *outcome else {
        panic!("the scripted completion must complete the turn")
    };
    assert_eq!(completed.turn(), turn);
    assert_eq!(completed.assistant_entries()[0].identity(), assistant_entry);
    assert_eq!(
        completed.assistant_entries()[0].payload(),
        &signalbox_domain::SemanticTranscriptEntryPayload::AssistantText {
            producing_call: call,
            value: assistant_text,
        }
    );
    let (_, _, _, _, _, provider, _, _, _, _) = service.into_parts();
    assert_eq!(provider.capability_preparation_count(), 1);
    assert_eq!(provider.interaction_count(), 1);
    let messages = provider
        .last_prepared_messages()
        .expect("the scripted provider observed the prepared messages");
    assert_eq!(messages.len(), 3);
    let ModelConversationMessage::User {
        accepted_input: message_input,
        content,
        ..
    } = &messages[0]
    else {
        panic!("the first provider message is the turn-origin user input");
    };
    assert_eq!(*message_input, accepted_input);
    assert_eq!(
        content
            .single_text()
            .expect("the turn-origin user input carries exactly one text part")
            .as_str(),
        initial_content
    );
    let ModelConversationMessage::User {
        accepted_input: message_input,
        content,
        ..
    } = &messages[1]
    else {
        panic!("the second provider message is the first steering input");
    };
    assert_eq!(*message_input, steering_inputs[0]);
    assert_eq!(
        content
            .single_text()
            .expect("the first steering input carries exactly one text part")
            .as_str(),
        first_steering_content
    );
    let ModelConversationMessage::User {
        accepted_input: message_input,
        content,
        ..
    } = &messages[2]
    else {
        panic!("the third provider message is the second steering input");
    };
    assert_eq!(*message_input, steering_inputs[1]);
    assert_eq!(
        content
            .single_text()
            .expect("the second steering input carries exactly one text part")
            .as_str(),
        second_steering_content
    );

    let durable_terminal: (i64, i64, i64, i64) = sqlx::query_as(
        "SELECT
            (SELECT count(*) FROM model_call
              WHERE model_call_id = $1
                AND state_kind = 'terminal'
                AND terminal_disposition_kind = 'completed'
                AND context_frontier_id = $4),
            (SELECT count(*) FROM turn_lifecycle
              WHERE turn_id = $2
                AND state_kind = 'terminal'
                AND terminal_frontier_id = $3),
            (SELECT count(*) FROM accepted_input
              WHERE accepted_input_id = ANY($5)
                AND disposition_kind = 'consumed_as_steering'
                AND consuming_model_call_id = $1),
            (SELECT count(*) FROM semantic_transcript_entry
              WHERE semantic_entry_id = ANY($6)
                AND payload_kind = 'steering_accepted_input'
                AND steering_source_turn_id = $2)",
    )
    .bind(call.into_uuid())
    .bind(turn.into_uuid())
    .bind(terminal_frontier.into_uuid())
    .bind(steering_frontier.into_uuid())
    .bind(steering_inputs.map(AcceptedInputId::into_uuid).to_vec())
    .bind(
        steering_entries
            .map(SemanticTranscriptEntryId::into_uuid)
            .to_vec(),
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(durable_terminal, (1, 1, 2, 2));
    let transcript = ProcessReadRepository::new(pool.clone())
        .read_transcript(session)
        .await?
        .expect("the completed session has a transcript projection");
    assert_projected_steering_entry(
        &transcript.entries()[1],
        steering_inputs[0],
        turn,
        "first steering",
    );
    assert_projected_steering_entry(
        &transcript.entries()[2],
        steering_inputs[1],
        turn,
        "second steering",
    );
    let successor_input = AcceptedInputId::from_uuid(Uuid::from_u128(0x19e4));
    let successor_turn = TurnId::from_uuid(Uuid::from_u128(0x1ae4));
    let successor = submit_repository
        .handle(
            start_input(
                0x14e5,
                0x18e1,
                "request after consumed-steering restart",
                1,
                ModelSelectionOverride::UseSessionDefault,
            ),
            successor_input,
            Some(successor_turn),
        )
        .await?;
    assert!(matches!(
        successor,
        SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Applied(
            SubmitInputAppliedResult::TurnOrigin(_)
        ))
    ));
    assert_eq!(
        submit_repository
            .handle(
                first_steering_command,
                AcceptedInputId::from_uuid(Uuid::from_u128(0x19f2)),
                None,
            )
            .await?,
        first_steering
    );
    assert_eq!(
        submit_repository
            .handle(
                second_steering_command,
                AcceptedInputId::from_uuid(Uuid::from_u128(0x19f3)),
                None,
            )
            .await?,
        second_steering
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// S03 / S04 / S07 / INV-006 / INV-016 / INV-029 / INV-034: a restart-parked
/// ambiguous model call wedges the session — the scan classifies nothing, the
/// wait stays visible across a second restart, and ordinary input is refused —
/// and the user reconciliation decision then terminalizes the exact ambiguity
/// without inventing an outcome, releases the slot, and lets the session
/// activate the accepted successor.
///
/// This is one restart-and-recovery contract, so it stays one test
/// (testing-style rule 17): CONTRIBUTING's restart category conjoins the final
/// state, the absence of forbidden effects, and scan idempotency, and each step
/// below runs against the durable state the previous step committed. Every
/// assertion names the leg it guards (rule 20) so a failure identifies which
/// guarantee broke rather than only that the timeline broke.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s04_inv029_inv034_user_reconciliation_releases_a_restart_parked_ambiguous_turn()
-> Result<(), Box<dyn Error>> {
    let (container, pool, database_url) = migrated_postgres().await?;
    let parked = checkpoint_restart_model_call(&pool, 0xB100, true).await?;

    pool.close().await;
    let restarted_pool = PgPoolOptions::new()
        .max_connections(4)
        .connect_with(local_test_connection_options(&database_url)?)
        .await?;
    let mut scan = StartupScanService::new(
        FixedStartupScanIds::new(
            [
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xB201)),
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xB203)),
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xB205)),
            ],
            [
                ContextFrontierId::from_uuid(Uuid::from_u128(0xB202)),
                ContextFrontierId::from_uuid(Uuid::from_u128(0xB204)),
                ContextFrontierId::from_uuid(Uuid::from_u128(0xB206)),
            ],
        ),
        PostgresStartupScanRepository::new(restarted_pool.clone()),
    );

    let first_restart = scan.execute().await?;
    assert_eq!(
        first_restart.recovered_turn_count(),
        0,
        "an unobserved issued call parks its turn instead of terminalizing it"
    );
    assert_eq!(
        first_restart.awaiting_recovery_decision_sessions(),
        &[parked.session],
        "the restart that parks the turn reports the wait it just created"
    );

    let parked_shape: (String, String, String, String, String, Uuid) = sqlx::query_as(
        "SELECT call.state_kind,
                call.terminal_disposition_kind,
                attempt.state_kind,
                attempt.end_disposition,
                turn.active_phase_kind,
                turn.recovery_model_call_id
           FROM model_call AS call
           JOIN turn_attempt AS attempt
             ON attempt.turn_attempt_id = call.turn_attempt_id
           JOIN turn_lifecycle AS turn
             ON turn.turn_id = call.turn_id
          WHERE call.model_call_id = $1",
    )
    .bind(parked.call.into_uuid())
    .fetch_one(&restarted_pool)
    .await?;
    assert_eq!(
        parked_shape,
        (
            "terminal".into(),
            "ambiguous".into(),
            "ended".into(),
            "lost".into(),
            "awaiting_model_call_recovery".into(),
            parked.call.into_uuid(),
        ),
        "the restart boundary leaves the exact durable ambiguity it observed"
    );
    assert_eq!(
        ProcessReadRepository::new(restarted_pool.clone())
            .model_call_recovery_precondition(parked.session)
            .await?,
        ProcessModelCallRecoveryPrecondition::Parked { turn: parked.turn },
        "the operator surface can see the wait it is expected to decide"
    );

    let wedged = SubmitInputRepository::new(restarted_pool.clone())
        .handle(
            start_input(
                0xB210,
                0xB101,
                "work refused while the ambiguity is unreconciled",
                1,
                ModelSelectionOverride::UseSessionDefault,
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(0xB211)),
            Some(TurnId::from_uuid(Uuid::from_u128(0xB212))),
        )
        .await?;
    assert_eq!(
        wedged,
        SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Rejected(
            SubmitInputRejectedResult::ActiveTurnPresent {
                session: parked.session,
                active_turn: parked.turn,
            }
        )),
        "the slot is never released without a user decision"
    );

    let second_restart = scan.execute().await?;
    assert_eq!(
        second_restart.recovered_turn_count(),
        0,
        "a re-run scan reclassifies nothing it already parked"
    );
    assert_eq!(
        second_restart.awaiting_recovery_decision_sessions(),
        &[parked.session],
        "the wait stays reported until a decision resolves it"
    );

    let successor = TurnId::from_uuid(Uuid::from_u128(0xB222));
    let reconciled = SubmitInputRepository::new(restarted_pool.clone())
        .handle(
            input_with_delivery(
                0xB220,
                0xB101,
                "continue after the user reconciliation decision",
                DeliveryRequest::Interrupt {
                    expected_active_turn: parked.turn,
                    descendant_scope: DescendantTerminationScope::ParentAlone,
                    configuration: input_choices(1, ModelSelectionOverride::UseSessionDefault),
                },
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(0xB221)),
            Some(successor),
        )
        .await?;
    assert!(
        matches!(
            reconciled,
            SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Applied(
                SubmitInputAppliedResult::TurnOrigin(_)
            ))
        ),
        "the user decision is accepted as the successor origin"
    );

    let reconciled_shape: (String, String, Uuid, Uuid, i64) = sqlx::query_as(
        "SELECT turn.state_kind,
                turn.terminal_disposition_kind,
                turn.terminal_attempt_id,
                turn.terminal_model_call_id,
                (SELECT count(*)
                   FROM turn_terminal_outbox_event
                  WHERE disposition_kind = 'reconciliation_required'
                  AND turn_id = $1
                    AND model_call_id = $2)
           FROM turn_lifecycle AS turn
          WHERE turn.turn_id = $1",
    )
    .bind(parked.turn.into_uuid())
    .bind(parked.call.into_uuid())
    .fetch_one(&restarted_pool)
    .await?;
    assert_eq!(
        reconciled_shape,
        (
            "terminal".into(),
            "reconciliation_required".into(),
            parked.attempt.into_uuid(),
            parked.call.into_uuid(),
            1,
        ),
        "reconciliation records the exact durable ambiguity instead of a fabricated outcome"
    );
    let ambiguous_call_unchanged: (String, String) = sqlx::query_as(
        "SELECT state_kind, terminal_disposition_kind
           FROM model_call
          WHERE model_call_id = $1",
    )
    .bind(parked.call.into_uuid())
    .fetch_one(&restarted_pool)
    .await?;
    assert_eq!(
        ambiguous_call_unchanged,
        ("terminal".into(), "ambiguous".into()),
        "reconciliation never rewrites the ambiguous call it reports"
    );
    assert_eq!(
        ProcessReadRepository::new(restarted_pool.clone())
            .model_call_recovery_precondition(parked.session)
            .await?,
        ProcessModelCallRecoveryPrecondition::NoParkedTurn,
        "the decided wait no longer offers itself to the operator surface"
    );

    let healed_restart = scan.execute().await?;
    assert_eq!(
        healed_restart.recovered_turn_count(),
        0,
        "a re-run scan after the decision changes nothing"
    );
    assert_eq!(
        healed_restart.awaiting_recovery_decision_sessions(),
        &[] as &[SessionId],
        "a decided session is no longer reported as awaiting one"
    );

    let snapshot = ProcessReadRepository::new(restarted_pool.clone())
        .read_transcript(parked.session)
        .await?
        .expect("the reconciled session remains process-readable");
    let ProcessTurnState::ReconciliationRequired {
        terminal_attempt,
        operation,
        ..
    } = snapshot.turns()[0].state()
    else {
        panic!("the reconciled turn stays readable as reconciliation-required");
    };
    assert_eq!(
        *terminal_attempt, parked.attempt,
        "the readable turn retains its exact terminal attempt"
    );
    assert_eq!(
        *operation,
        ProcessReconciliationOperation::ModelCall(parked.call),
        "the readable turn retains its exact ambiguous call"
    );

    let activated = activate_earliest_queued_turn(
        &restarted_pool,
        EarliestQueuedTurnActivation {
            session: parked.session.into_uuid(),
            origin_entry: Uuid::from_u128(0xB230),
            starting_frontier: Uuid::from_u128(0xB231),
            initial_attempt: Uuid::from_u128(0xB232),
        },
    )
    .await?;
    assert_eq!(
        activated.turn(),
        successor,
        "the session activates the successor accepted by the reconciliation decision"
    );

    restarted_pool.close().await;
    drop(container);
    Ok(())
}

pub(crate) async fn park_restart_ambiguity(
    pool: &PgPool,
    seed: u128,
) -> Result<RestartModelCallFixture, Box<dyn Error>> {
    let parked = checkpoint_restart_model_call(pool, seed, true).await?;
    let mut scan = StartupScanService::new(
        FixedStartupScanIds::new(
            [
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 0x101)),
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 0x103)),
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 0x105)),
            ],
            [
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 0x102)),
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 0x104)),
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 0x106)),
            ],
        ),
        PostgresStartupScanRepository::new(pool.clone()),
    );
    let outcome = scan.execute().await?;
    assert_eq!(
        outcome.awaiting_recovery_decision_sessions(),
        &[parked.session]
    );
    Ok(parked)
}

fn automatic_recovery_status(snapshot: &ProcessTranscriptSnapshot) -> (u32, bool) {
    match snapshot.turns()[0].state() {
        ProcessTurnState::ActiveAwaitingModelCallRecovery {
            automatic_reconciliation_attempts,
            operator_action_required,
            ..
        } => (
            *automatic_reconciliation_attempts,
            *operator_action_required,
        ),
        _ => panic!("fixture turn must remain parked on its ambiguous model call"),
    }
}

async fn spend_automatic_reconciliation_budget(
    repository: &PostgresAutomaticReconciliationRepository,
    pool: &PgPool,
) -> Result<(), Box<dyn Error>> {
    for expected_attempt in 1_u32..=5 {
        let batch = repository.claim_due().await?;
        assert_eq!(batch.claimed().len(), 1);
        assert_eq!(batch.claimed()[0].attempt().get(), expected_attempt);
        repository
            .record_failure(
                batch.claimed()[0],
                AutomaticReconciliationFailureKind::Infrastructure,
            )
            .await?;
        sqlx::query(
            "UPDATE automatic_reconciliation
                SET next_attempt_at = statement_timestamp()
              WHERE turn_id = $1
                AND attempt_count < 5",
        )
        .bind(batch.claimed()[0].turn().into_uuid())
        .execute(pool)
        .await?;
    }
    Ok(())
}

/// S04 / S10: the daemon claims a typed durable attempt and uses the existing
/// reconciliation-required transition to release an automatically recovered
/// ambiguous model-call wait without rewriting the call's unknown outcome.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s04_automatic_reconciliation_records_the_operator_transition() -> Result<(), Box<dyn Error>>
{
    let (container, pool, _database_url) = migrated_postgres().await?;
    let parked = park_restart_ambiguity(&pool, 0xC100).await?;
    let repository = PostgresAutomaticReconciliationRepository::new(pool.clone()).with_policy(
        Some(5),
        Some(Duration::ZERO),
        Some(Duration::ZERO),
    );
    let steering = AcceptedInputId::from_uuid(Uuid::from_u128(0xC300));
    let steering_outcome = SubmitInputRepository::new(pool.clone())
        .handle(
            SubmitInput::new(
                DurableCommandId::from_uuid(Uuid::from_u128(0xC301)),
                parked.session,
                UserContent::try_text(String::from("steering retained by automatic recovery"))
                    .expect("fixture steering content is admitted"),
                DeliveryRequest::NextSafePoint {
                    expected_active_turn: parked.turn,
                },
            ),
            steering,
            None,
        )
        .await?;

    let batch = repository.claim_due().await?;
    let claimed = batch.claimed()[0];
    let outcome = repository.reconcile(claimed).await?;
    let unchargeable = GoalRepository::new(pool.clone())
        .unchargeable_automatic_resume_turns(parked.session, &[parked.turn])
        .await?;
    let durable: (String, String, String, i32, i64) = sqlx::query_as(
        "SELECT lifecycle.state_kind,
                lifecycle.terminal_disposition_kind,
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
    .bind(parked.turn.into_uuid())
    .fetch_one(&pool)
    .await?;
    let ambiguous_call: (String, String) = sqlx::query_as(
        "SELECT state_kind, terminal_disposition_kind
           FROM model_call
          WHERE model_call_id = $1",
    )
    .bind(parked.call.into_uuid())
    .fetch_one(&pool)
    .await?;

    assert_eq!(batch.claimed().len(), 1);
    assert_eq!(batch.exhausted(), &[]);
    assert_eq!(claimed.session(), parked.session);
    assert_eq!(claimed.turn(), parked.turn);
    assert_eq!(
        claimed.operation(),
        AutomaticReconciliationOperation::ModelCall(parked.call)
    );
    assert_eq!(claimed.attempt().get(), 1);
    assert_eq!(outcome, AutomaticReconciliationOutcome::Reconciled);
    assert_eq!(unchargeable.as_ref(), &[parked.turn]);
    assert_eq!(
        durable,
        (
            "terminal".into(),
            "reconciliation_required".into(),
            "reconciled".into(),
            1,
            1,
        )
    );
    assert_eq!(ambiguous_call, ("terminal".into(), "ambiguous".into()));
    let transcript = ProcessReadRepository::new(pool.clone())
        .read_transcript(parked.session)
        .await?
        .expect("the automatically reconciled terminal turn remains readable");
    assert!(matches!(
        transcript.turns()[0].state(),
        ProcessTurnState::ReconciliationRequired { .. }
    ));
    assert!(matches!(
        steering_outcome,
        SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Applied(
            SubmitInputAppliedResult::PendingSteering(_)
        ))
    ));
    let successor: Uuid = sqlx::query_scalar(
        "SELECT origin_turn_id
           FROM accepted_input
          WHERE accepted_input_id = $1
            AND disposition_kind = 'reclassified_as_turn_origin'",
    )
    .bind(steering.into_uuid())
    .fetch_one(&pool)
    .await?;
    let activated = activate_earliest_queued_turn(
        &pool,
        EarliestQueuedTurnActivation {
            session: parked.session.into_uuid(),
            origin_entry: Uuid::from_u128(0xC302),
            starting_frontier: Uuid::from_u128(0xC303),
            initial_attempt: Uuid::from_u128(0xC304),
        },
    )
    .await?;
    assert_eq!(activated.turn().into_uuid(), successor);

    pool.close().await;
    drop(container);
    Ok(())
}

/// S04: PostgreSQL, rather than a dropped client future, ends a recovery
/// transaction that cannot reach the commit-ordered outbox allocator. The
/// failed attempt therefore leaves no backend queued behind that allocator.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s04_automatic_reconciliation_server_bound_releases_its_database_work()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let parked = park_restart_ambiguity(&pool, 0xC500).await?;
    let repository = PostgresAutomaticReconciliationRepository::new(pool.clone());
    let batch = repository.claim_due().await?;
    let claimed = batch.claimed()[0];
    let mut allocator_holder = pool.begin().await?;
    let _: bool = sqlx::query_scalar(
        "SELECT singleton
           FROM outbox_sequence_state
          WHERE singleton
          FOR UPDATE",
    )
    .fetch_one(&mut *allocator_holder)
    .await?;
    let bounded_repository = repository.clone();
    let started = tokio::time::Instant::now();
    let reconciliation = tokio::spawn(async move { bounded_repository.reconcile(claimed).await });
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    // The probe counts blocked backends rather than one statement's text: the
    // reconciliation transaction reaches the commit-ordered allocator through a
    // durable trigger, and `pg_stat_activity` reports the top-level statement
    // that fired it, not the allocator lock the trigger takes.
    let waiting_before_timeout: i64 = sqlx::query_scalar(
        "SELECT count(*)
           FROM pg_stat_activity
          WHERE datname = current_database()
            AND wait_event_type = 'Lock'",
    )
    .fetch_one(&pool)
    .await?;
    let error = reconciliation
        .await?
        .expect_err("the database-side lock budget ends the blocked recovery");
    let elapsed = started.elapsed();
    let waiting_after_timeout: i64 = sqlx::query_scalar(
        "SELECT count(*)
           FROM pg_stat_activity
          WHERE datname = current_database()
            AND wait_event_type = 'Lock'",
    )
    .fetch_one(&pool)
    .await?;

    assert_eq!(batch.claimed().len(), 1);
    assert_eq!(claimed.session(), parked.session);
    assert_eq!(waiting_before_timeout, 1);
    assert_eq!(
        error.operator_failure_class(),
        OperatorFailureClass::Infrastructure {
            commit_ambiguous: false,
        }
    );
    assert!(elapsed < std::time::Duration::from_secs(5));
    assert_eq!(waiting_after_timeout, 0);

    allocator_holder.rollback().await?;
    pool.close().await;
    drop(container);
    Ok(())
}

/// S04 / S10: the existing operator reconciliation may win after an automatic
/// attempt is claimed; that attempt records supersession and never applies a
/// second terminal transition.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s04_operator_reconciliation_supersedes_a_claimed_automatic_attempt()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0xE100;
    let parked = park_restart_ambiguity(&pool, seed).await?;
    let repository = PostgresAutomaticReconciliationRepository::new(pool.clone());
    let batch = repository.claim_due().await?;
    let successor = TurnId::from_uuid(Uuid::from_u128(seed + 0x203));

    let operator = SubmitInputRepository::new(pool.clone())
        .handle(
            input_with_delivery(
                seed + 0x200,
                seed + 1,
                "operator wins automatic reconciliation race",
                DeliveryRequest::Interrupt {
                    expected_active_turn: parked.turn,
                    descendant_scope: DescendantTerminationScope::ParentAlone,
                    configuration: input_choices(1, ModelSelectionOverride::UseSessionDefault),
                },
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(seed + 0x202)),
            Some(successor),
        )
        .await?;
    let automatic = repository.claim_due().await?;
    let durable: (String, String, i64) = sqlx::query_as(
        "SELECT recovery.state_kind,
                attempt.outcome_kind,
                (SELECT count(*)
                   FROM turn_terminal_outbox_event
                  WHERE disposition_kind = 'reconciliation_required'
                  AND turn_id = recovery.turn_id)
           FROM automatic_reconciliation AS recovery
           JOIN automatic_reconciliation_attempt AS attempt
             ON attempt.turn_id = recovery.turn_id
            AND attempt.attempt_ordinal = recovery.attempt_count
          WHERE recovery.turn_id = $1",
    )
    .bind(parked.turn.into_uuid())
    .fetch_one(&pool)
    .await?;

    assert!(matches!(
        operator,
        SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Applied(
            SubmitInputAppliedResult::TurnOrigin(_)
        ))
    ));
    assert_eq!(batch.claimed().len(), 1);
    assert_eq!(automatic.claimed(), &[]);
    assert_eq!(automatic.exhausted(), &[]);
    assert_eq!(durable, ("superseded".into(), "superseded".into(), 1));
    let transcript = ProcessReadRepository::new(pool.clone())
        .read_transcript(parked.session)
        .await?
        .expect("the operator-reconciled terminal turn remains readable");
    assert!(matches!(
        transcript.turns()[0].state(),
        ProcessTurnState::ReconciliationRequired { .. }
    ));

    pool.close().await;
    drop(container);
    Ok(())
}

/// S04 / S10: an attempt that meets a held session scheduler row gives the row
/// up inside the database, so a busy row costs one classified infrastructure
/// failure with nothing written rather than a pooled connection checked out for
/// the whole real wait.
///
/// The attempt's other bound is its caller's client-side timeout, and dropping
/// that future queues a `ROLLBACK` instead of cancelling the running statement:
/// the backend would keep waiting and the connection would stay held while the
/// caller retried. The wait is therefore bounded inside the transaction or not
/// at all, and the retry after the contention clears is what shows the failure
/// is ordinary back pressure rather than a spent attempt.
///
/// The wrapper is the production one. `signalboxd`'s watchdog wraps every one of
/// these calls in a client-side bound set at a multiple of
/// [`RECONCILIATION_LOCK_WAIT`], and its compile-time assertion keeps the two in
/// step; a test that wrapped a wider bound of its own would pass on an ordering
/// the daemon never runs, which is exactly how an equal-deadline pair hides.
/// Both sides are asserted: the failure arrives before the caller's bound, and
/// not before the database budget it is supposed to have spent — together, that
/// `55P03` and not the client is what ended the wait.
///
/// The wait also runs far past [`RECONCILIATION_ACQUIRE_WAIT`], which is sound
/// only because that budget stops at the acquisition. A bound that still
/// covered `BEGIN` would be the client giving up on a connection the backend
/// was still using, which is the strand these tests exist to keep closed.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s04_a_contended_automatic_attempt_gives_the_row_up_inside_the_database()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let parked = park_restart_ambiguity(&pool, 0xF100).await?;
    let repository = PostgresAutomaticReconciliationRepository::new(pool.clone());
    let batch = repository.claim_due().await?;
    let claimed = batch.claimed()[0];
    let caller_bound = production_reconciliation_caller_bound();

    let mut holder = pool.begin().await?;
    sqlx::query(
        "SELECT session_id
           FROM session_scheduler
          WHERE session_id = $1
          FOR UPDATE",
    )
    .bind(parked.session.into_uuid())
    .fetch_one(&mut *holder)
    .await?;
    let started = std::time::Instant::now();
    let contended = tokio::time::timeout(caller_bound, repository.reconcile(claimed))
        .await
        .expect("the database budget expires before the production caller bound")
        .expect_err("a held scheduler row cannot be reconciled");
    let waited = started.elapsed();
    let parked_still: (String, String) = sqlx::query_as(
        "SELECT lifecycle.state_kind, recovery.state_kind
           FROM turn_lifecycle AS lifecycle
           JOIN automatic_reconciliation AS recovery
             ON recovery.turn_id = lifecycle.turn_id
          WHERE lifecycle.turn_id = $1",
    )
    .bind(parked.turn.into_uuid())
    .fetch_one(&pool)
    .await?;
    holder.rollback().await?;
    let retried = repository.reconcile(claimed).await?;

    let (commit_ambiguous, source) = reconciliation_database_failure(&contended);
    assert!(!commit_ambiguous);
    assert_eq!(
        source
            .as_database_error()
            .and_then(|failure| failure.code())
            .as_deref(),
        Some("55P03"),
        "the wait ends as lock_not_available, not as an open-ended block"
    );
    assert_eq!(
        contended.failure_kind(),
        AutomaticReconciliationFailureKind::Infrastructure
    );
    assert!(matches!(
        contended.operator_failure_class(),
        OperatorFailureClass::Infrastructure {
            commit_ambiguous: false
        }
    ));
    assert!(
        waited >= RECONCILIATION_LOCK_WAIT,
        "the attempt spent its database budget waiting, so {waited:?} is what \
         the server's lock_timeout ended rather than an earlier client giving up"
    );
    assert!(
        waited < caller_bound,
        "the database budget has to expire inside the caller's bound, but the \
         wait took {waited:?} of {caller_bound:?}"
    );
    assert_eq!(parked_still, ("active".into(), "attempting".into()));
    assert_eq!(retried, AutomaticReconciliationOutcome::Reconciled);

    pool.close().await;
    drop(container);
    Ok(())
}

/// The client-side bound `signalboxd` wraps each reconciliation call in.
///
/// Derived from the published database-side budget the same way production
/// derives it, so raising either one cannot leave this test asserting against a
/// pairing the daemon does not run.
fn production_reconciliation_caller_bound() -> std::time::Duration {
    reconciliation_deadline(None)
}

/// Returns the commit ambiguity and driver failure a database-class error carries.
///
/// Reading a variant out is branching, which rule 2 keeps out of a test body.
/// The helper absorbs only that: which failure each test expects, and what it
/// asserts about it, stay at the call site.
#[track_caller]
fn reconciliation_database_failure(
    error: &AutomaticReconciliationRepositoryError,
) -> (bool, &sqlx::Error) {
    match error {
        AutomaticReconciliationRepositoryError::Database {
            commit_ambiguous,
            source,
        } => (*commit_ambiguous, source),
        other => panic!("expected an ordinary database failure, got {other}"),
    }
}

/// S04 / S10: the durable failure record is bounded inside the database too, so
/// a run of contended attempts cannot strand a pooled connection apiece.
///
/// This transaction updates the attempt row and its recovery row, and both are
/// rows another daemon's claim scan already writes — it settles abandoned
/// attempts and marks superseded recoveries against exactly these two tables.
/// Unbounded it is the sibling defect one step later: the caller's dropped
/// future queues a `ROLLBACK` rather than cancelling, so the backend keeps
/// waiting while the watchdog moves on. Failing as `55P03` costs nothing, since
/// the attempt is already spent and the claim scan's own abandonment settlement
/// reaches a record this transaction could not write.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s04_a_contended_failure_record_gives_the_row_up_inside_the_database()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let parked = park_restart_ambiguity(&pool, 0xF200).await?;
    let repository = PostgresAutomaticReconciliationRepository::new(pool.clone());
    let batch = repository.claim_due().await?;
    let claimed = batch.claimed()[0];
    let caller_bound = production_reconciliation_caller_bound();

    let mut holder = pool.begin().await?;
    sqlx::query(
        "SELECT turn_id
           FROM automatic_reconciliation_attempt
          WHERE turn_id = $1 AND attempt_ordinal = $2
          FOR UPDATE",
    )
    .bind(parked.turn.into_uuid())
    .bind(i64::from(claimed.attempt().get()))
    .fetch_one(&mut *holder)
    .await?;
    let started = std::time::Instant::now();
    let contended = tokio::time::timeout(
        caller_bound,
        repository.record_failure(claimed, AutomaticReconciliationFailureKind::Infrastructure),
    )
    .await
    .expect("the database budget expires before the production caller bound")
    .expect_err("a held attempt row cannot be recorded against");
    let waited = started.elapsed();
    holder.rollback().await?;
    let recorded = repository
        .record_failure(claimed, AutomaticReconciliationFailureKind::Infrastructure)
        .await;
    let settled: (String, String) = sqlx::query_as(
        "SELECT attempt.outcome_kind, recovery.state_kind
           FROM automatic_reconciliation_attempt AS attempt
           JOIN automatic_reconciliation AS recovery
             ON recovery.turn_id = attempt.turn_id
          WHERE attempt.turn_id = $1 AND attempt.attempt_ordinal = $2",
    )
    .bind(parked.turn.into_uuid())
    .bind(i64::from(claimed.attempt().get()))
    .fetch_one(&pool)
    .await?;

    let (commit_ambiguous, source) = reconciliation_database_failure(&contended);
    assert!(!commit_ambiguous);
    assert_eq!(
        source
            .as_database_error()
            .and_then(|failure| failure.code())
            .as_deref(),
        Some("55P03"),
        "the wait ends as lock_not_available, not as an open-ended block"
    );
    assert!(
        waited >= RECONCILIATION_LOCK_WAIT && waited < caller_bound,
        "the server's budget is what ended the wait, but it took {waited:?}"
    );
    assert!(
        recorded.is_ok(),
        "the record is written once the contention clears"
    );
    assert_eq!(
        settled,
        ("infrastructure_failure".into(), "scheduled".into()),
        "the retried record classifies the attempt and reschedules the recovery"
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// S04 / S10: the acquisition budget bounds reaching a pooled connection and
/// nothing past it, so a pool with nothing left to hand out costs one
/// classified infrastructure failure that wrote nothing, rather than a watchdog
/// wake spent waiting out the driver's own thirty-second acquisition timeout.
///
/// Abandoning an acquisition is the one cancellation on this path that is free:
/// no transaction has begun and nothing has been sent, so no backend is left
/// running a statement and no connection is left checked out. That is exactly
/// why the budget stops there. `Pool::begin` would put `BEGIN` inside it, and
/// cancelling that is not a smaller failure but the original one — a queued
/// `ROLLBACK` on a connection held until the backend answers, under the
/// slowdown that made `BEGIN` slow to begin with, for one watchdog attempt
/// after another.
///
/// The pool is exhausted rather than the server slowed because that is the same
/// wait from the caller's side and is deterministic. Only the lower bound is
/// asserted, and the constant is read rather than restated: reaching it is what
/// shows the acquisition budget ran out rather than something earlier failing.
/// An upper bound would be measuring the host instead — elapsed time here
/// includes however long the test task waited to be rescheduled after the
/// budget became ready, so a paused runner could carry it past an unrelated
/// budget while the acquisition behaved exactly as asserted.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s04_an_exhausted_pool_ends_the_automatic_attempt_before_a_transaction_begins()
-> Result<(), Box<dyn Error>> {
    let (container, pool, database_url) = migrated_postgres().await?;
    let parked = park_restart_ambiguity(&pool, 0xF300).await?;
    let single = PgPoolOptions::new()
        .max_connections(1)
        .connect_with(local_test_connection_options(&database_url)?)
        .await?;
    let repository = PostgresAutomaticReconciliationRepository::new(single.clone());
    let batch = repository.claim_due().await?;
    let claimed = batch.claimed()[0];

    let held = single.acquire().await?;
    let started = std::time::Instant::now();
    let starved = repository
        .reconcile(claimed)
        .await
        .expect_err("an exhausted pool cannot be reconciled through");
    let waited = started.elapsed();
    let parked_still: (String, String) = sqlx::query_as(
        "SELECT lifecycle.state_kind, recovery.state_kind
           FROM turn_lifecycle AS lifecycle
           JOIN automatic_reconciliation AS recovery
             ON recovery.turn_id = lifecycle.turn_id
          WHERE lifecycle.turn_id = $1",
    )
    .bind(parked.turn.into_uuid())
    .fetch_one(&pool)
    .await?;
    drop(held);
    let retried = repository.reconcile(claimed).await?;

    let (commit_ambiguous, source) = reconciliation_database_failure(&starved);
    assert!(
        !commit_ambiguous,
        "no transaction began, so no commit could be in doubt"
    );
    assert!(
        matches!(source, sqlx::Error::PoolTimedOut),
        "the caller reads the driver's own acquisition failure, not a wrapper"
    );
    assert_eq!(
        starved.failure_kind(),
        AutomaticReconciliationFailureKind::Infrastructure
    );
    assert!(
        matches!(
            starved.operator_failure_class(),
            OperatorFailureClass::Infrastructure {
                commit_ambiguous: false
            }
        ),
        "an attempt that never began is unambiguous back pressure"
    );
    assert!(
        waited >= RECONCILIATION_ACQUIRE_WAIT,
        "the acquisition spent its budget, so {waited:?} is what that bound ended"
    );
    assert_eq!(
        parked_still,
        ("active".into(), "attempting".into()),
        "an attempt that never reached a connection wrote nothing"
    );
    assert_eq!(retried, AutomaticReconciliationOutcome::Reconciled);

    single.close().await;
    pool.close().await;
    drop(container);
    Ok(())
}

/// S04 / S10: infrastructure failures spend the exact automatic budget; only
/// then does the still-active ambiguity become a visible operator park.
/// S04 / S10: infrastructure failures spend the exact automatic budget; the
/// visible operator park can still be interrupted without leaving its durable
/// automatic record inconsistent with the terminal turn and queued successor.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s04_exhausted_automatic_reconciliation_is_visible_to_the_operator()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0xD100;
    let parked = park_restart_ambiguity(&pool, seed).await?;
    let repository = PostgresAutomaticReconciliationRepository::new(pool.clone()).with_policy(
        Some(5),
        Some(Duration::ZERO),
        Some(Duration::ZERO),
    );
    spend_automatic_reconciliation_budget(&repository, &pool).await?;

    let exhaustion = repository.claim_due().await?;
    let snapshot = ProcessReadRepository::new(pool.clone())
        .read_transcript(parked.session)
        .await?
        .expect("the parked session remains process-readable");
    let attempt_history: (i64, i64) = sqlx::query_as(
        "SELECT count(*),
                count(*) FILTER (WHERE outcome_kind = 'infrastructure_failure')
           FROM automatic_reconciliation_attempt
          WHERE turn_id = $1",
    )
    .bind(parked.turn.into_uuid())
    .fetch_one(&pool)
    .await?;

    assert_eq!(exhaustion.claimed(), &[]);
    assert_eq!(exhaustion.exhausted().len(), 1);
    assert_eq!(exhaustion.exhausted()[0].session(), parked.session);
    assert_eq!(exhaustion.exhausted()[0].turn(), parked.turn);
    assert_eq!(
        exhaustion.exhausted()[0].operation(),
        AutomaticReconciliationOperation::ModelCall(parked.call)
    );
    assert_eq!(automatic_recovery_status(&snapshot), (5, true));
    assert_eq!(attempt_history, (5, 5));

    let successor = TurnId::from_uuid(Uuid::from_u128(seed + 0x203));
    let operator = SubmitInputRepository::new(pool.clone())
        .handle(
            input_with_delivery(
                seed + 0x200,
                seed + 1,
                "operator recovers an exhausted automatic reconciliation",
                DeliveryRequest::Interrupt {
                    expected_active_turn: parked.turn,
                    descendant_scope: DescendantTerminationScope::ParentAlone,
                    configuration: input_choices(1, ModelSelectionOverride::UseSessionDefault),
                },
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(seed + 0x202)),
            Some(successor),
        )
        .await?;
    let transcript = ProcessReadRepository::new(pool.clone())
        .read_transcript(parked.session)
        .await?
        .expect("the operator-reconciled exhausted park remains readable");
    let preview = StartEligibleTurnRepository::new(pool.clone())
        .preview(
            parked.session,
            AcceptedInputTurnActivationIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 0x204)),
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 0x205)),
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 0x206)),
                TurnAttemptId::from_uuid(Uuid::from_u128(seed + 0x207)),
            ),
        )
        .await?
        .expect("the operator-created successor remains eligible");
    let durable: (String, i32) = sqlx::query_as(
        "SELECT state_kind, attempt_count
           FROM automatic_reconciliation
          WHERE turn_id = $1",
    )
    .bind(parked.turn.into_uuid())
    .fetch_one(&pool)
    .await?;

    assert!(matches!(
        operator,
        SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Applied(
            SubmitInputAppliedResult::TurnOrigin(_)
        ))
    ));
    assert!(matches!(
        transcript.turns()[0].state(),
        ProcessTurnState::ReconciliationRequired { .. }
    ));
    assert_eq!(preview.prepared().turn().turn(), successor);
    assert_eq!(durable, ("superseded".into(), 5));

    pool.close().await;
    drop(container);
    Ok(())
}

/// S04: first-time recovery discovery contends with an accepting operator
/// interrupt on the turn row instead of racing past its uncommitted
/// terminalization.
///
/// Without a lock that either side can see, discovery's `READ COMMITTED` snapshot
/// could enrol a fresh `scheduled` recovery for a turn the interrupt was
/// terminalizing, and the interrupt's own supersession could not see that
/// uncommitted insert. The committed pair is the shape `process_read` rejects as
/// corruption until a later supersession lap reaches it.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s04_recovery_discovery_waits_on_the_interrupted_turn_row() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0xD1A0;
    let parked = park_restart_ambiguity(&pool, seed).await?;
    // Unlike the waits the other `blocked_backends_reached` sites observe, this
    // one ends on the database's own budgets rather than on this test:
    // `claim_due` reaches its pooled connection within
    // `RECONCILIATION_ACQUIRE_WAIT` or gives up, and then waits for the row only
    // until `lock_timeout` raises `55P03`. Both are short enough that a
    // connection established *inside* that window consumes it — and connecting
    // is what this test would otherwise do twice over, because the pool holds
    // one idle connection here: the interrupt takes it, discovery opens a
    // second, and the observer's first sample opens a third. On a loaded host
    // those cold connects outlast the wait they exist to sample, and the test
    // then reports that discovery never waited when in fact it waited and was
    // timed out. Establishing all three up front leaves the window itself the
    // only thing the poll measures. Holding them at once is what forces the pool
    // to open one per holder; only the observer's is kept, and the other two go
    // back to the pool already established.
    let mut observer = pool.acquire().await?;
    let established = [pool.acquire().await?, pool.acquire().await?];
    drop(established);
    // The exact row lock an accepting interrupt's terminalizing `UPDATE`
    // takes, held open so discovery meets it before that interrupt commits.
    let mut interrupt_lock = pool.begin().await?;
    let locked_turn: Uuid = sqlx::query_scalar(
        "SELECT turn_id
           FROM turn_lifecycle
          WHERE session_id = $1 AND turn_id = $2
          FOR NO KEY UPDATE",
    )
    .bind(parked.session.into_uuid())
    .bind(parked.turn.into_uuid())
    .fetch_one(&mut *interrupt_lock)
    .await?;
    assert_eq!(locked_turn, parked.turn.into_uuid());
    // Nothing may stand between opening the window and sampling it.
    let discovery_pool = pool.clone();
    let discovery = tokio::spawn(async move {
        PostgresAutomaticReconciliationRepository::new(discovery_pool)
            .claim_due()
            .await
    });

    assert!(
        blocked_backends_reached_on(&mut observer, 1).await?,
        "discovery waits on the turn row the accepting interrupt terminalizes"
    );

    drop(observer);
    interrupt_lock.rollback().await?;
    let batch = discovery.await??;

    assert_eq!(batch.exhausted(), &[]);
    assert_eq!(batch.claimed().len(), 1);
    assert_eq!(batch.claimed()[0].session(), parked.session);
    assert_eq!(batch.claimed()[0].turn(), parked.turn);

    pool.close().await;
    drop(container);
    Ok(())
}

/// S03 / INV-014: a prepared model call remains discoverable for ordinary
/// active-turn resumption even when no tool round is active.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s03_inv014_prepared_model_call_is_resumable_without_tool_round()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let fixture = checkpoint_restart_model_call(&pool, 0xc700, false).await?;
    let active_tool_round = sqlx::query_scalar::<_, Option<Uuid>>(
        "SELECT active_tool_round_call_id
           FROM turn_lifecycle
          WHERE session_id = $1 AND turn_id = $2",
    )
    .bind(fixture.session.into_uuid())
    .bind(fixture.turn.into_uuid())
    .fetch_one(&pool)
    .await?;

    assert_eq!(active_tool_round, None);
    assert_eq!(
        PostgresToolLoopRepository::new(pool.clone())
            .find_resumable_turn(fixture.session)
            .await?,
        Some(fixture.turn)
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// S03 / S04 / S08 / INV-006 / INV-014 / INV-016 / INV-034: the production
/// startup repository applies call-aware recovery under its session lock:
/// Prepared remains retryable with its steering unchanged, an issued call becomes an exact
/// ambiguity wait, a stopped call terminalizes as reconciliation while
/// reclassifying its steering, that successor remains a valid replay origin,
/// and replay changes neither.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s03_s04_inv006_inv014_inv034_startup_recovery_leaves_zero_failed_turns()
-> Result<(), Box<dyn Error>> {
    let (container, pool, database_url) = migrated_postgres().await?;
    let prepared = checkpoint_restart_model_call(&pool, 0x2000, false).await?;
    let issued = checkpoint_restart_model_call(&pool, 0x3000, true).await?;
    let stopped = checkpoint_restart_model_call(&pool, 0x3500, true).await?;
    let prepared_steering = AcceptedInputId::from_uuid(Uuid::from_u128(0x6100));
    let issued_steering = AcceptedInputId::from_uuid(Uuid::from_u128(0x6101));
    let stopped_steering = AcceptedInputId::from_uuid(Uuid::from_u128(0x6102));
    assert!(matches!(
        SubmitInputRepository::new(pool.clone())
            .handle(
                SubmitInput::new(
                    DurableCommandId::from_uuid(Uuid::from_u128(0x4100)),
                    prepared.session,
                    UserContent::try_text(String::from("steering accepted before restart"))
                        .expect("fixture steering content is admitted"),
                    DeliveryRequest::NextSafePoint {
                        expected_active_turn: prepared.turn,
                    },
                ),
                prepared_steering,
                None,
            )
            .await?,
        SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Applied(
            SubmitInputAppliedResult::PendingSteering(_)
        ))
    ));
    assert!(matches!(
        SubmitInputRepository::new(pool.clone())
            .handle(
                SubmitInput::new(
                    DurableCommandId::from_uuid(Uuid::from_u128(0x4300)),
                    stopped.session,
                    UserContent::try_text(String::from("steering accepted before stopped restart"))
                        .expect("fixture steering content is admitted"),
                    DeliveryRequest::NextSafePoint {
                        expected_active_turn: stopped.turn,
                    },
                ),
                stopped_steering,
                None,
            )
            .await?,
        SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Applied(
            SubmitInputAppliedResult::PendingSteering(_)
        ))
    ));
    assert!(matches!(
        SubmitInputRepository::new(pool.clone())
            .handle(
                input_with_delivery(
                    0x4301,
                    0x3501,
                    "stop before restart",
                    DeliveryRequest::Interrupt {
                        expected_active_turn: stopped.turn,
                        descendant_scope: DescendantTerminationScope::ParentAlone,
                        configuration: input_choices(1, ModelSelectionOverride::UseSessionDefault,),
                    },
                ),
                AcceptedInputId::from_uuid(Uuid::from_u128(0x6103)),
                Some(TurnId::from_uuid(Uuid::from_u128(0x6203))),
            )
            .await?,
        SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Applied(
            SubmitInputAppliedResult::TurnOrigin(_)
        ))
    ));
    assert!(matches!(
        SubmitInputRepository::new(pool.clone())
            .handle(
                SubmitInput::new(
                    DurableCommandId::from_uuid(Uuid::from_u128(0x4200)),
                    issued.session,
                    UserContent::try_text(String::from("steering accepted before restart"))
                        .expect("fixture steering content is admitted"),
                    DeliveryRequest::NextSafePoint {
                        expected_active_turn: issued.turn,
                    },
                ),
                issued_steering,
                None,
            )
            .await?,
        SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Applied(
            SubmitInputAppliedResult::PendingSteering(_)
        ))
    ));

    pool.close().await;
    let restarted_pool = PgPoolOptions::new()
        .max_connections(4)
        .connect_with(local_test_connection_options(&database_url)?)
        .await?;
    let mut scan = StartupScanService::new(
        FixedStartupScanIds::new(
            [
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0x4001)),
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0x4002)),
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0x4003)),
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0x4004)),
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0x4005)),
            ],
            [
                ContextFrontierId::from_uuid(Uuid::from_u128(0x5001)),
                ContextFrontierId::from_uuid(Uuid::from_u128(0x5002)),
                ContextFrontierId::from_uuid(Uuid::from_u128(0x5003)),
                ContextFrontierId::from_uuid(Uuid::from_u128(0x5004)),
                ContextFrontierId::from_uuid(Uuid::from_u128(0x5005)),
            ],
        )
        .with_reclassified_turns([TurnId::from_uuid(Uuid::from_u128(0x6202))]),
        PostgresStartupScanRepository::new(restarted_pool.clone()),
    );

    let first = scan.execute().await?;
    assert_eq!(first.recovered_turn_count(), 1);
    let startup_recovery_failed_turns: i64 = sqlx::query_scalar(
        "SELECT count(*)
           FROM turn_lifecycle
          WHERE session_id = ANY($1::uuid[])
            AND terminal_disposition_kind = 'failed'",
    )
    .bind(vec![
        prepared.session.into_uuid(),
        issued.session.into_uuid(),
        stopped.session.into_uuid(),
    ])
    .fetch_one(&restarted_pool)
    .await?;
    assert_eq!(
        startup_recovery_failed_turns, 0,
        "startup recovery leaves no failed turn in this call-aware fixture"
    );

    let prepared_state: (
        String,
        Option<String>,
        String,
        Option<String>,
        String,
        Uuid,
        Uuid,
    ) = sqlx::query_as(
        "SELECT call.state_kind,
                call.terminal_disposition_kind,
                attempt.state_kind,
                attempt.end_disposition,
                turn.state_kind,
                turn.current_attempt_id,
                call.model_call_id
           FROM model_call AS call
           JOIN turn_attempt AS attempt
             ON attempt.turn_attempt_id = call.turn_attempt_id
           JOIN turn_lifecycle AS turn
             ON turn.turn_id = call.turn_id
          WHERE call.model_call_id = $1",
    )
    .bind(prepared.call.into_uuid())
    .fetch_one(&restarted_pool)
    .await?;
    assert_eq!(
        prepared_state,
        (
            "prepared".into(),
            None,
            "prepared".into(),
            None,
            "active".into(),
            prepared.attempt.into_uuid(),
            prepared.call.into_uuid(),
        )
    );

    let issued_state: (String, String, String, String, String, Uuid) = sqlx::query_as(
        "SELECT call.state_kind,
                call.terminal_disposition_kind,
                attempt.state_kind,
                attempt.end_disposition,
                turn.active_phase_kind,
                turn.recovery_model_call_id
           FROM model_call AS call
           JOIN turn_attempt AS attempt
             ON attempt.turn_attempt_id = call.turn_attempt_id
           JOIN turn_lifecycle AS turn
             ON turn.turn_id = call.turn_id
          WHERE call.model_call_id = $1",
    )
    .bind(issued.call.into_uuid())
    .fetch_one(&restarted_pool)
    .await?;
    assert_eq!(
        issued_state,
        (
            "terminal".into(),
            "ambiguous".into(),
            "ended".into(),
            "lost".into(),
            "awaiting_model_call_recovery".into(),
            issued.call.into_uuid(),
        )
    );
    let stopped_state: (String, String, String, String, String) = sqlx::query_as(
        "SELECT call.state_kind,
                call.terminal_disposition_kind,
                attempt.end_variant,
                attempt.end_disposition,
                turn.terminal_disposition_kind
           FROM model_call AS call
           JOIN turn_attempt AS attempt
             ON attempt.turn_attempt_id = call.turn_attempt_id
           JOIN turn_lifecycle AS turn
             ON turn.turn_id = call.turn_id
          WHERE call.model_call_id = $1",
    )
    .bind(stopped.call.into_uuid())
    .fetch_one(&restarted_pool)
    .await?;
    assert_eq!(
        stopped_state,
        (
            "terminal".into(),
            "ambiguous".into(),
            "after_cancellation".into(),
            "lost".into(),
            "reconciliation_required".into(),
        )
    );
    let steering_state: (String, Option<Uuid>, String, Option<Uuid>) = sqlx::query_as(
        "SELECT prepared.disposition_kind,
                prepared.origin_turn_id,
                issued.disposition_kind,
                issued.origin_turn_id
           FROM accepted_input AS prepared
           CROSS JOIN accepted_input AS issued
          WHERE prepared.accepted_input_id = $1
            AND issued.accepted_input_id = $2",
    )
    .bind(prepared_steering.into_uuid())
    .bind(issued_steering.into_uuid())
    .fetch_one(&restarted_pool)
    .await?;
    assert_eq!(
        steering_state,
        (
            "pending_steering".into(),
            None,
            "pending_steering".into(),
            None,
        )
    );
    let stopped_steering_state: (String, Option<Uuid>) = sqlx::query_as(
        "SELECT disposition_kind, origin_turn_id
           FROM accepted_input
          WHERE accepted_input_id = $1",
    )
    .bind(stopped_steering.into_uuid())
    .fetch_one(&restarted_pool)
    .await?;
    assert_eq!(
        stopped_steering_state,
        (
            "reclassified_as_turn_origin".into(),
            Some(Uuid::from_u128(0x6202)),
        )
    );
    assert!(matches!(
        SubmitInputRepository::new(restarted_pool.clone())
            .handle(
                start_input(
                    0x4302,
                    0x3501,
                    "work after reconciled restart",
                    1,
                    ModelSelectionOverride::UseSessionDefault,
                ),
                AcceptedInputId::from_uuid(Uuid::from_u128(0x6104)),
                Some(TurnId::from_uuid(Uuid::from_u128(0x6204))),
            )
            .await?,
        SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Applied(
            SubmitInputAppliedResult::TurnOrigin(_)
        ))
    ));

    let mut stale_recovery_ids = FixedStartupScanIds::new([], []);
    assert_eq!(
        PostgresStartupScanRepository::new(restarted_pool.clone())
            .recover(
                prepared.session,
                signalbox_domain::AcceptedInputTurnFailureIdentities::new(
                    SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0x6301)),
                    ContextFrontierId::from_uuid(Uuid::from_u128(0x6302)),
                ),
                &mut stale_recovery_ids,
            )
            .await?,
        StartupScanSessionOutcome::ResumablePreparedModelCall {
            turn: prepared.turn,
        }
    );

    let replay = scan.execute().await?;
    assert_eq!(replay.recovered_turn_count(), 0);
    let unchanged: (i64, i64, i64, i64) = sqlx::query_as(
        "SELECT
            (SELECT count(*) FROM model_call
              WHERE model_call_id IN ($1, $2) AND state_kind = 'terminal'),
            (SELECT count(*) FROM turn_attempt
              WHERE turn_attempt_id IN ($3, $4) AND state_kind = 'ended'),
            (SELECT count(*) FROM semantic_transcript_entry
              WHERE failed_turn_id = $5),
            (SELECT count(*) FROM semantic_transcript_entry
              WHERE failed_turn_id = $6)",
    )
    .bind(prepared.call.into_uuid())
    .bind(issued.call.into_uuid())
    .bind(prepared.attempt.into_uuid())
    .bind(issued.attempt.into_uuid())
    .bind(prepared.turn.into_uuid())
    .bind(issued.turn.into_uuid())
    .fetch_one(&restarted_pool)
    .await?;
    assert_eq!(unchanged, (1, 1, 0, 0));
    assert_ne!(prepared.session, issued.session);

    let activated_interrupt = activate_earliest_queued_turn(
        &restarted_pool,
        EarliestQueuedTurnActivation {
            session: stopped.session.into_uuid(),
            origin_entry: Uuid::from_u128(0x6400),
            starting_frontier: Uuid::from_u128(0x6401),
            initial_attempt: Uuid::from_u128(0x6402),
        },
    )
    .await?;
    assert_eq!(
        activated_interrupt.turn(),
        TurnId::from_uuid(Uuid::from_u128(0x6203))
    );
    record_empty_instruction_manifest(&restarted_pool, stopped.session).await?;
    let empty_targets =
        ModelTargetCatalog::try_from_definitions([]).expect("an empty target catalog is valid");
    let target_miss = PostgresModelCallRepository::new(
        restarted_pool.clone(),
        empty_targets,
        model_credential_reference(),
    );
    let PrepareInitialModelCallOutcome::TargetUnavailable(failed_interrupt) = target_miss
        .prepare_initial_call(
            stopped.session,
            ModelCallId::from_uuid(Uuid::from_u128(0x6403)),
            FailedModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0x6404)),
                ContextFrontierId::from_uuid(Uuid::from_u128(0x6405)),
            ),
            ContextFrontierId::from_uuid(Uuid::from_u128(0x6406)),
            |_| panic!("the interrupt successor has no pending steering"),
        )
        .await?
    else {
        panic!("the unavailable target must release the interrupt successor");
    };
    assert_eq!(
        failed_interrupt.turn(),
        TurnId::from_uuid(Uuid::from_u128(0x6203))
    );

    let activated_reclassified = activate_earliest_queued_turn(
        &restarted_pool,
        EarliestQueuedTurnActivation {
            session: stopped.session.into_uuid(),
            origin_entry: Uuid::from_u128(0x6410),
            starting_frontier: Uuid::from_u128(0x6411),
            initial_attempt: Uuid::from_u128(0x6412),
        },
    )
    .await?;
    let reclassified_turn = TurnId::from_uuid(Uuid::from_u128(0x6202));
    assert_eq!(activated_reclassified.turn(), reclassified_turn);

    let descendant_command = DurableCommandId::from_uuid(Uuid::from_u128(0x4303));
    let descendant = SubmitInputRepository::new(restarted_pool.clone())
        .handle(
            input_with_delivery(
                0x4303,
                0x3501,
                "work after reconciliation-origin steering",
                DeliveryRequest::AfterCurrentTurn {
                    expected_active_turn: reclassified_turn,
                    configuration: input_choices(1, ModelSelectionOverride::UseSessionDefault),
                },
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(0x6105)),
            Some(TurnId::from_uuid(Uuid::from_u128(0x6205))),
        )
        .await?;
    let SubmitInputHandlingOutcome::Recorded(descendant_result) = &descendant else {
        panic!("the descendant command was newly recorded");
    };
    assert!(matches!(
        descendant_result,
        SubmitInputResult::Applied(SubmitInputAppliedResult::TurnOrigin(_))
    ));
    assert_eq!(
        SubmitInputRepository::new(restarted_pool.clone())
            .load(descendant_command)
            .await?
            .expect("the descendant command must replay")
            .result(),
        descendant_result
    );

    restarted_pool.close().await;
    drop(container);
    Ok(())
}

/// S04 / INV-014 / INV-034: restart recovery reconstructs a committed call
/// from its durable provider target even after deployment configuration remaps
/// the selected model.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s04_inv014_inv034_restart_recovery_preserves_durable_target_after_catalog_remap()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0x7000;
    let fixture = checkpoint_restart_model_call(&pool, seed, false).await?;
    let selection = signalbox_domain::DirectModelSelection::from_uuid(Uuid::from_u128(seed + 5));
    let durable_provider = ProviderModelIdentity::from_uuid(Uuid::from_u128(seed + 6));
    let remapped_provider = ProviderModelIdentity::from_uuid(Uuid::from_u128(seed + 20));
    let remapped_targets = ModelTargetCatalog::try_from_definitions([ModelTargetDefinition::new(
        selection,
        ResolvedProviderTarget::naming(remapped_provider),
    )])
    .expect("one remapped target forms a catalog");
    let repository = PostgresModelCallRepository::new(
        pool.clone(),
        remapped_targets,
        model_credential_reference(),
    );

    let outcome = repository
        .recover_after_restart(
            fixture.session,
            fixture.call,
            FailedModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 30)),
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 31)),
            ),
        )
        .await?;
    let ModelCallTerminalOutcome::Failed(failed) = outcome else {
        panic!("the durable Prepared call must recover as known failure");
    };
    assert_eq!(
        failed
            .call()
            .expect("restart recovery retains the physical call")
            .target()
            .identity(),
        durable_provider
    );
    assert_ne!(durable_provider, remapped_provider);

    pool.close().await;
    drop(container);
    Ok(())
}

/// S04 / S08 / S09 / INV-016 / INV-053: steering accepted after send
/// authorization is atomically reclassified when the source completes. Its
/// immutable command still replays PendingSteering, while the inherited
/// successor enters the ordinary scheduler with the source's exact settings
/// evidence and activates after the terminal source.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s04_s08_s09_inv016_inv053_terminal_call_reclassifies_and_schedules_pending_steering()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let session = SessionId::from_uuid(Uuid::from_u128(0x8e4));
    let selection = signalbox_domain::DirectModelSelection::from_uuid(Uuid::from_u128(0xce4));
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(prepared_with_low_reasoning(0x4e4, 0x8e4, selection))
        .await?;

    let source_input = AcceptedInputId::from_uuid(Uuid::from_u128(0x9e4));
    let source_turn = TurnId::from_uuid(Uuid::from_u128(0xae4));
    let capabilities =
        ModelCapabilityCatalog::try_from_definitions([ModelCapabilityDefinition::new(
            selection,
            ModelCapabilities::new(
                BTreeSet::from([ReasoningLevel::Low]),
                FastModeSupport::Unsupported,
                BTreeSet::new(),
            ),
        )])
        .expect("one settings capability forms a catalog");
    let inputs = SubmitInputRepository::with_model_capabilities(pool.clone(), capabilities);
    let source_per_call = ModelSettingsOverlay::new(
        SettingOverlay::ProviderDefault,
        FastModeOverlay::Inherit,
        SettingOverlay::Inherit,
    );
    inputs
        .handle(
            SubmitInput::new(
                DurableCommandId::from_uuid(Uuid::from_u128(0x4e5)),
                session,
                UserContent::try_text(String::from("source request"))
                    .expect("fixture source content is admitted"),
                DeliveryRequest::StartWhenNoActiveTurn {
                    configuration: PerInputConfigurationChoices::with_model_settings(
                        SessionConfigurationDefaultsVersion::try_from_u64(1)
                            .expect("the fixture version is positive"),
                        ModelSelectionOverride::UseSessionDefault,
                        source_per_call,
                    ),
                },
            ),
            source_input,
            Some(source_turn),
        )
        .await?;
    let source_attempt = TurnAttemptId::from_uuid(Uuid::from_u128(0xbe4));
    let mut source_activation = StartEligibleTurnService::new(
        FixedStartEligibleTurnIds::new(
            [SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xde4))],
            [ContextFrontierId::from_uuid(Uuid::from_u128(0xee4))],
            [source_attempt],
        ),
        StartEligibleTurnRepository::new(pool.clone()),
    );
    assert!(matches!(
        source_activation.execute(session).await?,
        StartEligibleTurnOutcome::Activated(_)
    ));
    record_empty_instruction_manifest(&pool, session).await?;

    let provider = ProviderModelIdentity::from_uuid(Uuid::from_u128(0xfe4));
    let targets = ModelTargetCatalog::try_from_definitions([ModelTargetDefinition::new(
        selection,
        ResolvedProviderTarget::naming(provider),
    )])
    .expect("one target is a valid catalog");
    let mut calls =
        PostgresModelCallRepository::new(pool.clone(), targets, model_credential_reference());
    let call = ModelCallId::from_uuid(Uuid::from_u128(0xce5));
    let PrepareInitialModelCallOutcome::Checkpointed(checkpointed) = calls
        .prepare_initial_call(
            session,
            call,
            FailedModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xdf4)),
                ContextFrontierId::from_uuid(Uuid::from_u128(0xef4)),
            ),
            ContextFrontierId::from_uuid(Uuid::from_u128(0xff4)),
            |_| {
                (
                    SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xcf4)),
                    TurnId::from_uuid(Uuid::from_u128(0xcf5)),
                )
            },
        )
        .await?
    else {
        panic!("the exact Prepared fixture checkpoints")
    };
    assert_eq!(checkpointed, call);
    let AuthorizeModelCallOutcome::Authorized(authorized) =
        calls.authorize_send(session, call).await?
    else {
        panic!("the exact Prepared call must authorize")
    };
    let authorized = *authorized;

    let steering_command = DurableCommandId::from_uuid(Uuid::from_u128(0x4e6));
    let steering_input = AcceptedInputId::from_uuid(Uuid::from_u128(0x9e5));
    let recorded = inputs
        .handle(
            SubmitInput::new(
                steering_command,
                session,
                UserContent::try_text("follow-up steering".to_owned())
                    .expect("fixture content is valid"),
                DeliveryRequest::NextSafePoint {
                    expected_active_turn: source_turn,
                },
            ),
            steering_input,
            None,
        )
        .await?;
    assert!(matches!(
        recorded,
        SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Applied(
            SubmitInputAppliedResult::PendingSteering(_)
        ))
    ));

    let successor = TurnId::from_uuid(Uuid::from_u128(0xae5));
    let terminal_frontier = ContextFrontierId::from_uuid(Uuid::from_u128(0xee5));
    let outcome = calls
        .commit_observation(
            session,
            authorized
                .observation_correlation()
                .bind_terminal_observation(ModelCallTerminalObservation::Completed {
                    assistant_text: vec![
                        AssistantText::try_new("source reply".to_owned())
                            .expect("fixture assistant content is valid"),
                    ],
                }),
            signalbox_application::ModelCallTerminalIdentityCandidates::Exact(
                ModelCallTerminalIdentities::Completed(CompletedModelCallIdentities::new(
                    vec![SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xde5))],
                    SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xde6)),
                    terminal_frontier,
                )),
            ),
            |accepted| {
                assert_eq!(accepted, steering_input);
                successor
            },
        )
        .await?;
    let Some(ModelCallObservationCommitOutcome::Terminal(terminal)) = outcome else {
        panic!("the source call must complete");
    };
    let ModelCallTerminalOutcome::Completed(completed) = *terminal else {
        panic!("the source call must complete");
    };
    assert_eq!(completed.reclassified_pending_steering().len(), 1);
    assert_eq!(
        completed.reclassified_pending_steering()[0].turn(),
        successor
    );

    let durable: (String, Uuid, Uuid, String, i64, i64, i64, i64) = sqlx::query_as(
        "SELECT accepted.disposition_kind,
                accepted.expected_active_turn_id,
                accepted.origin_turn_id,
                successor.state_kind,
                (SELECT count(*)
                   FROM queued_input_origin AS queued
                  WHERE queued.turn_id = $3
                    AND queued.accepted_input_id = $1
                    AND queued.source_configuration_turn_id = $4
                    AND queued.defaults_version IS NULL
                    AND queued.requested_model_kind IS NULL
                    AND queued.frozen_model_kind IS NULL
                    AND queued.model_parameters IS NULL
                    AND queued.known_provider_failure_retry IS NULL
                    AND queued.model_fallback IS NULL),
                (SELECT count(*)
                   FROM input_accepted_outbox_event AS event
                  WHERE event.accepted_input_id = $1
                    AND event.session_id = $2
                    AND event.turn_id = $3
                    AND event.acceptance_position = accepted.acceptance_position),
                (SELECT count(*)
                   FROM turn_model_settings_resolved AS successor_settings
                   JOIN turn_model_settings_resolved AS source_settings
                     ON source_settings.turn_id = $4
                    AND source_settings.session_id = $2
                    AND successor_settings.defaults_version =
                        source_settings.defaults_version
                    AND successor_settings.selected_direct_model_id =
                        source_settings.selected_direct_model_id
                    AND successor_settings.per_call_model_settings =
                        source_settings.per_call_model_settings
                    AND successor_settings.resolved_model_settings =
                        source_settings.resolved_model_settings
                    AND successor_settings.adjusted_from_selection_id
                        IS NOT DISTINCT FROM
                        source_settings.adjusted_from_selection_id
                    AND successor_settings.adjustments = source_settings.adjustments
                  WHERE successor_settings.accepted_input_id = $1
                    AND successor_settings.turn_id = $3
                    AND successor_settings.session_id = $2),
                (SELECT count(*)
                   FROM turn_model_settings_resolved_outbox_event AS event
                  WHERE event.accepted_input_id = $1
                    AND event.session_id = $2)
           FROM accepted_input AS accepted
           JOIN turn_lifecycle AS successor
             ON successor.turn_id = accepted.origin_turn_id
          WHERE accepted.accepted_input_id = $1
            AND accepted.session_id = $2",
    )
    .bind(steering_input.into_uuid())
    .bind(session.into_uuid())
    .bind(successor.into_uuid())
    .bind(source_turn.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        durable,
        (
            "reclassified_as_turn_origin".into(),
            source_turn.into_uuid(),
            successor.into_uuid(),
            "queued".into(),
            1,
            1,
            1,
            1,
        )
    );

    let successor_settings_sequence: Decimal = sqlx::query_scalar(
        "SELECT event_sequence
           FROM turn_model_settings_resolved_outbox_event
          WHERE accepted_input_id = $1",
    )
    .bind(steering_input.into_uuid())
    .fetch_one(&pool)
    .await?;
    rewind_outbox_delivery_before(&pool, successor_settings_sequence).await?;
    assert_eq!(
        OutboxDispatcher::new(pool.clone())
            .dispatch_next(|event| {
                let DispatchedOutboxEventKind::TurnModelSettingsResolved(settings) = event.kind()
                else {
                    panic!("the selected sequence carries turn settings evidence")
                };
                assert_eq!(settings.accepted_input(), steering_input);
                assert_eq!(settings.per_call_override(), source_per_call);
                OutboxDeliveryDecision::Delivered
            })
            .await?,
        OutboxDispatchOutcome::Delivered {
            sequence: u64::try_from(successor_settings_sequence).expect("sequence fits u64")
        }
    );
    let snapshot = ProcessReadRepository::new(pool.clone())
        .read_transcript(session)
        .await?
        .expect("the reclassified transcript remains readable");
    let successor_settings = snapshot.turns()[1]
        .model_settings()
        .expect("the successor copied source settings evidence");
    assert_eq!(successor_settings.per_call_override(), source_per_call);

    let replay = inputs
        .load(steering_command)
        .await?
        .expect("the immutable command receipt must remain readable");
    let SubmitInputResult::Applied(SubmitInputAppliedResult::PendingSteering(pending)) =
        replay.result()
    else {
        panic!("the immutable command receipt retains PendingSteering")
    };
    assert_eq!(pending.accepted_input(), steering_input);
    assert_eq!(pending.binding().source_turn(), source_turn);
    let (eligible, _dispatch_starts, continuation) = PostgresEligibilitySweep::new(pool.clone())
        .find_sessions()
        .await?
        .into_parts();
    assert!(!continuation);
    assert_eq!(eligible, vec![session]);

    let mut successor_activation = StartEligibleTurnService::new(
        FixedStartEligibleTurnIds::new(
            [SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xde7))],
            [ContextFrontierId::from_uuid(Uuid::from_u128(0xee6))],
            [TurnAttemptId::from_uuid(Uuid::from_u128(0xbe5))],
        ),
        StartEligibleTurnRepository::new(pool.clone()),
    );
    let StartEligibleTurnOutcome::Activated(activated) =
        successor_activation.execute(session).await?
    else {
        panic!("the reclassified successor must activate");
    };
    assert_eq!(activated.turn(), successor);
    assert_eq!(
        activated.start().lineage(),
        AcceptedInputStartingLineage::After {
            immediate_predecessor: source_turn,
        }
    );
    assert_eq!(
        activated.configuration_provenance(),
        TurnConfigurationProvenance::InheritedForReclassifiedSteering(
            signalbox_domain::SteeringBinding::new(source_turn),
        )
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// S08 / S21 / INV-006 / INV-014 / INV-032 / INV-036: immutable target
/// resolution failure creates no targetless call, reclassifies the complete
/// pending steering prefix, and atomically closes the prepared attempt and turn
/// with its semantic failure boundary and typed outbox event.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s08_s21_inv006_inv014_inv032_inv036_target_unavailable_reclassifies_steering()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let session = SessionId::from_uuid(Uuid::from_u128(0x8f1));
    let direct_selection =
        signalbox_domain::DirectModelSelection::from_uuid(Uuid::from_u128(0xcf1));
    let mut create_service = CreateSessionService::new(
        FixedSessionIds::new([session]),
        CreateSessionRepository::new(pool.clone(), test_session_credential_pin()),
    );
    let CreateSessionOutcome::Applied(_) = create_service
        .execute(CreateSessionRequest::try_new(
            DurableCommandId::from_uuid(Uuid::from_u128(0x4f1)),
            SessionConfigurationDefaults::new(ModelSelectionRequest::Direct(direct_selection)),
        )?)
        .await?
    else {
        panic!("the target-miss fixture session must be created");
    };

    let accepted_input = AcceptedInputId::from_uuid(Uuid::from_u128(0x9f1));
    let turn = TurnId::from_uuid(Uuid::from_u128(0xaf1));
    let mut submit_service = SubmitInputService::new(
        FixedSubmitInputIds::new([accepted_input], [turn]),
        SubmitInputRepository::new(pool.clone()),
        AcceptingEligibilityNudge,
        signalbox_application::InProcessToolDispatchGate::default(),
    );
    let SubmitInputOutcome::Recorded(SubmitInputResult::Applied(
        SubmitInputAppliedResult::TurnOrigin(origin),
    )) = submit_service
        .execute(SubmitInputRequest::try_new(
            DurableCommandId::from_uuid(Uuid::from_u128(0x4f2)),
            session,
            UserContent::try_text("request with unavailable target".to_owned())
                .expect("fixture user content is admitted"),
            DeliveryRequest::StartWhenNoActiveTurn {
                configuration: input_choices(1, ModelSelectionOverride::UseSessionDefault),
            },
        )?)
        .await?
    else {
        panic!("the target-miss fixture input must be accepted");
    };
    assert_eq!(origin.accepted_input(), accepted_input);
    assert_eq!(origin.turn(), turn);

    let attempt = TurnAttemptId::from_uuid(Uuid::from_u128(0xbf1));
    let mut activation_service = StartEligibleTurnService::new(
        FixedStartEligibleTurnIds::new(
            [SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xdf1))],
            [ContextFrontierId::from_uuid(Uuid::from_u128(0xef1))],
            [attempt],
        ),
        StartEligibleTurnRepository::new(pool.clone()),
    );
    let StartEligibleTurnOutcome::Activated(activated) =
        activation_service.execute(session).await?
    else {
        panic!("the target-miss fixture turn must activate");
    };
    assert_eq!(activated.turn(), turn);
    record_empty_instruction_manifest(&pool, session).await?;

    let pending_input = AcceptedInputId::from_uuid(Uuid::from_u128(0x9f2));
    let reclassified_turn = TurnId::from_uuid(Uuid::from_u128(0xaf2));
    let SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Applied(
        SubmitInputAppliedResult::PendingSteering(_),
    )) = SubmitInputRepository::new(pool.clone())
        .handle(
            SubmitInput::new(
                DurableCommandId::from_uuid(Uuid::from_u128(0x4f3)),
                session,
                UserContent::try_text("steering before target miss".to_owned())
                    .expect("fixture steering is admitted"),
                DeliveryRequest::NextSafePoint {
                    expected_active_turn: turn,
                },
            ),
            pending_input,
            None,
        )
        .await?
    else {
        panic!("the target-miss fixture steering must remain pending");
    };

    let targets = ModelTargetCatalog::try_from_definitions([])
        .expect("an empty immutable target catalog is valid");
    let repository =
        PostgresModelCallRepository::new(pool.clone(), targets, model_credential_reference());
    let call_candidate = ModelCallId::from_uuid(Uuid::from_u128(0xcf2));
    let failure_entry = SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xdf2));
    let terminal_frontier = ContextFrontierId::from_uuid(Uuid::from_u128(0xef2));
    let collision = repository
        .prepare_initial_call(
            session,
            call_candidate,
            FailedModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xdf3)),
                ContextFrontierId::from_uuid(Uuid::from_u128(0xef3)),
            ),
            ContextFrontierId::from_uuid(Uuid::from_u128(0xff1)),
            |_| {
                (
                    SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xcf4)),
                    turn,
                )
            },
        )
        .await
        .expect_err("a source-turn fallback candidate must be retryable");
    assert!(matches!(
        collision,
        ModelCallRepositoryError::IdentityCollision(ModelCallIdentityCollision::ReclassifiedTurn)
    ));
    let PrepareInitialModelCallOutcome::TargetUnavailable(failed) = repository
        .prepare_initial_call(
            session,
            call_candidate,
            FailedModelCallTurnIdentities::new(failure_entry, terminal_frontier),
            ContextFrontierId::from_uuid(Uuid::from_u128(0xff2)),
            |_| {
                (
                    SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xcf3)),
                    reclassified_turn,
                )
            },
        )
        .await?
    else {
        panic!("the unavailable configured target must close without a call");
    };
    assert_eq!(failed.turn(), turn);
    assert!(failed.call().is_none());

    let reclassification_shape: (i64, i64) = sqlx::query_as(
        "SELECT
            (SELECT count(*) FROM accepted_input
              WHERE accepted_input_id = $1
                AND disposition_kind = 'reclassified_as_turn_origin'
                AND origin_turn_id = $2),
            (SELECT count(*) FROM queued_input_origin
              WHERE accepted_input_id = $1
                AND turn_id = $2
                AND source_configuration_turn_id = $3)",
    )
    .bind(pending_input.into_uuid())
    .bind(reclassified_turn.into_uuid())
    .bind(turn.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(reclassification_shape, (1, 1));

    let durable_shape: (i64, i64, i64, i64, i64) = sqlx::query_as(
        "SELECT
            (SELECT count(*) FROM model_call
              WHERE model_call_id = $1),
            (SELECT count(*) FROM turn_attempt
              WHERE turn_attempt_id = $2
                AND state_kind = 'ended'
                AND end_variant = 'without_stop'
                AND end_disposition = 'known_failure'),
            (SELECT count(*) FROM semantic_transcript_entry
              WHERE semantic_entry_id = $3
                AND payload_kind = 'turn_failed'
                AND failed_turn_id = $4),
            (SELECT count(*) FROM turn_lifecycle
              WHERE turn_id = $4
                AND state_kind = 'terminal'
                AND terminal_disposition_kind = 'failed'
                AND terminal_frontier_id = $5
                AND terminal_attempt_id = $2
                AND terminal_model_call_id IS NULL),
            (SELECT count(*) FROM turn_terminal_outbox_event
              WHERE disposition_kind = 'failed'
              AND turn_id = $4
                AND failure_entry_id = $3
                AND terminal_frontier_id = $5)",
    )
    .bind(call_candidate.into_uuid())
    .bind(attempt.into_uuid())
    .bind(failure_entry.into_uuid())
    .bind(turn.into_uuid())
    .bind(terminal_frontier.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(durable_shape, (0, 1, 1, 1, 1));

    pool.close().await;
    drop(container);
    Ok(())
}
