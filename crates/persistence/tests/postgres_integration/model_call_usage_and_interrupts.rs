//! Model call usage evidence, credential references, provider failure causes, and interrupt or stop
//! history.

use crate::*;

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn replayed_provider_reasoning_is_counted_only_without_output_coverage()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    for (offset, compacted, output_tokens) in [
        (0, false, Some(8)),
        (0x100, true, None),
        (0x200, false, None),
    ] {
        let seed = 0x6dc0 + offset;
        let (fixture, repository, authorized) =
            authorize_checkpointed_model_call(&pool, seed).await?;
        let correlation = authorized.observation_correlation();
        let raw =
            r#"{"type":"reasoning","id":"rs_fixture","summary":[],"encrypted_content":"opaque"}"#;
        let reasoning = AssistantResponsePart::ProviderReasoning(
            signalbox_domain::ProviderReasoningItem::try_new(raw.to_string())
                .expect("reasoning fixture"),
        );
        let (observation, entries) = if compacted {
            let block = ProviderCompactionBlock::try_new(String::from(
                r#"{"type":"compaction","content":"summary","encrypted_content":"opaque"}"#,
            ))
            .expect("compaction fixture");
            (
                ModelCallTerminalObservation::CompletedWithProviderCompaction {
                    response: vec![AssistantResponsePart::ProviderCompaction(block), reasoning],
                    retained_input_tokens: 19,
                    retained_output_tokens: 3,
                },
                2,
            )
        } else {
            (
                ModelCallTerminalObservation::CompletedWithProviderReasoning {
                    response: vec![reasoning],
                },
                1,
            )
        };
        let frontier = ContextFrontierId::from_uuid(Uuid::from_u128(seed + 24));
        repository
            .apply_terminal_observation(
                fixture.session,
                correlation.bind_terminal_observation_with_usage(
                    observation,
                    ProviderReportedTokenUsage::unreported()
                        .with_input_tokens(Some(70))
                        .with_output_tokens(output_tokens),
                ),
                ModelCallTerminalIdentities::Completed(CompletedModelCallIdentities::new(
                    (0..entries)
                        .map(|index| {
                            SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 20 + index))
                        })
                        .collect(),
                    SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 23)),
                    frontier,
                )),
                |_| panic!("no pending steering"),
            )
            .await?;
        let reported = repository
            .latest_reported_usage(
                fixture.session,
                correlation.target(),
                FastMode::Disabled,
                compacted,
                frontier,
            )
            .await?
            .expect("provider input usage retained");
        assert_eq!(
            reported.projected_unreported_content_bytes(),
            if compacted || output_tokens.is_some() {
                0
            } else {
                u64::try_from(raw.len())?
            }
        );
    }
    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn completed_provider_reasoning_retains_order_and_projects_a_marker()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0x6da0;
    let (fixture, repository, authorized) = authorize_checkpointed_model_call(&pool, seed).await?;
    let item_json = String::from(
        " { \"encrypted_content\": \"opaque continuation\", \"id\": \"rs_1\", \"type\": \"reasoning\", \"summary\": [] } ",
    );
    let item = signalbox_domain::ProviderReasoningItem::try_new(item_json.clone())
        .expect("the fixture carries a complete reasoning item");
    let reasoning_entry = SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 21));
    let observation = authorized
        .observation_correlation()
        .bind_terminal_observation(
            ModelCallTerminalObservation::CompletedWithProviderReasoning {
                response: vec![
                    AssistantResponsePart::Text(
                        AssistantText::try_new(String::from("before"))
                            .expect("nonempty fixture text"),
                    ),
                    AssistantResponsePart::ProviderReasoning(item),
                    AssistantResponsePart::Text(
                        AssistantText::try_new(String::from("after"))
                            .expect("nonempty fixture text"),
                    ),
                ],
            },
        );
    repository
        .apply_terminal_observation(
            fixture.session,
            observation.clone(),
            ModelCallTerminalIdentities::Completed(CompletedModelCallIdentities::new(
                vec![
                    SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 20)),
                    reasoning_entry,
                    SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 22)),
                ],
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 23)),
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 24)),
            )),
            |_| panic!("the fixture has no pending steering"),
        )
        .await?;
    assert_eq!(
        repository
            .reread_terminal_observation(fixture.session, &observation)
            .await?,
        RetainedModelCallObservationStatus::AlreadyCommitted,
    );
    let retained: (Option<Decimal>, Option<Decimal>) = sqlx::query_as(
        "SELECT retained_input_tokens, retained_output_tokens FROM model_call WHERE model_call_id = $1",
    )
    .bind(fixture.call.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(retained, (None, None));
    let stored: Vec<(String, String, Decimal, Option<Decimal>)> = sqlx::query_as(
        "SELECT payload_kind, assistant_text_value, assistant_response_part_ordinal,
                assistant_response_text_start_bytes
           FROM semantic_transcript_entry WHERE producing_model_call_id = $1
          ORDER BY assistant_response_part_ordinal",
    )
    .bind(fixture.call.into_uuid())
    .fetch_all(&pool)
    .await?;
    assert_eq!(
        stored,
        vec![
            (
                String::from("assistant_text"),
                String::from("before"),
                Decimal::ZERO,
                Some(Decimal::ZERO)
            ),
            (
                String::from("provider_reasoning"),
                item_json,
                Decimal::ONE,
                None
            ),
            (
                String::from("assistant_text"),
                String::from("after"),
                Decimal::from(2),
                Some(Decimal::from(6))
            ),
        ]
    );
    let snapshot = ProcessReadRepository::new(pool.clone())
        .read_transcript(fixture.session)
        .await?
        .expect("the completed turn remains readable");
    assert!(snapshot.entries().iter().any(|entry| matches!(entry,
        ProcessTranscriptEntry::ProviderReasoning { entry, turn, model_call, .. }
            if *entry == reasoning_entry && *turn == fixture.turn && *model_call == fixture.call
    )));

    let later_credential = ModelCallCredentialReference::new("later-primary");
    let producer_target = authorized.observation_correlation().target();
    let targets = ModelTargetCatalog::try_from_definitions([ModelTargetDefinition::new(
        DirectModelSelection::from_uuid(Uuid::from_u128(seed + 5)),
        producer_target,
    )])
    .expect("the later call uses the same configured model");
    let later_repository =
        PostgresModelCallRepository::new(pool.clone(), targets, later_credential.clone());
    SubmitInputRepository::new(pool.clone())
        .handle(
            start_input(
                seed + 40,
                seed + 1,
                "continue with another credential",
                1,
                ModelSelectionOverride::UseSessionDefault,
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(seed + 41)),
            Some(TurnId::from_uuid(Uuid::from_u128(seed + 42))),
        )
        .await?;
    activate_earliest_queued_turn(
        &pool,
        EarliestQueuedTurnActivation {
            session: fixture.session.into_uuid(),
            origin_entry: Uuid::from_u128(seed + 43),
            starting_frontier: Uuid::from_u128(seed + 44),
            initial_attempt: Uuid::from_u128(seed + 45),
        },
    )
    .await?;
    let later_call = ModelCallId::from_uuid(Uuid::from_u128(seed + 46));
    let outcome = later_repository
        .prepare_initial_call(
            fixture.session,
            later_call,
            FailedModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 47)),
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 48)),
            ),
            ContextFrontierId::from_uuid(Uuid::from_u128(seed + 49)),
            |_| panic!("no pending steering"),
        )
        .await?;
    assert!(
        matches!(outcome, PrepareInitialModelCallOutcome::Checkpointed(call) if call == later_call)
    );
    let PrepareInitialModelCallOutcome::Ready {
        request,
        credential_reference,
        system_prompt,
        tool_entries,
        reasoning_provenance,
        ..
    } = later_repository
        .prepare_initial_call(
            fixture.session,
            ModelCallId::from_uuid(Uuid::from_u128(seed + 50)),
            FailedModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 51)),
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 52)),
            ),
            ContextFrontierId::from_uuid(Uuid::from_u128(seed + 53)),
            |_| panic!("no pending steering"),
        )
        .await?
    else {
        panic!("the checkpoint reloads with producer facts");
    };
    assert_eq!(credential_reference, later_credential);
    assert_eq!(
        reasoning_provenance.as_ref(),
        &[signalbox_application::ProviderReasoningProvenance {
            source: SemanticTranscriptEntryRef::from_source(fixture.session, reasoning_entry),
            producing_call: fixture.call,
            producing_target: producer_target,
            producing_credential: model_credential_reference(),
        }]
    );
    assert!(matches!(signalbox_application::PreparedModelOperation::render(
        (*request).clone(), credential_reference.clone(), system_prompt.clone(), Box::new([]), &tool_entries, &[],
    ), Err(signalbox_application::ModelFrontierRenderingError::MissingOrMismatchedReasoningProvenance { .. })));
    let operation = signalbox_application::PreparedModelOperation::render(
        *request,
        credential_reference,
        system_prompt,
        Box::new([]),
        &tool_entries,
        &reasoning_provenance,
    )
    .expect("producer-qualified reasoning renders");
    assert_eq!(
        operation.reasoning_provenance(),
        reasoning_provenance.as_ref()
    );
    assert!(operation.messages().iter().any(|message| matches!(message,
        signalbox_application::ModelConversationMessage::ProviderReasoning { producing_call, .. } if *producing_call == fixture.call
    )));

    let mut corruption = pool.begin().await?;
    sqlx::query("ALTER TABLE semantic_transcript_entry DISABLE TRIGGER USER")
        .execute(&mut *corruption)
        .await?;
    sqlx::query("UPDATE semantic_transcript_entry SET assistant_text_value = $1 WHERE semantic_entry_id = $2")
        .bind(r#"{"type":"reasoning","id":"rs_1","encrypted_content":null}"#)
        .bind(reasoning_entry.into_uuid()).execute(&mut *corruption).await?;
    sqlx::query("ALTER TABLE semantic_transcript_entry ENABLE TRIGGER USER")
        .execute(&mut *corruption)
        .await?;
    corruption.commit().await?;
    assert!(
        ProcessReadRepository::new(pool.clone())
            .read_transcript(fixture.session)
            .await
            .is_err()
    );

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn steered_completion_accepts_interleaved_provider_reasoning() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0x6db0;
    let (fixture, repository, authorized) = authorize_checkpointed_model_call(&pool, seed).await?;
    let steering_input = AcceptedInputId::from_uuid(Uuid::from_u128(seed + 30));
    SubmitInputRepository::new(pool.clone())
        .handle(
            SubmitInput::new(
                DurableCommandId::from_uuid(Uuid::from_u128(seed + 29)),
                fixture.session,
                UserContent::try_text(String::from("steer after this response"))
                    .expect("valid steering text"),
                DeliveryRequest::NextSafePoint {
                    expected_active_turn: fixture.turn,
                },
            ),
            steering_input,
            None,
        )
        .await?;
    let item = signalbox_domain::ProviderReasoningItem::try_new(String::from(
        r#"{"type":"reasoning","id":"rs_steered","encrypted_content":"opaque"}"#,
    ))
    .expect("the fixture carries encrypted reasoning");
    let successor = TurnId::from_uuid(Uuid::from_u128(seed + 32));
    let observation = authorized
        .observation_correlation()
        .bind_terminal_observation(
            ModelCallTerminalObservation::CompletedWithProviderReasoning {
                response: vec![
                    AssistantResponsePart::Text(
                        AssistantText::try_new(String::from("before"))
                            .expect("nonempty fixture text"),
                    ),
                    AssistantResponsePart::ProviderReasoning(item),
                    AssistantResponsePart::Text(
                        AssistantText::try_new(String::from("after"))
                            .expect("nonempty fixture text"),
                    ),
                ],
            },
        );
    let outcome = repository
        .apply_terminal_observation(
            fixture.session,
            observation,
            ModelCallTerminalIdentities::Completed(CompletedModelCallIdentities::new(
                vec![
                    SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 20)),
                    SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 21)),
                    SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 22)),
                ],
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 23)),
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 24)),
            )),
            |accepted| {
                assert_eq!(accepted, steering_input);
                successor
            },
        )
        .await?;
    let ModelCallTerminalOutcome::Completed(completed) = outcome else {
        panic!("the steered response completes");
    };
    assert_eq!(
        completed.reclassified_pending_steering()[0].turn(),
        successor
    );
    assert!(
        ProcessReadRepository::new(pool.clone())
            .read_transcript(fixture.session)
            .await?
            .is_some()
    );
    pool.close().await;
    drop(container);
    Ok(())
}

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
            FastMode::Disabled,
            false,
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

