//! Delegated result and wake rereads, delegated turn commits, and foreground delegation resumption.

use crate::*;

/// S18: a retained child relationship does not make a
/// later accepted-input turn subject to delegated initial-result closure.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s18_successor_completion_rereads_without_delegated_result() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0xeb00;
    let fixture = authorize_delegated_successor_model_call_fixture(&pool, seed).await?;
    let observation = fixture
        .authorized
        .observation_correlation()
        .bind_terminal_observation(ModelCallTerminalObservation::Completed {
            assistant_text: vec![
                AssistantText::try_new(String::from("ordinary successor result"))
                    .expect("fixture successor result is admitted"),
            ],
        });
    fixture
        .repository
        .apply_terminal_observation(
            fixture.child,
            observation.clone(),
            ModelCallTerminalIdentities::Completed(CompletedModelCallIdentities::new(
                vec![SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(
                    seed + 0x210,
                ))],
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 0x211)),
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 0x212)),
            )),
            |_| panic!("the successor completion fixture has no steering"),
        )
        .await?;

    assert_eq!(
        fixture
            .repository
            .reread_terminal_observation(fixture.child, &observation)
            .await?,
        RetainedModelCallObservationStatus::AlreadyCommitted
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// S18: nonterminal observation reread likewise scopes
/// delegated-result absence to the exact delegation-origin turn.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s18_successor_tool_round_rereads_without_delegated_result() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0xec00;
    let fixture = authorize_delegated_successor_model_call_fixture(&pool, seed).await?;
    let request = ToolRequestId::from_uuid(Uuid::from_u128(seed + 0x210));
    let response =
        ToolUsingAssistantResponse::try_from_parts(vec![AssistantResponsePart::ToolCall(
            ToolCallProposal::new(
                ToolName::try_new(String::from("current_time")).expect("valid fixture tool name"),
                NormalizedToolArguments::try_from_provider_text(String::from("{}"))
                    .expect("bounded fixture arguments"),
            ),
        )])
        .expect("the proposal forms a tool-using response");
    let observation = fixture
        .authorized
        .observation_correlation()
        .bind_terminal_observation(ModelCallTerminalObservation::CompletedWithTools { response });
    fixture
        .repository
        .apply_terminal_observation(
            fixture.child,
            observation.clone(),
            ModelCallTerminalIdentities::ToolRound(ToolRoundModelCallIdentities::new(
                vec![ToolResponsePartIdentity::tool_call(
                    SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 0x211)),
                    request,
                    InitialToolApproval::Confirm,
                )],
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 0x212)),
                None,
            )),
            |_| panic!("the successor tool-round fixture has no steering"),
        )
        .await?;

    assert_eq!(
        fixture
            .repository
            .reread_terminal_observation(fixture.child, &observation)
            .await?,
        RetainedModelCallObservationStatus::AlreadyCommitted
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// S18: a terminal background-delivery wake
/// authenticates its model-call closure without initial-child result evidence.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s18_wake_completion_rereads_without_child_result() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0xed00;
    let fixture = authorize_delegated_successor_model_call_fixture(&pool, seed).await?;
    let observation = fixture
        .authorized
        .observation_correlation()
        .bind_terminal_observation(ModelCallTerminalObservation::Completed {
            assistant_text: vec![
                AssistantText::try_new(String::from("background wake result"))
                    .expect("fixture wake result is admitted"),
            ],
        });
    fixture
        .repository
        .apply_terminal_observation(
            fixture.child,
            observation.clone(),
            ModelCallTerminalIdentities::Completed(CompletedModelCallIdentities::new(
                vec![SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(
                    seed + 0x210,
                ))],
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 0x211)),
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 0x212)),
            )),
            |_| panic!("the wake completion fixture has no steering"),
        )
        .await?;
    reclassify_successor_as_delegated_wake(&pool, &fixture).await?;

    assert_eq!(
        fixture
            .repository
            .reread_terminal_observation(fixture.child, &observation)
            .await?,
        RetainedModelCallObservationStatus::AlreadyCommitted
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// S18: a continuing background-delivery wake
/// authenticates absence of initial-child result evidence.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s18_wake_tool_round_rereads_without_child_result() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0xee00;
    let fixture = authorize_delegated_successor_model_call_fixture(&pool, seed).await?;
    let request = ToolRequestId::from_uuid(Uuid::from_u128(seed + 0x210));
    let response =
        ToolUsingAssistantResponse::try_from_parts(vec![AssistantResponsePart::ToolCall(
            ToolCallProposal::new(
                ToolName::try_new(String::from("current_time")).expect("valid fixture tool name"),
                NormalizedToolArguments::try_from_provider_text(String::from("{}"))
                    .expect("bounded fixture arguments"),
            ),
        )])
        .expect("the proposal forms a tool-using response");
    let observation = fixture
        .authorized
        .observation_correlation()
        .bind_terminal_observation(ModelCallTerminalObservation::CompletedWithTools { response });
    fixture
        .repository
        .apply_terminal_observation(
            fixture.child,
            observation.clone(),
            ModelCallTerminalIdentities::ToolRound(ToolRoundModelCallIdentities::new(
                vec![ToolResponsePartIdentity::tool_call(
                    SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 0x211)),
                    request,
                    InitialToolApproval::Confirm,
                )],
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 0x212)),
                None,
            )),
            |_| panic!("the wake tool-round fixture has no steering"),
        )
        .await?;
    reclassify_successor_as_delegated_wake(&pool, &fixture).await?;

    assert_eq!(
        fixture
            .repository
            .reread_terminal_observation(fixture.child, &observation)
            .await?,
        RetainedModelCallObservationStatus::AlreadyCommitted
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// S18: a historical wake between accepted-input turns is
/// not the baseline that precedes the session's earliest accepted input.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s18_historical_wake_does_not_replace_accepted_baseline() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0xef00;
    let session = SessionId::from_uuid(Uuid::from_u128(seed + 1));
    let selection = DirectModelSelection::from_uuid(Uuid::from_u128(seed + 2));
    let provider = ProviderModelIdentity::from_uuid(Uuid::from_u128(seed + 3));
    let first_turn = TurnId::from_uuid(Uuid::from_u128(seed + 4));
    let first_accepted = AcceptedInputId::from_uuid(Uuid::from_u128(seed + 5));
    let second_turn = TurnId::from_uuid(Uuid::from_u128(seed + 6));
    let second_accepted = AcceptedInputId::from_uuid(Uuid::from_u128(seed + 7));
    let wake_turn = TurnId::from_uuid(Uuid::from_u128(seed + 8));
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(prepared(seed + 9, seed + 1, direct(seed + 2)))
        .await?;
    let input_repository = SubmitInputRepository::new(pool.clone());
    assert!(matches!(
        input_repository
            .handle(
                start_input(
                    seed + 10,
                    seed + 1,
                    "first accepted input",
                    1,
                    ModelSelectionOverride::UseSessionDefault,
                ),
                first_accepted,
                Some(first_turn),
            )
            .await?,
        SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Applied(
            SubmitInputAppliedResult::TurnOrigin(_)
        ))
    ));
    activate_earliest_queued_turn(
        &pool,
        EarliestQueuedTurnActivation {
            session: session.into_uuid(),
            origin_entry: Uuid::from_u128(seed + 11),
            starting_frontier: Uuid::from_u128(seed + 12),
            initial_attempt: Uuid::from_u128(seed + 13),
        },
    )
    .await?;
    let targets = ModelTargetCatalog::try_from_definitions([ModelTargetDefinition::new(
        selection,
        ResolvedProviderTarget::naming(provider),
    )])
    .expect("one historical-wake target forms a catalog");
    complete_text_turn(
        &pool,
        session,
        targets,
        model_credential_reference(),
        seed + 0x20,
        "first accepted result",
    )
    .await?;
    assert!(matches!(
        input_repository
            .handle(
                start_input(
                    seed + 0x40,
                    seed + 1,
                    "second accepted input",
                    1,
                    ModelSelectionOverride::UseSessionDefault,
                ),
                second_accepted,
                Some(second_turn),
            )
            .await?,
        SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Applied(
            SubmitInputAppliedResult::TurnOrigin(_)
        ))
    ));

    let mut transaction = pool.begin().await?;
    sqlx::raw_sql(
        "ALTER TABLE accepted_input DISABLE TRIGGER ALL;
         ALTER TABLE queued_input_origin DISABLE TRIGGER ALL;
         ALTER TABLE turn_lifecycle DISABLE TRIGGER ALL;
         ALTER TABLE semantic_transcript_entry DISABLE TRIGGER ALL;
         ALTER TABLE session_pending_delivery DISABLE TRIGGER ALL;
         ALTER TABLE session_delegation_wake_turn_origin DISABLE TRIGGER ALL;",
    )
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "UPDATE accepted_input SET acceptance_position = 3
          WHERE session_id = $1 AND accepted_input_id = $2",
    )
    .bind(session.into_uuid())
    .bind(second_accepted.into_uuid())
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "UPDATE queued_input_origin SET acceptance_position = 3
          WHERE session_id = $1 AND accepted_input_id = $2",
    )
    .bind(session.into_uuid())
    .bind(second_accepted.into_uuid())
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "UPDATE turn_lifecycle SET acceptance_position = 3
          WHERE session_id = $1 AND turn_id = $2",
    )
    .bind(session.into_uuid())
    .bind(second_turn.into_uuid())
    .execute(&mut *transaction)
    .await?;
    insert_historical_delegation_wake(
        &mut transaction,
        session,
        first_turn,
        wake_turn,
        selection,
        SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 0x42)),
        ContextFrontierId::from_uuid(Uuid::from_u128(seed + 0x43)),
    )
    .await?;
    sqlx::raw_sql(
        "ALTER TABLE accepted_input ENABLE TRIGGER ALL;
         ALTER TABLE queued_input_origin ENABLE TRIGGER ALL;
         ALTER TABLE turn_lifecycle ENABLE TRIGGER ALL;
         ALTER TABLE semantic_transcript_entry ENABLE TRIGGER ALL;
         ALTER TABLE session_pending_delivery ENABLE TRIGGER ALL;
         ALTER TABLE session_delegation_wake_turn_origin ENABLE TRIGGER ALL;",
    )
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await?;

    let prior_position: i64 = sqlx::query_scalar(
        "SELECT max(acceptance_position)::bigint
           FROM turn_lifecycle
          WHERE session_id = $1",
    )
    .bind(session.into_uuid())
    .fetch_one(&pool)
    .await?;
    let third_turn = TurnId::from_uuid(Uuid::from_u128(seed + 0x50));
    let third = input_repository
        .handle(
            start_input(
                seed + 0x51,
                seed + 1,
                "third accepted input",
                1,
                ModelSelectionOverride::UseSessionDefault,
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(seed + 0x52)),
            Some(third_turn),
        )
        .await?;
    let SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Applied(
        SubmitInputAppliedResult::TurnOrigin(applied),
    )) = third
    else {
        panic!("the historical wake leaves the third accepted input schedulable")
    };
    assert_eq!(
        applied.acceptance_position().as_u64(),
        u64::try_from(prior_position)? + 1
    );

    pool.close().await;
    drop(container);
    Ok(())
}

