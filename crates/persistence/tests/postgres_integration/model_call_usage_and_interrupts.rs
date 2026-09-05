//! Model call usage evidence, credential references, provider failure causes, and interrupt or stop history.

use crate::*;

fn expect_ready_model_call(
    outcome: PrepareInitialModelCallOutcome,
) -> Box<PreparedModelCallRequest> {
    match outcome {
        PrepareInitialModelCallOutcome::Ready { request, .. } => request,
        _ => panic!("the fixture call must resume from its Prepared checkpoint"),
    }
}

/// the credential-reference column is total; the migrated schema
/// rejects a NULL stored reference.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn model_call_credential_reference_is_total() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;

    let is_nullable: String = sqlx::query_scalar(
        "SELECT is_nullable
           FROM information_schema.columns
          WHERE table_schema = 'public'
            AND table_name = 'model_call'
            AND column_name = 'credential_reference'",
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(is_nullable, "NO");

    pool.close().await;
    drop(container);

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn model_call_usage_transcript_lookup_is_session_indexed() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;

    let index_definition: String = sqlx::query_scalar(
        "SELECT indexdef
           FROM pg_indexes
          WHERE schemaname = current_schema()
            AND indexname = 'model_call_usage_by_session_state_turn_call'",
    )
    .fetch_one(&pool)
    .await?;
    assert!(index_definition.contains("(session_id, state_kind, turn_id, model_call_id)"));

    pool.close().await;
    drop(container);
    Ok(())
}

/// Provider token fields reject fractional SQL input instead of rounding it
/// into nearby evidence before constraint validation.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn model_call_usage_rejects_fractional_evidence_without_rounding()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let fixture = checkpoint_restart_model_call(&pool, 0x6d00, true).await?;
    let fractional_input_tokens = Decimal::new(5, 1);

    let error = sqlx::query(
        "UPDATE model_call
            SET state_kind = 'terminal',
                terminal_disposition_kind = 'known_failed',
                usage_input_tokens = $1
          WHERE model_call_id = $2",
    )
    .bind(fractional_input_tokens)
    .bind(fixture.call.into_uuid())
    .execute(&pool)
    .await
    .expect_err("fractional provider usage must not be rounded");
    assert_eq!(
        error
            .as_database_error()
            .and_then(|database_error| database_error.constraint()),
        Some("model_call_usage_input_tokens_u64")
    );

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn model_call_usage_provenance_rejects_unknown_values() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let fixture = checkpoint_restart_model_call(&pool, 0x6d40, true).await?;

    let error = sqlx::query(
        "UPDATE model_call
            SET state_kind = 'terminal',
                terminal_disposition_kind = 'known_failed',
                usage_provenance_kind = 'inferred'
          WHERE model_call_id = $1",
    )
    .bind(fixture.call.into_uuid())
    .execute(&pool)
    .await
    .expect_err("the usage provenance vocabulary is closed");
    assert_eq!(
        error
            .as_database_error()
            .and_then(|database_error| database_error.constraint()),
        Some("model_call_usage_provenance_kind_closed")
    );

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn model_call_input_semantics_are_immutable() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let fixture = checkpoint_restart_model_call(&pool, 0x6d60, true).await?;

    let stored: bool = sqlx::query_scalar(
        "SELECT usage_input_includes_cache_tokens
           FROM model_call
          WHERE model_call_id = $1",
    )
    .bind(fixture.call.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert!(!stored);
    let error = sqlx::query(
        "UPDATE model_call
            SET usage_input_includes_cache_tokens = true
          WHERE model_call_id = $1",
    )
    .bind(fixture.call.into_uuid())
    .execute(&pool)
    .await
    .expect_err("a prepared call's input semantics must be immutable");
    assert_eq!(
        error
            .as_database_error()
            .and_then(|error| error.constraint()),
        Some("model_call_usage_metadata_immutable")
    );

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn model_call_input_semantics_keep_historical_unknown_and_new_default()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;

    let (is_nullable, column_default): (String, Option<String>) = sqlx::query_as(
        "SELECT is_nullable, column_default
           FROM information_schema.columns
          WHERE table_schema = 'public'
            AND table_name = 'model_call'
            AND column_name = 'usage_input_includes_cache_tokens'",
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(is_nullable, "YES");
    assert_eq!(column_default.as_deref(), Some("false"));

    pool.close().await;
    drop(container);
    Ok(())
}

/// An ambiguous provider round can still report the exact input it accepted.
/// That durable usage remains a conservative lower bound for pre-activation
/// compaction instead of being discarded solely because completion was
/// uncertain.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn ambiguous_model_call_usage_is_available_to_pre_activation_compaction()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0x6d70;
    let (fixture, repository, authorized) = authorize_checkpointed_model_call(&pool, seed).await?;
    let correlation = authorized.observation_correlation();
    let reported_usage = ProviderReportedTokenUsage::unreported()
        .with_input_tokens(Some(207_928))
        .with_output_tokens(Some(698));
    let observation = correlation.bind_terminal_observation_with_usage(
        ModelCallTerminalObservation::Ambiguous,
        reported_usage,
    );

    repository
        .apply_terminal_observation(
            fixture.session,
            observation,
            ModelCallTerminalIdentities::Ambiguous(AmbiguousModelCallTurnIdentities::new(
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 20)),
            )),
            |_| panic!("an ambiguous call creates no pending-steering successors"),
        )
        .await?;
    let retained = repository
        .latest_reported_usage(
            fixture.session,
            correlation.target(),
            correlation.frontier(),
        )
        .await?
        .expect("ambiguous provider-reported input remains available");

    assert_eq!(retained.usage(), reported_usage);
    assert!(!retained.input_includes_cache_tokens());
    assert!(retained.input_is_retained());
    assert!(!retained.output_is_retained());
    assert_eq!(retained.projected_unreported_content_bytes(), 0);

    pool.close().await;
    drop(container);
    Ok(())
}

/// A successful dedicated compaction call becomes the provider-confirmed
/// baseline until a later ordinary call reports usage. Its retained summary is
/// already represented by reported output tokens and is not counted twice,
/// while the reported input measures the source text that summary replaced and
/// is retained by nothing.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn context_compaction_usage_is_available_to_pre_activation_compaction()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let (is_nullable, column_default): (String, Option<String>) = sqlx::query_as(
        "SELECT is_nullable, column_default
           FROM information_schema.columns
          WHERE table_schema = 'public'
            AND table_name = 'context_compaction_model_call'
            AND column_name = 'usage_input_includes_cache_tokens'",
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(is_nullable, "YES");
    assert_eq!(column_default.as_deref(), Some("false"));

    let seed = 0x6d78;
    let (fixture, repository, authorized) = authorize_checkpointed_model_call(&pool, seed).await?;
    let retained_source_suffix = "context before compaction";
    let assistant = AssistantText::try_new(String::from(retained_source_suffix))
        .expect("fixture assistant text is admitted");
    let observation = authorized
        .observation_correlation()
        .bind_terminal_observation(ModelCallTerminalObservation::Completed {
            assistant_text: vec![assistant],
        });
    repository
        .apply_terminal_observation(
            fixture.session,
            observation,
            ModelCallTerminalIdentities::Completed(CompletedModelCallIdentities::new(
                vec![SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(
                    seed + 0x20,
                ))],
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 0x21)),
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 0x22)),
            )),
            |_| panic!("the fixture has no pending steering to reclassify"),
        )
        .await?;

    let target =
        ResolvedProviderTarget::naming(ProviderModelIdentity::from_uuid(Uuid::from_u128(seed + 6)));
    let compaction_repository = ContextCompactionRepository::new(pool.clone());
    let prepared = compaction_repository
        .prepare(PrepareContextCompactionRequest {
            command: DurableCommandId::from_uuid(Uuid::from_u128(seed + 0x30)),
            session: fixture.session,
            requested_through_position: Some(1),
            automatic_for_turn: None,
            defaults_version: SessionConfigurationDefaultsVersion::first(),
            selection: DirectModelSelection::from_uuid(Uuid::from_u128(seed + 5)),
            target,
            input_includes_cache_tokens: true,
            credential_reference: String::from("compaction usage fixture credential"),
            call: ModelCallId::from_uuid(Uuid::from_u128(seed + 0x31)),
            compaction: ContextCompactionId::from_uuid(Uuid::from_u128(seed + 0x32)),
            summary_entry: SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 0x33)),
            result_frontier: ContextFrontierId::from_uuid(Uuid::from_u128(seed + 0x34)),
        })
        .await?;
    let PrepareContextCompactionOutcome::Prepared(prepared) = prepared else {
        panic!("the completed turn has a compactable frontier")
    };
    compaction_repository.authorize(&prepared).await?;
    let compaction_usage = ContextCompactionTokenUsage::unreported()
        .with_input_tokens(Some(91))
        .with_output_tokens(Some(13))
        .with_cache_creation_input_tokens(Some(17))
        .with_cache_read_input_tokens(Some(19));
    compaction_repository
        .complete(&prepared, "retained context summary", compaction_usage)
        .await?;

    let suffix = "content appended after compaction";
    SubmitInputRepository::new(pool.clone())
        .handle(
            start_input(
                seed + 0x40,
                seed + 1,
                suffix,
                1,
                ModelSelectionOverride::UseSessionDefault,
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(seed + 0x41)),
            Some(TurnId::from_uuid(Uuid::from_u128(seed + 0x42))),
        )
        .await?;
    activate_earliest_queued_turn(
        &pool,
        EarliestQueuedTurnActivation {
            session: fixture.session.into_uuid(),
            origin_entry: Uuid::from_u128(seed + 0x43),
            starting_frontier: Uuid::from_u128(seed + 0x44),
            initial_attempt: Uuid::from_u128(seed + 0x45),
        },
    )
    .await?;

    let retained = repository
        .latest_reported_usage(
            fixture.session,
            target,
            ContextFrontierId::from_uuid(Uuid::from_u128(seed + 0x44)),
        )
        .await?
        .expect("the dedicated compaction usage becomes the current baseline");
    let expected_usage = ProviderReportedTokenUsage::unreported()
        .with_input_tokens(Some(91))
        .with_output_tokens(Some(13))
        .with_cache_creation_input_tokens(Some(17))
        .with_cache_read_input_tokens(Some(19));
    assert_eq!(retained.usage(), expected_usage);
    assert!(retained.input_includes_cache_tokens());
    assert!(
        !retained.input_is_retained(),
        "the summarized-away source the compaction reported as input is gone"
    );
    assert!(retained.output_is_retained());
    assert_eq!(
        retained.projected_unreported_content_bytes(),
        u64::try_from(retained_source_suffix.len() + suffix.len())?
    );

    let mutation_error = sqlx::query(
        "UPDATE context_compaction_model_call
            SET usage_input_includes_cache_tokens = false
          WHERE model_call_id = $1",
    )
    .bind(prepared.call().into_uuid())
    .execute(&pool)
    .await
    .expect_err("a prepared compaction call's input semantics are immutable");
    assert_eq!(
        mutation_error
            .as_database_error()
            .and_then(|error| error.constraint()),
        Some("context_compaction_input_semantics_immutable")
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// The queued-turn preflight scores the exact input it is about to send. Its
/// production caller previews an activation whose starting frontier and origin
/// entry no transaction has committed, so the reported-usage read takes the
/// preview's own model-visible membership and the content of the entries it
/// minted rather than a frontier identity durable rows cannot resolve.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn queued_turn_activation_preview_scores_its_own_input() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0x6d80;
    let (fixture, repository, authorized) = authorize_checkpointed_model_call(&pool, seed).await?;
    let correlation = authorized.observation_correlation();
    let reported_usage = ProviderReportedTokenUsage::unreported()
        .with_input_tokens(Some(4_000))
        .with_output_tokens(Some(0));
    let assistant = AssistantText::try_new(String::from("preview headroom historical reply"))
        .expect("fixture assistant text is admitted");
    let observation = correlation.bind_terminal_observation_with_usage(
        ModelCallTerminalObservation::Completed {
            assistant_text: vec![assistant],
        },
        reported_usage,
    );
    repository
        .apply_terminal_observation(
            fixture.session,
            observation,
            ModelCallTerminalIdentities::Completed(CompletedModelCallIdentities::new(
                vec![SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(
                    seed + 0x20,
                ))],
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 0x21)),
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 0x22)),
            )),
            |_| panic!("the fixture has no pending steering to reclassify"),
        )
        .await?;

    // Twenty-two ASCII characters and one two-byte "é": 24 UTF-8 bytes.
    let queued_input = "queued preview suffix é";
    SubmitInputRepository::new(pool.clone())
        .handle(
            start_input(
                seed + 0x40,
                seed + 1,
                queued_input,
                1,
                ModelSelectionOverride::UseSessionDefault,
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(seed + 0x41)),
            Some(TurnId::from_uuid(Uuid::from_u128(seed + 0x42))),
        )
        .await?;
    let previewed_frontier = ContextFrontierId::from_uuid(Uuid::from_u128(seed + 0x45));
    let preview = StartEligibleTurnRepository::new(pool.clone())
        .preview(
            fixture.session,
            AcceptedInputTurnActivationIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 0x43)),
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 0x44)),
                previewed_frontier,
                TurnAttemptId::from_uuid(Uuid::from_u128(seed + 0x46)),
            ),
        )
        .await?
        .expect("the queued turn has one uncommitted activation preview");
    let prospective = repository
        .preview_activation_operation(
            preview.prepared(),
            ModelCallId::from_uuid(Uuid::from_u128(seed + 0x47)),
        )
        .await?
        .expect("the preview reconstitutes its prospective first call");
    let operation = prospective.render(Box::new([]))?;
    assert_eq!(
        operation.request().call().frontier().snapshot(),
        previewed_frontier,
        "the preview call carries the starting frontier no transaction committed"
    );
    let committed_frontiers: i64 = sqlx::query_scalar(
        "SELECT count(*)
           FROM context_frontier
          WHERE owning_session_id = $1
            AND context_frontier_id = $2",
    )
    .bind(fixture.session.into_uuid())
    .bind(previewed_frontier.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(committed_frontiers, 0);

    let reported = repository
        .latest_reported_usage(
            fixture.session,
            correlation.target(),
            prospective.prospective_input(),
        )
        .await?
        .expect("the completed call reported input usage");

    assert_eq!(reported.usage(), reported_usage);
    assert!(reported.input_is_retained());
    assert!(reported.output_is_retained());
    assert_eq!(reported.projected_unreported_content_bytes(), 24);

    pool.close().await;
    drop(container);
    Ok(())
}