/// A provider compaction completed before a refusal is still the durable
/// context replacement. The refusal prose is omitted, while the opaque block,
/// retained-iteration usage, and exact terminal frontier commit together.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn refused_response_commits_prior_provider_compaction() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0x6d78;
    let (fixture, repository, authorized) = authorize_checkpointed_model_call(&pool, seed).await?;
    let correlation = authorized.observation_correlation();
    let compaction = ProviderCompactionBlock::try_new(String::from(
        r#"{"type":"compaction","content":"retained summary","encrypted_content":"opaque"}"#,
    ))
    .expect("fixture compaction block is valid");
    let compaction_entry = SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 20));
    let terminal_frontier = ContextFrontierId::from_uuid(Uuid::from_u128(seed + 21));
    let reported_usage = ProviderReportedTokenUsage::unreported()
        .with_input_tokens(Some(81))
        .with_output_tokens(Some(9));
    let observation = correlation.bind_terminal_observation_with_usage(
        ModelCallTerminalObservation::RefusedWithProviderCompaction {
            provider_compaction: vec![compaction.clone()],
            retained_input_tokens: 23,
            retained_output_tokens: 4,
        },
        reported_usage,
    );

    let outcome = repository
        .apply_terminal_observation(
            fixture.session,
            observation.clone(),
            ModelCallTerminalIdentities::Refused(
                RefusedModelCallTurnIdentities::new(terminal_frontier)
                    .with_provider_compaction_entries(vec![compaction_entry]),
            ),
            |_| panic!("the fixture has no pending steering to reclassify"),
        )
        .await?;
    let ModelCallTerminalOutcome::Refused(refused) = outcome else {
        panic!("the compacting refusal must remain refused");
    };
    assert_eq!(refused.provider_compaction_entries().len(), 1);
    assert_eq!(
        repository
            .reread_terminal_observation(fixture.session, &observation)
            .await?,
        RetainedModelCallObservationStatus::AlreadyCommitted
    );

    let durable: (String, Decimal, Decimal, String, Decimal) = sqlx::query_as(
        "SELECT call.terminal_disposition_kind,
                call.retained_input_tokens,
                call.retained_output_tokens,
                entry.assistant_text_value,
                entry.assistant_response_part_ordinal
           FROM model_call AS call
           JOIN semantic_transcript_entry AS entry
             ON entry.source_session_id = call.session_id
            AND entry.producing_model_call_id = call.model_call_id
          WHERE call.model_call_id = $1
            AND entry.semantic_entry_id = $2
            AND entry.payload_kind = 'provider_compaction'",
    )
    .bind(fixture.call.into_uuid())
    .bind(compaction_entry.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(durable.0, "refused");
    assert_eq!(durable.1, Decimal::from(23_u64));
    assert_eq!(durable.2, Decimal::from(4_u64));
    assert_eq!(durable.3, compaction.as_json());
    assert_eq!(durable.4, Decimal::ZERO);

    let retained = repository
        .latest_reported_usage(
            fixture.session,
            correlation.target(),
            FastMode::Disabled,
            true,
            terminal_frontier,
        )
        .await?
        .expect("refused provider compaction remains the latest context baseline");
    assert_eq!(retained.usage(), reported_usage);
    assert_eq!(retained.retained_input_tokens(), Some(23));
    assert_eq!(retained.retained_output_tokens(), Some(4));
    assert!(
        !retained.output_is_retained(),
        "refusal output never enters the next request"
    );
    let disabled = repository
        .latest_reported_usage(
            fixture.session,
            correlation.target(),
            FastMode::Disabled,
            false,
            terminal_frontier,
        )
        .await?
        .expect("aggregate usage remains a conservative fallback");
    assert_eq!(disabled.usage(), reported_usage);
    assert_eq!(disabled.retained_input_tokens(), None);
    assert_eq!(disabled.retained_output_tokens(), None);
    let (eligible, continuation) = PostgresEligibilitySweep::new(pool.clone())
        .find_sessions()
        .await?
        .into_parts();
    assert!(eligible.is_empty());
    assert!(!continuation);

    // Replace the valid compaction suffix to exercise the final-state constraint directly.
    sqlx::query("ALTER TABLE semantic_transcript_entry DISABLE TRIGGER USER")
        .execute(&pool)
        .await?;
    sqlx::query(
        "UPDATE semantic_transcript_entry
            SET payload_kind = 'provider_reasoning', assistant_text_value = $2
          WHERE semantic_entry_id = $1",
    )
    .bind(compaction_entry.into_uuid())
    .bind(r#"{"type":"reasoning","id":"rs_refused","encrypted_content":"opaque"}"#)
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE semantic_transcript_entry ENABLE TRIGGER USER")
        .execute(&pool)
        .await?;
    let invalid_suffix =
        sqlx::query("SELECT assert_turn_lifecycle_final_state_without_steering($1)")
            .bind(fixture.turn.into_uuid())
            .execute(&pool)
            .await
            .expect_err("a refused frontier cannot retain provider reasoning");
    assert_eq!(
        invalid_suffix
            .as_database_error()
            .and_then(|error| error.code()),
        Some("23514".into()),
    );

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn steered_refusal_commits_ordered_provider_compaction_suffix() -> Result<(), Box<dyn Error>>
{
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0x6d7a;
    let (fixture, repository, authorized) = authorize_checkpointed_model_call(&pool, seed).await?;
    let steering_input = AcceptedInputId::from_uuid(Uuid::from_u128(seed + 30));
    let recorded = SubmitInputRepository::new(pool.clone())
        .handle(
            SubmitInput::new(
                DurableCommandId::from_uuid(Uuid::from_u128(seed + 29)),
                fixture.session,
                UserContent::try_text(String::from("steer before compacting refusal"))
                    .expect("fixture steering is valid"),
                DeliveryRequest::NextSafePoint {
                    expected_active_turn: fixture.turn,
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

    let correlation = authorized.observation_correlation();
    let compaction = ProviderCompactionBlock::try_new(String::from(
        r#"{"type":"compaction","content":"steered retained summary"}"#,
    ))
    .expect("fixture compaction block is valid");
    let successor = TurnId::from_uuid(Uuid::from_u128(seed + 32));
    let observation = correlation.bind_terminal_observation_with_usage(
        ModelCallTerminalObservation::RefusedWithProviderCompaction {
            provider_compaction: vec![compaction],
            retained_input_tokens: 31,
            retained_output_tokens: 2,
        },
        ProviderReportedTokenUsage::unreported()
            .with_input_tokens(Some(44))
            .with_output_tokens(Some(2)),
    );
    let outcome = repository
        .apply_terminal_observation(
            fixture.session,
            observation,
            ModelCallTerminalIdentities::Refused(
                RefusedModelCallTurnIdentities::new(ContextFrontierId::from_uuid(Uuid::from_u128(
                    seed + 21,
                )))
                .with_provider_compaction_entries(vec![
                    SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 20)),
                ]),
            ),
            |accepted| {
                assert_eq!(accepted, steering_input);
                successor
            },
        )
        .await?;
    let ModelCallTerminalOutcome::Refused(refused) = outcome else {
        panic!("the steered compacting response must remain refused");
    };
    assert_eq!(refused.reclassified_pending_steering().len(), 1);
    assert_eq!(refused.reclassified_pending_steering()[0].turn(), successor);

    // Replace the valid compaction suffix to exercise the final-state constraint directly.
    sqlx::query("ALTER TABLE semantic_transcript_entry DISABLE TRIGGER USER")
        .execute(&pool)
        .await?;
    sqlx::query(
        "UPDATE semantic_transcript_entry
            SET payload_kind = 'provider_reasoning', assistant_text_value = $2
          WHERE semantic_entry_id = $1",
    )
    .bind(Uuid::from_u128(seed + 20))
    .bind(r#"{"type":"reasoning","id":"rs_refused","encrypted_content":"opaque"}"#)
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE semantic_transcript_entry ENABLE TRIGGER USER")
        .execute(&pool)
        .await?;
    let invalid_suffix = sqlx::query("SELECT assert_steering_turn_terminal_final_state($1)")
        .bind(fixture.turn.into_uuid())
        .execute(&pool)
        .await
        .expect_err("a refused frontier cannot retain provider reasoning");
    assert_eq!(
        invalid_suffix
            .as_database_error()
            .and_then(|error| error.code()),
        Some("23514".into()),
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// The historical mode label does not disqualify usage evidence when both
/// modes resolve to the same effective serving target.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn latest_reported_usage_crosses_fast_modes_for_the_same_effective_target()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0x6d79;
    let (fixture, repository, authorized) = authorize_checkpointed_model_call(&pool, seed).await?;
    let correlation = authorized.observation_correlation();
    let compaction = ProviderCompactionBlock::try_new(String::from(
        r#"{"type":"compaction","content":"base summary","encrypted_content":"opaque"}"#,
    ))
    .expect("fixture compaction block is valid");
    let terminal_frontier = ContextFrontierId::from_uuid(Uuid::from_u128(seed + 22));
    let observation = correlation.bind_terminal_observation_with_usage(
        ModelCallTerminalObservation::CompletedWithProviderCompaction {
            response: vec![AssistantResponsePart::ProviderCompaction(compaction)],
            retained_input_tokens: 19,
            retained_output_tokens: 3,
        },
        ProviderReportedTokenUsage::unreported()
            .with_input_tokens(Some(70))
            .with_output_tokens(Some(3)),
    );
    repository
        .apply_terminal_observation(
            fixture.session,
            observation,
            ModelCallTerminalIdentities::Completed(CompletedModelCallIdentities::new(
                vec![SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(
                    seed + 20,
                ))],
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 21)),
                terminal_frontier,
            )),
            |_| panic!("the fixture has no pending steering to reclassify"),
        )
        .await?;

    assert!(
        repository
            .latest_reported_usage(
                fixture.session,
                correlation.target(),
                FastMode::Disabled,
                true,
                terminal_frontier,
            )
            .await?
            .is_some()
    );
    assert!(
        repository
            .latest_reported_usage(
                fixture.session,
                correlation.target(),
                FastMode::Enabled,
                true,
                terminal_frontier,
            )
            .await?
            .is_some(),
        "the historical fast-mode spelling cannot hide same-target usage evidence"
    );
    let (eligible, continuation) = PostgresEligibilitySweep::new(pool.clone())
        .find_sessions()
        .await?
        .into_parts();
    assert!(eligible.is_empty());
    assert!(!continuation);

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn effective_target_baseline_rejects_changed_alternate_mapping() -> Result<(), Box<dyn Error>>
{
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0x6d7a;
    let session = SessionId::from_uuid(Uuid::from_u128(seed + 1));
    let turn = TurnId::from_uuid(Uuid::from_u128(seed + 2));
    let attempt = TurnAttemptId::from_uuid(Uuid::from_u128(seed + 3));
    let call = ModelCallId::from_uuid(Uuid::from_u128(seed + 4));
    let selection = DirectModelSelection::from_uuid(Uuid::from_u128(seed + 5));
    let selected_target =
        ResolvedProviderTarget::naming(ProviderModelIdentity::from_uuid(Uuid::from_u128(seed + 6)));
    let old_fast_target = ResolvedProviderTarget::naming(ProviderModelIdentity::from_uuid(
        Uuid::from_u128(seed + 30),
    ));
    let new_fast_target = ResolvedProviderTarget::naming(ProviderModelIdentity::from_uuid(
        Uuid::from_u128(seed + 31),
    ));

    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(prepared_with_fast_target(
            seed + 7,
            seed + 1,
            selection,
            old_fast_target,
        ))
        .await?;
    SubmitInputRepository::new(pool.clone())
        .handle(
            start_input(
                seed + 8,
                seed + 1,
                "alternate target baseline",
                1,
                ModelSelectionOverride::UseSessionDefault,
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(seed + 9)),
            Some(turn),
        )
        .await?;
    activate_earliest_queued_turn(
        &pool,
        EarliestQueuedTurnActivation {
            session: session.into_uuid(),
            origin_entry: Uuid::from_u128(seed + 10),
            starting_frontier: Uuid::from_u128(seed + 11),
            initial_attempt: attempt.into_uuid(),
        },
    )
    .await?;

    let targets = ModelTargetCatalog::try_from_definitions([ModelTargetDefinition::new(
        selection,
        selected_target,
    )])
    .expect("one mapped-fast fixture target forms a catalog");
    let old_families = ModelCredentialFamilyCatalog::try_new([
        (selected_target, Arc::<str>::from("test-model-family"), None),
        (old_fast_target, Arc::<str>::from("test-model-family"), None),
    ])
    .and_then(|catalog| catalog.with_fast_targets([(selected_target, old_fast_target)]))
    .expect("the original alternate target has a credential family");
    let repository = PostgresModelCallRepository::new(
        pool.clone(),
        targets.clone(),
        model_credential_reference(),
    )
    .with_session_credentials(old_families);
    assert!(matches!(
        repository
            .prepare_initial_call(
                session,
                call,
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
            .await?,
        PrepareInitialModelCallOutcome::Checkpointed(checkpointed) if checkpointed == call
    ));
    let AuthorizeModelCallOutcome::Authorized(authorized) =
        repository.authorize_send(session, call).await?
    else {
        panic!("the mapped-fast fixture call authorizes")
    };
    let stored_effective_target: Uuid = sqlx::query_scalar(
        "SELECT effective_provider_model_identity_id
           FROM model_call
          WHERE model_call_id = $1",
    )
    .bind(call.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        stored_effective_target,
        old_fast_target.identity().into_uuid()
    );

    let compaction = ProviderCompactionBlock::try_new(String::from(
        r#"{"type":"compaction","content":"old fast summary","encrypted_content":"opaque"}"#,
    ))
    .expect("fixture compaction block is valid");
    let terminal_frontier = ContextFrontierId::from_uuid(Uuid::from_u128(seed + 22));
    let observation = authorized
        .observation_correlation()
        .bind_terminal_observation_with_usage(
            ModelCallTerminalObservation::CompletedWithProviderCompaction {
                response: vec![AssistantResponsePart::ProviderCompaction(compaction)],
                retained_input_tokens: 19,
                retained_output_tokens: 3,
            },
            ProviderReportedTokenUsage::unreported()
                .with_input_tokens(Some(70))
                .with_output_tokens(Some(3)),
        );
    repository
        .apply_terminal_observation(
            session,
            observation,
            ModelCallTerminalIdentities::Completed(CompletedModelCallIdentities::new(
                vec![SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(
                    seed + 20,
                ))],
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 21)),
                terminal_frontier,
            )),
            |_| panic!("the fixture has no pending steering to reclassify"),
        )
        .await?;

    let equivalent_selected_target = ResolvedProviderTarget::naming(
        ProviderModelIdentity::from_uuid(Uuid::from_u128(seed + 32)),
    );
    let equivalent_families = ModelCredentialFamilyCatalog::try_new([
        (selected_target, Arc::<str>::from("test-model-family"), None),
        (
            equivalent_selected_target,
            Arc::<str>::from("test-model-family"),
            None,
        ),
        (old_fast_target, Arc::<str>::from("test-model-family"), None),
    ])
    .and_then(|catalog| {
        catalog.with_fast_targets([
            (selected_target, old_fast_target),
            (equivalent_selected_target, old_fast_target),
        ])
    })
    .expect("both selections share one effective target");
    let equivalent_selection = PostgresModelCallRepository::new(
        pool.clone(),
        targets.clone(),
        model_credential_reference(),
    )
    .with_session_credentials(equivalent_families);
    assert!(
        equivalent_selection
            .latest_reported_usage(
                session,
                equivalent_selected_target,
                FastMode::Enabled,
                true,
                terminal_frontier,
            )
            .await?
            .is_some(),
        "a different selection that maps to the same serving target reuses the baseline"
    );

    let new_families = ModelCredentialFamilyCatalog::try_new([
        (selected_target, Arc::<str>::from("test-model-family"), None),
        (new_fast_target, Arc::<str>::from("test-model-family"), None),
    ])
    .and_then(|catalog| catalog.with_fast_targets([(selected_target, new_fast_target)]))
    .expect("the replacement alternate target has a credential family");
    let restarted =
        PostgresModelCallRepository::new(pool.clone(), targets, model_credential_reference())
            .with_session_credentials(new_families);
    assert!(
        restarted
            .latest_reported_usage(
                session,
                selected_target,
                FastMode::Enabled,
                true,
                terminal_frontier,
            )
            .await?
            .is_none(),
        "a replacement alternate target cannot reuse the old serving target's baseline"
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// A prepared call refreshes its serving-target attribution when current
/// configuration does not contradict the frozen credential family or the
/// headroom limits that admitted it.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn effective_target_authorization_records_changed_mapping_after_restart()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0x6d79;
    let session = SessionId::from_uuid(Uuid::from_u128(seed + 1));
    let turn = TurnId::from_uuid(Uuid::from_u128(seed + 2));
    let attempt = TurnAttemptId::from_uuid(Uuid::from_u128(seed + 3));
    let call = ModelCallId::from_uuid(Uuid::from_u128(seed + 4));
    let selection = DirectModelSelection::from_uuid(Uuid::from_u128(seed + 5));
    let selected_target =
        ResolvedProviderTarget::naming(ProviderModelIdentity::from_uuid(Uuid::from_u128(seed + 6)));
    let old_fast_target = ResolvedProviderTarget::naming(ProviderModelIdentity::from_uuid(
        Uuid::from_u128(seed + 30),
    ));
    let new_fast_target = ResolvedProviderTarget::naming(ProviderModelIdentity::from_uuid(
        Uuid::from_u128(seed + 31),
    ));

    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(prepared_with_fast_target(
            seed + 7,
            seed + 1,
            selection,
            old_fast_target,
        ))
        .await?;
    SubmitInputRepository::new(pool.clone())
        .handle(
            start_input(
                seed + 8,
                seed + 1,
                "prepared alternate target",
                1,
                ModelSelectionOverride::UseSessionDefault,
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(seed + 9)),
            Some(turn),
        )
        .await?;
    activate_earliest_queued_turn(
        &pool,
        EarliestQueuedTurnActivation {
            session: session.into_uuid(),
            origin_entry: Uuid::from_u128(seed + 10),
            starting_frontier: Uuid::from_u128(seed + 11),
            initial_attempt: attempt.into_uuid(),
        },
    )
    .await?;

    let targets = ModelTargetCatalog::try_from_definitions([ModelTargetDefinition::new(
        selection,
        selected_target,
    )])
    .expect("one mapped-fast fixture target forms a catalog");
    let old_families = ModelCredentialFamilyCatalog::try_new([
        (selected_target, Arc::<str>::from("test-model-family"), None),
        (old_fast_target, Arc::<str>::from("test-model-family"), None),
    ])
    .and_then(|catalog| catalog.with_fast_targets([(selected_target, old_fast_target)]))
    .expect("the original alternate target has a credential family");
    let repository = PostgresModelCallRepository::new(
        pool.clone(),
        targets.clone(),
        model_credential_reference(),
    )
    .with_session_credentials(old_families)
    .with_continuation_usage_limits([ToolContinuationUsageLimit::new(
        selected_target,
        FastMode::Enabled,
        10,
        100,
    )]);
    assert!(matches!(
        repository
            .prepare_initial_call(
                session,
                call,
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
            .await?,
        PrepareInitialModelCallOutcome::Checkpointed(checkpointed) if checkpointed == call
    ));
    let prepared_evidence: (
        Option<String>,
        Option<Decimal>,
        Option<Decimal>,
        Option<bool>,
    ) = sqlx::query_as(
        "SELECT prepared_credential_model_family,
                    prepared_max_output_tokens,
                    prepared_context_window_tokens,
                    prepared_provider_compaction_replay
               FROM model_call
              WHERE model_call_id = $1",
    )
    .bind(call.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        prepared_evidence,
        (
            Some("test-model-family".to_owned()),
            Some(Decimal::from(10)),
            Some(Decimal::from(100)),
            Some(false),
        )
    );

    let mutation = sqlx::query(
        "UPDATE model_call
            SET effective_provider_model_identity_id = $1
          WHERE model_call_id = $2",
    )
    .bind(new_fast_target.identity().into_uuid())
    .bind(call.into_uuid())
    .execute(&pool)
    .await
    .expect_err("a prepared call's effective target is immutable");
    assert_eq!(
        mutation.as_database_error().and_then(|error| error.code()),
        Some("23514".into())
    );
    let mutation = sqlx::query(
        "UPDATE model_call
            SET prepared_context_window_tokens = 99
          WHERE model_call_id = $1",
    )
    .bind(call.into_uuid())
    .execute(&pool)
    .await
    .expect_err("a prepared call's headroom evidence is immutable");
    assert_eq!(
        mutation.as_database_error().and_then(|error| error.code()),
        Some("23514".into())
    );
    let mut constraint_transaction = pool.begin().await?;
    sqlx::query("ALTER TABLE model_call DISABLE TRIGGER model_call_changes_are_guarded")
        .execute(&mut *constraint_transaction)
        .await?;
    let partial_limit = sqlx::query(
        "UPDATE model_call
            SET prepared_max_output_tokens = NULL
          WHERE model_call_id = $1",
    )
    .bind(call.into_uuid())
    .execute(&mut *constraint_transaction)
    .await
    .expect_err("partial prepared limit evidence violates its completeness constraint");
    assert_eq!(
        partial_limit
            .as_database_error()
            .and_then(|error| error.constraint()),
        Some("model_call_prepared_limit_evidence_complete")
    );
    constraint_transaction.rollback().await?;

    let changed_same_target_family = ModelCredentialFamilyCatalog::try_new([
        (selected_target, Arc::<str>::from("test-model-family"), None),
        (
            old_fast_target,
            Arc::<str>::from("other-model-family"),
            None,
        ),
    ])
    .and_then(|catalog| catalog.with_fast_targets([(selected_target, old_fast_target)]))
    .expect("the unchanged target has a different credential family after restart");
    let changed_family = PostgresModelCallRepository::new(
        pool.clone(),
        targets.clone(),
        model_credential_reference(),
    )
    .with_session_credentials(changed_same_target_family)
    .with_continuation_usage_limits([ToolContinuationUsageLimit::new(
        selected_target,
        FastMode::Enabled,
        10,
        100,
    )]);
    assert!(matches!(
        changed_family.authorize_send(session, call).await?,
        AuthorizeModelCallOutcome::NoSend
    ));

    let unchanged_target_families = ModelCredentialFamilyCatalog::try_new([
        (selected_target, Arc::<str>::from("test-model-family"), None),
        (old_fast_target, Arc::<str>::from("test-model-family"), None),
    ])
    .and_then(|catalog| catalog.with_fast_targets([(selected_target, old_fast_target)]))
    .expect("the unchanged target retains its credential family");
    let narrower_same_target = PostgresModelCallRepository::new(
        pool.clone(),
        targets.clone(),
        model_credential_reference(),
    )
    .with_session_credentials(unchanged_target_families.clone())
    .with_continuation_usage_limits([ToolContinuationUsageLimit::new(
        selected_target,
        FastMode::Enabled,
        10,
        50,
    )]);
    assert!(matches!(
        narrower_same_target.authorize_send(session, call).await?,
        AuthorizeModelCallOutcome::NoSend
    ));

    let changed_replay = PostgresModelCallRepository::new(
        pool.clone(),
        targets.clone(),
        model_credential_reference(),
    )
    .with_session_credentials(unchanged_target_families)
    .with_continuation_usage_limits([ToolContinuationUsageLimit::new(
        selected_target,
        FastMode::Enabled,
        10,
        100,
    )
    .with_provider_compaction_replay()]);
    assert!(matches!(
        changed_replay.authorize_send(session, call).await?,
        AuthorizeModelCallOutcome::NoSend
    ));

    let different_families = ModelCredentialFamilyCatalog::try_new([
        (selected_target, Arc::<str>::from("test-model-family"), None),
        (
            new_fast_target,
            Arc::<str>::from("other-model-family"),
            None,
        ),
    ])
    .and_then(|catalog| catalog.with_fast_targets([(selected_target, new_fast_target)]))
    .expect("the replacement alternate target has a distinct credential family");
    let incompatible = PostgresModelCallRepository::new(
        pool.clone(),
        targets.clone(),
        model_credential_reference(),
    )
    .with_session_credentials(different_families);
    assert!(matches!(
        incompatible.authorize_send(session, call).await?,
        AuthorizeModelCallOutcome::NoSend
    ));
    let unchanged: (String, Uuid, String) = sqlx::query_as(
        "SELECT state_kind, effective_provider_model_identity_id, credential_reference
           FROM model_call
          WHERE model_call_id = $1",
    )
    .bind(call.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        unchanged,
        (
            "prepared".to_owned(),
            old_fast_target.identity().into_uuid(),
            "test-model-primary".to_owned(),
        )
    );

    let narrower_families = ModelCredentialFamilyCatalog::try_new([
        (selected_target, Arc::<str>::from("test-model-family"), None),
        (new_fast_target, Arc::<str>::from("test-model-family"), None),
    ])
    .and_then(|catalog| catalog.with_fast_targets([(selected_target, new_fast_target)]))
    .expect("both alternate targets share the fixture credential family");
    let narrower = PostgresModelCallRepository::new(
        pool.clone(),
        targets.clone(),
        model_credential_reference(),
    )
    .with_session_credentials(narrower_families)
    .with_continuation_usage_limits([ToolContinuationUsageLimit::new(
        selected_target,
        FastMode::Enabled,
        10,
        50,
    )]);
    assert!(matches!(
        narrower.authorize_send(session, call).await?,
        AuthorizeModelCallOutcome::NoSend
    ));

    let new_families = ModelCredentialFamilyCatalog::try_new([
        (selected_target, Arc::<str>::from("test-model-family"), None),
        (new_fast_target, Arc::<str>::from("test-model-family"), None),
    ])
    .and_then(|catalog| catalog.with_fast_targets([(selected_target, new_fast_target)]))
    .expect("the replacement alternate target has a credential family");
    let restarted =
        PostgresModelCallRepository::new(pool.clone(), targets, model_credential_reference())
            .with_session_credentials(new_families)
            .with_continuation_usage_limits([ToolContinuationUsageLimit::new(
                selected_target,
                FastMode::Enabled,
                10,
                100,
            )]);
    assert!(matches!(
        restarted.authorize_send(session, call).await?,
        AuthorizeModelCallOutcome::Authorized(_)
    ));
    let durable: (String, Uuid) = sqlx::query_as(
        "SELECT state_kind, effective_provider_model_identity_id
           FROM model_call
          WHERE model_call_id = $1",
    )
    .bind(call.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        durable,
        (
            "in_flight".to_owned(),
            new_fast_target.identity().into_uuid()
        )
    );

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn latest_reported_usage_excludes_unreplayed_provider_compaction_bytes()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0x6d7b;
    let (fixture, repository, authorized) = authorize_checkpointed_model_call(&pool, seed).await?;
    let correlation = authorized.observation_correlation();
    repository
        .apply_terminal_observation(
            fixture.session,
            correlation.bind_terminal_observation_with_usage(
                ModelCallTerminalObservation::Completed {
                    assistant_text: vec![
                        AssistantText::try_new(String::from("reported baseline reply"))
                            .expect("fixture assistant text is valid"),
                    ],
                },
                ProviderReportedTokenUsage::unreported()
                    .with_input_tokens(Some(80))
                    .with_output_tokens(Some(3)),
            ),
            ModelCallTerminalIdentities::Completed(CompletedModelCallIdentities::new(
                vec![SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(
                    seed + 20,
                ))],
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 21)),
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 22)),
            )),
            |_| panic!("the fixture has no pending steering to reclassify"),
        )
        .await?;

    let second_turn = TurnId::from_uuid(Uuid::from_u128(seed + 42));
    SubmitInputRepository::new(pool.clone())
        .handle(
            start_input(
                seed + 40,
                seed + 1,
                "request after another target compacted",
                1,
                ModelSelectionOverride::UseSessionDefault,
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(seed + 41)),
            Some(second_turn),
        )
        .await?;
    activate_earliest_queued_turn(
        &pool,
        EarliestQueuedTurnActivation {
            session: fixture.session.into_uuid(),
            origin_entry: Uuid::from_u128(seed + 43),
            starting_frontier: Uuid::from_u128(seed + 44),
            initial_attempt: Uuid::from_u128(seed + 45),
        },
    )
    .await?;
    let second_call = ModelCallId::from_uuid(Uuid::from_u128(seed + 46));
    assert!(matches!(
        repository
            .prepare_initial_call(
                fixture.session,
                second_call,
                FailedModelCallTurnIdentities::new(
                    SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 47)),
                    ContextFrontierId::from_uuid(Uuid::from_u128(seed + 48)),
                ),
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 49)),
                |_| panic!("the fixture has no pending steering to reclassify"),
            )
            .await?,
        PrepareInitialModelCallOutcome::Checkpointed(checkpointed) if checkpointed == second_call
    ));
    assert!(matches!(
        repository
            .prepare_initial_call(
                fixture.session,
                ModelCallId::from_uuid(Uuid::from_u128(seed + 50)),
                FailedModelCallTurnIdentities::new(
                    SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 51)),
                    ContextFrontierId::from_uuid(Uuid::from_u128(seed + 52)),
                ),
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 53)),
                |_| panic!("the fixture has no pending steering to reclassify"),
            )
            .await?,
        PrepareInitialModelCallOutcome::Ready { .. }
    ));
    let AuthorizeModelCallOutcome::Authorized(second_authorized) = repository
        .authorize_send(fixture.session, second_call)
        .await?
    else {
        panic!("the retained second call authorizes");
    };
    let compaction = ProviderCompactionBlock::try_new(String::from(
        r#"{"type":"compaction","content":"opaque bytes omitted after target switch","encrypted_content":"ciphertext"}"#,
    ))
    .expect("fixture compaction block is valid");
    let compaction_bytes = u64::try_from(compaction.as_json().len())?;
    let terminal_frontier = ContextFrontierId::from_uuid(Uuid::from_u128(seed + 56));
    repository
        .apply_terminal_observation(
            fixture.session,
            second_authorized
                .observation_correlation()
                .bind_terminal_observation(
                    ModelCallTerminalObservation::CompletedWithProviderCompaction {
                        response: vec![AssistantResponsePart::ProviderCompaction(compaction)],
                        retained_input_tokens: 15,
                        retained_output_tokens: 2,
                    },
                ),
            ModelCallTerminalIdentities::Completed(CompletedModelCallIdentities::new(
                vec![SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(
                    seed + 54,
                ))],
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 55)),
                terminal_frontier,
            )),
            |_| panic!("the fixture has no pending steering to reclassify"),
        )
        .await?;

    let replayed = repository
        .latest_reported_usage(
            fixture.session,
            correlation.target(),
            FastMode::Disabled,
            true,
            terminal_frontier,
        )
        .await?
        .expect("the earlier reported call remains the baseline");
    let omitted = repository
        .latest_reported_usage(
            fixture.session,
            correlation.target(),
            FastMode::Disabled,
            false,
            terminal_frontier,
        )
        .await?
        .expect("the earlier reported call remains the baseline");
    assert_eq!(
        replayed.projected_unreported_content_bytes(),
        omitted
            .projected_unreported_content_bytes()
            .saturating_add(compaction_bytes)
    );

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn unreported_tool_round_counts_replayed_provider_reasoning() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0x6e7b;
    let (fixture, repository, authorized) = authorize_checkpointed_model_call(&pool, seed).await?;
    let correlation = authorized.observation_correlation();
    repository
        .apply_terminal_observation(
            fixture.session,
            correlation.bind_terminal_observation_with_usage(
                ModelCallTerminalObservation::Completed {
                    assistant_text: vec![
                        AssistantText::try_new(String::from("reported baseline reply"))
                            .expect("fixture assistant text is valid"),
                    ],
                },
                ProviderReportedTokenUsage::unreported()
                    .with_input_tokens(Some(80))
                    .with_output_tokens(Some(3)),
            ),
            ModelCallTerminalIdentities::Completed(CompletedModelCallIdentities::new(
                vec![SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(
                    seed + 20,
                ))],
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 21)),
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 22)),
            )),
            |_| panic!("the fixture has no pending steering to reclassify"),
        )
        .await?;

    let second_turn = TurnId::from_uuid(Uuid::from_u128(seed + 42));
    SubmitInputRepository::new(pool.clone())
        .handle(
            start_input(
                seed + 40,
                seed + 1,
                "request before unreported tool round",
                1,
                ModelSelectionOverride::UseSessionDefault,
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(seed + 41)),
            Some(second_turn),
        )
        .await?;
    activate_earliest_queued_turn(
        &pool,
        EarliestQueuedTurnActivation {
            session: fixture.session.into_uuid(),
            origin_entry: Uuid::from_u128(seed + 43),
            starting_frontier: Uuid::from_u128(seed + 44),
            initial_attempt: Uuid::from_u128(seed + 45),
        },
    )
    .await?;
    let second_call = ModelCallId::from_uuid(Uuid::from_u128(seed + 46));
    assert!(matches!(
        repository
            .prepare_initial_call(
                fixture.session,
                second_call,
                FailedModelCallTurnIdentities::new(
                    SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 47)),
                    ContextFrontierId::from_uuid(Uuid::from_u128(seed + 48)),
                ),
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 49)),
                |_| panic!("the fixture has no pending steering to reclassify"),
            )
            .await?,
        PrepareInitialModelCallOutcome::Checkpointed(checkpointed) if checkpointed == second_call
    ));
    assert!(matches!(
        repository
            .prepare_initial_call(
                fixture.session,
                ModelCallId::from_uuid(Uuid::from_u128(seed + 50)),
                FailedModelCallTurnIdentities::new(
                    SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 51)),
                    ContextFrontierId::from_uuid(Uuid::from_u128(seed + 52)),
                ),
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 53)),
                |_| panic!("the fixture has no pending steering to reclassify"),
            )
            .await?,
        PrepareInitialModelCallOutcome::Ready { .. }
    ));
    let AuthorizeModelCallOutcome::Authorized(second_authorized) = repository
        .authorize_send(fixture.session, second_call)
        .await?
    else {
        panic!("the retained second call authorizes");
    };
    let raw_reasoning = r#"{"type":"reasoning","id":"rs_unreported","summary":[],"encrypted_content":"opaque continuation"}"#;
    let reasoning = signalbox_domain::ProviderReasoningItem::try_new(String::from(raw_reasoning))
        .expect("complete reasoning fixture");
    let response = ToolUsingAssistantResponse::try_from_parts(vec![
        AssistantResponsePart::ProviderReasoning(reasoning),
        AssistantResponsePart::ToolCall(ToolCallProposal::new(
            ToolName::try_new(String::from("current_time")).expect("fixture tool name"),
            NormalizedToolArguments::try_from_provider_text(String::from("{}"))
                .expect("fixture arguments"),
        )),
    ])
    .expect("response carries a tool call");
    let terminal_frontier = ContextFrontierId::from_uuid(Uuid::from_u128(seed + 57));
    repository
        .apply_terminal_observation(
            fixture.session,
            second_authorized
                .observation_correlation()
                .bind_terminal_observation(ModelCallTerminalObservation::CompletedWithTools {
                    response,
                    retained_input_tokens: None,
                    retained_output_tokens: None,
                }),
            ModelCallTerminalIdentities::ToolRound(ToolRoundModelCallIdentities::new(
                vec![
                    ToolResponsePartIdentity::provider_reasoning(
                        SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 54)),
                    ),
                    ToolResponsePartIdentity::tool_call(
                        SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 55)),
                        ToolRequestId::from_uuid(Uuid::from_u128(seed + 56)),
                        InitialToolApproval::Confirm,
                    ),
                ],
                terminal_frontier,
                None,
            )),
            |_| panic!("no pending steering"),
        )
        .await?;
    let reported = repository
        .latest_reported_usage(
            fixture.session,
            correlation.target(),
            FastMode::Disabled,
            false,
            terminal_frontier,
        )
        .await?
        .expect("the older usage-bearing call remains the baseline");
    assert_eq!(reported.usage().input_tokens(), Some(80));
    assert_eq!(
        reported.projected_unreported_content_bytes(),
        u64::try_from(
            "request before unreported tool round".len()
                + "current_time".len()
                + "{}".len()
                + raw_reasoning.len()
        )?
    );
    pool.close().await;
    drop(container);
    Ok(())
}