async fn insert_historical_delegation_wake(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session: SessionId,
    predecessor: TurnId,
    wake: TurnId,
    selection: DirectModelSelection,
    terminal_entry: SemanticTranscriptEntryId,
    terminal_frontier: ContextFrontierId,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO semantic_transcript_entry
            (source_session_id, semantic_entry_id, payload_kind,
             completed_turn_id)
         VALUES ($1, $2, 'turn_completed', $3)",
    )
    .bind(session.into_uuid())
    .bind(terminal_entry.into_uuid())
    .bind(wake.into_uuid())
    .execute(&mut **transaction)
    .await?;
    sqlx::query(
        "INSERT INTO context_frontier
            (owning_session_id, context_frontier_id,
             prefix_context_frontier_id, member_count)
         SELECT $1, $2, source.terminal_frontier_id,
                prefix.member_count + 1
           FROM turn_lifecycle AS source
           JOIN context_frontier AS prefix
             ON prefix.owning_session_id = source.session_id
            AND prefix.context_frontier_id = source.terminal_frontier_id
          WHERE source.session_id = $1 AND source.turn_id = $3",
    )
    .bind(session.into_uuid())
    .bind(terminal_frontier.into_uuid())
    .bind(predecessor.into_uuid())
    .execute(&mut **transaction)
    .await?;
    sqlx::query(
        "INSERT INTO context_frontier_delta
            (owning_session_id, context_frontier_id, member_position,
             source_session_id, semantic_entry_id)
         SELECT $1, $2, prefix.member_count + 1, $1, $3
           FROM turn_lifecycle AS source
           JOIN context_frontier AS prefix
             ON prefix.owning_session_id = source.session_id
            AND prefix.context_frontier_id = source.terminal_frontier_id
          WHERE source.session_id = $1 AND source.turn_id = $4",
    )
    .bind(session.into_uuid())
    .bind(terminal_frontier.into_uuid())
    .bind(terminal_entry.into_uuid())
    .bind(predecessor.into_uuid())
    .execute(&mut **transaction)
    .await?;
    sqlx::query(
        "INSERT INTO turn_lifecycle
         SELECT (jsonb_populate_record(
                    NULL::turn_lifecycle,
                    to_jsonb(source) || jsonb_build_object(
                        'turn_id', $3,
                        'origin_kind', 'delegation',
                        'origin_accepted_input_id', NULL,
                        'acceptance_position', 2,
                        'start_lineage_kind', 'after',
                        'immediate_predecessor_turn_id', $2,
                        'starting_frontier_id', source.terminal_frontier_id,
                        'terminal_frontier_id', $4
                    )
                )).*
           FROM turn_lifecycle AS source
          WHERE source.session_id = $1 AND source.turn_id = $2",
    )
    .bind(session.into_uuid())
    .bind(predecessor.into_uuid())
    .bind(wake.into_uuid())
    .bind(terminal_frontier.into_uuid())
    .execute(&mut **transaction)
    .await?;
    sqlx::query(
        "INSERT INTO session_pending_delivery
            (recipient_session_id, delivery_sequence, delivery_kind)
         VALUES ($1, 1, 'background_result')",
    )
    .bind(session.into_uuid())
    .execute(&mut **transaction)
    .await?;
    sqlx::query(
        "INSERT INTO session_delegation_wake_turn_origin
            (turn_id, recipient_session_id, admission_position,
             first_delivery_sequence, through_delivery_sequence,
             defaults_version, requested_model_kind,
             requested_direct_model_selection_id, frozen_model_kind,
             frozen_direct_model_selection_id)
         VALUES ($1, $2, 2, 1, 1, 1, 'direct', $3, 'direct', $3)",
    )
    .bind(wake.into_uuid())
    .bind(session.into_uuid())
    .bind(selection.into_uuid())
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

/// S18: a failed delegated initial turn remains a complete
/// semantic subject when the child accepts its next user turn, even though the
/// failed call produced no assistant entry.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s18_failed_delegated_subject_allows_successor_input() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0xed00;
    let fixture = delegated_capability_failure_fixture(&pool, seed).await?;
    let successor = TurnId::from_uuid(Uuid::from_u128(seed + 0x100));
    let outcome = SubmitInputRepository::new(pool.clone())
        .handle(
            start_input(
                seed + 0x101,
                fixture.child.as_uuid().as_u128(),
                "continue after delegated failure",
                1,
                ModelSelectionOverride::UseSessionDefault,
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(seed + 0x102)),
            Some(successor),
        )
        .await?;

    assert!(matches!(
        outcome,
        SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Applied(
            SubmitInputAppliedResult::TurnOrigin(_)
        ))
    ));

    pool.close().await;
    drop(container);
    Ok(())
}

/// S18: a retained relationship with a missing
/// immutable initial task is corruption, not an ordinary-session reread.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s18_capability_reread_requires_initial_task() -> Result<(), Box<dyn Error>> {
    assert_delegated_capability_reread_rejects_damage(
        0xe080,
        DelegatedCapabilityResultDamage::InitialTask,
    )
    .await
}

/// S18: ambiguous capability-failure reread
/// authenticates the delegated child result itself.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s18_capability_reread_requires_delegated_result() -> Result<(), Box<dyn Error>> {
    assert_delegated_capability_reread_rejects_damage(
        0xe100,
        DelegatedCapabilityResultDamage::Result,
    )
    .await
}

/// S18: ambiguous capability-failure reread
/// authenticates the exact delegated parent update satellite.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s18_capability_reread_requires_delegated_update() -> Result<(), Box<dyn Error>> {
    assert_delegated_capability_reread_rejects_damage(
        0xe200,
        DelegatedCapabilityResultDamage::Update,
    )
    .await
}

/// S18: ambiguous capability-failure reread
/// requires the canonical delegated parent-update outbox kind.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s18_capability_reread_requires_update_header_kind() -> Result<(), Box<dyn Error>> {
    assert_delegated_capability_reread_rejects_damage(
        0xe280,
        DelegatedCapabilityResultDamage::UpdateHeaderKind,
    )
    .await
}

/// S18: ambiguous capability-failure reread
/// authenticates the exact delegated parent wake satellite.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s18_capability_reread_requires_delegated_wake() -> Result<(), Box<dyn Error>> {
    assert_delegated_capability_reread_rejects_damage(0xe300, DelegatedCapabilityResultDamage::Wake)
        .await
}

