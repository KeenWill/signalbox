//! Placement loss closes logical requests before dispatch.

use super::*;
use signalbox_persistence::tool_loop::PostgresToolLoopRepository;

struct UndispatchedBatch {
    session: SessionId,
    turn: TurnId,
    request: ToolRequestId,
    attempt: ToolAttemptId,
    store: RunnerProtocolStore,
    connection: signalbox_persistence::runner_protocol::RunnerConnectionSnapshot,
}

async fn prepared_batch(pool: &PgPool) -> Result<UndispatchedBatch, Box<dyn Error>> {
    let (session, turn, producing_attempt) = insert_running_turn(pool).await?;
    insert_physical_attempt(pool, INITIAL_PHYSICAL_ATTEMPT).await?;
    let request = ToolRequestId::from_uuid(uuid(INITIAL_PHYSICAL_ATTEMPT.request));
    let attempt = ToolAttemptId::from_uuid(uuid(INITIAL_PHYSICAL_ATTEMPT.attempt));
    let call = ModelCallId::from_uuid(uuid(
        INITIAL_PHYSICAL_ATTEMPT.request + RELATED_IDENTITY_OFFSET,
    ));
    let boundary = ContextFrontierId::from_uuid(Uuid::now_v7());
    attach_continuing_tool_round_projection(
        pool,
        session,
        turn,
        producing_attempt,
        call,
        request,
        boundary,
    )
    .await?;
    let executing_attempt = Uuid::now_v7();
    let mut transaction = pool.begin().await?;
    sqlx::raw_sql("ALTER TABLE turn_attempt DISABLE TRIGGER ALL; ALTER TABLE turn_lifecycle DISABLE TRIGGER ALL; ALTER TABLE tool_attempt DISABLE TRIGGER ALL;")
        .execute(&mut *transaction).await?;
    sqlx::query("UPDATE turn_attempt SET state_kind = 'ended', end_variant = 'without_stop', end_disposition = 'yielded_to_durable_wait' WHERE turn_attempt_id = $1")
        .bind(producing_attempt.into_uuid()).execute(&mut *transaction).await?;
    sqlx::query("INSERT INTO turn_attempt (turn_attempt_id, turn_id, session_id, continued_from_attempt_id, state_kind) VALUES ($1, $2, $3, $4, 'prepared')")
        .bind(executing_attempt).bind(turn.into_uuid()).bind(session.into_uuid()).bind(producing_attempt.into_uuid()).execute(&mut *transaction).await?;
    sqlx::query("UPDATE turn_lifecycle SET active_tool_round_call_id = $1, current_attempt_id = $2 WHERE turn_id = $3")
        .bind(call.into_uuid()).bind(executing_attempt).bind(turn.into_uuid()).execute(&mut *transaction).await?;
    sqlx::query("UPDATE tool_attempt SET state_kind = 'prepared', issuing_turn_attempt_id = $1 WHERE attempt_id = $2")
        .bind(executing_attempt).bind(attempt.into_uuid()).execute(&mut *transaction).await?;
    sqlx::raw_sql("ALTER TABLE turn_attempt ENABLE TRIGGER ALL; ALTER TABLE turn_lifecycle ENABLE TRIGGER ALL; ALTER TABLE tool_attempt ENABLE TRIGGER ALL;")
        .execute(&mut *transaction).await?;
    transaction.commit().await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let enrollment = enrollment();
    store.insert_enrollment(&enrollment).await?;
    store.register(&enrollment, advertisement()).await?;
    let connection = store.open_connection(enrollment.enrollment()).await?;
    store
        .store_placement(
            &SessionRunnerPlacement::new(session, exact_runner_request(enrollment.runner())),
            None,
            None,
        )
        .await?;
    Ok(UndispatchedBatch {
        session,
        turn,
        request,
        attempt,
        store,
        connection,
    })
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn placement_loss_retires_prepared_attempt_without_result_projection()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = prepared_batch(&pool).await?;
    fixture
        .store
        .transition_connection(
            enrollment().enrollment(),
            fixture.connection.epoch(),
            RunnerConnectionTransition::TransportClosed,
        )
        .await?;
    let loss = fixture
        .store
        .load_current_connection_loss(enrollment().enrollment())
        .await?
        .expect("connection loss is durable");
    fixture
        .store
        .propagate_connection_loss_session(loss, fixture.session)
        .await?;
    let repository = PostgresToolLoopRepository::new(pool.clone());
    let batch = repository
        .load_active_batch(fixture.session, fixture.turn)
        .await?
        .expect("closed request remains in the active batch");
    assert_eq!(
        batch.requests()[0].inadmissible_reason(),
        Some(signalbox_domain::ToolInadmissibleReason::PlacementLost)
    );
    assert!(batch.approval(fixture.request).is_none());
    #[derive(Debug, PartialEq, sqlx::FromRow)]
    struct RetiredAttempt {
        state_kind: String,
        terminal_disposition_kind: String,
        error_kind: String,
        error_detail: String,
    }
    let facts: RetiredAttempt = sqlx::query_as("SELECT state_kind, terminal_disposition_kind, error_kind, error_detail FROM tool_attempt WHERE attempt_id = $1")
        .bind(fixture.attempt.into_uuid()).fetch_one(&pool).await?;
    assert_eq!(
        facts,
        RetiredAttempt {
            state_kind: "terminal".to_owned(),
            terminal_disposition_kind: "known_failed".to_owned(),
            error_kind: "execution_failed".to_owned(),
            error_detail: "placement_lost".to_owned()
        }
    );
    let projected: i64 = sqlx::query_scalar("SELECT count(*) FROM semantic_transcript_entry WHERE tool_result_request_id = $1 OR tool_result_attempt_id = $2")
        .bind(fixture.request.into_uuid()).bind(fixture.attempt.into_uuid()).fetch_one(&pool).await?;
    assert_eq!(projected, 0);
    let projection = batch
        .prepare_result_projection(
            vec![SemanticTranscriptEntryId::from_uuid(Uuid::now_v7())],
            ContextFrontierId::from_uuid(Uuid::now_v7()),
        )
        .expect("the inadmissible request completes the batch");
    assert_eq!(
        projection.entries()[0].payload(),
        &signalbox_domain::SemanticTranscriptEntryPayload::ToolInadmissible {
            request: fixture.request
        }
    );
    assert_eq!(
        fixture
            .store
            .propagate_connection_loss_session(loss, fixture.session)
            .await?,
        RunnerConnectionLossSessionDisposition::Replayed
    );
    Ok(())
}