/// Compaction coverage follows model-visible projected order. A successor
/// compaction that summarizes its predecessor's summary leaves that summary
/// invisible, so the retained-content allowance excludes it even though the
/// summary was appended physically after the successor's through-entry.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn successor_compaction_coverage_follows_projected_order() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0x6d88;
    let (fixture, repository, authorized) = authorize_checkpointed_model_call(&pool, seed).await?;
    let assistant = AssistantText::try_new(String::from("summarized away by the successor"))
        .expect("fixture assistant text is admitted");
    let observation = authorized
        .observation_correlation()
        .bind_terminal_observation(ModelCallTerminalObservation::Completed {
            assistant_text: vec![assistant],
        });
    repository
        .apply_terminal_observation(
            fixture.session,
            observation,
            ModelCallTerminalIdentities::Completed(CompletedModelCallIdentities::new(
                vec![SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(
                    seed + 0x20,
                ))],
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 0x21)),
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 0x22)),
            )),
            |_| panic!("the fixture has no pending steering to reclassify"),
        )
        .await?;

    let target =
        ResolvedProviderTarget::naming(ProviderModelIdentity::from_uuid(Uuid::from_u128(seed + 6)));
    let compaction_repository = ContextCompactionRepository::new(pool.clone());
    let PrepareContextCompactionOutcome::Prepared(predecessor) = compaction_repository
        .prepare(PrepareContextCompactionRequest {
            command: DurableCommandId::from_uuid(Uuid::from_u128(seed + 0x30)),
            session: fixture.session,
            requested_through_position: Some(1),
            automatic_for_turn: None,
            defaults_version: SessionConfigurationDefaultsVersion::first(),
            selection: DirectModelSelection::from_uuid(Uuid::from_u128(seed + 5)),
            target,
            input_includes_cache_tokens: true,
            credential_reference: String::from("predecessor compaction credential"),
            call: ModelCallId::from_uuid(Uuid::from_u128(seed + 0x31)),
            compaction: ContextCompactionId::from_uuid(Uuid::from_u128(seed + 0x32)),
            summary_entry: SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 0x33)),
            result_frontier: ContextFrontierId::from_uuid(Uuid::from_u128(seed + 0x34)),
        })
        .await?
    else {
        panic!("the completed turn has a compactable frontier")
    };
    compaction_repository.authorize(&predecessor).await?;
    compaction_repository
        .complete(
            &predecessor,
            "predecessor summary the successor summarizes away",
            ContextCompactionTokenUsage::unreported()
                .with_input_tokens(Some(101))
                .with_output_tokens(Some(11)),
        )
        .await?;

    let PrepareContextCompactionOutcome::Prepared(successor) = compaction_repository
        .prepare(PrepareContextCompactionRequest {
            command: DurableCommandId::from_uuid(Uuid::from_u128(seed + 0x38)),
            session: fixture.session,
            requested_through_position: Some(2),
            automatic_for_turn: None,
            defaults_version: SessionConfigurationDefaultsVersion::first(),
            selection: DirectModelSelection::from_uuid(Uuid::from_u128(seed + 5)),
            target,
            input_includes_cache_tokens: true,
            credential_reference: String::from("successor compaction credential"),
            call: ModelCallId::from_uuid(Uuid::from_u128(seed + 0x39)),
            compaction: ContextCompactionId::from_uuid(Uuid::from_u128(seed + 0x3a)),
            summary_entry: SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 0x3b)),
            result_frontier: ContextFrontierId::from_uuid(Uuid::from_u128(seed + 0x3c)),
        })
        .await?
    else {
        panic!("the predecessor summary and its retained suffix compact again")
    };
    compaction_repository.authorize(&successor).await?;
    compaction_repository
        .complete(
            &successor,
            "successor summary",
            ContextCompactionTokenUsage::unreported()
                .with_input_tokens(Some(103))
                .with_output_tokens(Some(13)),
        )
        .await?;

    // Twenty-eight ASCII characters and one two-byte "é": 30 UTF-8 bytes.
    let appended_input = "successor compaction suffix é";
    SubmitInputRepository::new(pool.clone())
        .handle(
            start_input(
                seed + 0x40,
                seed + 1,
                appended_input,
                1,
                ModelSelectionOverride::UseSessionDefault,
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(seed + 0x41)),
            Some(TurnId::from_uuid(Uuid::from_u128(seed + 0x42))),
        )
        .await?;
    activate_earliest_queued_turn(
        &pool,
        EarliestQueuedTurnActivation {
            session: fixture.session.into_uuid(),
            origin_entry: Uuid::from_u128(seed + 0x43),
            starting_frontier: Uuid::from_u128(seed + 0x44),
            initial_attempt: Uuid::from_u128(seed + 0x45),
        },
    )
    .await?;

    let retained = repository
        .latest_reported_usage(
            fixture.session,
            target,
            ContextFrontierId::from_uuid(Uuid::from_u128(seed + 0x44)),
        )
        .await?
        .expect("the successor compaction usage becomes the current baseline");

    assert!(!retained.input_is_retained());
    assert_eq!(
        retained.projected_unreported_content_bytes(),
        30,
        "only the appended input remains model-visible after the successor compaction"
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// A provider can reject an oversized request before reporting usage. The
/// preserved failure frontier forces one successor compaction, while the
/// completed compaction result supersedes that pressure evidence.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn request_too_large_failure_forces_one_successor_compaction() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0x6d7c;
    let (fixture, repository, authorized) = authorize_checkpointed_model_call(&pool, seed).await?;
    let target = authorized.observation_correlation().target();
    let failed_frontier = ContextFrontierId::from_uuid(Uuid::from_u128(seed + 0x22));
    let observation = authorized
        .observation_correlation()
        .bind_provider_failure_observation_with_usage(
            ProviderModelCallFailureCause::RequestTooLarge,
            ProviderReportedTokenUsage::unreported(),
        );
    repository
        .apply_terminal_observation(
            fixture.session,
            observation,
            ModelCallTerminalIdentities::Failed(FailedModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 0x21)),
                failed_frontier,
            )),
            |_| panic!("the fixture has no pending steering to reclassify"),
        )
        .await?;

    assert!(
        repository
            .request_too_large_requires_compaction(fixture.session, target, failed_frontier)
            .await?
    );

    let compaction_repository = ContextCompactionRepository::new(pool.clone());
    let result_frontier = ContextFrontierId::from_uuid(Uuid::from_u128(seed + 0x34));
    let prepared = compaction_repository
        .prepare(PrepareContextCompactionRequest {
            command: DurableCommandId::from_uuid(Uuid::from_u128(seed + 0x30)),
            session: fixture.session,
            requested_through_position: Some(1),
            automatic_for_turn: None,
            defaults_version: SessionConfigurationDefaultsVersion::first(),
            selection: DirectModelSelection::from_uuid(Uuid::from_u128(seed + 5)),
            target,
            input_includes_cache_tokens: false,
            credential_reference: String::from("request-size recovery fixture credential"),
            call: ModelCallId::from_uuid(Uuid::from_u128(seed + 0x31)),
            compaction: ContextCompactionId::from_uuid(Uuid::from_u128(seed + 0x32)),
            summary_entry: SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 0x33)),
            result_frontier,
        })
        .await?;
    let PrepareContextCompactionOutcome::Prepared(prepared) = prepared else {
        panic!("the failed turn retains a compactable frontier")
    };
    compaction_repository.authorize(&prepared).await?;
    compaction_repository
        .complete(
            &prepared,
            "bounded request-size recovery summary",
            ContextCompactionTokenUsage::unreported(),
        )
        .await?;

    assert!(
        !repository
            .request_too_large_requires_compaction(fixture.session, target, result_frontier)
            .await?
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// cancellation evidence cannot carry provider usage because neither
/// cancellation-confirmed nor pre-send cancellation reports token evidence.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn cancelled_model_call_usage_is_unreported() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let fixture = checkpoint_restart_model_call(&pool, 0x6d80, true).await?;
    let reported_output_tokens = Decimal::from(1_u64);

    let error = sqlx::query(
        "UPDATE model_call
            SET state_kind = 'terminal',
                terminal_disposition_kind = 'cancelled',
                usage_output_tokens = $1
          WHERE model_call_id = $2",
    )
    .bind(reported_output_tokens)
    .bind(fixture.call.into_uuid())
    .execute(&pool)
    .await
    .expect_err("cancelled calls cannot carry provider usage");
    assert_eq!(
        error
            .as_database_error()
            .and_then(|database_error| database_error.constraint()),
        Some("model_call_cancelled_usage_is_unreported")
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// a call terminalized directly from Prepared cannot carry usage because
/// no provider send was authorized.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn unsent_model_call_usage_is_unreported() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let fixture = checkpoint_restart_model_call(&pool, 0x6e00, false).await?;
    let reported_input_tokens = Decimal::from(1_u64);

    let error = sqlx::query(
        "UPDATE model_call
            SET state_kind = 'terminal',
                terminal_disposition_kind = 'known_failed',
                usage_input_tokens = $1
          WHERE model_call_id = $2",
    )
    .bind(reported_input_tokens)
    .bind(fixture.call.into_uuid())
    .execute(&pool)
    .await
    .expect_err("an unsent call cannot carry provider usage");
    assert_eq!(
        error
            .as_database_error()
            .and_then(|database_error| database_error.constraint()),
        Some("model_call_unsent_usage_unreported")
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// a call terminalized directly from Prepared cannot carry a
/// provider-failure cause because no provider send was authorized.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn unsent_model_call_provider_failure_cause_is_absent() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let fixture = checkpoint_restart_model_call(&pool, 0x6e80, false).await?;

    let error = sqlx::query(
        "UPDATE model_call
            SET state_kind = 'terminal',
                terminal_disposition_kind = 'known_failed',
                terminal_provider_failure_cause = 'quota_exhausted'
          WHERE model_call_id = $1",
    )
    .bind(fixture.call.into_uuid())
    .execute(&pool)
    .await
    .expect_err("an unsent call cannot carry a provider-failure cause");
    assert_eq!(
        error
            .as_database_error()
            .and_then(|database_error| database_error.constraint()),
        Some("model_call_unsent_provider_failure_cause_absent")
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// a reference pinned on a new model call cannot be replaced or
/// cleared.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn model_call_credential_reference_is_immutable() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let fixture = checkpoint_restart_model_call(&pool, 0x6f00, false).await?;

    let replacement = sqlx::query(
        "UPDATE model_call
            SET credential_reference = 'replacement-provider-reference'
          WHERE model_call_id = $1",
    )
    .bind(fixture.call.into_uuid())
    .execute(&pool)
    .await
    .expect_err("a pinned credential reference cannot be replaced");
    assert_eq!(
        replacement
            .as_database_error()
            .and_then(|error| error.code()),
        Some("23514".into())
    );

    let clearing = sqlx::query(
        "UPDATE model_call
            SET credential_reference = NULL
          WHERE model_call_id = $1",
    )
    .bind(fixture.call.into_uuid())
    .execute(&pool)
    .await
    .expect_err("a pinned credential reference cannot be cleared");
    assert_eq!(
        clearing.as_database_error().and_then(|error| error.code()),
        Some("23514".into())
    );

    let stored: String = sqlx::query_scalar(
        "SELECT credential_reference
           FROM model_call
          WHERE model_call_id = $1",
    )
    .bind(fixture.call.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(stored, model_credential_reference().as_str());

    pool.close().await;
    drop(container);

    Ok(())
}

/// A definitive attachment-preparation failure closes its prepared call and
/// retains its durable cause.
///
/// `model_call_changes_are_guarded` raises on every update whose OLD row is
/// already terminal, so the cause is only writable by the same
/// Prepared-to-terminal statement that closes the call. A follow-up update
/// aborts the whole failure transaction instead, leaving the call and its turn
/// open, which is why this exercises the `Some(..)` closure end to end rather
/// than asserting the column shape alone. The pairing constraint is then probed
/// on its own inside a rolled-back transaction that suspends that guard, because
/// a maximum without a cause is reachable only on a row already closed as a
/// known failure.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn definitive_attachment_failure_closes_its_call_with_a_durable_cause()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0x7100;
    let fixture = checkpoint_restart_model_call(&pool, seed, false).await?;
    let selection = signalbox_domain::DirectModelSelection::from_uuid(Uuid::from_u128(seed + 5));
    let provider = ProviderModelIdentity::from_uuid(Uuid::from_u128(seed + 6));
    let targets = ModelTargetCatalog::try_from_definitions([ModelTargetDefinition::new(
        selection,
        ResolvedProviderTarget::naming(provider),
    )])
    .expect("one restart fixture target forms a catalog");
    let repository =
        PostgresModelCallRepository::new(pool.clone(), targets, model_credential_reference());

    let failed = repository
        .fail_prepared_call(
            fixture.session,
            fixture.call,
            PreparedModelCallFailureCause::CapabilityKnownFailure,
            Some(AttachmentPreparationFailure::Missing),
            FailedModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 14)),
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 15)),
            ),
            |_| panic!("the fixture has no pending steering to reclassify"),
        )
        .await?;
    assert_eq!(
        failed.call().expect("the prepared call closes").id(),
        fixture.call
    );

    let durable_cause: (String, Option<String>, Option<Decimal>) = sqlx::query_as(
        "SELECT state_kind,
                terminal_attachment_preparation_failure_cause,
                terminal_attachment_preparation_failure_maximum_bytes
           FROM model_call
          WHERE model_call_id = $1",
    )
    .bind(fixture.call.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        durable_cause,
        ("terminal".to_owned(), Some("missing".to_owned()), None)
    );

    // A maximum retained without the cause that names it describes no
    // `AttachmentPreparationFailure`, so the pairing constraint rejects it
    // rather than leaving the row for a reread to reject. Every other terminal
    // fact is already durable and unchanged here, so this constraint is the
    // only one the statement can violate.
    let mut stripped_cause = pool.begin().await?;
    sqlx::query("ALTER TABLE model_call DISABLE TRIGGER USER")
        .execute(&mut *stripped_cause)
        .await?;
    let stripped_cause_error = sqlx::query(
        "UPDATE model_call
            SET terminal_attachment_preparation_failure_cause = NULL,
                terminal_attachment_preparation_failure_maximum_bytes = 1
          WHERE model_call_id = $1",
    )
    .bind(fixture.call.into_uuid())
    .execute(&mut *stripped_cause)
    .await
    .expect_err("a retained maximum cannot outlive its cause");
    assert_eq!(
        stripped_cause_error
            .as_database_error()
            .and_then(|error| error.constraint()),
        Some("model_call_attachment_preparation_failure_cause_shape")
    );
    stripped_cause.rollback().await?;

    // The reread only reports a committed closure when the durable cause still
    // matches the failure the caller is reconciling.
    assert_eq!(
        repository
            .reread_prepared_failure(
                fixture.session,
                fixture.call,
                Some(AttachmentPreparationFailure::Missing)
            )
            .await?,
        RetainedPreparedFailureStatus::AlreadyCommitted
    );
    assert!(matches!(
        repository
            .reread_prepared_failure(fixture.session, fixture.call, None)
            .await,
        Err(ModelCallRepositoryError::InvalidTransition(_))
    ));

    // The turn closed with the call, rather than being left open by a rolled
    // back failure transaction.
    let terminal_execution: (Uuid, Uuid) = sqlx::query_as(
        "SELECT terminal_attempt_id, terminal_model_call_id
           FROM turn_lifecycle
          WHERE turn_id = $1
            AND state_kind = 'terminal'
            AND terminal_disposition_kind = 'failed'",
    )
    .bind(fixture.turn.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        terminal_execution,
        (fixture.attempt.into_uuid(), fixture.call.into_uuid())
    );

    pool.close().await;
    drop(container);

    Ok(())
}