/// S18: ambiguous capability-failure reread
/// requires the canonical delegated parent-wake outbox kind.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s18_capability_reread_requires_wake_header_kind() -> Result<(), Box<dyn Error>> {
    assert_delegated_capability_reread_rejects_damage(
        0xe380,
        DelegatedCapabilityResultDamage::WakeHeaderKind,
    )
    .await
}

/// S18: a completed delegated observation reread
/// authenticates its exact delivered child result.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s18_completed_observation_reread_requires_result() -> Result<(), Box<dyn Error>> {
    assert_delegated_observation_reread_requires_result(
        0xe400,
        DelegatedObservationDisposition::Completed,
    )
    .await
}

/// S18: a known-failed delegated observation
/// reread authenticates its exact delivered child result.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s18_failed_observation_reread_requires_result() -> Result<(), Box<dyn Error>> {
    assert_delegated_observation_reread_requires_result(
        0xe500,
        DelegatedObservationDisposition::KnownFailed,
    )
    .await
}

/// S18: a refused delegated observation reread
/// authenticates its exact delivered child result.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s18_refused_observation_reread_requires_result() -> Result<(), Box<dyn Error>> {
    assert_delegated_observation_reread_requires_result(
        0xe600,
        DelegatedObservationDisposition::Refused,
    )
    .await
}

/// S18: a cancelled delegated observation reread
/// authenticates its exact delivered child result.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s18_cancelled_observation_reread_requires_result() -> Result<(), Box<dyn Error>> {
    assert_delegated_observation_reread_requires_result(
        0xe700,
        DelegatedObservationDisposition::Cancelled,
    )
    .await
}

/// S18: authoritative terminal reread
/// authenticates the complete delivery set for waits that predated the result.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s18_observation_reread_requires_wait_delivery() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0xe800;
    let fixture = authorize_delegated_model_call_fixture(&pool, seed).await?;
    let observation = fixture
        .authorized
        .observation_correlation()
        .bind_terminal_observation(ModelCallTerminalObservation::Completed {
            assistant_text: vec![
                AssistantText::try_new(String::from("delivered delegated result"))
                    .expect("fixture delegated result is admitted"),
            ],
        });
    fixture
        .repository
        .apply_terminal_observation(
            fixture.child,
            observation.clone(),
            ModelCallTerminalIdentities::Completed(CompletedModelCallIdentities::new(
                vec![SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(
                    seed + 30,
                ))],
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 31)),
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 32)),
            )),
            |_| panic!("the delegated observation fixture has no steering"),
        )
        .await?;
    let awaiting_request = ToolRequestId::from_uuid(Uuid::from_u128(seed + 40));
    sqlx::raw_sql(
        "ALTER TABLE session_delegation_wait DISABLE TRIGGER ALL;
         ALTER TABLE session_pending_delivery DISABLE TRIGGER ALL;
         ALTER TABLE session_child_result_delivery DISABLE TRIGGER ALL;",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO session_delegation_wait
            (awaiting_tool_request_id, spawning_tool_request_id,
             parent_session_id, parent_turn_id, child_session_id, wait_mode)
         SELECT $1, relation.spawning_tool_request_id,
                relation.parent_session_id, relation.parent_turn_id,
                relation.child_session_id, 'background'
           FROM session_delegation AS relation
          WHERE relation.spawning_tool_request_id = $2",
    )
    .bind(awaiting_request.into_uuid())
    .bind(fixture.spawning_request.into_uuid())
    .execute(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO session_pending_delivery
            (recipient_session_id, delivery_sequence, delivery_kind)
         VALUES ($1, 1, 'background_result')",
    )
    .bind(fixture.parent.into_uuid())
    .execute(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO session_child_result_delivery
            (awaiting_tool_request_id, spawning_tool_request_id,
             parent_session_id, delivery_sequence, delivery_kind)
         VALUES ($1, $2, $3, 1, 'background_result')",
    )
    .bind(awaiting_request.into_uuid())
    .bind(fixture.spawning_request.into_uuid())
    .bind(fixture.parent.into_uuid())
    .execute(&pool)
    .await?;
    sqlx::raw_sql(
        "ALTER TABLE session_delegation_wait ENABLE TRIGGER ALL;
         ALTER TABLE session_pending_delivery ENABLE TRIGGER ALL;
         ALTER TABLE session_child_result_delivery ENABLE TRIGGER ALL;",
    )
    .execute(&pool)
    .await?;
    assert_eq!(
        fixture
            .repository
            .reread_terminal_observation(fixture.child, &observation)
            .await?,
        RetainedModelCallObservationStatus::AlreadyCommitted
    );
    sqlx::query("ALTER TABLE session_child_result_delivery DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query(
        "DELETE FROM session_child_result_delivery
          WHERE awaiting_tool_request_id = $1",
    )
    .bind(awaiting_request.into_uuid())
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE session_child_result_delivery ENABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    let error = fixture
        .repository
        .reread_terminal_observation(fixture.child, &observation)
        .await
        .expect_err("a terminal reread requires every pre-existing wait delivery");

    assert!(matches!(
        error,
        ModelCallRepositoryError::InvalidTransition(
            "retained observation delegated result closure changed"
        )
    ));

    pool.close().await;
    drop(container);
    Ok(())
}

/// S18: a tool-round observation cannot retain
/// child-result closure while the delegated child continues.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s18_tool_round_reread_rejects_delegated_result() -> Result<(), Box<dyn Error>> {
    assert_delegated_nonterminal_reread_rejects_result(
        0xe900,
        DelegatedNonterminalObservation::CompletedWithTools,
    )
    .await
}

/// S18: an ambiguous observation cannot retain
/// child-result closure while recovery remains authoritative.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s18_ambiguous_reread_rejects_delegated_result() -> Result<(), Box<dyn Error>> {
    assert_delegated_nonterminal_reread_rejects_result(
        0xea00,
        DelegatedNonterminalObservation::Ambiguous,
    )
    .await
}