async fn parked_batch(pool: &PgPool) -> Result<UndispatchedBatch, Box<dyn Error>> {
    let fixture = prepared_batch(pool).await?;
    let mut transaction = pool.begin().await?;
    sqlx::raw_sql("ALTER TABLE tool_attempt DISABLE TRIGGER ALL; ALTER TABLE turn_attempt DISABLE TRIGGER ALL; ALTER TABLE turn_lifecycle DISABLE TRIGGER ALL; ALTER TABLE tool_approval_decision DISABLE TRIGGER ALL;")
        .execute(&mut *transaction).await?;
    let executing_attempt: Uuid = sqlx::query_scalar(
        "SELECT issuing_turn_attempt_id FROM tool_attempt WHERE attempt_id = $1",
    )
    .bind(fixture.attempt.into_uuid())
    .fetch_one(&mut *transaction)
    .await?;
    sqlx::query("DELETE FROM tool_attempt WHERE attempt_id = $1")
        .bind(fixture.attempt.into_uuid())
        .execute(&mut *transaction)
        .await?;
    sqlx::query("DELETE FROM tool_approval_decision WHERE request_id = $1")
        .bind(fixture.request.into_uuid())
        .execute(&mut *transaction)
        .await?;
    sqlx::query("UPDATE turn_lifecycle SET active_phase_kind = 'awaiting_tool_approval', current_attempt_id = NULL, approval_tool_request_id = $1 WHERE turn_id = $2")
        .bind(fixture.request.into_uuid()).bind(fixture.turn.into_uuid()).execute(&mut *transaction).await?;
    sqlx::query("DELETE FROM turn_attempt WHERE turn_attempt_id = $1")
        .bind(executing_attempt)
        .execute(&mut *transaction)
        .await?;
    sqlx::raw_sql("ALTER TABLE tool_attempt ENABLE TRIGGER ALL; ALTER TABLE turn_attempt ENABLE TRIGGER ALL; ALTER TABLE turn_lifecycle ENABLE TRIGGER ALL; ALTER TABLE tool_approval_decision ENABLE TRIGGER ALL;")
        .execute(&mut *transaction).await?;
    transaction.commit().await?;
    Ok(fixture)
}

