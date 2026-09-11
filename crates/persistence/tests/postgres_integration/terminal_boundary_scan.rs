//! Terminal tool-result validation across a long retained compaction suffix.

use crate::*;

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn long_compaction_suffix_validates_without_repeated_history_walks()
-> Result<(), Box<dyn Error>> {
    const RETAINED_SUMMARIES: usize = 512;
    let (_container, pool, fixture, summaries) =
        failed_tool_turn_with_summaries(RETAINED_SUMMARIES).await?;
    assert_eq!(summaries.len(), RETAINED_SUMMARIES);
    let mut transaction = pool.begin().await?;
    flatten_terminal_frontier(&mut transaction, fixture).await?;
    sqlx::query("SET LOCAL statement_timeout = '8s'")
        .execute(&mut *transaction)
        .await?;
    sqlx::query("SELECT assert_tool_loop_turn_final_state($1)")
        .bind(fixture.turn.into_uuid())
        .execute(&mut *transaction)
        .await?;
    transaction.rollback().await?;
    pool.close().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn mismatched_compaction_prefix_cannot_hide_terminal_result_corruption()
-> Result<(), Box<dyn Error>> {
    const RETAINED_SUMMARIES: usize = 3;
    let (_container, pool, fixture, summaries) =
        failed_tool_turn_with_summaries(RETAINED_SUMMARIES).await?;
    let mut transaction = pool.begin().await?;
    flatten_terminal_frontier(&mut transaction, fixture).await?;
    sqlx::raw_sql("SET LOCAL statement_timeout = '8s';")
        .execute(&mut *transaction)
        .await?;
    sqlx::query("UPDATE context_frontier_delta SET semantic_entry_id = $2 WHERE owning_session_id = $1 AND semantic_entry_id = $3 AND context_frontier_id = (SELECT result_frontier_id FROM context_compaction WHERE session_id = $1 AND summary_entry_id = $3)")
        .bind(fixture.session.into_uuid()).bind(summaries[1].into_uuid()).bind(summaries[0].into_uuid()).execute(&mut *transaction).await?;
    let error = sqlx::query("SELECT assert_tool_loop_turn_final_state($1)")
        .bind(fixture.turn.into_uuid())
        .execute(&mut *transaction)
        .await
        .expect_err("a compaction boundary must retain the exact prefix before it");
    assert_eq!(
        error
            .as_database_error()
            .and_then(|error| error.constraint()),
        Some("tool_loop_terminal_result_suffix_exact")
    );
    transaction.rollback().await?;
    pool.close().await;
    Ok(())
}

/// Gives the terminal snapshot its own exact membership before its source
/// chain is checked, matching a terminal write that flattens retained boundaries.
async fn flatten_terminal_frontier(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    fixture: RestartModelCallFixture,
) -> Result<(), sqlx::Error> {
    sqlx::query("CREATE TEMP TABLE terminal_members ON COMMIT DROP AS SELECT member.* FROM turn_lifecycle lifecycle CROSS JOIN LATERAL resolve_context_frontier_members(lifecycle.session_id, lifecycle.terminal_frontier_id) member WHERE lifecycle.turn_id = $1")
        .bind(fixture.turn.into_uuid()).execute(&mut **transaction).await?;
    sqlx::raw_sql("ALTER TABLE context_frontier DISABLE TRIGGER context_frontier_is_append_only;
        ALTER TABLE context_frontier_delta DISABLE TRIGGER context_frontier_member_is_append_only;
        UPDATE context_frontier SET prefix_context_frontier_id = NULL WHERE (owning_session_id,context_frontier_id) IN (SELECT owning_session_id,context_frontier_id FROM terminal_members);
        INSERT INTO context_frontier_delta (owning_session_id,context_frontier_id,member_position,source_session_id,semantic_entry_id) SELECT owning_session_id,context_frontier_id,member_position,source_session_id,semantic_entry_id FROM terminal_members ON CONFLICT DO NOTHING;
").execute(&mut **transaction).await?;
    Ok(())
}

fn continuation_identities() -> signalbox_application::ToolContinuationIdentities {
    signalbox_application::ToolContinuationIdentities::new(
        vec![SemanticTranscriptEntryId::from_uuid(Uuid::now_v7())],
        ContextFrontierId::from_uuid(Uuid::now_v7()),
        ModelCallId::from_uuid(Uuid::now_v7()),
        FailedModelCallTurnIdentities::new(
            SemanticTranscriptEntryId::from_uuid(Uuid::now_v7()),
            ContextFrontierId::from_uuid(Uuid::now_v7()),
        ),
        ContextFrontierId::from_uuid(Uuid::now_v7()),
    )
}

/// Completes one tool result, retains the requested summaries, and closes the
/// checkpoint through the ordinary compaction-preparation failure path.
async fn failed_tool_turn_with_summaries(
    count: usize,
) -> Result<
    (
        TestDatabase,
        PgPool,
        RestartModelCallFixture,
        Vec<SemanticTranscriptEntryId>,
    ),
    Box<dyn Error>,
> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    const FIXTURE_SEED: u128 = 0x1379_0000;
    let seed = FIXTURE_SEED;
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
    // Six three-byte characters exceed the remaining fifteen-byte allowance.
    let result_text = String::from("界界界界界界");
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
    let result_entry = SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 0x26));
    let result_frontier = ContextFrontierId::from_uuid(Uuid::from_u128(seed + 0x27));
    let outcome = continuing_repository
        .prepare_continuation(
            fixture.session,
            fixture.turn,
            fixture.call,
            signalbox_application::ToolContinuationIdentities::new(
                vec![result_entry],
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
    assert_eq!(required, fixture.turn);

    // Each summary stays larger than the fixture's 100-token window.
    let summary_text = "retained tool checkpoint summary".repeat(32);
    let compaction = ContextCompactionRepository::new(pool.clone());
    let mut summaries = Vec::new();
    let mut checkpoint = result_frontier;
    for _ in 0..count {
        let preview = compaction
            .preview_automatic_range(fixture.session)
            .await?
            .expect("checkpoint is compactable");
        let through = preview
            .members()
            .last()
            .expect("checkpoint is nonempty")
            .position();
        let summary_entry = SemanticTranscriptEntryId::from_uuid(Uuid::now_v7());
        let summary_frontier = ContextFrontierId::from_uuid(Uuid::now_v7());
        let PrepareContextCompactionOutcome::Prepared(prepared) = compaction
            .prepare(PrepareContextCompactionRequest {
                command: DurableCommandId::from_uuid(Uuid::now_v7()),
                session: fixture.session,
                requested_through_position: Some(through),
                automatic_for_turn: Some(fixture.turn),
                defaults_version: SessionConfigurationDefaultsVersion::first(),
                selection,
                target,
                input_includes_cache_tokens: false,
                credential_reference: String::from("compaction-fixture"),
                call: ModelCallId::from_uuid(Uuid::now_v7()),
                compaction: ContextCompactionId::from_uuid(Uuid::now_v7()),
                summary_entry,
                result_frontier: summary_frontier,
            })
            .await?
        else {
            panic!("checkpoint admits automatic compaction");
        };
        compaction.authorize(&prepared).await?;
        compaction
            .complete(
                &prepared,
                &summary_text,
                ContextCompactionTokenUsage::unreported(),
            )
            .await?;
        let outcome = continuing_repository
            .prepare_continuation(
                fixture.session,
                fixture.turn,
                fixture.call,
                continuation_identities(),
                |_| panic!("fixture has no steering"),
            )
            .await?;
        assert!(
            matches!(
                outcome,
                signalbox_application::PrepareToolContinuationOutcome::ContextCompactionRequired(_)
            ),
            "oversized fixture summary requires another compaction: {outcome:?}"
        );
        summaries.push(summary_entry);
        checkpoint = summary_frontier;
    }
    let outcome = continuing_repository
        .fail_compaction_checkpoint(
            fixture.session,
            fixture.turn,
            fixture.call,
            checkpoint,
            continuation_identities(),
            |_| panic!("fixture has no steering"),
        )
        .await?;
    assert!(matches!(
        outcome,
        signalbox_application::PrepareToolContinuationOutcome::ContextCompactionFailed(_)
    ));
    Ok((container, pool, fixture, summaries))
}