/// S17: known delegated tool-crash recovery publishes the
/// typed failed child result, parent update, and parent wake atomically.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s17_delegated_tool_crash_publishes_failed_result() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0xe000;
    let fixture = prepare_delegated_tool_crash_fixture(&pool, seed).await?;
    let mut ids = delegated_tool_crash_scan_ids(seed);
    let recovered = PostgresStartupScanRepository::new(pool.clone())
        .recover(
            fixture.child,
            delegated_tool_crash_failure_ids(seed),
            &mut ids,
        )
        .await?;
    let evidence: (String, String, i64, i64) = sqlx::query_as(
        "SELECT result.outcome_kind, event.reason_kind,
                (SELECT count(*) FROM delegation_update_outbox_event AS parent_update
                  WHERE parent_update.result_spawning_request_id = $1
                    AND parent_update.session_id = $2),
                (SELECT count(*) FROM delegation_wake_outbox_event AS wake
                  WHERE wake.result_spawning_request_id = $1
                    AND wake.session_id = $2)
           FROM session_child_result AS result
           JOIN session_delegation_event AS event
             ON event.spawning_tool_request_id = result.spawning_tool_request_id
            AND event.event_ordinal = result.event_ordinal
          WHERE result.spawning_tool_request_id = $1",
    )
    .bind(fixture.spawning_request.into_uuid())
    .bind(fixture.parent.into_uuid())
    .fetch_one(&pool)
    .await?;

    let StartupScanSessionOutcome::RecoveredToolAttempt(outcome) = recovered else {
        panic!("the delegated tool crash is recovered")
    };
    assert!(matches!(*outcome, ToolAttemptCrashOutcome::KnownFailed(_)));
    assert_eq!(
        evidence,
        ("child_failed".into(), "child_execution_failed".into(), 1, 1)
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// S18: startup classifies an undecodable delegated active
/// phase as durable corruption rather than retryable database failure.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s18_delegated_null_active_phase_fails_closed() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0xe400;
    let fixture = prepare_delegated_tool_crash_fixture(&pool, seed).await?;
    sqlx::query("ALTER TABLE turn_lifecycle DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query(
        "UPDATE turn_lifecycle
            SET active_phase_kind = NULL
          WHERE turn_id = $1 AND session_id = $2",
    )
    .bind(fixture.turn.into_uuid())
    .bind(fixture.child.into_uuid())
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE turn_lifecycle ENABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    let mut ids = delegated_tool_crash_scan_ids(seed);
    let error = PostgresStartupScanRepository::new(pool.clone())
        .recover(
            fixture.child,
            delegated_tool_crash_failure_ids(seed),
            &mut ids,
        )
        .await
        .expect_err("a null delegated active phase is durable corruption");

    assert_eq!(
        error.operator_failure_class(),
        OperatorFailureClass::FailClosedCorruption
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// S17: delegated startup crash recovery takes the parent
/// endpoint prefix before the child scheduler.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s17_delegated_tool_crash_locks_parent_before_child_scheduler() -> Result<(), Box<dyn Error>>
{
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0xe200;
    let fixture = prepare_delegated_tool_crash_fixture(&pool, seed).await?;
    let mut parent_lock = pool.begin().await?;
    sqlx::query("SELECT session_id FROM session WHERE session_id = $1 FOR NO KEY UPDATE")
        .bind(fixture.parent.into_uuid())
        .execute(&mut *parent_lock)
        .await?;
    let recovery_pool = pool.clone();
    let recovery = tokio::spawn(async move {
        let mut ids = delegated_tool_crash_scan_ids(seed);
        PostgresStartupScanRepository::new(recovery_pool)
            .recover(
                fixture.child,
                delegated_tool_crash_failure_ids(seed),
                &mut ids,
            )
            .await
    });
    assert!(
        blocked_backends_reached(&pool, 1).await?,
        "delegated recovery waits on the parent endpoint lock"
    );
    sqlx::query("SET LOCAL lock_timeout = '250ms'")
        .execute(&mut *parent_lock)
        .await?;
    let locked_child: Uuid = sqlx::query_scalar(
        "SELECT session_id
           FROM session_scheduler
          WHERE session_id = $1
          FOR UPDATE",
    )
    .bind(fixture.child.into_uuid())
    .fetch_one(&mut *parent_lock)
    .await?;
    assert_eq!(locked_child, fixture.child.into_uuid());
    parent_lock.rollback().await?;
    let recovered = recovery.await??;
    let StartupScanSessionOutcome::RecoveredToolAttempt(outcome) = recovered else {
        panic!("the delegated tool crash is recovered")
    };
    assert!(matches!(*outcome, ToolAttemptCrashOutcome::KnownFailed(_)));

    pool.close().await;
    drop(container);
    Ok(())
}

/// S17: completing a delegated initial task atomically creates its
/// typed returned result, parent update, and parent wake before commit.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s17_delegated_completion_materializes_result_update_and_wake() -> Result<(), Box<dyn Error>>
{
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0xd500;
    let (parent, child, child_turn, spawning_request, selection) =
        activate_delegated_result_fixture(&pool, seed).await?;
    let provider = ProviderModelIdentity::from_uuid(Uuid::from_u128(seed + 20));
    let targets = ModelTargetCatalog::try_from_definitions([ModelTargetDefinition::new(
        selection,
        ResolvedProviderTarget::naming(provider),
    )])
    .expect("one delegated completion target forms a catalog");
    let expected_content = "delivered child content";
    complete_text_turn(
        &pool,
        child,
        targets,
        model_credential_reference(),
        seed + 30,
        expected_content,
    )
    .await?;

    let materialized: DelegatedResultMaterializationEvidence = sqlx::query_as(
        "SELECT result.outcome_kind, result.content_text,
                event.reason_kind, event.provenance_kind,
                lifecycle.terminal_disposition_kind,
                (SELECT count(*) FROM delegation_update_outbox_event AS parent_update
                  WHERE parent_update.result_spawning_request_id = $1
                    AND parent_update.session_id = $2
                    AND parent_update.content_text = result.content_text) AS parent_update_count,
                (SELECT count(*) FROM delegation_wake_outbox_event AS wake
                  WHERE wake.result_spawning_request_id = $1
                    AND wake.session_id = $2) AS parent_wake_count
           FROM session_child_result AS result
           JOIN session_delegation_event AS event
             ON event.spawning_tool_request_id = result.spawning_tool_request_id
            AND event.event_ordinal = result.event_ordinal
           JOIN turn_lifecycle AS lifecycle
             ON lifecycle.turn_id = $3 AND lifecycle.session_id = $4
          WHERE result.spawning_tool_request_id = $1",
    )
    .bind(spawning_request.into_uuid())
    .bind(parent.into_uuid())
    .bind(child_turn.into_uuid())
    .bind(child.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(materialized.outcome_kind, "result_returned");
    assert_eq!(materialized.content_text.as_deref(), Some(expected_content));
    assert_eq!(materialized.reason_kind, "child_completed");
    assert_eq!(materialized.provenance_kind, "child_turn");
    assert_eq!(materialized.terminal_disposition_kind, "completed");
    assert_eq!(materialized.parent_update_count, 1);
    assert_eq!(materialized.parent_wake_count, 1);

    pool.close().await;
    drop(container);
    Ok(())
}

/// S17: initial target resolution failure for a delegated child
/// atomically materializes the failed result, parent update, and parent wake.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s17_delegated_initial_target_failure_materializes_parent_delivery()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0xd550;
    let (parent, child, child_turn, spawning_request, _selection) =
        activate_delegated_result_fixture(&pool, seed).await?;
    let targets =
        ModelTargetCatalog::try_from_definitions([]).expect("an empty target catalog is valid");
    let repository =
        PostgresModelCallRepository::new(pool.clone(), targets, model_credential_reference());
    let outcome = repository
        .prepare_initial_call(
            child,
            ModelCallId::from_uuid(Uuid::from_u128(seed + 20)),
            FailedModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 21)),
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 22)),
            ),
            ContextFrontierId::from_uuid(Uuid::from_u128(seed + 23)),
            |_| {
                (
                    SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 24)),
                    TurnId::from_uuid(Uuid::from_u128(seed + 25)),
                )
            },
        )
        .await?;
    let PrepareInitialModelCallOutcome::TargetUnavailable(_) = outcome else {
        panic!("the empty target catalog must fail the delegated initial call")
    };
    let materialized: DelegatedResultMaterializationEvidence = sqlx::query_as(
        "SELECT result.outcome_kind, result.content_text,
                event.reason_kind, event.provenance_kind,
                lifecycle.terminal_disposition_kind,
                (SELECT count(*) FROM delegation_update_outbox_event AS update
                  WHERE update.result_spawning_request_id = $1
                    AND update.session_id = $2) AS parent_update_count,
                (SELECT count(*) FROM delegation_wake_outbox_event AS wake
                  WHERE wake.result_spawning_request_id = $1
                    AND wake.session_id = $2) AS parent_wake_count
           FROM session_child_result AS result
           JOIN session_delegation_event AS event
             ON event.spawning_tool_request_id = result.spawning_tool_request_id
            AND event.event_ordinal = result.event_ordinal
           JOIN turn_lifecycle AS lifecycle
             ON lifecycle.turn_id = $3 AND lifecycle.session_id = $4
          WHERE result.spawning_tool_request_id = $1",
    )
    .bind(spawning_request.into_uuid())
    .bind(parent.into_uuid())
    .bind(child_turn.into_uuid())
    .bind(child.into_uuid())
    .fetch_one(&pool)
    .await?;

    assert_eq!(materialized.outcome_kind, "child_failed");
    assert_eq!(materialized.content_text, None);
    assert_eq!(materialized.reason_kind, "child_execution_failed");
    assert_eq!(materialized.provenance_kind, "child_turn");
    assert_eq!(materialized.terminal_disposition_kind, "failed");
    assert_eq!(materialized.parent_update_count, 1);
    assert_eq!(materialized.parent_wake_count, 1);

    pool.close().await;
    drop(container);
    Ok(())
}