/// an uncertain capability-failure closure is reconciled from exact
/// durable Prepared or complete known-failure state, including its terminal
/// attempt and call provenance, before any resubmission.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn model_call_prepared_failure_reread_distinguishes_pending_and_committed()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0x7000;
    let fixture = checkpoint_restart_model_call(&pool, seed, false).await?;
    let selection = signalbox_domain::DirectModelSelection::from_uuid(Uuid::from_u128(seed + 5));
    let provider = ProviderModelIdentity::from_uuid(Uuid::from_u128(seed + 6));
    let targets = ModelTargetCatalog::try_from_definitions([ModelTargetDefinition::new(
        selection,
        ResolvedProviderTarget::naming(provider),
    )])
    .expect("one restart fixture target forms a catalog");
    let repository =
        PostgresModelCallRepository::new(pool.clone(), targets, model_credential_reference());

    let mut call_only = pool.begin().await?;
    let call_only_error = sqlx::query(
        "UPDATE turn_lifecycle
            SET state_kind = 'terminal',
                terminal_frontier_id = starting_frontier_id,
                active_phase_kind = NULL,
                current_attempt_id = NULL,
                terminal_disposition_kind = 'failed',
                terminal_cause_kind = 'model_call_failed',
                terminal_attempt_id = NULL,
                terminal_model_call_id = $1
          WHERE turn_id = $2",
    )
    .bind(fixture.call.into_uuid())
    .bind(fixture.turn.into_uuid())
    .execute(&mut *call_only)
    .await
    .expect_err("a failed lifecycle cannot retain call-only provenance");
    assert_eq!(
        call_only_error
            .as_database_error()
            .and_then(|error| error.constraint()),
        Some("turn_lifecycle_state_payload_shape")
    );
    call_only.rollback().await?;

    assert_eq!(
        repository
            .reread_prepared_failure(fixture.session, fixture.call, None)
            .await?,
        RetainedPreparedFailureStatus::Pending
    );
    let failed = repository
        .fail_prepared_call(
            fixture.session,
            fixture.call,
            PreparedModelCallFailureCause::CapabilityKnownFailure,
            None,
            FailedModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 14)),
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 15)),
            ),
            |_| panic!("the fixture has no pending steering to reclassify"),
        )
        .await?;
    assert_eq!(
        failed.call().expect("the prepared call closes").id(),
        fixture.call
    );
    assert_eq!(
        repository
            .reread_prepared_failure(fixture.session, fixture.call, None)
            .await?,
        RetainedPreparedFailureStatus::AlreadyCommitted
    );
    let terminal_execution: (Uuid, Uuid) = sqlx::query_as(
        "SELECT terminal_attempt_id, terminal_model_call_id
           FROM turn_lifecycle
          WHERE turn_id = $1
            AND state_kind = 'terminal'
            AND terminal_disposition_kind = 'failed'",
    )
    .bind(fixture.turn.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        terminal_execution,
        (fixture.attempt.into_uuid(), fixture.call.into_uuid())
    );

    // A new durable input forces the scheduling loader to reconstruct the
    // complete failed prefix before it can append queued work.
    assert!(matches!(
        SubmitInputRepository::new(pool.clone())
            .handle(
                start_input(
                    seed + 16,
                    seed + 1,
                    "work after failed model call",
                    1,
                    ModelSelectionOverride::UseSessionDefault,
                ),
                AcceptedInputId::from_uuid(Uuid::from_u128(seed + 17)),
                Some(TurnId::from_uuid(Uuid::from_u128(seed + 18))),
            )
            .await?,
        SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Applied(
            SubmitInputAppliedResult::TurnOrigin(_)
        ))
    ));

    sqlx::query("ALTER TABLE turn_terminal_outbox_event DISABLE TRIGGER USER")
        .execute(&pool)
        .await?;
    sqlx::query(
        "DELETE FROM turn_terminal_outbox_event WHERE disposition_kind = 'failed' AND turn_id = $1",
    )
    .bind(fixture.turn.into_uuid())
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE turn_terminal_outbox_event ENABLE TRIGGER USER")
        .execute(&pool)
        .await?;
    assert!(matches!(
        repository
            .reread_prepared_failure(fixture.session, fixture.call, None)
            .await,
        Err(ModelCallRepositoryError::InvalidTransition(
            "retained prepared failure durable closure is incomplete"
        ))
    ));

    let issued_seed = seed + 0x100;
    let (issued, issued_repository, authorized) =
        authorize_checkpointed_model_call(&pool, issued_seed).await?;
    let observation = authorized
        .observation_correlation()
        .bind_terminal_observation(ModelCallTerminalObservation::KnownFailed);
    issued_repository
        .apply_terminal_observation(
            issued.session,
            observation.clone(),
            ModelCallTerminalIdentities::Failed(FailedModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(issued_seed + 17)),
                ContextFrontierId::from_uuid(Uuid::from_u128(issued_seed + 18)),
            )),
            |_| panic!("the fixture has no pending steering to reclassify"),
        )
        .await?;
    assert!(matches!(
        issued_repository
            .reread_prepared_failure(issued.session, issued.call, None)
            .await,
        Err(ModelCallRepositoryError::InvalidTransition(
            "retained prepared failure durable closure is incomplete"
        ))
    ));
    assert_eq!(
        issued_repository
            .reread_terminal_observation(issued.session, &observation)
            .await?,
        RetainedModelCallObservationStatus::AlreadyCommitted
    );

    sqlx::query("ALTER TABLE turn_terminal_outbox_event DISABLE TRIGGER USER")
        .execute(&pool)
        .await?;
    sqlx::query(
        "DELETE FROM turn_terminal_outbox_event WHERE disposition_kind = 'failed' AND turn_id = $1",
    )
    .bind(issued.turn.into_uuid())
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE turn_terminal_outbox_event ENABLE TRIGGER USER")
        .execute(&pool)
        .await?;
    assert!(matches!(
        issued_repository
            .reread_terminal_observation(issued.session, &observation)
            .await,
        Err(ModelCallRepositoryError::InvalidTransition(
            "retained observation terminal closure changed"
        ))
    ));

    pool.close().await;
    drop(container);
    Ok(())
}