async fn lose_batch(fixture: &UndispatchedBatch) -> Result<(), Box<dyn Error>> {
    fixture
        .store
        .transition_connection(
            enrollment().enrollment(),
            fixture.connection.epoch(),
            RunnerConnectionTransition::TransportClosed,
        )
        .await?;
    let loss = fixture
        .store
        .load_current_connection_loss(enrollment().enrollment())
        .await?
        .expect("loss is durable");
    fixture
        .store
        .propagate_connection_loss_session(loss, fixture.session)
        .await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn placement_loss_closes_approval_wait_without_execution_evidence()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = parked_batch(&pool).await?;
    lose_batch(&fixture).await?;
    let batch = PostgresToolLoopRepository::new(pool.clone())
        .load_active_batch(fixture.session, fixture.turn)
        .await?
        .expect("the resumed batch remains active");
    assert!(batch.awaiting_approval().is_none());
    assert_eq!(
        batch.requests()[0].inadmissible_reason(),
        Some(signalbox_domain::ToolInadmissibleReason::PlacementLost)
    );
    let evidence: i64 = sqlx::query_scalar("SELECT (SELECT count(*) FROM tool_approval_decision WHERE request_id = $1) + (SELECT count(*) FROM tool_attempt WHERE request_id = $1) + (SELECT count(*) FROM tool_approval_judge_model_call WHERE request_id = $1) + (SELECT count(*) FROM semantic_transcript_entry WHERE tool_result_request_id = $1)")
        .bind(fixture.request.into_uuid()).fetch_one(&pool).await?;
    assert_eq!(evidence, 0);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn placement_loss_retires_prepared_judge_before_resuming_batch() -> Result<(), Box<dyn Error>>
{
    let (_container, pool) = migrated_postgres().await?;
    let fixture = parked_batch(&pool).await?;
    let judge = Uuid::now_v7();
    sqlx::query("INSERT INTO tool_approval_judge_model_call (model_call_id, request_id, session_id, turn_id, direct_model_selection_id, resolved_provider_model_identity_id, credential_reference, state_kind) SELECT $1, request_id, session_id, turn_id, $2, $3, 'fixture-credential-reference', 'prepared' FROM tool_request WHERE request_id = $4")
        .bind(judge).bind(uuid(0xa101)).bind(uuid(0xa159)).bind(fixture.request.into_uuid()).execute(&pool).await?;
    lose_batch(&fixture).await?;
    let terminal: String = sqlx::query_scalar("SELECT terminal_disposition_kind FROM tool_approval_judge_model_call WHERE model_call_id = $1")
        .bind(judge).fetch_one(&pool).await?;
    assert_eq!(terminal, "cancelled");
    let batch = PostgresToolLoopRepository::new(pool.clone())
        .load_active_batch(fixture.session, fixture.turn)
        .await?
        .expect("judge retirement and closure resume the batch");
    assert!(batch.awaiting_approval().is_none());
    assert_eq!(
        batch.requests()[0].inadmissible_reason(),
        Some(signalbox_domain::ToolInadmissibleReason::PlacementLost)
    );
    Ok(())
}

fn model_repository(
    pool: &PgPool,
) -> signalbox_persistence::model_execution::PostgresModelCallRepository {
    let targets = signalbox_domain::ModelTargetCatalog::try_from_definitions([
        signalbox_domain::ModelTargetDefinition::new(
            DirectModelSelection::from_uuid(uuid(0xa101)),
            signalbox_domain::ResolvedProviderTarget::naming(
                signalbox_domain::ProviderModelIdentity::from_uuid(uuid(0xa159)),
            ),
        ),
    ])
    .expect("fixture selection resolves to the producing model");
    signalbox_persistence::model_execution::PostgresModelCallRepository::new(
        pool.clone(),
        targets,
        signalbox_application::ModelCallCredentialReference::new("fixture-credential-reference"),
    )
}

async fn judge_observation_closes_lost_request(
    recommendation: signalbox_domain::DelegateApprovalRecommendation,
) -> Result<(), Box<dyn Error>> {
    use signalbox_application::ApprovalJudgeCompletionIdentities;
    use signalbox_persistence::approval_judge::{
        CompleteApprovalJudgeOutcome, PrepareApprovalJudgeOutcome,
    };
    let (_container, pool) = migrated_postgres().await?;
    let fixture = parked_batch(&pool).await?;
    sqlx::raw_sql("ALTER TABLE tool_request DISABLE TRIGGER ALL;")
        .execute(&pool)
        .await?;
    sqlx::query("UPDATE tool_request SET approval_posture = 'delegated' WHERE request_id = $1")
        .bind(fixture.request.into_uuid())
        .execute(&pool)
        .await?;
    sqlx::raw_sql("ALTER TABLE tool_request ENABLE TRIGGER ALL;")
        .execute(&pool)
        .await?;
    let repository = model_repository(&pool).approval_judge_repository();
    let PrepareApprovalJudgeOutcome::Ready(prepared) = repository
        .prepare(
            fixture.session,
            fixture.turn,
            ModelCallId::from_uuid(Uuid::now_v7()),
            None,
        )
        .await?
    else {
        panic!("the delegated request prepares a judge");
    };
    repository.authorize(&prepared).await?;
    lose_batch(&fixture).await?;
    let parked = PostgresToolLoopRepository::new(pool.clone())
        .load_active_batch(fixture.session, fixture.turn)
        .await?
        .expect("in-flight judge retains the wait");
    assert_eq!(
        parked.awaiting_approval().map(|waiting| waiting.request()),
        Some(fixture.request)
    );
    assert!(parked.requests()[0].inadmissible_reason().is_none());
    let identities = ApprovalJudgeCompletionIdentities::new(
        TurnAttemptId::from_uuid(Uuid::now_v7()),
        SemanticTranscriptEntryId::from_uuid(Uuid::now_v7()),
        ContextFrontierId::from_uuid(Uuid::now_v7()),
    );
    let rationale = signalbox_domain::ToolDecisionRationale::try_new(
        "The requested work is permitted.".to_owned(),
    )?;
    let outcome = repository
        .complete(
            &prepared,
            recommendation,
            rationale.clone(),
            signalbox_domain::ProviderReportedTokenUsage::unreported(),
            identities,
            |_| SemanticTranscriptEntryId::from_uuid(Uuid::now_v7()),
        )
        .await?;
    assert_eq!(outcome, CompleteApprovalJudgeOutcome::ClosedInadmissible);
    let resumed = PostgresToolLoopRepository::new(pool.clone())
        .load_active_batch(fixture.session, fixture.turn)
        .await?
        .expect("observation and closure resume the batch atomically");
    assert!(resumed.awaiting_approval().is_none());
    assert!(resumed.approval(fixture.request).is_none());
    assert_eq!(
        resumed.requests()[0].inadmissible_reason(),
        Some(signalbox_domain::ToolInadmissibleReason::PlacementLost)
    );
    let replay = repository
        .complete(
            &prepared,
            recommendation,
            rationale,
            signalbox_domain::ProviderReportedTokenUsage::unreported(),
            identities,
            |_| panic!("equal observation replay creates no entries"),
        )
        .await?;
    assert_eq!(replay, outcome);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn placement_loss_waits_for_judge_approval_and_retires_its_decision()
-> Result<(), Box<dyn Error>> {
    judge_observation_closes_lost_request(signalbox_domain::DelegateApprovalRecommendation::Approve)
        .await
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn placement_loss_waits_for_judge_denial_and_retires_its_decision()
-> Result<(), Box<dyn Error>> {
    judge_observation_closes_lost_request(signalbox_domain::DelegateApprovalRecommendation::Deny)
        .await
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn placement_loss_waits_for_judge_escalation_and_retires_its_decision()
-> Result<(), Box<dyn Error>> {
    judge_observation_closes_lost_request(
        signalbox_domain::DelegateApprovalRecommendation::EscalateToHuman,
    )
    .await
}

async fn continue_inadmissible_batch(target_available: bool) -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = parked_batch(&pool).await?;
    lose_batch(&fixture).await?;
    let repository = if target_available {
        model_repository(&pool)
    } else {
        sqlx::raw_sql("ALTER TABLE model_call DISABLE TRIGGER ALL;")
            .execute(&pool)
            .await?;
        sqlx::query("UPDATE model_call SET usage_input_tokens = 100 WHERE turn_id = $1")
            .bind(fixture.turn.into_uuid())
            .execute(&pool)
            .await?;
        sqlx::raw_sql("ALTER TABLE model_call ENABLE TRIGGER ALL;")
            .execute(&pool)
            .await?;
        model_repository(&pool).with_continuation_usage_limits([
            signalbox_persistence::model_execution::ToolContinuationUsageLimit::new(
                signalbox_domain::ResolvedProviderTarget::naming(
                    signalbox_domain::ProviderModelIdentity::from_uuid(uuid(0xa159)),
                ),
                signalbox_domain::FastMode::Disabled,
                10,
                100,
            ),
        ])
    };
    let batch = repository
        .tool_loop_repository()
        .load_active_batch(fixture.session, fixture.turn)
        .await?
        .expect("batch is available for continuation");
    let continuation_call = ModelCallId::from_uuid(Uuid::now_v7());
    let result = SemanticTranscriptEntryId::from_uuid(Uuid::now_v7());
    let outcome = repository
        .tool_loop_repository()
        .prepare_continuation(
            fixture.session,
            fixture.turn,
            batch.producing_call(),
            signalbox_application::ToolContinuationIdentities::new(
                vec![result],
                ContextFrontierId::from_uuid(Uuid::now_v7()),
                continuation_call,
                signalbox_domain::FailedModelCallTurnIdentities::new(
                    SemanticTranscriptEntryId::from_uuid(Uuid::now_v7()),
                    ContextFrontierId::from_uuid(Uuid::now_v7()),
                ),
                ContextFrontierId::from_uuid(Uuid::now_v7()),
            ),
            |_| panic!("fixture has no steering"),
        )
        .await?;
    if target_available {
        assert!(matches!(
            outcome,
            signalbox_application::PrepareToolContinuationOutcome::Checkpointed(_)
        ));
    } else {
        assert!(matches!(
            outcome,
            signalbox_application::PrepareToolContinuationOutcome::ContextCompactionRequired(_)
        ));
    }
    let payload: String = sqlx::query_scalar("SELECT payload_kind FROM semantic_transcript_entry WHERE source_session_id = $1 AND semantic_entry_id = $2")
        .bind(fixture.session.into_uuid()).bind(result.into_uuid()).fetch_one(&pool).await?;
    assert_eq!(payload, "tool_inadmissible");
    if target_available {
        let state: String =
            sqlx::query_scalar("SELECT state_kind FROM model_call WHERE model_call_id = $1")
                .bind(continuation_call.into_uuid())
                .fetch_one(&pool)
                .await?;
        assert_eq!(state, "prepared");
        let signalbox_application::AuthorizeModelCallOutcome::Authorized(authorized) = repository
            .authorize_send(fixture.session, continuation_call)
            .await?
        else {
            panic!("the continued call authorizes from the inadmissible result");
        };
        let next_request = ToolRequestId::from_uuid(Uuid::now_v7());
        let observation = authorized
            .observation_correlation()
            .bind_terminal_observation(
                signalbox_domain::ModelCallTerminalObservation::CompletedWithTools {
                    response: signalbox_domain::ToolUsingAssistantResponse::try_from_parts(vec![
                        signalbox_domain::AssistantResponsePart::ToolCall(
                            signalbox_domain::ToolCallProposal::new(
                                ToolName::try_new("inspect".to_owned()).expect("fixture tool name"),
                                NormalizedToolArguments::try_from_provider_text("{}".to_owned())
                                    .expect("fixture arguments"),
                            ),
                        ),
                    ])
                    .expect("one valid tool proposal"),
                    retained_input_tokens: None,
                    retained_output_tokens: None,
                },
            );
        repository
            .apply_terminal_observation(
                fixture.session,
                observation,
                signalbox_domain::ModelCallTerminalIdentities::ToolRound(
                    signalbox_domain::ToolRoundModelCallIdentities::new(
                        vec![signalbox_domain::ToolResponsePartIdentity::tool_call(
                            SemanticTranscriptEntryId::from_uuid(Uuid::now_v7()),
                            next_request,
                            signalbox_domain::InitialToolApproval::Confirm,
                        )],
                        ContextFrontierId::from_uuid(Uuid::now_v7()),
                        None,
                    ),
                ),
                |_| panic!("the fixture has no steering"),
            )
            .await?;
        let next_batch = repository
            .tool_loop_repository()
            .load_active_batch(fixture.session, fixture.turn)
            .await?
            .expect("a later model observation resumes the next batch");
        assert_eq!(
            next_batch.requests()[0].inadmissible_reason(),
            Some(signalbox_domain::ToolInadmissibleReason::PlacementLost)
        );
        assert!(next_batch.awaiting_approval().is_none());
        let evidence: i64 = sqlx::query_scalar("SELECT (SELECT count(*) FROM tool_approval_decision WHERE request_id = $1) + (SELECT count(*) FROM tool_attempt WHERE request_id = $1) + (SELECT count(*) FROM tool_approval_judge_model_call WHERE request_id = $1)")
            .bind(next_request.into_uuid()).fetch_one(&pool).await?;
        assert_eq!(evidence, 0);
    } else {
        let state: String =
            sqlx::query_scalar("SELECT state_kind FROM turn_lifecycle WHERE turn_id = $1")
                .bind(fixture.turn.into_uuid())
                .fetch_one(&pool)
                .await?;
        assert_eq!(state, "terminal");
    }
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn placement_loss_only_batch_commits_result_and_next_model_call() -> Result<(), Box<dyn Error>>
{
    continue_inadmissible_batch(true).await
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn placement_loss_resolution_survives_failed_continuation() -> Result<(), Box<dyn Error>> {
    continue_inadmissible_batch(false).await
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn placement_loss_closes_earlier_approved_requests_in_a_parked_batch()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = prepared_batch(&pool).await?;
    let pending = ToolRequestId::from_uuid(Uuid::now_v7());
    let entry = Uuid::now_v7();
    let mut transaction = pool.begin().await?;
    sqlx::raw_sql("ALTER TABLE tool_attempt DISABLE TRIGGER ALL; ALTER TABLE turn_attempt DISABLE TRIGGER ALL; ALTER TABLE turn_lifecycle DISABLE TRIGGER ALL; ALTER TABLE tool_request DISABLE TRIGGER ALL; ALTER TABLE tool_round DISABLE TRIGGER ALL; ALTER TABLE context_frontier DISABLE TRIGGER ALL; ALTER TABLE context_frontier_delta DISABLE TRIGGER ALL; ALTER TABLE semantic_transcript_entry DISABLE TRIGGER ALL;")
        .execute(&mut *transaction).await?;
    let executing_attempt: Uuid = sqlx::query_scalar(
        "SELECT issuing_turn_attempt_id FROM tool_attempt WHERE attempt_id = $1",
    )
    .bind(fixture.attempt.into_uuid())
    .fetch_one(&mut *transaction)
    .await?;
    sqlx::query("DELETE FROM tool_attempt WHERE attempt_id = $1")
        .bind(fixture.attempt.into_uuid())
        .execute(&mut *transaction)
        .await?;
    sqlx::query("INSERT INTO tool_request (request_id, session_id, turn_id, producing_model_call_id, request_ordinal, tool_name, arguments_kind, arguments_text, approval_posture) SELECT $1, session_id, turn_id, producing_model_call_id, 1, tool_name, 'json', '{}', 'human' FROM tool_request WHERE request_id = $2")
        .bind(pending.into_uuid()).bind(fixture.request.into_uuid()).execute(&mut *transaction).await?;
    sqlx::query("INSERT INTO semantic_transcript_entry (source_session_id, semantic_entry_id, payload_kind, producing_model_call_id, assistant_tool_request_id, assistant_response_part_ordinal) SELECT session_id, $1, 'assistant_tool_use', producing_model_call_id, request_id, 1 FROM tool_request WHERE request_id = $2")
        .bind(entry).bind(pending.into_uuid()).execute(&mut *transaction).await?;
    sqlx::query("INSERT INTO context_frontier_delta (owning_session_id, context_frontier_id, member_position, source_session_id, semantic_entry_id) SELECT round.session_id, boundary_frontier_id, frontier.member_count + 1, round.session_id, $1 FROM tool_round AS round JOIN context_frontier AS frontier ON frontier.owning_session_id = round.session_id AND frontier.context_frontier_id = round.boundary_frontier_id WHERE round.turn_id = $2")
        .bind(entry).bind(fixture.turn.into_uuid()).execute(&mut *transaction).await?;
    sqlx::query("UPDATE context_frontier AS frontier SET member_count = member_count + 1 FROM tool_round AS round WHERE round.turn_id = $1 AND frontier.owning_session_id = round.session_id AND frontier.context_frontier_id = round.boundary_frontier_id")
        .bind(fixture.turn.into_uuid()).execute(&mut *transaction).await?;
    sqlx::query(
        "UPDATE tool_round SET response_part_count = 2, request_count = 2 WHERE turn_id = $1",
    )
    .bind(fixture.turn.into_uuid())
    .execute(&mut *transaction)
    .await?;
    sqlx::query("UPDATE turn_lifecycle SET active_phase_kind = 'awaiting_tool_approval', current_attempt_id = NULL, approval_tool_request_id = $1 WHERE turn_id = $2")
        .bind(pending.into_uuid()).bind(fixture.turn.into_uuid()).execute(&mut *transaction).await?;
    sqlx::query("DELETE FROM turn_attempt WHERE turn_attempt_id = $1")
        .bind(executing_attempt)
        .execute(&mut *transaction)
        .await?;
    sqlx::raw_sql("ALTER TABLE tool_attempt ENABLE TRIGGER ALL; ALTER TABLE turn_attempt ENABLE TRIGGER ALL; ALTER TABLE turn_lifecycle ENABLE TRIGGER ALL; ALTER TABLE tool_request ENABLE TRIGGER ALL; ALTER TABLE tool_round ENABLE TRIGGER ALL; ALTER TABLE context_frontier ENABLE TRIGGER ALL; ALTER TABLE context_frontier_delta ENABLE TRIGGER ALL; ALTER TABLE semantic_transcript_entry ENABLE TRIGGER ALL;")
        .execute(&mut *transaction).await?;
    transaction.commit().await?;
    lose_batch(&fixture).await?;
    let batch = PostgresToolLoopRepository::new(pool.clone())
        .load_active_batch(fixture.session, fixture.turn)
        .await?
        .expect("all closed requests resume the batch");
    assert!(batch.awaiting_approval().is_none());
    assert!(batch.approval(fixture.request).is_none());
    let resolution = Some(signalbox_domain::ToolInadmissibleReason::PlacementLost);
    assert_eq!(batch.requests()[0].inadmissible_reason(), resolution);
    assert_eq!(batch.requests()[1].inadmissible_reason(), resolution);
    let projection = batch
        .prepare_result_projection(
            vec![
                SemanticTranscriptEntryId::from_uuid(Uuid::now_v7()),
                SemanticTranscriptEntryId::from_uuid(Uuid::now_v7()),
            ],
            ContextFrontierId::from_uuid(Uuid::now_v7()),
        )
        .expect("both closed requests project in proposal order");
    assert_eq!(
        projection.entries()[0].payload(),
        &signalbox_domain::SemanticTranscriptEntryPayload::ToolInadmissible {
            request: fixture.request
        }
    );
    assert_eq!(
        projection.entries()[1].payload(),
        &signalbox_domain::SemanticTranscriptEntryPayload::ToolInadmissible { request: pending }
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn placement_loss_preserves_an_executor_dispatched_attempt() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = prepared_batch(&pool).await?;
    let repository = PostgresToolLoopRepository::new(pool.clone());
    let dispatched = repository
        .authorize_attempt(fixture.session, fixture.turn, fixture.attempt)
        .await?;
    lose_batch(&fixture).await?;
    let batch = repository
        .load_active_batch(fixture.session, fixture.turn)
        .await?
        .expect("the dispatched request still owns its completion");
    assert!(batch.requests()[0].inadmissible_reason().is_none());
    let signalbox_domain::ReconstitutedToolAttempt::Current(attempt) = batch
        .attempt(fixture.request)
        .expect("dispatched attempt is retained")
    else {
        panic!("loss does not terminalize dispatched work");
    };
    assert_eq!(
        attempt.state(),
        signalbox_domain::CurrentToolAttemptState::InFlight
    );
    assert_eq!(attempt.attempt(), dispatched.correlation().attempt());
    assert_eq!(attempt.generation(), dispatched.correlation().generation());
    Ok(())
}