/// S17: reconciliation-required delegated work remains unresolved
/// relationship work and cannot publish a child result, parent update, or wake.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s17_delegated_reconciliation_withholds_result_and_parent_delivery()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0xd600;
    let (fixture, repository, authorized) = authorize_checkpointed_model_call(&pool, seed).await?;
    let selection = DirectModelSelection::from_uuid(Uuid::from_u128(seed + 5));
    let (parent, spawning_request) = attach_delegation_relationship_fixture(
        &pool,
        fixture.session,
        fixture.turn,
        selection,
        seed + 0x100,
    )
    .await?;
    sqlx::query(
        "ALTER TABLE turn_lifecycle
         DISABLE TRIGGER turn_lifecycle_requires_typed_origin",
    )
    .execute(&pool)
    .await?;
    SubmitInputRepository::new(pool.clone())
        .handle(
            input_with_delivery(
                seed + 30,
                seed + 1,
                "interrupt the ambiguous delegated call",
                DeliveryRequest::Interrupt {
                    expected_active_turn: fixture.turn,
                    descendant_scope: DescendantTerminationScope::ParentAlone,
                    configuration: input_choices(1, ModelSelectionOverride::UseSessionDefault),
                },
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(seed + 31)),
            Some(TurnId::from_uuid(Uuid::from_u128(seed + 32))),
        )
        .await?;
    let observation = authorized
        .observation_correlation()
        .bind_terminal_observation(ModelCallTerminalObservation::Ambiguous);
    let terminal = repository
        .apply_terminal_observation(
            fixture.session,
            observation,
            ModelCallTerminalIdentities::Ambiguous(
                signalbox_domain::AmbiguousModelCallTurnIdentities::new(
                    ContextFrontierId::from_uuid(Uuid::from_u128(seed + 33)),
                ),
            ),
            |_| panic!("the delegated fixture has no pending steering"),
        )
        .await?;
    sqlx::query(
        "ALTER TABLE turn_lifecycle
         ENABLE TRIGGER turn_lifecycle_requires_typed_origin",
    )
    .execute(&pool)
    .await?;
    let evidence: (i64, i64, i64, i64, i64) = sqlx::query_as(
        "SELECT
            (SELECT count(*) FROM turn_lifecycle
              WHERE session_id = $1 AND turn_id = $2
                AND state_kind = 'terminal'
                AND terminal_disposition_kind = 'reconciliation_required'),
            (SELECT count(*) FROM session_child_result
              WHERE spawning_tool_request_id = $3),
            (SELECT count(*) FROM session_delegation_event
              WHERE spawning_tool_request_id = $3
                AND event_kind = 'outcome_recorded'),
            (SELECT count(*) FROM delegation_update_outbox_event
              WHERE session_id = $4
                AND result_spawning_request_id = $3),
            (SELECT count(*) FROM delegation_wake_outbox_event
              WHERE session_id = $4
                AND result_spawning_request_id = $3)",
    )
    .bind(fixture.session.into_uuid())
    .bind(fixture.turn.into_uuid())
    .bind(spawning_request.into_uuid())
    .bind(parent.into_uuid())
    .fetch_one(&pool)
    .await?;

    assert!(matches!(
        terminal,
        ModelCallTerminalOutcome::ReconciliationRequired(_)
    ));
    assert_eq!(evidence, (1, 0, 0, 0, 0));

    pool.close().await;
    drop(container);
    Ok(())
}

/// S17: daemon-owned reconciliation of an ambiguous delegated
/// initial task atomically publishes an unavailable child result and wakes its
/// parent while retaining the reconciliation-required turn boundary.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s17_automatic_delegated_reconciliation_closes_parent_delivery()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0xd680;
    let fixture = authorize_delegated_model_call_fixture(&pool, seed).await?;
    let observation = fixture
        .authorized
        .observation_correlation()
        .bind_terminal_observation(ModelCallTerminalObservation::Ambiguous);
    let parked = fixture
        .repository
        .apply_terminal_observation(
            fixture.child,
            observation,
            ModelCallTerminalIdentities::Ambiguous(AmbiguousModelCallTurnIdentities::new(
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 30)),
            )),
            |_| panic!("the delegated recovery fixture has no pending steering"),
        )
        .await?;
    let recovery = PostgresAutomaticReconciliationRepository::new(pool.clone());
    let batch = recovery.claim_due().await?;
    let claimed = batch.claimed()[0];
    let outcome = recovery.reconcile(claimed).await?;
    let evidence: (
        String,
        String,
        String,
        String,
        Option<String>,
        i64,
        i64,
        i64,
    ) = sqlx::query_as(
        "SELECT lifecycle.terminal_disposition_kind,
                    result.outcome_kind,
                    event.reason_kind,
                    automatic.state_kind,
                    result.content_text,
                    (SELECT count(*) FROM delegation_update_outbox_event AS parent_update
                      WHERE parent_update.session_id = $2
                        AND parent_update.result_spawning_request_id = $1),
                    (SELECT count(*) FROM delegation_wake_outbox_event AS parent_wake
                      WHERE parent_wake.session_id = $2
                        AND parent_wake.result_spawning_request_id = $1),
                    (SELECT count(*) FROM turn_terminal_outbox_event AS turn_event
                      WHERE turn_event.disposition_kind = 'reconciliation_required'
                      AND turn_event.session_id = $3
                        AND turn_event.turn_id = $4)
               FROM session_child_result AS result
               JOIN session_delegation_event AS event
                 ON event.spawning_tool_request_id = result.spawning_tool_request_id
                AND event.event_ordinal = result.event_ordinal
               JOIN session_delegation_initial_task AS task
                 ON task.spawning_tool_request_id = result.spawning_tool_request_id
               JOIN turn_lifecycle AS lifecycle
                 ON lifecycle.session_id = task.child_session_id
                AND lifecycle.turn_id = task.turn_id
               JOIN automatic_reconciliation AS automatic
                 ON automatic.session_id = lifecycle.session_id
                AND automatic.turn_id = lifecycle.turn_id
              WHERE result.spawning_tool_request_id = $1",
    )
    .bind(fixture.spawning_request.into_uuid())
    .bind(fixture.parent.into_uuid())
    .bind(fixture.child.into_uuid())
    .bind(claimed.turn().into_uuid())
    .fetch_one(&pool)
    .await?;

    assert!(matches!(
        parked,
        ModelCallTerminalOutcome::AwaitingRecovery(_)
    ));
    assert_eq!(batch.claimed().len(), 1);
    assert_eq!(batch.exhausted(), &[]);
    assert_eq!(claimed.session(), fixture.child);
    assert_eq!(outcome, AutomaticReconciliationOutcome::Reconciled);
    assert_eq!(
        evidence,
        (
            "reconciliation_required".into(),
            "child_failed".into(),
            "child_result_unavailable".into(),
            "reconciled".into(),
            None,
            1,
            1,
            1,
        )
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// S17: a child terminal commit takes the canonical parent
/// session before the child scheduler and relationship, matching peer-message
/// and descendant-cascade lock order.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s17_delegated_terminal_result_locks_parent_before_relationship()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0xd700;
    let fixture = authorize_delegated_model_call_fixture(&pool, seed).await?;
    let mut parent_lock = pool.begin().await?;
    sqlx::query("SELECT session_id FROM session WHERE session_id = $1 FOR NO KEY UPDATE")
        .bind(fixture.parent.into_uuid())
        .execute(&mut *parent_lock)
        .await?;
    let child = fixture.child;
    let spawning_request = fixture.spawning_request;
    let terminal = tokio::spawn(async move {
        let observation = fixture
            .authorized
            .observation_correlation()
            .bind_terminal_observation(ModelCallTerminalObservation::Completed {
                assistant_text: vec![
                    AssistantText::try_new(String::from("ordered result"))
                        .expect("fixture result content is admitted"),
                ],
            });
        fixture
            .repository
            .apply_terminal_observation(
                child,
                observation,
                ModelCallTerminalIdentities::Completed(CompletedModelCallIdentities::new(
                    vec![SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(
                        seed + 30,
                    ))],
                    SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 31)),
                    ContextFrontierId::from_uuid(Uuid::from_u128(seed + 32)),
                )),
                |_| panic!("the delegated fixture has no pending steering"),
            )
            .await
    });
    assert!(
        blocked_backends_reached(&pool, 1).await?,
        "the child terminal commit must wait on the parent session lock"
    );
    sqlx::query("SET LOCAL lock_timeout = '250ms'")
        .execute(&mut *parent_lock)
        .await?;
    let locked_child_scheduler: Uuid = sqlx::query_scalar(
        "SELECT session_id
           FROM session_scheduler
          WHERE session_id = $1
          FOR UPDATE",
    )
    .bind(child.into_uuid())
    .fetch_one(&mut *parent_lock)
    .await?;
    let locked_request: Uuid = sqlx::query_scalar(
        "SELECT spawning_tool_request_id
           FROM session_delegation
          WHERE spawning_tool_request_id = $1
          FOR UPDATE",
    )
    .bind(spawning_request.into_uuid())
    .fetch_one(&mut *parent_lock)
    .await?;
    assert_eq!(locked_child_scheduler, child.into_uuid());
    assert_eq!(locked_request, spawning_request.into_uuid());
    parent_lock.commit().await?;
    let committed = terminal.await??;
    let result_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM session_child_result
          WHERE spawning_tool_request_id = $1",
    )
    .bind(spawning_request.into_uuid())
    .fetch_one(&pool)
    .await?;

    assert!(matches!(committed, ModelCallTerminalOutcome::Completed(_)));
    assert_eq!(result_count, 1);

    pool.close().await;
    drop(container);
    Ok(())
}