/// retained prepared failure and ambiguous
/// authorization rereads accept an exact interrupt-caused cancellation of the
/// still-Prepared call as authoritative no-work, and reject an incomplete
/// cancellation closure.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn failure_rereads_accept_prepared_cancellation() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0x7580;
    let fixture = checkpoint_restart_model_call(&pool, seed, false).await?;
    let selection = signalbox_domain::DirectModelSelection::from_uuid(Uuid::from_u128(seed + 5));
    let provider = ProviderModelIdentity::from_uuid(Uuid::from_u128(seed + 6));
    let targets = ModelTargetCatalog::try_from_definitions([ModelTargetDefinition::new(
        selection,
        ResolvedProviderTarget::naming(provider),
    )])
    .expect("one restart fixture target forms a catalog");
    let repository =
        PostgresModelCallRepository::new(pool.clone(), targets, model_credential_reference());
    let prepared = expect_ready_model_call(
        repository
            .prepare_initial_call(
                fixture.session,
                ModelCallId::from_uuid(Uuid::from_u128(seed + 22)),
                FailedModelCallTurnIdentities::new(
                    SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 23)),
                    ContextFrontierId::from_uuid(Uuid::from_u128(seed + 24)),
                ),
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 25)),
                |_| {
                    (
                        SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 26)),
                        TurnId::from_uuid(Uuid::from_u128(seed + 27)),
                    )
                },
            )
            .await?,
    );

    SubmitInputRepository::new(pool.clone())
        .handle(
            input_with_delivery(
                seed + 19,
                seed + 1,
                "cancel retained prepared failure",
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
    assert_eq!(
        repository
            .reread_prepared_failure(fixture.session, fixture.call, None)
            .await?,
        RetainedPreparedFailureStatus::Cancelled
    );
    assert_eq!(
        repository
            .reread_ambiguous_authorization(fixture.session, &prepared)
            .await?,
        ModelCallAuthorizationReread::Cancelled
    );

    sqlx::query("ALTER TABLE turn_terminal_outbox_event DISABLE TRIGGER USER")
        .execute(&pool)
        .await?;
    sqlx::query("DELETE FROM turn_terminal_outbox_event WHERE disposition_kind = 'cancelled' AND turn_id = $1")
        .bind(fixture.turn.into_uuid())
        .execute(&pool)
        .await?;
    sqlx::query("ALTER TABLE turn_terminal_outbox_event ENABLE TRIGGER USER")
        .execute(&pool)
        .await?;
    assert!(matches!(
        repository
            .reread_prepared_failure(fixture.session, fixture.call, None)
            .await,
        Err(ModelCallRepositoryError::InvalidTransition(
            "retained prepared failure cancellation closure is incomplete"
        ))
    ));
    assert!(matches!(
        repository
            .reread_ambiguous_authorization(fixture.session, &prepared)
            .await,
        Err(ModelCallRepositoryError::InvalidTransition(
            "ambiguous authorization terminal cancellation closure is incomplete"
        ))
    ));

    pool.close().await;
    drop(container);
    Ok(())
}

/// docs/spec/model-call-execution.md: retained non-completed observations
/// converge only when their complete disposition-specific durable closure
/// remains present.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn model_call_noncompleted_rereads_validate_each_durable_closure()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;

    let cancelled_seed = 0x7200;
    let (cancelled, cancelled_repository, cancelled_authorized) =
        authorize_checkpointed_model_call(&pool, cancelled_seed).await?;
    let cancelled_observation = cancelled_authorized
        .observation_correlation()
        .bind_terminal_observation(ModelCallTerminalObservation::Cancelled);
    cancelled_repository
        .apply_terminal_observation(
            cancelled.session,
            cancelled_observation.clone(),
            ModelCallTerminalIdentities::PhysicalCancellation(
                PhysicalCancellationModelCallTurnIdentities::new(
                    SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(cancelled_seed + 17)),
                    ContextFrontierId::from_uuid(Uuid::from_u128(cancelled_seed + 18)),
                ),
            ),
            |_| panic!("the fixture has no pending steering to reclassify"),
        )
        .await?;
    assert_eq!(
        cancelled_repository
            .reread_terminal_observation(cancelled.session, &cancelled_observation)
            .await?,
        RetainedModelCallObservationStatus::AlreadyCommitted
    );
    let cancelled_failure_snapshot = ProcessReadRepository::new(pool.clone())
        .read_transcript(cancelled.session)
        .await?
        .expect("the failed-after-cancellation session has a transcript projection");
    let ProcessTurnState::Failed {
        terminal_attempt: Some(terminal_attempt),
        terminal_model_call: Some(terminal_call),
        ..
    } = cancelled_failure_snapshot.turns()[0].state()
    else {
        panic!("the failed projection must retain its cancelled call");
    };
    assert_eq!(*terminal_attempt, cancelled.attempt);
    assert_eq!(terminal_call.call(), cancelled.call);
    assert_eq!(
        terminal_call.disposition(),
        ProcessFailedModelCallDisposition::Cancelled
    );
    sqlx::query("ALTER TABLE turn_terminal_outbox_event DISABLE TRIGGER USER")
        .execute(&pool)
        .await?;
    sqlx::query(
        "DELETE FROM turn_terminal_outbox_event WHERE disposition_kind = 'failed' AND turn_id = $1",
    )
    .bind(cancelled.turn.into_uuid())
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE turn_terminal_outbox_event ENABLE TRIGGER USER")
        .execute(&pool)
        .await?;
    assert!(matches!(
        cancelled_repository
            .reread_terminal_observation(cancelled.session, &cancelled_observation)
            .await,
        Err(ModelCallRepositoryError::InvalidTransition(
            "retained observation terminal closure changed"
        ))
    ));

    let refused_seed = 0x7300;
    let (refused, refused_repository, refused_authorized) =
        authorize_checkpointed_model_call(&pool, refused_seed).await?;
    let refused_observation = refused_authorized
        .observation_correlation()
        .bind_terminal_observation(ModelCallTerminalObservation::Refused);
    refused_repository
        .apply_terminal_observation(
            refused.session,
            refused_observation.clone(),
            ModelCallTerminalIdentities::Refused(RefusedModelCallTurnIdentities::new(
                ContextFrontierId::from_uuid(Uuid::from_u128(refused_seed + 17)),
            )),
            |_| panic!("the fixture has no pending steering to reclassify"),
        )
        .await?;
    assert_eq!(
        refused_repository
            .reread_terminal_observation(refused.session, &refused_observation)
            .await?,
        RetainedModelCallObservationStatus::AlreadyCommitted
    );
    let refused_sequence: Decimal = sqlx::query_scalar(
        "SELECT event_sequence
           FROM turn_terminal_outbox_event
          WHERE disposition_kind = 'refused'
          AND turn_id = $1",
    )
    .bind(refused.turn.into_uuid())
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
    .bind(Uuid::from_u128(refused_seed + 19))
    .bind(Uuid::from_u128(refused_seed + 20))
    .bind(refused.turn.into_uuid())
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE turn_lifecycle ENABLE TRIGGER USER")
        .execute(&pool)
        .await?;
    sqlx::query(
        "ALTER TABLE outbox_consumer_cursor
         DISABLE TRIGGER outbox_consumer_cursor_advances_prefix",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE outbox_consumer_cursor
            SET delivered_through = $1 - 1,
                last_delivery_xid = pg_current_xact_id()
          WHERE consumer_name = 'process_protocol'",
    )
    .bind(refused_sequence)
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE outbox_consumer_cursor
         ENABLE TRIGGER outbox_consumer_cursor_advances_prefix",
    )
    .execute(&pool)
    .await?;
    assert!(matches!(
        OutboxDispatcher::new(pool.clone())
            .dispatch_next(|_| panic!("cross-wired refused ownership must not be offered"))
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
    .bind(refused.attempt.into_uuid())
    .bind(refused.call.into_uuid())
    .bind(refused.turn.into_uuid())
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE turn_lifecycle ENABLE TRIGGER USER")
        .execute(&pool)
        .await?;
    sqlx::query("ALTER TABLE turn_terminal_outbox_event DISABLE TRIGGER USER")
        .execute(&pool)
        .await?;
    sqlx::query("DELETE FROM turn_terminal_outbox_event WHERE disposition_kind = 'refused' AND turn_id = $1")
        .bind(refused.turn.into_uuid())
        .execute(&pool)
        .await?;
    sqlx::query("ALTER TABLE turn_terminal_outbox_event ENABLE TRIGGER USER")
        .execute(&pool)
        .await?;
    assert!(matches!(
        refused_repository
            .reread_terminal_observation(refused.session, &refused_observation)
            .await,
        Err(ModelCallRepositoryError::InvalidTransition(
            "retained observation terminal closure changed"
        ))
    ));

    let ambiguous_seed = 0x7400;
    let (ambiguous, ambiguous_repository, ambiguous_authorized) =
        authorize_checkpointed_model_call(&pool, ambiguous_seed).await?;
    let ambiguous_observation = ambiguous_authorized
        .observation_correlation()
        .bind_terminal_observation(ModelCallTerminalObservation::Ambiguous);
    ambiguous_repository
        .apply_terminal_observation(
            ambiguous.session,
            ambiguous_observation.clone(),
            ModelCallTerminalIdentities::Ambiguous(
                signalbox_domain::AmbiguousModelCallTurnIdentities::new(
                    ContextFrontierId::from_uuid(Uuid::from_u128(ambiguous_seed + 20)),
                ),
            ),
            |_| panic!("Ambiguous creates no pending-steering successors"),
        )
        .await?;
    assert_eq!(
        ambiguous_repository
            .reread_terminal_observation(ambiguous.session, &ambiguous_observation)
            .await?,
        RetainedModelCallObservationStatus::AlreadyCommitted
    );
    sqlx::query("ALTER TABLE model_call_transition_outbox_event DISABLE TRIGGER USER")
        .execute(&pool)
        .await?;
    sqlx::query(
        "DELETE FROM model_call_transition_outbox_event
          WHERE model_call_id = $1
            AND call_state_kind = 'terminal'",
    )
    .bind(ambiguous.call.into_uuid())
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE model_call_transition_outbox_event ENABLE TRIGGER USER")
        .execute(&pool)
        .await?;
    assert!(matches!(
        ambiguous_repository
            .reread_terminal_observation(ambiguous.session, &ambiguous_observation)
            .await,
        Err(ModelCallRepositoryError::InvalidTransition(
            "retained observation terminal closure changed"
        ))
    ));

    pool.close().await;
    drop(container);
    Ok(())
}

/// S03 / S09: interrupting
/// an issued call atomically records its stop proof and cancellation request;
/// the durable signal resolves, physical cancellation closes the turn with its
/// exact attempt history, and both command and observation replays converge on
/// the recorded outcome.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn issued_interrupt_requests_and_confirms_durable_cancellation() -> Result<(), Box<dyn Error>>
{
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0x7600;
    let (fixture, model_repository, prepared, authorized) =
        authorize_checkpointed_model_call_with_prepared(&pool, seed).await?;
    let interrupt = input_with_delivery(
        seed + 19,
        seed + 1,
        "stop issued call",
        DeliveryRequest::Interrupt {
            expected_active_turn: fixture.turn,
            descendant_scope: DescendantTerminationScope::ParentAlone,
            configuration: input_choices(1, ModelSelectionOverride::UseSessionDefault),
        },
    );
    let successor_input = AcceptedInputId::from_uuid(Uuid::from_u128(seed + 20));
    let successor_turn = TurnId::from_uuid(Uuid::from_u128(seed + 21));
    let interrupt_outcome = SubmitInputRepository::new(pool.clone())
        .handle(interrupt.clone(), successor_input, Some(successor_turn))
        .await?;
    let SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Applied(
        SubmitInputAppliedResult::TurnOrigin(applied),
    )) = &interrupt_outcome
    else {
        panic!("the interrupt must record its successor origin")
    };
    assert_eq!(applied.turn(), successor_turn);
    assert_eq!(
        applied
            .applied_interrupt()
            .expect("the successor retains interrupt proof")
            .proof()
            .predecessor(),
        fixture.turn
    );

    let stopped_shape: (i64, i64, i64) = sqlx::query_as(
        "SELECT
            (SELECT count(*)
               FROM turn_attempt
              WHERE turn_attempt_id = $1
                AND state_kind = 'stop_requested'
                AND interrupt_command_id = $4
                AND interrupt_predecessor_turn_id = $2),
            (SELECT count(*)
               FROM model_call
              WHERE model_call_id = $3
                AND state_kind = 'cancellation_requested'),
            (SELECT count(*)
               FROM model_call_transition_outbox_event
              WHERE model_call_id = $3
                AND call_state_kind = 'cancellation_requested')",
    )
    .bind(fixture.attempt.into_uuid())
    .bind(fixture.turn.into_uuid())
    .bind(fixture.call.into_uuid())
    .bind(Uuid::from_u128(seed + 19))
    .fetch_one(&pool)
    .await?;
    assert_eq!(stopped_shape, (1, 1, 1));
    let stopped_snapshot = ProcessReadRepository::new(pool.clone())
        .read_transcript(fixture.session)
        .await?
        .expect("the stopped session has a transcript projection");
    assert_running_current_model_call(
        stopped_snapshot.turns()[0].state(),
        fixture.attempt,
        fixture.call,
        ProcessCurrentModelCallState::CancellationRequested,
    );

    let ModelCallAuthorizationReread::CancellationRequested(stopped) = model_repository
        .reread_ambiguous_authorization(fixture.session, &prepared)
        .await?
    else {
        panic!("the authoritative reread must retain stopped non-consumption")
    };
    assert_eq!(
        stopped.observation_correlation(),
        authorized.observation_correlation()
    );

    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        AuthorizeModelCallTransaction::cancellation_signal(
            &model_repository,
            fixture.session,
            fixture.call,
        ),
    )
    .await
    .expect("durable cancellation signal resolves after the stop commit");

    let observation = stopped
        .observation_correlation()
        .bind_terminal_observation(ModelCallTerminalObservation::Cancelled);
    let terminal = model_repository
        .apply_terminal_observation(
            fixture.session,
            observation.clone(),
            ModelCallTerminalIdentities::PhysicalCancellation(
                PhysicalCancellationModelCallTurnIdentities::new(
                    SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 22)),
                    ContextFrontierId::from_uuid(Uuid::from_u128(seed + 23)),
                ),
            ),
            |_| panic!("the fixture has no pending steering to reclassify"),
        )
        .await?;
    let ModelCallTerminalOutcome::Cancelled(cancelled) = &terminal else {
        panic!("the terminal observation must cancel the interrupted call")
    };
    assert_eq!(cancelled.turn(), fixture.turn);
    assert_eq!(
        cancelled
            .call()
            .expect("physical cancellation retains its call")
            .id(),
        fixture.call
    );

    let terminal_shape: (i64, i64, i64, i64) = sqlx::query_as(
        "SELECT
            (SELECT count(*)
               FROM turn_lifecycle
              WHERE turn_id = $1
                AND state_kind = 'terminal'
                AND terminal_disposition_kind = 'cancelled'),
            (SELECT count(*)
               FROM turn_attempt
              WHERE turn_attempt_id = $2
                AND state_kind = 'ended'
                AND end_variant = 'after_cancellation'
                AND end_disposition = 'cancelled'),
            (SELECT count(*)
               FROM semantic_transcript_entry
              WHERE cancelled_turn_id = $1
                AND payload_kind = 'turn_cancelled'),
            (SELECT count(*)
               FROM turn_terminal_outbox_event
              WHERE disposition_kind = 'cancelled'
              AND turn_id = $1)",
    )
    .bind(fixture.turn.into_uuid())
    .bind(fixture.attempt.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(terminal_shape, (1, 1, 1, 1));
    let cancelled_snapshot = ProcessReadRepository::new(pool.clone())
        .read_transcript(fixture.session)
        .await?
        .expect("the cancelled session has a transcript projection");
    assert_eq!(
        cancelled_snapshot.turns()[0].state(),
        &ProcessTurnState::Cancelled {
            terminal_frontier: ContextFrontierId::from_uuid(Uuid::from_u128(seed + 23)),
            terminal_attempt: fixture.attempt,
            terminal_call: Some(fixture.call),
        }
    );
    let Some(ProcessTranscriptEntry::TurnCancelled { entry, turn, .. }) =
        cancelled_snapshot.entries().last()
    else {
        panic!("the transcript ends with the cancellation marker")
    };
    assert_eq!(
        *entry,
        SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 22))
    );
    assert_eq!(*turn, fixture.turn);
    assert_eq!(
        model_repository
            .reread_terminal_observation(fixture.session, &observation)
            .await?,
        RetainedModelCallObservationStatus::AlreadyCommitted
    );
    assert_eq!(
        SubmitInputRepository::new(pool.clone())
            .handle(
                interrupt,
                AcceptedInputId::from_uuid(Uuid::from_u128(seed + 24)),
                Some(TurnId::from_uuid(Uuid::from_u128(seed + 25))),
            )
            .await?,
        interrupt_outcome
    );

    let cancellation_events = drain_cancellation_dispatches(&pool).await?;
    assert_eq!(
        cancellation_events,
        vec![(
            fixture.session,
            fixture.turn,
            SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 22)),
            ContextFrontierId::from_uuid(Uuid::from_u128(seed + 23)),
        )]
    );

    sqlx::query("ALTER TABLE turn_attempt DISABLE TRIGGER USER")
        .execute(&pool)
        .await?;
    sqlx::query(
        "INSERT INTO turn_attempt
            (turn_attempt_id, turn_id, session_id, continued_from_attempt_id,
             state_kind, end_variant, end_disposition)
         VALUES ($1, $2, $3, $4, 'ended', 'without_stop', 'known_failure')",
    )
    .bind(Uuid::from_u128(seed + 26))
    .bind(fixture.turn.into_uuid())
    .bind(fixture.session.into_uuid())
    .bind(fixture.attempt.into_uuid())
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE turn_attempt ENABLE TRIGGER USER")
        .execute(&pool)
        .await?;
    let cardinality_error = sqlx::query("SELECT assert_turn_lifecycle_final_state($1)")
        .bind(fixture.turn.into_uuid())
        .execute(&pool)
        .await
        .expect_err("a cancelled turn cannot hide an additional ended attempt");
    assert_eq!(
        cardinality_error
            .as_database_error()
            .and_then(|error| error.code()),
        Some("23514".into())
    );
    assert!(cardinality_error.as_database_error().is_some_and(|error| {
        error
            .message()
            .contains("lacks its exact single ended attempt history")
    }));

    pool.close().await;
    drop(container);
    Ok(())
}