/// A successful dedicated compaction call becomes the provider-confirmed
/// baseline until a later ordinary call reports usage. Headroom measures its
/// admitted summary bytes independently of the dedicated call's billed output.
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
            FastMode::Disabled,
            false,
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
    assert!(!retained.output_is_retained());
    assert_eq!(
        retained.projected_unreported_content_bytes(),
        u64::try_from(retained_source_suffix.len() + suffix.len() + 24)?
    );

    let fast_target = ResolvedProviderTarget::naming(ProviderModelIdentity::from_uuid(
        Uuid::from_u128(seed + 0x50),
    ));
    let fast_targets = ModelTargetCatalog::try_from_definitions([ModelTargetDefinition::new(
        DirectModelSelection::from_uuid(Uuid::from_u128(seed + 5)),
        target,
    )])
    .expect("one dedicated-compaction target forms a catalog");
    let equivalent_selected_target = ResolvedProviderTarget::naming(
        ProviderModelIdentity::from_uuid(Uuid::from_u128(seed + 0x51)),
    );
    let equivalent_families = ModelCredentialFamilyCatalog::try_new([
        (target, Arc::<str>::from("test-model-family"), None),
        (
            equivalent_selected_target,
            Arc::<str>::from("test-model-family"),
            None,
        ),
    ])
    .and_then(|catalog| catalog.with_fast_targets([(equivalent_selected_target, target)]))
    .expect("the alternate selection maps to the compaction serving target");
    let equivalent_selection = PostgresModelCallRepository::new(
        pool.clone(),
        fast_targets.clone(),
        model_credential_reference(),
    )
    .with_session_credentials(equivalent_families);
    assert!(
        equivalent_selection
            .latest_reported_usage(
                fixture.session,
                equivalent_selected_target,
                FastMode::Enabled,
                false,
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 0x44)),
            )
            .await?
            .is_some(),
        "a dedicated compaction remains eligible through another selection for its serving target"
    );

    let fast_families = ModelCredentialFamilyCatalog::try_new([
        (target, Arc::<str>::from("test-model-family"), None),
        (fast_target, Arc::<str>::from("test-model-family"), None),
    ])
    .and_then(|catalog| catalog.with_fast_targets([(target, fast_target)]))
    .expect("the replacement fast target shares the fixture credential family");
    let restarted =
        PostgresModelCallRepository::new(pool.clone(), fast_targets, model_credential_reference())
            .with_session_credentials(fast_families);
    assert!(
        restarted
            .latest_reported_usage(
                fixture.session,
                target,
                FastMode::Enabled,
                false,
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 0x44)),
            )
            .await?
            .is_none(),
        "a dedicated compaction from another effective target is not a baseline"
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
            FastMode::Disabled,
            false,
            prospective.prospective_input(),
        )
        .await?
        .expect("the completed call reported input usage");

    assert_eq!(reported.usage(), reported_usage);
    assert!(reported.input_is_retained());
    assert!(reported.output_is_retained());
    assert_eq!(reported.projected_unreported_content_bytes(), 24);

    let encoded_input = r#"[{"type":"message","role":"user","content":"queued preview suffix é"}]"#;
    let rendered = std::collections::BTreeMap::from([
        (
            SemanticTranscriptEntryRef::from_source(
                fixture.session,
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 0x44)),
            ),
            encoded_input.len() as u64,
        ),
        (
            SemanticTranscriptEntryRef::from_source(
                fixture.session,
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 0x20)),
            ),
            // This historical output remains covered by the reported usage.
            127,
        ),
    ]);
    let measured = repository
        .latest_reported_usage(
            fixture.session,
            correlation.target(),
            FastMode::Disabled,
            false,
            signalbox_persistence::model_execution::ProspectiveModelInput::Rendered(&rendered),
        )
        .await?
        .expect("reported baseline");
    assert_eq!(
        measured.projected_unreported_content_bytes(),
        encoded_input.len() as u64
    );

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
async fn automatic_compaction_can_summarize_a_retained_summary_again() -> Result<(), Box<dyn Error>>
{
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
            automatic_for_turn: Some(fixture.turn),
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

    let remaining_source = compaction_repository
        .preview_automatic_range(fixture.session)
        .await?
        .expect("the predecessor retains its terminal suffix");
    let through = remaining_source
        .members()
        .last()
        .expect("a nonempty suffix")
        .position();
    let PrepareContextCompactionOutcome::Prepared(successor) = compaction_repository
        .prepare(PrepareContextCompactionRequest {
            command: DurableCommandId::from_uuid(Uuid::from_u128(seed + 0x38)),
            session: fixture.session,
            requested_through_position: Some(through),
            automatic_for_turn: Some(fixture.turn),
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

    let compacted = compaction_repository
        .preview_automatic_range(fixture.session)
        .await?
        .expect("the compacted frontier remains visible");
    assert_eq!(compacted.members().len(), 1);
    // A fresh repository models a later scheduler pass with no in-memory budget.
    let repeated = ContextCompactionRepository::new(pool.clone())
        .prepare(PrepareContextCompactionRequest {
            command: DurableCommandId::from_uuid(Uuid::now_v7()),
            session: fixture.session,
            requested_through_position: Some(compacted.members()[0].position()),
            automatic_for_turn: Some(fixture.turn),
            defaults_version: SessionConfigurationDefaultsVersion::first(),
            selection: predecessor.selection(),
            target,
            input_includes_cache_tokens: true,
            credential_reference: String::from("successor compaction credential"),
            call: ModelCallId::from_uuid(Uuid::now_v7()),
            compaction: ContextCompactionId::from_uuid(Uuid::now_v7()),
            summary_entry: SemanticTranscriptEntryId::from_uuid(Uuid::now_v7()),
            result_frontier: ContextFrontierId::from_uuid(Uuid::now_v7()),
        })
        .await?;
    let PrepareContextCompactionOutcome::Prepared(repeated) = repeated else {
        panic!("a retained summary can be reduced by another compaction")
    };
    compaction_repository.authorize(&repeated).await?;
    compaction_repository
        .complete(
            &repeated,
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
            FastMode::Disabled,
            false,
            ContextFrontierId::from_uuid(Uuid::from_u128(seed + 0x44)),
        )
        .await?
        .expect("the successor compaction usage becomes the current baseline");

    assert!(!retained.input_is_retained());
    assert_eq!(
        retained.projected_unreported_content_bytes(),
        47,
        "the 17-byte summary and 30-byte appended input remain model-visible"
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
    assert_next_outbox_event_quarantined(
        &pool,
        OutboxRowCorruption::InvalidTerminalEventCorrelation,
    )
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

/// interrupting an issued call atomically records its stop proof and cancellation request; the
/// durable signal resolves, physical cancellation closes the turn with its exact attempt history,
/// and both command and observation replays converge on the recorded outcome.
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

/// ambiguity observed before or after an applied interrupt terminalizes as exact proof-bearing
/// reconciliation, and retained observation and origin rereads recognize the committed closure.
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

/// the stop-request migration keeps each stopping rejection paired with its immutable delivery and
/// admits only a known-failed call as failed post-cancellation provenance.
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
          WHERE proname = 'assert_failed_terminal_execution_before_credential_wait_release'",
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

/// completion and restart can win after a durable stop request without erasing the applied
/// interrupt. Terminal reload accepts the completion race, while restart retains an ambiguous call
/// in proof-bearing terminal reconciliation.
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

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn prepared_call_requires_catalog_facts_only_for_rendered_attachments()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    const FIXTURE_SEED: u128 = 0x129300;
    let seed = FIXTURE_SEED;
    let compacted_digest = BlobDigest::digest(b"compacted attachment");
    let visible_digest = BlobDigest::digest(b"visible attachment");
    let visible_length = 7_u64;
    let mut catalog = pool.begin().await?;
    sqlx::query("INSERT INTO blob_store_binding (store_name, namespace_id) VALUES ('compaction-attachment-fixture', $1)")
        .bind(Uuid::now_v7()).execute(&mut *catalog).await?;
    sqlx::query("INSERT INTO blob (digest, byte_length) VALUES ($1, 1), ($2, $3)")
        .bind(compacted_digest.as_bytes().as_slice())
        .bind(visible_digest.as_bytes().as_slice())
        .bind(Decimal::from(visible_length))
        .execute(&mut *catalog)
        .await?;
    sqlx::query("INSERT INTO blob_replica (digest, store_name, object_key) VALUES ($1, 'compaction-attachment-fixture', 'compacted'), ($2, 'compaction-attachment-fixture', 'visible')")
        .bind(compacted_digest.as_bytes().as_slice()).bind(visible_digest.as_bytes().as_slice())
        .execute(&mut *catalog).await?;
    catalog.commit().await?;
    let (fixture, repository, authorized) =
        authorize_checkpointed_model_call_with_attachment(&pool, seed, Some(compacted_digest))
            .await?;
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

    // Remove only the compacted-away catalog fact to distinguish semantic
    // history authentication from the rendered request's attachment needs.
    let mut corruption = pool.begin().await?;
    sqlx::query("ALTER TABLE blob DISABLE TRIGGER ALL")
        .execute(&mut *corruption)
        .await?;
    sqlx::query("DELETE FROM blob WHERE digest = $1")
        .bind(compacted_digest.as_bytes().as_slice())
        .execute(&mut *corruption)
        .await?;
    sqlx::query("ALTER TABLE blob ENABLE TRIGGER ALL")
        .execute(&mut *corruption)
        .await?;
    corruption.commit().await?;
    let suffix = "content appended after compaction";
    SubmitInputRepository::new(pool.clone())
        .with_attachment_maximum_bytes(FIXTURE_ATTACHMENT_MAXIMUM_BYTES)
        .handle(
            start_input_with_attachment(
                seed + 0x40,
                seed + 1,
                suffix,
                1,
                ModelSelectionOverride::UseSessionDefault,
                Some(visible_digest),
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

    let next_call = ModelCallId::from_uuid(Uuid::now_v7());
    let failure = FailedModelCallTurnIdentities::new(
        SemanticTranscriptEntryId::from_uuid(Uuid::now_v7()),
        ContextFrontierId::from_uuid(Uuid::now_v7()),
    );
    let steering = ContextFrontierId::from_uuid(Uuid::now_v7());
    let checkpoint = repository
        .prepare_initial_call(
            fixture.session,
            next_call,
            failure.clone(),
            steering,
            |_| panic!("no pending steering exists in the fixture"),
        )
        .await?;
    assert!(matches!(
        checkpoint,
        PrepareInitialModelCallOutcome::Checkpointed(_)
    ));
    let ready = repository
        .prepare_initial_call(fixture.session, next_call, failure, steering, |_| {
            panic!("the prepared replay cannot consume steering")
        })
        .await?;
    let PrepareInitialModelCallOutcome::Ready { request, .. } = ready else {
        panic!("the exact rendered attachment inventory can be prepared");
    };
    assert_eq!(request.attachment_byte_length(compacted_digest), None);
    assert_eq!(
        request
            .attachment_byte_length(visible_digest)
            .map(|length| length.get()),
        Some(visible_length)
    );
    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn compacted_attachment_cannot_authorize_blob_read() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    const FIXTURE_SEED: u128 = 0x133400;
    let seed = FIXTURE_SEED;
    let compacted_digest = BlobDigest::digest(b"compacted attachment");
    let visible_digest = BlobDigest::digest(b"visible attachment");
    let visible_length = 7_u64;
    let mut catalog = pool.begin().await?;
    sqlx::query("INSERT INTO blob_store_binding (store_name, namespace_id) VALUES ('compaction-attachment-fixture', $1)")
        .bind(Uuid::now_v7()).execute(&mut *catalog).await?;
    sqlx::query("INSERT INTO blob (digest, byte_length) VALUES ($1, 1), ($2, $3)")
        .bind(compacted_digest.as_bytes().as_slice())
        .bind(visible_digest.as_bytes().as_slice())
        .bind(Decimal::from(visible_length))
        .execute(&mut *catalog)
        .await?;
    sqlx::query("INSERT INTO blob_replica (digest, store_name, object_key) VALUES ($1, 'compaction-attachment-fixture', 'compacted'), ($2, 'compaction-attachment-fixture', 'visible')")
        .bind(compacted_digest.as_bytes().as_slice()).bind(visible_digest.as_bytes().as_slice())
        .execute(&mut *catalog).await?;
    catalog.commit().await?;
    let (fixture, repository, authorized) =
        authorize_checkpointed_model_call_with_attachment(&pool, seed, Some(compacted_digest))
            .await?;
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
        .with_attachment_maximum_bytes(FIXTURE_ATTACHMENT_MAXIMUM_BYTES)
        .handle(
            start_input_with_attachment(
                seed + 0x40,
                seed + 1,
                suffix,
                1,
                ModelSelectionOverride::UseSessionDefault,
                Some(visible_digest),
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

    let next_call = ModelCallId::from_uuid(Uuid::now_v7());
    let failure = FailedModelCallTurnIdentities::new(
        SemanticTranscriptEntryId::from_uuid(Uuid::now_v7()),
        ContextFrontierId::from_uuid(Uuid::now_v7()),
    );
    let steering = ContextFrontierId::from_uuid(Uuid::now_v7());
    let checkpoint = repository
        .prepare_initial_call(
            fixture.session,
            next_call,
            failure.clone(),
            steering,
            |_| panic!("no pending steering exists in the fixture"),
        )
        .await?;
    assert!(matches!(
        checkpoint,
        PrepareInitialModelCallOutcome::Checkpointed(_)
    ));

    let AuthorizeModelCallOutcome::Authorized(authorized) = repository
        .authorize_send(fixture.session, next_call)
        .await?
    else {
        panic!("the compacted call authorizes");
    };
    let request_id = ToolRequestId::from_uuid(Uuid::now_v7());
    let response =
        ToolUsingAssistantResponse::try_from_parts(vec![AssistantResponsePart::ToolCall(
            ToolCallProposal::new(
                ToolName::try_new("blob_read".to_owned()).expect("fixture tool name"),
                NormalizedToolArguments::try_from_provider_text(format!(
                    r#"{{"digest":"{compacted_digest}","offset_bytes":"0","length_bytes":"1"}}"#
                ))
                .expect("fixture blob arguments"),
            ),
        )])
        .expect("fixture tool response");
    repository
        .apply_terminal_observation(
            fixture.session,
            authorized
                .observation_correlation()
                .bind_terminal_observation(ModelCallTerminalObservation::CompletedWithTools {
                    response,
                    retained_input_tokens: None,
                    retained_output_tokens: None,
                }),
            ModelCallTerminalIdentities::ToolRound(ToolRoundModelCallIdentities::new(
                vec![ToolResponsePartIdentity::tool_call(
                    SemanticTranscriptEntryId::from_uuid(Uuid::now_v7()),
                    request_id,
                    InitialToolApproval::PolicyAuto,
                )],
                ContextFrontierId::from_uuid(Uuid::now_v7()),
                Some(TurnAttemptId::from_uuid(Uuid::now_v7())),
            )),
            |_| panic!("no pending steering"),
        )
        .await?;
    let tools = PostgresToolLoopRepository::new(pool.clone());
    let batch = tools
        .load_active_batch(fixture.session, authorized.turn())
        .await?
        .expect("compacted fixture tool batch");
    assert!(
        tools
            .resolve_visible_attachment(&batch.requests()[0], compacted_digest, None)
            .await?
            .is_none()
    );
    assert!(
        tools
            .resolve_visible_attachment(&batch.requests()[0], visible_digest, None)
            .await?
            .is_some()
    );
    let attempt = ToolAttemptId::from_uuid(Uuid::now_v7());
    let turn = authorized.turn();
    tools
        .prepare_next_attempt(fixture.session, turn, attempt, ToolEffectClass::EffectFree)
        .await?;
    let outcome = tools
        .authorize_attempt_with_preauthorization(
            fixture.session,
            turn,
            attempt,
            signalbox_application::ToolPreauthorization::BlobRead {
                digest: compacted_digest,
                decoded_bytes: std::num::NonZeroU64::MIN,
            },
        )
        .await?;
    assert_eq!(
        outcome,
        signalbox_application::ToolAttemptAuthorizationOutcome::PreauthorizationRejected {
            detail: signalbox_domain::ToolExecutionErrorDetail::try_new(
                "blob_not_visible".to_owned()
            )
            .expect("fixed rejection detail"),
        }
    );
    let state: String =
        sqlx::query_scalar("SELECT state_kind FROM tool_attempt WHERE attempt_id = $1")
            .bind(attempt.into_uuid())
            .fetch_one(&pool)
            .await?;
    assert_eq!(state, "prepared");
    pool.close().await;
    drop(container);
    Ok(())
}

/// Ambiguous-call census evidence commits with the physical outcome and retains
/// the exact request correlation after the runtime report has been dropped.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn ambiguous_model_call_diagnostic_evidence_survives_terminal_commit()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0x6e70;
    let (fixture, repository, authorized) = authorize_checkpointed_model_call(&pool, seed).await?;
    let diagnostic = signalbox_domain::ModelCallAmbiguityEvidence::new(
        "classification_point=runtime_terminal_report\nloss_point=stream_timeout\ndetail=read deadline",
    );
    let observation = authorized
        .observation_correlation()
        .bind_terminal_observation(ModelCallTerminalObservation::Ambiguous)
        .with_ambiguity_evidence(Some(diagnostic.clone()));
    repository
        .apply_terminal_observation(
            fixture.session,
            observation.clone(),
            ModelCallTerminalIdentities::Ambiguous(AmbiguousModelCallTurnIdentities::new(
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 20)),
            )),
            |_| panic!("no pending steering"),
        )
        .await?;
    assert_eq!(
        repository
            .reread_terminal_observation(fixture.session, &observation)
            .await?,
        RetainedModelCallObservationStatus::AlreadyCommitted
    );
    let stored: (String, String, Decimal, Uuid) = sqlx::query_as(
        "SELECT terminal_disposition_kind, terminal_ambiguity_evidence, terminal_ambiguity_evidence_original_bytes, context_frontier_id FROM model_call WHERE model_call_id = $1",
    ).bind(fixture.call.into_uuid()).fetch_one(&pool).await?;
    assert_eq!(stored.0, "ambiguous");
    assert_eq!(stored.1, diagnostic.summary());
    assert_eq!(stored.2, Decimal::from(diagnostic.original_bytes() as u64));
    assert_eq!(stored.3, observation.correlation().frontier().into_uuid());
    pool.close().await;
    drop(container);
    Ok(())
}

/// Lost runtime reports remain distinguishable from a report of no response.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn ambiguous_model_call_without_a_report_retains_the_terminalization_boundary()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0x6f70;
    let (fixture, repository, _authorized) = authorize_checkpointed_model_call(&pool, seed).await?;
    repository
        .recover_after_restart(
            fixture.session,
            fixture.call,
            FailedModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 19)),
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 20)),
            ),
        )
        .await?;
    let stored: String = sqlx::query_scalar(
        "SELECT terminal_ambiguity_evidence FROM model_call WHERE model_call_id = $1",
    )
    .bind(fixture.call.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        stored,
        "classification_point=terminalization_without_runtime_report\nprior_call_state=in_flight"
    );
    pool.close().await;
    drop(container);
    Ok(())
}