/// S18: input submitted to a delegated child takes the canonical
/// parent endpoint before the child session, scheduler, and relationship.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s18_delegated_input_locks_parent_before_child_scheduler() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0xd780;
    let fixture = authorize_delegated_model_call_fixture(&pool, seed).await?;
    let mut parent_lock = pool.begin().await?;
    sqlx::query("SELECT session_id FROM session WHERE session_id = $1 FOR NO KEY UPDATE")
        .bind(fixture.parent.into_uuid())
        .execute(&mut *parent_lock)
        .await?;
    let child = fixture.child;
    let spawning_request = fixture.spawning_request;
    let submit_pool = pool.clone();
    let submitted = tokio::spawn(async move {
        SubmitInputRepository::new(submit_pool)
            .handle(
                SubmitInput::new(
                    DurableCommandId::from_uuid(Uuid::from_u128(seed + 40)),
                    child,
                    UserContent::try_text(String::from("must retain the delegated active turn"))
                        .expect("fixture input content is admitted"),
                    DeliveryRequest::StartWhenNoActiveTurn {
                        configuration: input_choices(1, ModelSelectionOverride::UseSessionDefault),
                    },
                ),
                AcceptedInputId::from_uuid(Uuid::from_u128(seed + 41)),
                Some(TurnId::from_uuid(Uuid::from_u128(seed + 42))),
            )
            .await
    });
    assert!(
        blocked_backends_reached(&pool, 1).await?,
        "delegated child input must wait on the parent endpoint lock"
    );
    sqlx::query("SET LOCAL lock_timeout = '250ms'")
        .execute(&mut *parent_lock)
        .await?;
    let locked_child_session: Uuid = sqlx::query_scalar(
        "SELECT session_id FROM session WHERE session_id = $1 FOR NO KEY UPDATE",
    )
    .bind(child.into_uuid())
    .fetch_one(&mut *parent_lock)
    .await?;
    let locked_child_scheduler: Uuid = sqlx::query_scalar(
        "SELECT session_id FROM session_scheduler WHERE session_id = $1 FOR UPDATE",
    )
    .bind(child.into_uuid())
    .fetch_one(&mut *parent_lock)
    .await?;
    let locked_request: Uuid = sqlx::query_scalar(
        "SELECT spawning_tool_request_id
           FROM session_delegation
          WHERE spawning_tool_request_id = $1
          FOR UPDATE",
    )
    .bind(spawning_request.into_uuid())
    .fetch_one(&mut *parent_lock)
    .await?;
    assert_eq!(locked_child_session, child.into_uuid());
    assert_eq!(locked_child_scheduler, child.into_uuid());
    assert_eq!(locked_request, spawning_request.into_uuid());
    parent_lock.commit().await?;
    let outcome = submitted.await??;
    let SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Rejected(
        SubmitInputRejectedResult::ActiveTurnPresent { .. },
    )) = outcome
    else {
        panic!("delegated input must preserve the child's active turn");
    };

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn delegated_initial_task_activates_without_an_accepted_input() -> Result<(), Box<dyn Error>>
{
    let (container, pool, _database_url) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(prepared(0xd401, 0xd402, direct(0xd403)))
        .await?;
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(prepared(0xd404, 0xd405, direct(0xd450)))
        .await?;
    let parent = Uuid::from_u128(0xd402);
    let child = Uuid::from_u128(0xd405);
    let parent_turn = Uuid::from_u128(0xd406);
    let child_turn = Uuid::from_u128(0xd407);
    let spawning_request = Uuid::from_u128(0xd408);
    let task_entry = Uuid::from_u128(0xd409);
    let task_content = "inspect the delegated activation";
    let selection = Uuid::from_u128(0xd403);
    let mut fixture = pool.begin().await?;
    sqlx::raw_sql(
        "ALTER TABLE session_delegation DISABLE TRIGGER ALL;
         ALTER TABLE session_delegation_initial_task DISABLE TRIGGER ALL;
         ALTER TABLE session_delegation_event DISABLE TRIGGER ALL;",
    )
    .execute(&mut *fixture)
    .await?;
    sqlx::query(
        "INSERT INTO session_delegation_event
            (spawning_tool_request_id, event_ordinal, event_kind,
             provenance_kind, provenance_session_id, provenance_turn_id,
             provenance_tool_request_id)
         VALUES ($1, 1, 'spawned', 'tool_request', $2, $3, $1)",
    )
    .bind(spawning_request)
    .bind(parent)
    .bind(parent_turn)
    .execute(&mut *fixture)
    .await?;
    sqlx::query(
        "INSERT INTO session_delegation
            (spawning_tool_request_id, parent_session_id, parent_turn_id,
             child_session_id, policy_kind)
         VALUES ($1, $2, $3, $4, 'background')",
    )
    .bind(spawning_request)
    .bind(parent)
    .bind(parent_turn)
    .bind(child)
    .execute(&mut *fixture)
    .await?;
    sqlx::query(
        "INSERT INTO turn_lifecycle
            (turn_id, session_id, origin_kind, origin_accepted_input_id,
             acceptance_position, state_kind)
         VALUES ($1, $2, 'delegation', NULL, 1, 'queued')",
    )
    .bind(child_turn)
    .bind(child)
    .execute(&mut *fixture)
    .await?;
    sqlx::query(
        "INSERT INTO session_delegation_initial_task
            (spawning_tool_request_id, child_session_id, turn_id,
             semantic_entry_id, admission_position, defaults_version,
             requested_model_kind, requested_direct_model_selection_id,
             frozen_model_kind, frozen_direct_model_selection_id, task_content)
         VALUES ($1, $2, $3, $4, 1, 1, 'direct', $5, 'direct', $5, $6)",
    )
    .bind(spawning_request)
    .bind(child)
    .bind(child_turn)
    .bind(task_entry)
    .bind(selection)
    .bind(task_content)
    .execute(&mut *fixture)
    .await?;
    sqlx::query(
        "INSERT INTO semantic_transcript_entry
            (source_session_id, semantic_entry_id, payload_kind,
             delegated_task_spawning_tool_request_id)
         VALUES ($1, $2, 'delegated_task', $3)",
    )
    .bind(child)
    .bind(task_entry)
    .bind(spawning_request)
    .execute(&mut *fixture)
    .await?;
    sqlx::raw_sql(
        "ALTER TABLE session_delegation ENABLE TRIGGER ALL;
         ALTER TABLE session_delegation_initial_task ENABLE TRIGGER ALL;
         ALTER TABLE session_delegation_event ENABLE TRIGGER ALL;",
    )
    .execute(&mut *fixture)
    .await?;
    fixture.commit().await?;

    let starting_frontier = ContextFrontierId::from_uuid(Uuid::from_u128(0xd40a));
    let initial_attempt = TurnAttemptId::from_uuid(Uuid::from_u128(0xd40b));
    let activation = StartEligibleTurnRepository::new(pool.clone());
    let preview = activation
        .preview(
            SessionId::from_uuid(child),
            AcceptedInputTurnActivationIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xd40c)),
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xd40d)),
                starting_frontier,
                initial_attempt,
            ),
        )
        .await?
        .expect("the delegated child task has one exact activation preview");
    let provider = ProviderModelIdentity::from_uuid(Uuid::from_u128(0xd40e));
    let targets = ModelTargetCatalog::try_from_definitions([ModelTargetDefinition::new(
        DirectModelSelection::from_uuid(selection),
        ResolvedProviderTarget::naming(provider),
    )])
    .expect("one delegated fixture target forms a catalog");
    let model_calls =
        PostgresModelCallRepository::new(pool.clone(), targets, model_credential_reference());
    let operation = model_calls
        .preview_activation_operation(
            preview.prepared(),
            ModelCallId::from_uuid(Uuid::from_u128(0xd40f)),
        )
        .await?
        .expect("an admitted credential previews the activation operation")
        .render(Box::new([]))?;
    let frontier_entries = operation
        .request()
        .frontier_entries()
        .map(signalbox_domain::SemanticTranscriptEntry::identity)
        .collect::<Vec<_>>();
    assert_eq!(
        frontier_entries,
        [SemanticTranscriptEntryId::from_uuid(task_entry)]
    );
    let CommitActivationPreviewOutcome::Activated(activated) =
        activation.commit_preview(preview).await?
    else {
        panic!("the unchanged delegated child activation must commit");
    };
    record_empty_instruction_manifest(&pool, SessionId::from_uuid(child)).await?;
    let delegated = activated
        .delegated()
        .expect("activation preserves its delegated origin family");
    assert_eq!(delegated.turn(), TurnId::from_uuid(child_turn));
    assert_eq!(
        delegated.spawning_request().map(ToolRequestId::into_uuid),
        Some(spawning_request)
    );
    assert_eq!(
        delegated
            .task()
            .map(signalbox_domain::DelegationContent::as_str),
        Some(task_content)
    );
    assert_eq!(delegated.start().frontier().snapshot(), starting_frontier);
    assert_eq!(activated.accepted_input(), None);

    let submit = SubmitInputRepository::new(pool.clone());
    let blocked_start = submit
        .handle(
            start_input(
                0xd410,
                0xd405,
                "must not steal the delegated slot",
                1,
                ModelSelectionOverride::UseSessionDefault,
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(0xd411)),
            Some(TurnId::from_uuid(Uuid::from_u128(0xd412))),
        )
        .await?;
    let SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Rejected(
        SubmitInputRejectedResult::ActiveTurnPresent { active_turn, .. },
    )) = blocked_start
    else {
        panic!("the delegated task must retain the active slot");
    };
    assert_eq!(active_turn, TurnId::from_uuid(child_turn));

    let safe_point = submit
        .handle(
            input_with_delivery(
                0xd413,
                0xd405,
                "steer the delegated child",
                DeliveryRequest::NextSafePoint {
                    expected_active_turn: TurnId::from_uuid(child_turn),
                },
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(0xd414)),
            None,
        )
        .await?;
    let SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Applied(
        SubmitInputAppliedResult::PendingSteering(steering),
    )) = safe_point
    else {
        panic!("safe-point steering must bind to the delegated task");
    };
    assert_eq!(
        steering.binding().source_turn(),
        TurnId::from_uuid(child_turn)
    );

    let consuming_call = ModelCallId::from_uuid(Uuid::from_u128(0xd418));
    let PrepareInitialModelCallOutcome::Checkpointed(checkpointed_call) = model_calls
        .prepare_initial_call(
            SessionId::from_uuid(child),
            consuming_call,
            FailedModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xd419)),
                ContextFrontierId::from_uuid(Uuid::from_u128(0xd41a)),
            ),
            ContextFrontierId::from_uuid(Uuid::from_u128(0xd41b)),
            |_| {
                (
                    SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xd41c)),
                    TurnId::from_uuid(Uuid::from_u128(0xd41d)),
                )
            },
        )
        .await?
    else {
        panic!("delegated steering must checkpoint with its consuming call");
    };
    assert_eq!(checkpointed_call, consuming_call);
    let PrepareInitialModelCallOutcome::Ready {
        request: reloaded_call,
        ..
    } = model_calls
        .prepare_initial_call(
            SessionId::from_uuid(child),
            ModelCallId::from_uuid(Uuid::from_u128(0xd41e)),
            FailedModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xd41f)),
                ContextFrontierId::from_uuid(Uuid::from_u128(0xd420)),
            ),
            ContextFrontierId::from_uuid(Uuid::from_u128(0xd421)),
            |_| {
                (
                    SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xd422)),
                    TurnId::from_uuid(Uuid::from_u128(0xd423)),
                )
            },
        )
        .await?
    else {
        panic!("delegated consumed steering must reload its prepared call");
    };
    assert_eq!(reloaded_call.call().id(), consuming_call);

    let interrupt = submit
        .handle(
            input_with_delivery(
                0xd415,
                0xd405,
                "interrupt the delegated child",
                DeliveryRequest::Interrupt {
                    expected_active_turn: TurnId::from_uuid(child_turn),
                    descendant_scope: DescendantTerminationScope::ParentAlone,
                    configuration: input_choices(1, ModelSelectionOverride::UseSessionDefault),
                },
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(0xd416)),
            Some(TurnId::from_uuid(Uuid::from_u128(0xd417))),
        )
        .await?;
    let SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Applied(
        SubmitInputAppliedResult::TurnOrigin(origin),
    )) = interrupt
    else {
        panic!("the delegated task must accept a correlated interrupt");
    };
    assert!(origin.applied_interrupt().is_some());

    pool.close().await;
    drop(container);
    Ok(())
}