/// S04 / S07: ambiguity observed
/// before or after an applied interrupt terminalizes as exact proof-bearing
/// reconciliation, and retained observation and origin rereads recognize the
/// committed closure.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn stopped_ambiguity_commits_reconciliation_and_rereads_exactly() -> Result<(), Box<dyn Error>>
{
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0x7680;
    let (fixture, model_repository, authorized) =
        authorize_checkpointed_model_call(&pool, seed).await?;
    SubmitInputRepository::new(pool.clone())
        .handle(
            input_with_delivery(
                seed + 19,
                seed + 1,
                "stop before ambiguous result",
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

    let observation = authorized
        .observation_correlation()
        .bind_terminal_observation(ModelCallTerminalObservation::Ambiguous);
    let terminal = model_repository
        .apply_terminal_observation(
            fixture.session,
            observation.clone(),
            ModelCallTerminalIdentities::Ambiguous(
                signalbox_domain::AmbiguousModelCallTurnIdentities::new(
                    ContextFrontierId::from_uuid(Uuid::from_u128(seed + 22)),
                ),
            ),
            |_| panic!("the fixture has no pending steering to reclassify"),
        )
        .await?;
    let ModelCallTerminalOutcome::ReconciliationRequired(reconciliation) = &terminal else {
        panic!("the stopped ambiguous call must require reconciliation")
    };
    assert_eq!(reconciliation.turn(), fixture.turn);
    assert_eq!(reconciliation.call().id(), fixture.call);
    let signalbox_domain::TurnDisposition::ReconciliationRequired { marker } =
        reconciliation.disposition()
    else {
        panic!("the terminal disposition retains reconciliation evidence")
    };
    assert!(marker.ambiguous_operations().contains(
        signalbox_domain::IssuedOperationRef::ModelCall(fixture.call)
    ));
    assert_eq!(
        model_repository
            .reread_terminal_observation(fixture.session, &observation)
            .await?,
        RetainedModelCallObservationStatus::AlreadyCommitted
    );
    let stored: (i64, i64, i64) = sqlx::query_as(
        "SELECT
            (SELECT count(*)
              FROM turn_attempt
              WHERE turn_attempt_id = $1
                AND end_variant = 'after_cancellation'
                AND end_disposition = 'ambiguous'),
            (SELECT count(*)
               FROM turn_lifecycle
              WHERE turn_id = $2
                AND state_kind = 'terminal'
                AND terminal_disposition_kind = 'reconciliation_required'),
            (SELECT count(*)
               FROM turn_terminal_outbox_event
              WHERE disposition_kind = 'reconciliation_required'
              AND turn_id = $2
                AND model_call_id = $3)",
    )
    .bind(fixture.attempt.into_uuid())
    .bind(fixture.turn.into_uuid())
    .bind(fixture.call.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(stored, (1, 1, 1));
    let reconciliation_snapshot = ProcessReadRepository::new(pool.clone())
        .read_transcript(fixture.session)
        .await?
        .expect("the reconciliation-required session has a transcript projection");
    assert_eq!(
        reconciliation_snapshot.turns()[0].state(),
        &ProcessTurnState::ReconciliationRequired {
            terminal_frontier: ContextFrontierId::from_uuid(Uuid::from_u128(seed + 22)),
            terminal_attempt: fixture.attempt,
            operation: ProcessReconciliationOperation::ModelCall(fixture.call),
        }
    );

    let reconciliation_events = drain_reconciliation_dispatches(&pool).await?;
    assert_eq!(
        reconciliation_events,
        vec![(
            fixture.session,
            fixture.turn,
            DispatchedReconciliationOperation::ModelCall(fixture.call),
            ContextFrontierId::from_uuid(Uuid::from_u128(seed + 22)),
        )]
    );

    let waiting_seed = seed + 0x20;
    let (waiting, waiting_repository, waiting_authorized) =
        authorize_checkpointed_model_call(&pool, waiting_seed).await?;
    let waiting_observation = waiting_authorized
        .observation_correlation()
        .bind_terminal_observation(ModelCallTerminalObservation::Ambiguous);
    let waiting_outcome = waiting_repository
        .apply_terminal_observation(
            waiting.session,
            waiting_observation.clone(),
            ModelCallTerminalIdentities::Ambiguous(
                signalbox_domain::AmbiguousModelCallTurnIdentities::new(
                    ContextFrontierId::from_uuid(Uuid::from_u128(waiting_seed + 22)),
                ),
            ),
            |_| panic!("the fixture has no pending steering to reclassify"),
        )
        .await?;
    let ModelCallTerminalOutcome::AwaitingRecovery(ambiguous) = &waiting_outcome else {
        panic!("the unstopped ambiguous call must await recovery")
    };
    assert_eq!(ambiguous.turn(), waiting.turn);
    assert_eq!(ambiguous.call().id(), waiting.call);
    let waiting_snapshot = ProcessReadRepository::new(pool.clone())
        .read_transcript(waiting.session)
        .await?
        .expect("the ambiguous call has a transcript projection");
    assert_eq!(
        waiting_snapshot.turns()[0].state(),
        &ProcessTurnState::ActiveAwaitingModelCallRecovery {
            ended_attempt: waiting.attempt,
            recovery_call: waiting.call,
            automatic_reconciliation_attempts: 0,
            operator_action_required: false,
        }
    );
    assert_eq!(waiting_snapshot.entries().len(), 1);
    let submit_repository = SubmitInputRepository::new(pool.clone());
    let waiting_steering_command = input_with_delivery(
        waiting_seed + 0x100,
        waiting_seed + 1,
        "steering retained through existing ambiguity wait",
        DeliveryRequest::NextSafePoint {
            expected_active_turn: waiting.turn,
        },
    );
    assert!(matches!(
        submit_repository
            .handle(
                waiting_steering_command,
                AcceptedInputId::from_uuid(Uuid::from_u128(waiting_seed + 0x101)),
                None,
            )
            .await?,
        SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Applied(
            SubmitInputAppliedResult::PendingSteering(_)
        ))
    ));
    let waiting_interrupt = submit_repository
        .handle(
            input_with_delivery(
                waiting_seed + 23,
                waiting_seed + 1,
                "interrupt existing ambiguity wait",
                DeliveryRequest::Interrupt {
                    expected_active_turn: waiting.turn,
                    descendant_scope: DescendantTerminationScope::ParentAlone,
                    configuration: input_choices(1, ModelSelectionOverride::UseSessionDefault),
                },
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(waiting_seed + 24)),
            Some(TurnId::from_uuid(Uuid::from_u128(waiting_seed + 25))),
        )
        .await?;
    assert!(matches!(
        waiting_interrupt,
        SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Applied(
            SubmitInputAppliedResult::TurnOrigin(_)
        ))
    ));
    assert_eq!(
        waiting_repository
            .reread_terminal_observation(waiting.session, &waiting_observation)
            .await?,
        RetainedModelCallObservationStatus::AlreadyCommitted
    );
    let waiting_stored: (i64, i64, i64) = sqlx::query_as(
        "SELECT
            (SELECT count(*)
               FROM turn_attempt
              WHERE turn_attempt_id = $1
                AND end_variant = 'without_stop'
                AND end_disposition = 'ambiguous'),
            (SELECT count(*)
               FROM turn_lifecycle
              WHERE turn_id = $2
                AND state_kind = 'terminal'
                AND terminal_disposition_kind = 'reconciliation_required'),
            (SELECT count(*)
               FROM turn_terminal_outbox_event
              WHERE disposition_kind = 'reconciliation_required'
              AND turn_id = $2
                AND model_call_id = $3)",
    )
    .bind(waiting.attempt.into_uuid())
    .bind(waiting.turn.into_uuid())
    .bind(waiting.call.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(waiting_stored, (1, 1, 1));

    let activated_interrupt = activate_earliest_queued_turn(
        &pool,
        EarliestQueuedTurnActivation {
            session: waiting.session.into_uuid(),
            origin_entry: Uuid::from_u128(waiting_seed + 0x110),
            starting_frontier: Uuid::from_u128(waiting_seed + 0x111),
            initial_attempt: Uuid::from_u128(waiting_seed + 0x112),
        },
    )
    .await?;
    assert_eq!(
        activated_interrupt.turn(),
        TurnId::from_uuid(Uuid::from_u128(waiting_seed + 25))
    );
    let unavailable = PostgresModelCallRepository::new(
        pool.clone(),
        ModelTargetCatalog::try_from_definitions([]).expect("an empty target catalog is valid"),
        model_credential_reference(),
    );
    assert!(matches!(
        unavailable
            .prepare_initial_call(
                waiting.session,
                ModelCallId::from_uuid(Uuid::from_u128(waiting_seed + 0x113)),
                FailedModelCallTurnIdentities::new(
                    SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(waiting_seed + 0x114)),
                    ContextFrontierId::from_uuid(Uuid::from_u128(waiting_seed + 0x115)),
                ),
                ContextFrontierId::from_uuid(Uuid::from_u128(waiting_seed + 0x116)),
                |_| panic!("the interrupt successor has no pending steering"),
            )
            .await?,
        PrepareInitialModelCallOutcome::TargetUnavailable(_)
    ));
    let activated_reclassified = activate_earliest_queued_turn(
        &pool,
        EarliestQueuedTurnActivation {
            session: waiting.session.into_uuid(),
            origin_entry: Uuid::from_u128(waiting_seed + 0x120),
            starting_frontier: Uuid::from_u128(waiting_seed + 0x121),
            initial_attempt: Uuid::from_u128(waiting_seed + 0x122),
        },
    )
    .await?;
    let descendant_command = input_with_delivery(
        waiting_seed + 0x123,
        waiting_seed + 1,
        "descendant of reconciliation-origin steering",
        DeliveryRequest::AfterCurrentTurn {
            expected_active_turn: activated_reclassified.turn(),
            configuration: input_choices(1, ModelSelectionOverride::UseSessionDefault),
        },
    );
    let descendant_outcome = submit_repository
        .handle(
            descendant_command.clone(),
            AcceptedInputId::from_uuid(Uuid::from_u128(waiting_seed + 0x124)),
            Some(TurnId::from_uuid(Uuid::from_u128(waiting_seed + 0x125))),
        )
        .await?;
    assert!(matches!(
        &descendant_outcome,
        SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Applied(
            SubmitInputAppliedResult::TurnOrigin(_)
        ))
    ));
    assert_eq!(
        submit_repository
            .handle(
                descendant_command,
                AcceptedInputId::from_uuid(Uuid::from_u128(waiting_seed + 0x126)),
                Some(TurnId::from_uuid(Uuid::from_u128(waiting_seed + 0x127))),
            )
            .await?,
        descendant_outcome
    );

    let failed_seed = seed + 0x40;
    let (failed, failed_repository, failed_authorized) =
        authorize_checkpointed_model_call(&pool, failed_seed).await?;
    SubmitInputRepository::new(pool.clone())
        .handle(
            input_with_delivery(
                failed_seed + 19,
                failed_seed + 1,
                "stop before known failure",
                DeliveryRequest::Interrupt {
                    expected_active_turn: failed.turn,
                    descendant_scope: DescendantTerminationScope::ParentAlone,
                    configuration: input_choices(1, ModelSelectionOverride::UseSessionDefault),
                },
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(failed_seed + 20)),
            Some(TurnId::from_uuid(Uuid::from_u128(failed_seed + 21))),
        )
        .await?;
    let failed_observation = failed_authorized
        .observation_correlation()
        .bind_terminal_observation(ModelCallTerminalObservation::KnownFailed);
    failed_repository
        .apply_terminal_observation(
            failed.session,
            failed_observation.clone(),
            ModelCallTerminalIdentities::Failed(FailedModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(failed_seed + 22)),
                ContextFrontierId::from_uuid(Uuid::from_u128(failed_seed + 23)),
            )),
            |_| panic!("the fixture has no pending steering to reclassify"),
        )
        .await?;
    assert_eq!(
        failed_repository
            .reread_terminal_observation(failed.session, &failed_observation)
            .await?,
        RetainedModelCallObservationStatus::AlreadyCommitted
    );
    let failed_snapshot = ProcessReadRepository::new(pool.clone())
        .read_transcript(failed.session)
        .await?
        .expect("the failed session has a transcript projection");
    let ProcessTurnState::Failed {
        terminal_frontier,
        terminal_attempt: Some(terminal_attempt),
        terminal_model_call: Some(terminal_call),
    } = failed_snapshot.turns()[0].state()
    else {
        panic!("the failed projection must retain its physical evidence");
    };
    assert_eq!(
        *terminal_frontier,
        ContextFrontierId::from_uuid(Uuid::from_u128(failed_seed + 23))
    );
    assert_eq!(*terminal_attempt, failed.attempt);
    assert_eq!(terminal_call.call(), failed.call);
    assert_eq!(
        terminal_call.disposition(),
        ProcessFailedModelCallDisposition::KnownFailed
    );

    let refused_seed = seed + 0x80;
    let (refused, refused_repository, refused_authorized) =
        authorize_checkpointed_model_call(&pool, refused_seed).await?;
    SubmitInputRepository::new(pool.clone())
        .handle(
            input_with_delivery(
                refused_seed + 19,
                refused_seed + 1,
                "stop before refusal",
                DeliveryRequest::Interrupt {
                    expected_active_turn: refused.turn,
                    descendant_scope: DescendantTerminationScope::ParentAlone,
                    configuration: input_choices(1, ModelSelectionOverride::UseSessionDefault),
                },
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(refused_seed + 20)),
            Some(TurnId::from_uuid(Uuid::from_u128(refused_seed + 21))),
        )
        .await?;
    let refused_observation = refused_authorized
        .observation_correlation()
        .bind_terminal_observation(ModelCallTerminalObservation::Refused);
    refused_repository
        .apply_terminal_observation(
            refused.session,
            refused_observation.clone(),
            ModelCallTerminalIdentities::Refused(
                signalbox_domain::RefusedModelCallTurnIdentities::new(
                    ContextFrontierId::from_uuid(Uuid::from_u128(refused_seed + 22)),
                ),
            ),
            |_| panic!("the fixture has no pending steering to reclassify"),
        )
        .await?;
    assert_eq!(
        refused_repository
            .reread_terminal_observation(refused.session, &refused_observation)
            .await?,
        RetainedModelCallObservationStatus::AlreadyCommitted
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// A definitive provider failure persists and projects only its closed cause
/// classification, independently of provider-authored native evidence.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn provider_failure_cause_round_trips_through_persistence_and_process_read()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0x76c0;
    let (fixture, repository, authorized) = authorize_checkpointed_model_call(&pool, seed).await?;
    let observation = authorized
        .observation_correlation()
        .bind_provider_failure_observation_with_usage(
            ProviderModelCallFailureCause::RateLimited,
            ProviderReportedTokenUsage::unreported(),
        );
    repository
        .apply_terminal_observation(
            fixture.session,
            observation.clone(),
            ModelCallTerminalIdentities::Failed(FailedModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 17)),
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 18)),
            )),
            |_| panic!("the fixture has no pending steering to reclassify"),
        )
        .await?;
    let stored_cause: Option<String> = sqlx::query_scalar(
        "SELECT terminal_provider_failure_cause
           FROM model_call
          WHERE model_call_id = $1",
    )
    .bind(fixture.call.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(stored_cause.as_deref(), Some("rate_limited"));
    assert_eq!(
        GoalRepository::new(pool.clone())
            .unchargeable_automatic_resume_turns(fixture.session, &[fixture.turn])
            .await?
            .as_ref(),
        &[fixture.turn]
    );
    assert_eq!(
        repository
            .reread_terminal_observation(fixture.session, &observation)
            .await?,
        RetainedModelCallObservationStatus::AlreadyCommitted
    );
    let snapshot = ProcessReadRepository::new(pool.clone())
        .read_transcript(fixture.session)
        .await?
        .expect("the failed session has a transcript projection");
    let ProcessTurnState::Failed {
        terminal_model_call: Some(terminal_call),
        ..
    } = snapshot.turns()[0].state()
    else {
        panic!("the failed projection retains its terminal call");
    };
    assert_eq!(
        terminal_call.provider_failure_cause(),
        Some(ProcessProviderModelCallFailureCause::RateLimited)
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// S07 / S08: the stop-request migration keeps
/// each stopping rejection paired with its immutable delivery and admits only
/// a known-failed call as failed post-cancellation provenance.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn stop_request_schema_keeps_delivery_and_failure_shapes_closed() -> Result<(), Box<dyn Error>>
{
    let (container, pool, _database_url) = migrated_postgres().await?;

    let result_shape: String = sqlx::query_scalar(
        "SELECT pg_get_constraintdef(oid)
           FROM pg_constraint
          WHERE conname = 'submit_input_command_result_shape'",
    )
    .fetch_one(&pool)
    .await?;
    assert!(result_shape.contains(
        "((rejection_kind = 'safe_point_unavailable_while_stopping'::text) AND \
         (delivery_kind = 'next_safe_point'::text))"
    ));
    assert!(result_shape.contains(
        "((rejection_kind = 'interrupt_already_applied'::text) AND \
         (delivery_kind = 'interrupt'::text))"
    ));

    let context_headroom_assertion: String = sqlx::query_scalar(
        "SELECT pg_get_functiondef(oid)
           FROM pg_proc
          WHERE proname = 'assert_failed_terminal_execution_final_state'",
    )
    .fetch_one(&pool)
    .await?;
    assert!(
        context_headroom_assertion.contains("FROM tool_continuation_context_headroom AS headroom")
    );
    assert!(
        context_headroom_assertion
            .contains("PERFORM assert_failed_terminal_execution_before_context_headroom(")
    );
    assert!(context_headroom_assertion.contains("checked_turn_id"));

    let failed_assertion: String = sqlx::query_scalar(
        "SELECT pg_get_functiondef(oid)
           FROM pg_proc
          WHERE proname = 'assert_failed_terminal_execution_before_context_headroom'",
    )
    .fetch_one(&pool)
    .await?;
    assert!(failed_assertion.contains("terminal_disposition_kind = 'known_failed'"));
    assert!(
        !failed_assertion.contains("terminal_disposition_kind IN ('known_failed', 'cancelled')")
    );
    assert!(!failed_assertion.contains("end_disposition IN ('known_failure', 'lost')"));
    assert!(failed_assertion.contains("FROM credential_pool_terminal_exhaustion AS exhausted"));
    assert!(
        failed_assertion
            .contains("PERFORM assert_failed_terminal_execution_before_credential_pools(")
    );
    assert!(failed_assertion.contains("checked_turn_id"));
    let ordinary_failed_assertion: String = sqlx::query_scalar(
        "SELECT pg_get_functiondef(oid)
           FROM pg_proc
          WHERE proname = 'assert_failed_terminal_execution_before_credential_pools'",
    )
    .fetch_one(&pool)
    .await?;
    assert!(ordinary_failed_assertion.contains("attempt_count <> 1"));

    let seed = 0x75c0;
    let (failed, failed_repository, failed_authorized) =
        authorize_checkpointed_model_call(&pool, seed).await?;
    SubmitInputRepository::new(pool.clone())
        .handle(
            input_with_delivery(
                seed + 19,
                seed + 1,
                "stop before cardinality check",
                DeliveryRequest::Interrupt {
                    expected_active_turn: failed.turn,
                    descendant_scope: DescendantTerminationScope::ParentAlone,
                    configuration: input_choices(1, ModelSelectionOverride::UseSessionDefault),
                },
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(seed + 20)),
            Some(TurnId::from_uuid(Uuid::from_u128(seed + 21))),
        )
        .await?;
    let failed_observation = failed_authorized
        .observation_correlation()
        .bind_terminal_observation(ModelCallTerminalObservation::KnownFailed);
    failed_repository
        .apply_terminal_observation(
            failed.session,
            failed_observation,
            ModelCallTerminalIdentities::Failed(FailedModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 22)),
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 23)),
            )),
            |_| panic!("the fixture has no pending steering to reclassify"),
        )
        .await?;
    sqlx::query("ALTER TABLE turn_attempt DISABLE TRIGGER USER")
        .execute(&pool)
        .await?;
    sqlx::query(
        "INSERT INTO turn_attempt
            (turn_attempt_id, turn_id, session_id, continued_from_attempt_id,
             state_kind, end_variant, end_disposition)
         VALUES ($1, $2, $3, $4, 'ended', 'without_stop', 'known_failure')",
    )
    .bind(Uuid::from_u128(seed + 24))
    .bind(failed.turn.into_uuid())
    .bind(failed.session.into_uuid())
    .bind(failed.attempt.into_uuid())
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE turn_attempt ENABLE TRIGGER USER")
        .execute(&pool)
        .await?;
    let cardinality_error = sqlx::query("SELECT assert_failed_terminal_execution_final_state($1)")
        .bind(failed.turn.into_uuid())
        .execute(&pool)
        .await
        .expect_err("a cancellation failure cannot hide an additional ended attempt");
    assert_eq!(
        cardinality_error
            .as_database_error()
            .and_then(|error| error.code()),
        Some("23514".into())
    );
    assert!(cardinality_error.as_database_error().is_some_and(|error| {
        error
            .message()
            .contains("post-cancellation failure lacks its exact single attempt")
    }));

    pool.close().await;
    drop(container);
    Ok(())
}