/// Error diagnostics are retained before crash classification and survive it.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn ambiguous_model_call_keeps_provider_error_evidence_through_crash_classification()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0x6f90;
    let (fixture, mut repository, authorized) =
        authorize_checkpointed_model_call(&pool, seed).await?;
    let diagnostic = signalbox_domain::ModelCallAmbiguityEvidence::new(
        "classification_point=provider_invocation_error\ncause=unsupported_completion_material",
    );
    signalbox_application::CommitModelCallObservationTransaction::retain_provider_failure_evidence(
        &mut repository,
        authorized.observation_correlation(),
        diagnostic.clone(),
    )
    .await?;
    let state: String =
        sqlx::query_scalar("SELECT state_kind FROM model_call WHERE model_call_id = $1")
            .bind(fixture.call.into_uuid())
            .fetch_one(&pool)
            .await?;
    assert_eq!(state, "in_flight");
    repository
        .recover_after_restart(
            fixture.session,
            fixture.call,
            FailedModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 19)),
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 20)),
            ),
        )
        .await?;
    let stored: String = sqlx::query_scalar(
        "SELECT terminal_ambiguity_evidence FROM model_call WHERE model_call_id = $1",
    )
    .bind(fixture.call.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(stored, diagnostic.summary());
    pool.close().await;
    drop(container);
    Ok(())
}