/// S03 / S10: the Postgres safety-net sweep finds durable
/// queued work and resumable tool batches while excluding unrelated active
/// model work.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s03_postgres_sweep_reconstructs_only_candidate_sessions() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(prepared(0x389, 0x789, direct(0x889)))
        .await?;
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(prepared(0x38a, 0x78a, direct(0x88a)))
        .await?;
    let queued_session = SessionId::from_uuid(Uuid::from_u128(0x789));
    let active_session = SessionId::from_uuid(Uuid::from_u128(0x78a));
    SubmitInputRepository::new(pool.clone())
        .handle(
            start_input(
                0x38b,
                0x789,
                "queued sweep candidate",
                1,
                ModelSelectionOverride::UseSessionDefault,
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(0x989)),
            Some(TurnId::from_uuid(Uuid::from_u128(0xa89))),
        )
        .await?;
    SubmitInputRepository::new(pool.clone())
        .handle(
            start_input(
                0x38c,
                0x78a,
                "active sweep exclusion",
                1,
                ModelSelectionOverride::UseSessionDefault,
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(0x98a)),
            Some(TurnId::from_uuid(Uuid::from_u128(0xa8a))),
        )
        .await?;
    let mut activation = StartEligibleTurnService::new(
        FixedStartEligibleTurnIds::new(
            [SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xd8a))],
            [ContextFrontierId::from_uuid(Uuid::from_u128(0xe8a))],
            [TurnAttemptId::from_uuid(Uuid::from_u128(0xb8a))],
        ),
        StartEligibleTurnRepository::new(pool.clone()),
    );
    assert!(matches!(
        activation.execute(active_session).await?,
        StartEligibleTurnOutcome::Activated(_)
    ));
    let tool_seed = 0x7900;
    let (tool_fixture, _, _, tool_request) =
        checkpoint_confirmed_tool_round(&pool, tool_seed, "current_time", "{}").await?;
    PostgresToolLoopRepository::new(pool.clone())
        .decide(
            DecideToolRequest::try_new(
                DurableCommandId::from_uuid(Uuid::from_u128(tool_seed + 24)),
                tool_request,
                ToolApprovalDecision::Approve,
            )
            .expect("fixture decision command is valid"),
            || TurnAttemptId::from_uuid(Uuid::from_u128(tool_seed + 23)),
        )
        .await?;

    let mut sweep = PostgresEligibilitySweep::new(pool.clone());
    let (candidates, _dispatch_starts, continuation) = EligibilitySweep::find_sessions(&mut sweep)
        .await?
        .into_parts();
    assert!(!continuation);
    let queued_index_count = sqlx::query_scalar::<_, i64>(
        "SELECT count(*)
           FROM pg_indexes
          WHERE schemaname = current_schema()
            AND tablename = 'turn_lifecycle'
            AND indexname = 'turn_lifecycle_queued_by_session'",
    )
    .fetch_one(&pool)
    .await?;

    assert_eq!(candidates, vec![queued_session, tool_fixture.session]);
    assert_eq!(
        PostgresToolLoopRepository::new(pool.clone())
            .find_resumable_turn(tool_fixture.session)
            .await?,
        Some(tool_fixture.turn)
    );
    assert_eq!(queued_index_count, 1);

    pool.close().await;
    drop(container);
    Ok(())
}

/// S17: a foreground result remains discoverable by the durable
/// reconciliation sweep after its best-effort same-process nudge is lost.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s17_foreground_delegation_result_is_a_durable_sweep_candidate()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let session_uuid = insert_outbox_session_fixture(&pool, 0xa900).await?;
    let turn_uuid = Uuid::from_u128(0xa901);
    let awaiting_request_uuid = Uuid::from_u128(0xa902);
    let spawning_request_uuid = Uuid::from_u128(0xa903);
    let child_uuid = Uuid::from_u128(0xa904);
    let starting_frontier_uuid = Uuid::from_u128(0xa905);
    let producing_call_uuid = Uuid::from_u128(0xa906);
    let origin_input_uuid = Uuid::from_u128(0xa907);

    sqlx::raw_sql(
        "ALTER TABLE turn_lifecycle DISABLE TRIGGER ALL;
         ALTER TABLE session_delegation_wait DISABLE TRIGGER ALL;
         ALTER TABLE session_child_result_delivery DISABLE TRIGGER ALL;",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO session_delegation_wait
            (awaiting_tool_request_id, spawning_tool_request_id,
             parent_session_id, parent_turn_id, child_session_id, wait_mode)
         VALUES ($1, $2, $3, $4, $5, 'foreground')",
    )
    .bind(awaiting_request_uuid)
    .bind(spawning_request_uuid)
    .bind(session_uuid)
    .bind(turn_uuid)
    .bind(child_uuid)
    .execute(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO session_child_result_delivery
            (awaiting_tool_request_id, spawning_tool_request_id,
             parent_session_id, delivery_sequence, delivery_kind)
         VALUES ($1, $2, $3, NULL, NULL)",
    )
    .bind(awaiting_request_uuid)
    .bind(spawning_request_uuid)
    .bind(session_uuid)
    .execute(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO turn_lifecycle
            (turn_id, session_id, origin_accepted_input_id,
             acceptance_position, state_kind, start_lineage_kind,
             starting_frontier_id, active_phase_kind,
             child_wait_request_id, active_tool_round_call_id, origin_kind)
         VALUES ($1, $2, $3, 1, 'active', 'first_in_session', $4,
                 'awaiting_child', $5, $6, 'accepted_input')",
    )
    .bind(turn_uuid)
    .bind(session_uuid)
    .bind(origin_input_uuid)
    .bind(starting_frontier_uuid)
    .bind(awaiting_request_uuid)
    .bind(producing_call_uuid)
    .execute(&pool)
    .await?;

    let (candidates, _dispatch_starts, continuation) = PostgresEligibilitySweep::new(pool.clone())
        .find_sessions()
        .await?
        .into_parts();
    assert!(!continuation);
    assert_eq!(candidates, vec![SessionId::from_uuid(session_uuid)]);

    pool.close().await;
    drop(container);
    Ok(())
}