/// S03 / S04 / S07: completion and restart can
/// win after a durable stop request without erasing the applied interrupt.
/// Terminal reload accepts the completion race, while restart retains an
/// ambiguous call in proof-bearing terminal reconciliation.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn interrupt_completion_and_restart_races_retain_stop_history() -> Result<(), Box<dyn Error>>
{
    let (container, pool, _database_url) = migrated_postgres().await?;

    let completed_seed = 0x7700;
    let (completed, completed_repository, completed_authorized) =
        authorize_checkpointed_model_call(&pool, completed_seed).await?;
    let completed_interrupt = input_with_delivery(
        completed_seed + 19,
        completed_seed + 1,
        "completion race interrupt",
        DeliveryRequest::Interrupt {
            expected_active_turn: completed.turn,
            descendant_scope: DescendantTerminationScope::ParentAlone,
            configuration: input_choices(1, ModelSelectionOverride::UseSessionDefault),
        },
    );
    let completed_interrupt_outcome = SubmitInputRepository::new(pool.clone())
        .handle(
            completed_interrupt.clone(),
            AcceptedInputId::from_uuid(Uuid::from_u128(completed_seed + 20)),
            Some(TurnId::from_uuid(Uuid::from_u128(completed_seed + 21))),
        )
        .await?;
    assert!(matches!(
        completed_interrupt_outcome,
        SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Applied(
            SubmitInputAppliedResult::TurnOrigin(_)
        ))
    ));
    let assistant = AssistantText::try_new(String::from("already completed"))
        .expect("fixture assistant text is admitted");
    let completed_observation = completed_authorized
        .observation_correlation()
        .bind_terminal_observation(ModelCallTerminalObservation::Completed {
            assistant_text: vec![assistant],
        });
    let completed_outcome = completed_repository
        .apply_terminal_observation(
            completed.session,
            completed_observation.clone(),
            ModelCallTerminalIdentities::Completed(CompletedModelCallIdentities::new(
                vec![SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(
                    completed_seed + 22,
                ))],
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(completed_seed + 23)),
                ContextFrontierId::from_uuid(Uuid::from_u128(completed_seed + 24)),
            )),
            |_| panic!("the fixture has no pending steering to reclassify"),
        )
        .await?;
    let ModelCallTerminalOutcome::Completed(outcome) = &completed_outcome else {
        panic!("the physical completion must remain completed")
    };
    let signalbox_domain::AttemptEnd::AfterCancellation { disposition, .. } =
        outcome.attempt().end()
    else {
        panic!("the completed call retains its cancellation history")
    };
    assert_eq!(
        *disposition,
        signalbox_domain::CancellationStopDisposition::TurnCompleted
    );
    assert_eq!(
        completed_repository
            .reread_terminal_observation(completed.session, &completed_observation)
            .await?,
        RetainedModelCallObservationStatus::AlreadyCommitted
    );
    assert!(matches!(
        SubmitInputRepository::new(pool.clone())
            .handle(
                start_input(
                    completed_seed + 25,
                    completed_seed + 1,
                    "work after completion race",
                    1,
                    ModelSelectionOverride::UseSessionDefault,
                ),
                AcceptedInputId::from_uuid(Uuid::from_u128(completed_seed + 26)),
                Some(TurnId::from_uuid(Uuid::from_u128(completed_seed + 27))),
            )
            .await?,
        SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Applied(
            SubmitInputAppliedResult::TurnOrigin(_)
        ))
    ));

    let restart_seed = 0x7800;
    let (restarted, restarted_repository, _) =
        authorize_checkpointed_model_call(&pool, restart_seed).await?;
    let restart_interrupt = input_with_delivery(
        restart_seed + 19,
        restart_seed + 1,
        "restart race interrupt",
        DeliveryRequest::Interrupt {
            expected_active_turn: restarted.turn,
            descendant_scope: DescendantTerminationScope::ParentAlone,
            configuration: input_choices(1, ModelSelectionOverride::UseSessionDefault),
        },
    );
    SubmitInputRepository::new(pool.clone())
        .handle(
            restart_interrupt,
            AcceptedInputId::from_uuid(Uuid::from_u128(restart_seed + 20)),
            Some(TurnId::from_uuid(Uuid::from_u128(restart_seed + 21))),
        )
        .await?;
    let restart_outcome = restarted_repository
        .recover_after_restart(
            restarted.session,
            restarted.call,
            FailedModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(restart_seed + 22)),
                ContextFrontierId::from_uuid(Uuid::from_u128(restart_seed + 23)),
            ),
        )
        .await?;
    let ModelCallTerminalOutcome::ReconciliationRequired(reconciliation) = &restart_outcome else {
        panic!("restart loss after cancellation must require reconciliation")
    };
    let signalbox_domain::AttemptEnd::AfterCancellation { disposition, .. } =
        reconciliation.attempt().end()
    else {
        panic!("restart reconciliation retains its cancellation history")
    };
    assert_eq!(
        *disposition,
        signalbox_domain::CancellationStopDisposition::Lost
    );
    let restart_terminal_shape: (i64, i64, i64) = sqlx::query_as(
        "SELECT
            (SELECT count(*)
               FROM turn_lifecycle
              WHERE turn_id = $1
                AND state_kind = 'terminal'
                AND terminal_disposition_kind = 'reconciliation_required'),
            (SELECT count(*)
               FROM model_call
              WHERE model_call_id = $2
                AND state_kind = 'terminal'
                AND terminal_disposition_kind = 'ambiguous'),
            (SELECT count(*)
               FROM turn_terminal_outbox_event
              WHERE disposition_kind = 'reconciliation_required'
              AND turn_id = $1
                AND model_call_id = $2)",
    )
    .bind(restarted.turn.into_uuid())
    .bind(restarted.call.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(restart_terminal_shape, (1, 1, 1));

    pool.close().await;
    drop(container);
    Ok(())
}