/// S17: a parent-only interrupt closes a foreground
/// child wait without fabricating a child result or requiring cascade output.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s17_parent_only_interrupt_closes_foreground_wait_without_result()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0xaa00;
    let (fixture, spawning_request, awaiting_request) =
        checkpoint_foreground_child_wait_without_result(&pool, seed).await?;
    let successor = TurnId::from_uuid(Uuid::from_u128(seed + 0xf2));
    let outcome = SubmitInputRepository::new(pool.clone())
        .handle(
            input_with_delivery(
                seed + 0xf0,
                seed + 1,
                "interrupt only the foreground-waiting parent",
                DeliveryRequest::Interrupt {
                    expected_active_turn: fixture.turn,
                    descendant_scope: DescendantTerminationScope::ParentAlone,
                    configuration: input_choices(1, ModelSelectionOverride::UseSessionDefault),
                },
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(seed + 0xf1)),
            Some(successor),
        )
        .await?;
    let evidence: (i64, i64, i64) = sqlx::query_as(
        "SELECT
            (SELECT count(*) FROM semantic_transcript_entry
              WHERE source_session_id = $1
                AND payload_kind = 'tool_closed_by_turn_end'
                AND tool_result_request_id = $2),
            (SELECT count(*) FROM semantic_transcript_entry
              WHERE source_session_id = $1
                AND payload_kind = 'delegation_result'
                AND delegation_result_spawning_tool_request_id = $3),
            (SELECT count(*) FROM session_child_result
              WHERE spawning_tool_request_id = $3)",
    )
    .bind(fixture.session.into_uuid())
    .bind(awaiting_request.into_uuid())
    .bind(spawning_request.into_uuid())
    .fetch_one(&pool)
    .await?;

    assert!(matches!(
        outcome,
        SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Applied(
            SubmitInputAppliedResult::TurnOrigin(_)
        ))
    ));
    assert_eq!(evidence, (1, 0, 0));

    pool.close().await;
    drop(container);
    Ok(())
}

/// S17: a durable foreground result reopens its exact
/// parked tool batch under a fresh continued attempt after restart.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s17_foreground_delegation_result_resumes_parked_tool_batch() -> Result<(), Box<dyn Error>>
{
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0xab00;
    let (fixture, _, _, requests) = checkpoint_confirmed_tool_batch(
        &pool,
        seed,
        &[("spawn_session", "{}"), ("await_session", "{}")],
    )
    .await?;
    let [spawning_request, awaiting_request] = requests.as_slice() else {
        panic!("the foreground fixture has spawn and await requests")
    };
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(prepared(seed + 0x100, seed + 0x101, direct(seed + 5)))
        .await?;
    let child = Uuid::from_u128(seed + 0x101);
    let repository = PostgresToolLoopRepository::new(pool.clone());
    let issuing_attempt = TurnAttemptId::from_uuid(Uuid::from_u128(seed + 0xe0));
    repository
        .decide(
            decide_tool_request(
                DurableCommandId::from_uuid(Uuid::from_u128(seed + 0xd0)),
                *spawning_request,
                ToolApprovalDecision::Approve,
            ),
            || panic!("the first approval does not start execution"),
        )
        .await?;
    repository
        .decide(
            decide_tool_request(
                DurableCommandId::from_uuid(Uuid::from_u128(seed + 0xd1)),
                *awaiting_request,
                ToolApprovalDecision::Approve,
            ),
            || issuing_attempt,
        )
        .await?;
    sqlx::raw_sql(
        "ALTER TABLE tool_attempt DISABLE TRIGGER ALL;
         ALTER TABLE turn_attempt DISABLE TRIGGER ALL;
         ALTER TABLE turn_lifecycle DISABLE TRIGGER ALL;",
    )
    .execute(&pool)
    .await?;
    let spawn_attempt = ToolAttemptId::from_uuid(Uuid::from_u128(seed + 0xe1));
    repository
        .prepare_next_attempt(
            fixture.session,
            fixture.turn,
            spawn_attempt,
            ToolEffectClass::EffectFree,
        )
        .await?
        .expect("the approved spawn request prepares one attempt");
    let authorized_spawn = repository
        .authorize_attempt(fixture.session, fixture.turn, spawn_attempt)
        .await?;
    repository
        .commit_observation(authorized_spawn.executor_fence().bind(
            ToolAttemptObservation::Completed {
                result: ToolResultContent::Text(
                    ToolResultText::try_new(child.to_string()).expect("bounded child identity"),
                ),
            },
        ))
        .await?;
    let await_attempt = ToolAttemptId::from_uuid(Uuid::from_u128(seed + 0xe2));
    repository
        .prepare_next_attempt(
            fixture.session,
            fixture.turn,
            await_attempt,
            ToolEffectClass::EffectFree,
        )
        .await?
        .expect("the approved await request prepares one attempt");

    sqlx::raw_sql(
        "ALTER TABLE session_delegation DISABLE TRIGGER ALL;
         ALTER TABLE session_delegation_event DISABLE TRIGGER ALL;
         ALTER TABLE session_delegation_wait DISABLE TRIGGER ALL;
         ALTER TABLE session_child_result DISABLE TRIGGER ALL;
         ALTER TABLE session_child_result_delivery DISABLE TRIGGER ALL;
         ALTER TABLE tool_attempt DISABLE TRIGGER ALL;
         ALTER TABLE turn_attempt DISABLE TRIGGER ALL;
         ALTER TABLE turn_lifecycle DISABLE TRIGGER ALL;",
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
    .bind("delivered foreground result")
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
    sqlx::query(
        "UPDATE turn_attempt
            SET state_kind = 'ended', end_variant = 'without_stop',
                end_disposition = 'yielded_to_durable_wait'
          WHERE turn_attempt_id = $1",
    )
    .bind(issuing_attempt.into_uuid())
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE turn_lifecycle
            SET active_phase_kind = 'awaiting_child', current_attempt_id = NULL,
                child_wait_request_id = $1
          WHERE turn_id = $2",
    )
    .bind(awaiting_request.into_uuid())
    .bind(fixture.turn.into_uuid())
    .execute(&pool)
    .await?;
    sqlx::raw_sql(
        "ALTER TABLE session_delegation ENABLE TRIGGER ALL;
         ALTER TABLE session_delegation_event ENABLE TRIGGER ALL;
         ALTER TABLE session_delegation_wait ENABLE TRIGGER ALL;
         ALTER TABLE session_child_result ENABLE TRIGGER ALL;
         ALTER TABLE session_child_result_delivery ENABLE TRIGGER ALL;
         ALTER TABLE tool_attempt ENABLE TRIGGER ALL;
         ALTER TABLE turn_attempt ENABLE TRIGGER ALL;
         ALTER TABLE turn_lifecycle ENABLE TRIGGER ALL;",
    )
    .execute(&pool)
    .await?;

    let steering = SubmitInputRepository::new(pool.clone())
        .handle(
            input_with_delivery(
                seed + 0xf0,
                seed + 1,
                "steer the foreground child wait",
                DeliveryRequest::NextSafePoint {
                    expected_active_turn: fixture.turn,
                },
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(seed + 0xf1)),
            None,
        )
        .await?;
    assert!(matches!(
        steering,
        SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Applied(
            SubmitInputAppliedResult::PendingSteering(_)
        ))
    ));

    assert_eq!(
        repository.find_resumable_turn(fixture.session).await?,
        Some(fixture.turn)
    );
    let continuation = TurnAttemptId::from_uuid(Uuid::from_u128(seed + 0xe3));
    assert!(
        PostgresToolLoopRepository::new(pool.clone())
            .resume_child_wait(fixture.session, fixture.turn, continuation)
            .await?
    );
    let resumed = repository
        .load_active_batch(fixture.session, fixture.turn)
        .await?
        .expect("the resumed tool batch reconstitutes after restart");
    let signalbox_domain::ToolBatchPhase::Executing { turn_attempt } = resumed.phase() else {
        panic!("the delivered foreground result must resume execution");
    };
    assert_eq!(turn_attempt, continuation);

    pool.close().await;
    drop(container);
    Ok(())
}