/// Steering into a stopping turn is accepted, not rejected for state; the
/// cancellation boundary reclassifies it into a queued successor and settles
/// it `delivered`, as it settles the interrupt's own origin.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn steering_accepted_while_stopping_is_reclassified_at_cancellation()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0x7a00;
    let (fixture, model_repository, _, authorized) =
        authorize_checkpointed_model_call_with_prepared(&pool, seed).await?;
    let inputs = SubmitInputRepository::new(pool.clone());
    let interrupt_command = seed + 19;
    let steering_command = seed + 30;
    let successor_turn = TurnId::from_uuid(Uuid::from_u128(seed + 21));
    let interrupt_outcome = inputs
        .handle(
            input_with_delivery(
                interrupt_command,
                seed + 1,
                "stop issued call",
                DeliveryRequest::Interrupt {
                    expected_active_turn: fixture.turn,
                    descendant_scope: DescendantTerminationScope::ParentAlone,
                    configuration: input_choices(1, ModelSelectionOverride::UseSessionDefault),
                },
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(seed + 20)),
            Some(successor_turn),
        )
        .await?;
    assert!(matches!(
        interrupt_outcome,
        SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Applied(
            SubmitInputAppliedResult::TurnOrigin(_)
        ))
    ));

    let steering_outcome = inputs
        .handle(
            input_with_delivery(
                steering_command,
                seed + 1,
                "steer the stopping turn",
                DeliveryRequest::NextSafePoint {
                    expected_active_turn: fixture.turn,
                },
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(seed + 31)),
            None,
        )
        .await?;
    assert!(
        matches!(
            steering_outcome,
            SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Applied(
                SubmitInputAppliedResult::PendingSteering(_)
            ))
        ),
        "a stopping turn still accepts steering: {steering_outcome:?}"
    );

    let reclassified_turn = TurnId::from_uuid(Uuid::from_u128(seed + 32));
    let terminal = model_repository
        .apply_terminal_observation(
            fixture.session,
            authorized
                .observation_correlation()
                .bind_terminal_observation(ModelCallTerminalObservation::Cancelled),
            ModelCallTerminalIdentities::PhysicalCancellation(
                PhysicalCancellationModelCallTurnIdentities::new(
                    SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 22)),
                    ContextFrontierId::from_uuid(Uuid::from_u128(seed + 23)),
                ),
            ),
            |_| reclassified_turn,
        )
        .await?;
    assert!(matches!(terminal, ModelCallTerminalOutcome::Cancelled(_)));

    let receipts: Vec<(Uuid, String, Option<Uuid>)> = sqlx::query_as(
        "SELECT receipt.command_id, receipt.outcome_kind, receipt.delivered_turn_id
           FROM injection_settled_outbox_event AS receipt
          WHERE receipt.command_id = ANY($1)
          ORDER BY receipt.event_sequence",
    )
    .bind(vec![
        Uuid::from_u128(interrupt_command),
        Uuid::from_u128(steering_command),
    ])
    .fetch_all(&pool)
    .await?;
    assert_eq!(
        receipts,
        vec![
            (
                Uuid::from_u128(interrupt_command),
                String::from("delivered"),
                Some(successor_turn.into_uuid()),
            ),
            (
                Uuid::from_u128(steering_command),
                String::from("delivered"),
                Some(reclassified_turn.into_uuid()),
            ),
        ]
    );
    let queued: (String, String) = sqlx::query_as(
        "SELECT accepted.disposition_kind, successor.state_kind
           FROM accepted_input AS accepted
           JOIN turn_lifecycle AS successor ON successor.turn_id = accepted.origin_turn_id
          WHERE accepted.accepted_input_id = $1",
    )
    .bind(Uuid::from_u128(seed + 31))
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        queued,
        (
            String::from("reclassified_as_turn_origin"),
            String::from("queued")
        )
    );

    pool.close().await;
    drop(container);
    Ok(())
}
