//! Approval judge preparation and the approval guard over user, delegate, and automatic decisions.

use crate::*;

/// A second fixture tool, distinct from `APPROVAL_TOOL_NAME`, so a mixed batch
/// can park one request for the judge without that request resembling the
/// re-proposal a recorded override pre-approves.
const JUDGED_TOOL_NAME: &str = "current_weather";
const FAILURE_ENTRY_ID_OFFSET: u128 = 0x1_000;
const TERMINAL_FRONTIER_ID_OFFSET: u128 = 0x1_001;
const CLOSED_RESULT_ID_OFFSET: u128 = 0x2_000_000;

fn approval_judge_completion_identities(
    fresh_seed: u128,
    attempt_seed: u128,
) -> ApprovalJudgeCompletionIdentities {
    ApprovalJudgeCompletionIdentities::new(
        TurnAttemptId::from_uuid(Uuid::from_u128(attempt_seed)),
        SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(fresh_seed + FAILURE_ENTRY_ID_OFFSET)),
        ContextFrontierId::from_uuid(Uuid::from_u128(fresh_seed + TERMINAL_FRONTIER_ID_OFFSET)),
    )
}

fn approval_judge_closed_result_entry(request: ToolRequestId) -> SemanticTranscriptEntryId {
    SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(
        request.as_uuid().as_u128() + CLOSED_RESULT_ID_OFFSET,
    ))
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn approval_judge_terminal_transition_accepts_estimated_usage_provenance()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0x7ed0;
    let (fixture, _, _, requests) = checkpoint_tool_batch_with_approval(
        &pool,
        seed,
        APPROVAL_PROPOSAL,
        InitialToolApproval::Delegated,
    )
    .await?;
    let [request] = requests.as_slice() else {
        panic!("the fixture has one delegated request")
    };
    let estimated_input_tokens = Decimal::from(13_u32);
    let mut transaction = pool.begin().await?;
    let judge_call = persist_delegated_denial_fixture(
        &mut transaction,
        &fixture,
        *request,
        seed + 0xe0,
        TurnAttemptId::from_uuid(Uuid::from_u128(seed + 0xd1)),
        Some(estimated_input_tokens),
        Some(APPROVAL_JUDGE_ESTIMATED_PROVENANCE),
    )
    .await?;
    transaction.commit().await?;

    let stored_usage: (String, Decimal) = sqlx::query_as(
        "SELECT usage_provenance_kind, input_tokens
           FROM tool_approval_judge_model_call
          WHERE model_call_id = $1",
    )
    .bind(judge_call)
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        stored_usage,
        (
            APPROVAL_JUDGE_ESTIMATED_PROVENANCE.to_owned(),
            estimated_input_tokens,
        )
    );

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn human_only_request_never_prepares_a_delegate_judge_call() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0x7ea0;
    let (fixture, model_repository, _, request) =
        checkpoint_confirmed_tool_round(&pool, seed, APPROVAL_TOOL_NAME, APPROVAL_ARGUMENTS)
            .await?;
    let outcome = model_repository
        .approval_judge_repository()
        .prepare(
            fixture.session,
            fixture.turn,
            ModelCallId::from_uuid(Uuid::from_u128(seed + 0xe0)),
            None,
        )
        .await?;
    let judge_calls: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM tool_approval_judge_model_call WHERE request_id = $1",
    )
    .bind(request.into_uuid())
    .fetch_one(&pool)
    .await?;

    assert_eq!(outcome, PrepareApprovalJudgeOutcome::NoWork);
    assert_eq!(judge_calls, 0);

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn approval_judge_repository_defaults_to_durable_producing_model_after_catalog_removal()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0x7ec0;
    let (fixture, _model_repository, _, requests) = checkpoint_tool_batch_with_approval(
        &pool,
        seed,
        APPROVAL_PROPOSAL,
        InitialToolApproval::Delegated,
    )
    .await?;
    let repository = PostgresModelCallRepository::new(
        pool.clone(),
        ModelTargetCatalog::try_from_definitions([])
            .expect("an empty replacement target catalog is valid"),
        model_credential_reference(),
    )
    .approval_judge_repository();
    let prepared = ready_approval_judge(
        repository
            .prepare(
                fixture.session,
                fixture.turn,
                ModelCallId::from_uuid(Uuid::from_u128(seed + 0xe0)),
                None,
            )
            .await?,
    );
    let producing_selection: Uuid = sqlx::query_scalar(
        "SELECT COALESCE(direct_model_selection_id, frozen_alias_selected_direct_id)
           FROM model_call WHERE model_call_id = $1",
    )
    .bind(fixture.call.into_uuid())
    .fetch_one(&pool)
    .await?;
    let producing_target: Uuid = sqlx::query_scalar(
        "SELECT resolved_provider_model_identity_id
           FROM model_call WHERE model_call_id = $1",
    )
    .bind(fixture.call.into_uuid())
    .fetch_one(&pool)
    .await?;

    assert_eq!(prepared.request().id(), requests[0]);
    assert_eq!(prepared.selection().into_uuid(), producing_selection);
    assert_eq!(prepared.target().identity().into_uuid(), producing_target);

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn approval_judge_default_preserves_producing_credential_after_route_removal()
-> Result<(), Box<dyn Error>> {
    const UNRELATED_FAMILY: &str = "unrelated-model-family";

    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0x7ec8;
    let (fixture, _model_repository, _, _) = checkpoint_tool_batch_with_approval(
        &pool,
        seed,
        APPROVAL_PROPOSAL,
        InitialToolApproval::Delegated,
    )
    .await?;
    let unrelated_target = ResolvedProviderTarget::naming(ProviderModelIdentity::from_uuid(
        Uuid::from_u128(seed + 0xe1),
    ));
    let credential_families = ModelCredentialFamilyCatalog::try_new([(
        unrelated_target,
        Arc::<str>::from(UNRELATED_FAMILY),
        None,
    )])
    .expect("the unrelated credential route forms a catalog");
    let repository = PostgresModelCallRepository::new(
        pool.clone(),
        ModelTargetCatalog::try_from_definitions([])
            .expect("an empty replacement target catalog is valid"),
        model_credential_reference(),
    )
    .with_session_credentials(credential_families)
    .approval_judge_repository();
    let prepared = ready_approval_judge(
        repository
            .prepare(
                fixture.session,
                fixture.turn,
                ModelCallId::from_uuid(Uuid::from_u128(seed + 0xe0)),
                None,
            )
            .await?,
    );
    let producing_credential: String =
        sqlx::query_scalar("SELECT credential_reference FROM model_call WHERE model_call_id = $1")
            .bind(fixture.call.into_uuid())
            .fetch_one(&pool)
            .await?;

    assert_eq!(prepared.credential_reference(), producing_credential);

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn approval_judge_repeated_authorization_returns_no_send() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0x7ed0;
    let (fixture, model_repository, _, _) = checkpoint_tool_batch_with_approval(
        &pool,
        seed,
        APPROVAL_PROPOSAL,
        InitialToolApproval::Delegated,
    )
    .await?;
    let repository = model_repository.approval_judge_repository();
    let prepared = ready_approval_judge(
        repository
            .prepare(
                fixture.session,
                fixture.turn,
                ModelCallId::from_uuid(Uuid::from_u128(seed + 0xe0)),
                None,
            )
            .await?,
    );

    let authorization = authorized_approval_judge(repository.authorize(&prepared).await?);
    drop(authorization);
    let retry = repository.authorize(&prepared).await?;

    assert_eq!(retry, AuthorizeApprovalJudgeOutcome::NoSend);

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn approval_judge_terminal_authorization_recheck_returns_no_send()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0x7ed8;
    let (fixture, model_repository, _, _) = checkpoint_tool_batch_with_approval(
        &pool,
        seed,
        APPROVAL_PROPOSAL,
        InitialToolApproval::Delegated,
    )
    .await?;
    let repository = model_repository.approval_judge_repository();
    let prepared = ready_approval_judge(
        repository
            .prepare(
                fixture.session,
                fixture.turn,
                ModelCallId::from_uuid(Uuid::from_u128(seed + 0xe0)),
                None,
            )
            .await?,
    );
    let authorization = authorized_approval_judge(repository.authorize(&prepared).await?);
    repository
        .fail(
            &prepared,
            FailedApprovalJudgeDisposition::KnownFailed,
            ProviderReportedTokenUsage::unreported(),
        )
        .await?;

    let retry = repository.authorize(&prepared).await?;

    assert_eq!(retry, AuthorizeApprovalJudgeOutcome::NoSend);
    drop(authorization);
    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn approval_judge_repository_atomically_applies_provenanced_approval()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0x7ee0;
    let (fixture, model_repository, _, requests) = checkpoint_tool_batch_with_approval(
        &pool,
        seed,
        APPROVAL_PROPOSAL,
        InitialToolApproval::Delegated,
    )
    .await?;
    let request = requests[0];
    let repository = model_repository.approval_judge_repository();
    let judge_call = ModelCallId::from_uuid(Uuid::from_u128(seed + 0xe0));
    let prepared = ready_approval_judge(
        repository
            .prepare(fixture.session, fixture.turn, judge_call, None)
            .await?,
    );

    assert_eq!(prepared.request().id(), request);
    repository.authorize(&prepared).await?;
    let rationale = ToolDecisionRationale::try_new(String::from(APPROVAL_JUDGE_RATIONALE))?;
    let outcome = repository
        .complete(
            &prepared,
            DelegateApprovalRecommendation::Approve,
            rationale,
            ProviderReportedTokenUsage::unreported().with_input_tokens(Some(13)),
            approval_judge_completion_identities(seed, seed + 0xe1),
            approval_judge_closed_result_entry,
        )
        .await?;
    let stored: AppliedApprovalJudgeProjection = sqlx::query_as(
        "SELECT judge.state_kind AS judge_state,
                judge.recommendation_kind AS recommendation,
                decision.decision_source,
                decision.delegate_model_selection_id,
                decision.delegate_model_call_id, decision.rationale,
                lifecycle.active_phase_kind AS active_phase
           FROM tool_approval_judge_model_call AS judge
           JOIN tool_approval_decision AS decision
             ON decision.request_id = judge.request_id
           JOIN turn_lifecycle AS lifecycle
             ON lifecycle.turn_id = judge.turn_id
            AND lifecycle.session_id = judge.session_id
          WHERE judge.model_call_id = $1",
    )
    .bind(judge_call.into_uuid())
    .fetch_one(&pool)
    .await?;

    assert_eq!(outcome, CompleteApprovalJudgeOutcome::Decided);
    assert_eq!(stored.judge_state, "terminal");
    assert_eq!(stored.recommendation, APPROVAL_RECOMMENDATION);
    assert_eq!(stored.decision_source, APPROVAL_DELEGATE_SOURCE);
    assert_eq!(
        stored.delegate_model_selection_id,
        prepared.selection().into_uuid()
    );
    assert_eq!(stored.delegate_model_call_id, prepared.call().into_uuid());
    assert_eq!(stored.rationale, APPROVAL_JUDGE_RATIONALE);
    assert_eq!(stored.active_phase, "running");
    let (event_turn, approval) = dispatched_tool_approval_decision(&pool, request)
        .await?
        .expect("the delegate decision appends its typed outbox event");
    assert_eq!(event_turn, fixture.turn);
    assert_eq!(approval.decision(), &ToolApprovalDecision::Approve);
    assert_eq!(
        approval.decider(),
        Some(&ToolApprovalDecider::Delegate {
            model: prepared.selection(),
            call: prepared.call(),
        })
    );
    assert_eq!(
        approval.rationale().map(ToolDecisionRationale::as_str),
        Some(APPROVAL_JUDGE_RATIONALE)
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// The judge arm of the transcript projection's `UNION ALL` reaches process
/// readers: a terminal delegate judge call's reported tokens join the producing
/// model call's own row in `ProcessTranscriptSnapshot::model_call_usage`, which
/// carries exactly those two calls.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn terminal_approval_judge_usage_joins_the_transcript_usage_projection()
-> Result<(), Box<dyn Error>> {
    const JUDGE_INPUT_TOKENS: u64 = 13;
    const JUDGE_OUTPUT_TOKENS: u64 = 7;

    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0x7f40;
    let (fixture, model_repository, _, _) = checkpoint_tool_batch_with_approval(
        &pool,
        seed,
        APPROVAL_PROPOSAL,
        InitialToolApproval::Delegated,
    )
    .await?;
    let repository = model_repository.approval_judge_repository();
    let judge_call = ModelCallId::from_uuid(Uuid::from_u128(seed + 0xe0));
    let prepared = ready_approval_judge(
        repository
            .prepare(fixture.session, fixture.turn, judge_call, None)
            .await?,
    );
    let rationale = ToolDecisionRationale::try_new(String::from(APPROVAL_JUDGE_RATIONALE))?;

    repository.authorize(&prepared).await?;
    repository
        .complete(
            &prepared,
            DelegateApprovalRecommendation::Approve,
            rationale,
            ProviderReportedTokenUsage::unreported()
                .with_input_tokens(Some(JUDGE_INPUT_TOKENS))
                .with_output_tokens(Some(JUDGE_OUTPUT_TOKENS)),
            approval_judge_completion_identities(seed, seed + 0xe1),
            approval_judge_closed_result_entry,
        )
        .await?;
    let snapshot = ProcessReadRepository::new(pool.clone())
        .read_transcript(fixture.session)
        .await?
        .expect("the delegated fixture session stays process-readable");
    let usage = snapshot.model_call_usage();

    assert_eq!(usage.len(), 2);
    assert_eq!(usage[0].turn(), fixture.turn);
    assert_eq!(usage[0].call(), fixture.call);
    assert_eq!(usage[0].usage().input_tokens(), None);
    assert_eq!(usage[0].usage().output_tokens(), None);
    assert_eq!(usage[0].usage().cache_creation_input_tokens(), None);
    assert_eq!(usage[0].usage().cache_read_input_tokens(), None);
    assert_eq!(usage[1].turn(), fixture.turn);
    assert_eq!(usage[1].call(), judge_call);
    assert_eq!(
        usage[1].provenance(),
        ProcessModelCallUsageProvenance::Reported
    );
    assert_eq!(usage[1].usage().input_tokens(), Some(JUDGE_INPUT_TOKENS));
    assert_eq!(usage[1].usage().output_tokens(), Some(JUDGE_OUTPUT_TOKENS));
    assert_eq!(usage[1].usage().cache_creation_input_tokens(), None);
    assert_eq!(usage[1].usage().cache_read_input_tokens(), None);

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn approval_judge_repository_atomically_applies_a_delegate_denial()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0x7ef8;
    let (fixture, model_repository, _, requests) = checkpoint_tool_batch_with_approval(
        &pool,
        seed,
        APPROVAL_PROPOSAL,
        InitialToolApproval::Delegated,
    )
    .await?;
    let request = requests[0];
    let repository = model_repository.approval_judge_repository();
    let judge_call = ModelCallId::from_uuid(Uuid::from_u128(seed + 0xe0));
    let prepared = ready_approval_judge(
        repository
            .prepare(fixture.session, fixture.turn, judge_call, None)
            .await?,
    );

    repository.authorize(&prepared).await?;
    let rationale = ToolDecisionRationale::try_new(String::from(APPROVAL_JUDGE_RATIONALE))?;
    let outcome = repository
        .complete(
            &prepared,
            DelegateApprovalRecommendation::Deny,
            rationale,
            ProviderReportedTokenUsage::unreported(),
            approval_judge_completion_identities(seed, seed + 0xe1),
            approval_judge_closed_result_entry,
        )
        .await?;
    let stored: DeniedApprovalJudgeProjection = sqlx::query_as(
        "SELECT judge.state_kind AS judge_state,
                judge.recommendation_kind AS recommendation,
                decision.decision_kind, decision.decision_source,
                decision.denial_reason, decision.rationale,
                lifecycle.active_phase_kind AS active_phase
           FROM tool_approval_judge_model_call AS judge
           JOIN tool_approval_decision AS decision
             ON decision.request_id = judge.request_id
           JOIN turn_lifecycle AS lifecycle
             ON lifecycle.turn_id = judge.turn_id
            AND lifecycle.session_id = judge.session_id
          WHERE judge.model_call_id = $1",
    )
    .bind(judge_call.into_uuid())
    .fetch_one(&pool)
    .await?;

    assert_eq!(outcome, CompleteApprovalJudgeOutcome::Decided);
    assert_eq!(stored.judge_state, "terminal");
    assert_eq!(stored.recommendation, APPROVAL_DENIAL);
    assert_eq!(stored.decision_kind, APPROVAL_DENIAL);
    assert_eq!(stored.decision_source, APPROVAL_DELEGATE_SOURCE);
    assert_eq!(
        stored.denial_reason.as_deref(),
        Some(APPROVAL_JUDGE_RATIONALE)
    );
    assert_eq!(stored.rationale, APPROVAL_JUDGE_RATIONALE);
    assert_eq!(stored.active_phase, "running");
    let (event_turn, approval) = dispatched_tool_approval_decision(&pool, request)
        .await?
        .expect("the delegate denial appends its typed outbox event");
    assert_eq!(event_turn, fixture.turn);
    assert_eq!(
        approval.decision(),
        &ToolApprovalDecision::Deny {
            reason: Some(
                ToolDenialReason::try_new(String::from(APPROVAL_JUDGE_RATIONALE))
                    .expect("fixture rationale is an admitted reason")
            )
        }
    );

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn approval_judge_completion_replay_rejects_another_continuation_identity()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0x7ee8;
    let (fixture, model_repository, _, _) = checkpoint_tool_batch_with_approval(
        &pool,
        seed,
        APPROVAL_PROPOSAL,
        InitialToolApproval::Delegated,
    )
    .await?;
    let repository = model_repository.approval_judge_repository();
    let prepared = ready_approval_judge(
        repository
            .prepare(
                fixture.session,
                fixture.turn,
                ModelCallId::from_uuid(Uuid::from_u128(seed + 0xe0)),
                None,
            )
            .await?,
    );
    let rationale = ToolDecisionRationale::try_new(String::from(APPROVAL_JUDGE_RATIONALE))?;
    let persisted_attempt = TurnAttemptId::from_uuid(Uuid::from_u128(seed + 0xe1));
    let conflicting_attempt = TurnAttemptId::from_uuid(Uuid::from_u128(seed + 0xe2));

    drop(authorized_approval_judge(
        repository.authorize(&prepared).await?,
    ));
    repository
        .complete(
            &prepared,
            DelegateApprovalRecommendation::Approve,
            rationale.clone(),
            ProviderReportedTokenUsage::unreported(),
            approval_judge_completion_identities(seed, persisted_attempt.into_uuid().as_u128()),
            approval_judge_closed_result_entry,
        )
        .await?;
    let error = repository
        .complete(
            &prepared,
            DelegateApprovalRecommendation::Approve,
            rationale,
            ProviderReportedTokenUsage::unreported(),
            approval_judge_completion_identities(
                seed + 0x10,
                conflicting_attempt.into_uuid().as_u128(),
            ),
            approval_judge_closed_result_entry,
        )
        .await
        .expect_err("a replay cannot substitute another continuation identity");

    assert_eq!(
        error.operator_failure_class(),
        OperatorFailureClass::FailClosedCorruption,
        "unexpected replay error: {error:?}"
    );

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn approval_judge_completion_identity_collision_rolls_back_for_retry()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0x7ef0;
    let (fixture, model_repository, _, _) = checkpoint_tool_batch_with_approval(
        &pool,
        seed,
        APPROVAL_PROPOSAL,
        InitialToolApproval::Delegated,
    )
    .await?;
    let repository = model_repository.approval_judge_repository();
    let prepared = ready_approval_judge(
        repository
            .prepare(
                fixture.session,
                fixture.turn,
                ModelCallId::from_uuid(Uuid::from_u128(seed + 0xe0)),
                None,
            )
            .await?,
    );
    repository.authorize(&prepared).await?;
    let rationale = ToolDecisionRationale::try_new(String::from(APPROVAL_JUDGE_RATIONALE))?;

    let collision = repository
        .complete(
            &prepared,
            DelegateApprovalRecommendation::Approve,
            rationale.clone(),
            ProviderReportedTokenUsage::unreported(),
            approval_judge_completion_identities(seed, fixture.attempt.into_uuid().as_u128()),
            approval_judge_closed_result_entry,
        )
        .await
        .expect_err("a taken continuation identity rolls back the completion");
    let retry = repository
        .complete(
            &prepared,
            DelegateApprovalRecommendation::Approve,
            rationale,
            ProviderReportedTokenUsage::unreported(),
            approval_judge_completion_identities(seed + 0x10, seed + 0xe1),
            approval_judge_closed_result_entry,
        )
        .await?;

    assert_eq!(
        collision.operator_failure_class(),
        OperatorFailureClass::IdentityCollision
    );
    assert_eq!(retry, CompleteApprovalJudgeOutcome::Decided);

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn approval_judge_repository_escalation_keeps_the_request_parked_for_user_decision()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0x7f20;
    let (fixture, model_repository, _, requests) = checkpoint_tool_batch_with_approval(
        &pool,
        seed,
        APPROVAL_PROPOSAL,
        InitialToolApproval::Delegated,
    )
    .await?;
    let request = requests[0];
    let repository = model_repository.approval_judge_repository();
    let prepared = ready_approval_judge(
        repository
            .prepare(
                fixture.session,
                fixture.turn,
                ModelCallId::from_uuid(Uuid::from_u128(seed + 0xe0)),
                None,
            )
            .await?,
    );

    repository.authorize(&prepared).await?;
    let outcome = repository
        .complete(
            &prepared,
            DelegateApprovalRecommendation::EscalateToHuman,
            ToolDecisionRationale::try_new(String::from(APPROVAL_JUDGE_RATIONALE))?,
            ProviderReportedTokenUsage::unreported(),
            approval_judge_completion_identities(seed, seed + 0xe1),
            approval_judge_closed_result_entry,
        )
        .await?;
    let parked: EscalatedApprovalJudgeProjection = sqlx::query_as(
        "SELECT judge.state_kind AS judge_state,
                judge.recommendation_kind AS recommendation,
                EXISTS (
                    SELECT 1 FROM tool_approval_decision
                     WHERE request_id = judge.request_id
                ) AS decision_exists,
                lifecycle.active_phase_kind AS active_phase,
                lifecycle.approval_tool_request_id
           FROM tool_approval_judge_model_call AS judge
           JOIN turn_lifecycle AS lifecycle
             ON lifecycle.turn_id = judge.turn_id
            AND lifecycle.session_id = judge.session_id
          WHERE judge.model_call_id = $1",
    )
    .bind(prepared.call().into_uuid())
    .fetch_one(&pool)
    .await?;
    let user_decision = model_repository
        .tool_loop_repository()
        .decide(
            decide_tool_request(
                DurableCommandId::from_uuid(Uuid::from_u128(seed + 0xe2)),
                request,
                ToolApprovalDecision::Approve,
            ),
            || TurnAttemptId::from_uuid(Uuid::from_u128(seed + 0xe3)),
        )
        .await?;

    assert_eq!(outcome, CompleteApprovalJudgeOutcome::EscalatedToHuman);
    assert_eq!(parked.judge_state, "terminal");
    assert_eq!(parked.recommendation, "escalate_to_human");
    assert!(!parked.decision_exists);
    assert_eq!(parked.active_phase, "awaiting_tool_approval");
    assert_eq!(parked.approval_tool_request_id, request.into_uuid());
    assert_eq!(
        applied_tool_decision(&user_decision).resolution().request(),
        request
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// A judge decides under the goal statement it read while being prepared, and
/// the session is unlocked for the whole provider round-trip that follows, so a
/// user stop lands between the read and the commit. Completion resolves the
/// statement again under its own lock and finds nothing, which withdraws the
/// authority the recommendation was formed under: the approval the judge
/// returned never becomes a decision, and the request stays parked for the
/// human who now owns it.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn approval_judge_completion_escalates_after_the_judged_goal_is_stopped()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0x7f60;
    let (fixture, model_repository, _, requests) = checkpoint_tool_batch_with_approval(
        &pool,
        seed,
        APPROVAL_PROPOSAL,
        InitialToolApproval::Delegated,
    )
    .await?;
    let request = requests[0];
    let statement = commission_fixture_session_goal(&pool, fixture.session, seed + 0xf0).await?;
    // The checkpoint accepted the fixture turn's input as `seed + 9`; binding
    // that turn to the generation is the dispatch shape whose authority the
    // judge reads, now that an unrecorded turn resolves to no statement.
    bind_commissioned_goal_to_turn(
        &pool,
        fixture.session,
        fixture.turn,
        AcceptedInputId::from_uuid(Uuid::from_u128(seed + 9)),
    )
    .await?;
    let repository = model_repository.approval_judge_repository();
    let prepared = ready_approval_judge(
        repository
            .prepare(
                fixture.session,
                fixture.turn,
                ModelCallId::from_uuid(Uuid::from_u128(seed + 0xe0)),
                None,
            )
            .await?,
    );

    repository.authorize(&prepared).await?;
    stop_fixture_session_goal(&pool, fixture.session, seed + 0xf4).await?;
    let outcome = repository
        .complete(
            &prepared,
            DelegateApprovalRecommendation::Approve,
            ToolDecisionRationale::try_new(String::from(APPROVAL_JUDGE_RATIONALE))?,
            ProviderReportedTokenUsage::unreported(),
            approval_judge_completion_identities(seed, seed + 0xe1),
            approval_judge_closed_result_entry,
        )
        .await?;
    let parked: EscalatedApprovalJudgeProjection = sqlx::query_as(
        "SELECT judge.state_kind AS judge_state,
                judge.recommendation_kind AS recommendation,
                EXISTS (
                    SELECT 1 FROM tool_approval_decision
                     WHERE request_id = judge.request_id
                ) AS decision_exists,
                lifecycle.active_phase_kind AS active_phase,
                lifecycle.approval_tool_request_id
           FROM tool_approval_judge_model_call AS judge
           JOIN turn_lifecycle AS lifecycle
             ON lifecycle.turn_id = judge.turn_id
            AND lifecycle.session_id = judge.session_id
          WHERE judge.model_call_id = $1",
    )
    .bind(prepared.call().into_uuid())
    .fetch_one(&pool)
    .await?;

    assert_eq!(prepared.session_context().goal(), Some(&statement));
    assert_eq!(outcome, CompleteApprovalJudgeOutcome::EscalatedToHuman);
    assert_eq!(parked.judge_state, "terminal");
    assert_eq!(parked.recommendation, "escalate_to_human");
    assert!(!parked.decision_exists);
    assert_eq!(parked.active_phase, "awaiting_tool_approval");
    assert_eq!(parked.approval_tool_request_id, request.into_uuid());

    pool.close().await;
    drop(container);
    Ok(())
}

/// Rebinds a commissioned generation's goal turn to an already-accepted turn.
///
/// Dispatch commissions a goal against the turn carrying its tagged context,
/// so the judged work turn is the generation's own recorded goal turn.
/// `commission_fixture_session_goal` mints a queued candidate instead, because
/// the user-attach path cannot bind an existing turn; this rewrite restates
/// the binding dispatch would have written. Triggers are disabled exactly as
/// the declaration fixture below does: this states correlation facts, not
/// lifecycle history.
async fn bind_commissioned_goal_to_turn(
    pool: &PgPool,
    session: SessionId,
    turn: TurnId,
    accepted_input: AcceptedInputId,
) -> Result<(), Box<dyn Error>> {
    sqlx::query("ALTER TABLE goal_turn DISABLE TRIGGER ALL")
        .execute(pool)
        .await?;
    let rebound = sqlx::query(
        "UPDATE goal_turn SET turn_id = $1, accepted_input_id = $2
          WHERE session_id = $3 AND goal_generation = 1",
    )
    .bind(turn.into_uuid())
    .bind(accepted_input.into_uuid())
    .bind(session.into_uuid())
    .execute(pool)
    .await?
    .rows_affected();
    sqlx::query("ALTER TABLE goal_turn ENABLE TRIGGER ALL")
        .execute(pool)
        .await?;
    assert_eq!(
        rebound, 1,
        "the commissioned generation has one goal turn to bind"
    );
    Ok(())
}

/// Inserts the durable `goal_declare` shape `declare_achieved` authenticates:
/// one producing call whose response is exactly one assistant-text part
/// carrying the report followed by the final `goal_declare` tool-use part.
///
/// Triggers are disabled around the inserts exactly as the goal suite's
/// declaration fixture does: the queued goal turn has no lifecycle or
/// model-call rows, and this fixture states correlation facts, not lifecycle
/// history.
async fn insert_goal_declaration_request(
    pool: &PgPool,
    session: SessionId,
    goal_turn: TurnId,
    request: ToolRequestId,
    report_text: &str,
) -> Result<(), Box<dyn Error>> {
    let producing_call = Uuid::from_u128(request.into_uuid().as_u128() + 0x1000);
    sqlx::query("ALTER TABLE tool_request DISABLE TRIGGER ALL")
        .execute(pool)
        .await?;
    sqlx::query(
        "INSERT INTO tool_request
            (request_id, session_id, turn_id, producing_model_call_id,
             request_ordinal, tool_name, arguments_kind, arguments_text)
         VALUES ($1, $2, $3, $4, 0, 'goal_declare', 'json', $5)",
    )
    .bind(request.into_uuid())
    .bind(session.into_uuid())
    .bind(goal_turn.into_uuid())
    .bind(producing_call)
    .bind(r#"{"transition":"achieved"}"#)
    .execute(pool)
    .await?;
    sqlx::query("ALTER TABLE semantic_transcript_entry DISABLE TRIGGER ALL")
        .execute(pool)
        .await?;
    sqlx::query(
        "INSERT INTO semantic_transcript_entry
            (source_session_id, semantic_entry_id, payload_kind,
             assistant_text_value, producing_model_call_id,
             assistant_response_part_ordinal, assistant_tool_request_id,
             assistant_response_text_start_bytes)
         VALUES ($1, $2, 'assistant_text', $4, $3, 0, NULL, 0),
                ($1, $5, 'assistant_tool_use', NULL, $3, 1, $6, NULL)",
    )
    .bind(session.into_uuid())
    .bind(Uuid::from_u128(request.into_uuid().as_u128() + 0x2000))
    .bind(producing_call)
    .bind(report_text)
    .bind(Uuid::from_u128(request.into_uuid().as_u128() + 0x3000))
    .bind(request.into_uuid())
    .execute(pool)
    .await?;
    sqlx::query("ALTER TABLE semantic_transcript_entry ENABLE TRIGGER ALL")
        .execute(pool)
        .await?;
    sqlx::query("ALTER TABLE tool_request ENABLE TRIGGER ALL")
        .execute(pool)
        .await?;
    Ok(())
}

/// A goal can close by model declaration while the judge's provider
/// round-trip is outstanding, and `declare_achieved` serializes on the
/// session row without ever taking the scheduler row, so the completion
/// recheck excludes it only by holding the session row from before its
/// authority read until commit. This holds the session row the way every
/// goal transition's first statement does, proves the achievement and then
/// the completion queue behind it in that order, shows completion holds no
/// scheduler lock while it waits — a scheduler-first completion would
/// deadlock the holder's own scheduler acquisition — and requires the
/// completion released after the achievement commits to escalate instead of
/// committing the approval it formed under the discharged statement.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn approval_judge_completion_serializes_with_a_concurrent_goal_achievement()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0x7f80;
    let (fixture, model_repository, _, requests) = checkpoint_tool_batch_with_approval(
        &pool,
        seed,
        APPROVAL_PROPOSAL,
        InitialToolApproval::Delegated,
    )
    .await?;
    let request = requests[0];
    let statement = commission_fixture_session_goal(&pool, fixture.session, seed + 0xf0).await?;
    // The checkpoint accepted the fixture turn's input as `seed + 9`; binding
    // that turn to the generation is the dispatch shape whose authority the
    // judge reads, and it makes the judged turn the generation's current goal
    // turn — the one a model declaration must come from.
    bind_commissioned_goal_to_turn(
        &pool,
        fixture.session,
        fixture.turn,
        AcceptedInputId::from_uuid(Uuid::from_u128(seed + 9)),
    )
    .await?;
    let repository = model_repository.approval_judge_repository();
    let prepared = ready_approval_judge(
        repository
            .prepare(
                fixture.session,
                fixture.turn,
                ModelCallId::from_uuid(Uuid::from_u128(seed + 0xe0)),
                None,
            )
            .await?,
    );
    repository.authorize(&prepared).await?;
    let report_text = String::from("the approval fixture goal is achieved");
    let declaration_request = ToolRequestId::from_uuid(Uuid::from_u128(seed + 0xd0));
    insert_goal_declaration_request(
        &pool,
        fixture.session,
        fixture.turn,
        declaration_request,
        &report_text,
    )
    .await?;

    let mut goal_lock_holder = pool.begin().await?;
    sqlx::query("SELECT session_id FROM session WHERE session_id = $1 FOR NO KEY UPDATE")
        .bind(fixture.session.into_uuid())
        .fetch_one(&mut *goal_lock_holder)
        .await?;
    let achievement_repository = GoalRepository::new(pool.clone());
    let achievement_session = fixture.session;
    let achievement_report =
        GoalReport::try_new(report_text.clone()).expect("the fixture report is admitted");
    let achievement_provenance = GoalModelProvenance::new(fixture.turn, declaration_request);
    let achievement = tokio::spawn(async move {
        achievement_repository
            .declare_achieved(
                achievement_session,
                achievement_report,
                achievement_provenance,
                signalbox_domain::FinishCheckVerdict::Unverified,
            )
            .await
    });
    assert!(
        blocked_backends_reached(&pool, 1).await?,
        "the achievement must wait on the held session row"
    );
    let completion_repository = model_repository.approval_judge_repository();
    let completion_prepared = prepared.clone();
    let completion_rationale =
        ToolDecisionRationale::try_new(String::from(APPROVAL_JUDGE_RATIONALE))?;
    let completion = tokio::spawn(async move {
        completion_repository
            .complete(
                &completion_prepared,
                DelegateApprovalRecommendation::Approve,
                completion_rationale,
                ProviderReportedTokenUsage::unreported(),
                approval_judge_completion_identities(seed, seed + 0xe1),
                approval_judge_closed_result_entry,
            )
            .await
    });
    assert!(
        blocked_backends_reached(&pool, 2).await?,
        "completion must queue on the session row behind the achievement"
    );
    tokio::time::timeout(
        std::time::Duration::from_secs(20),
        sqlx::query("SELECT session_id FROM session_scheduler WHERE session_id = $1 FOR UPDATE")
            .bind(fixture.session.into_uuid())
            .fetch_one(&mut *goal_lock_holder),
    )
    .await
    .expect("the session-row holder must acquire the scheduler while completion waits")?;
    goal_lock_holder.commit().await?;

    let achieved = tokio::time::timeout(std::time::Duration::from_secs(20), achievement)
        .await
        .expect("the released achievement must finish")??;
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(20), completion)
        .await
        .expect("the released completion must finish")??;
    let parked: EscalatedApprovalJudgeProjection = sqlx::query_as(
        "SELECT judge.state_kind AS judge_state,
                judge.recommendation_kind AS recommendation,
                EXISTS (
                    SELECT 1 FROM tool_approval_decision
                     WHERE request_id = judge.request_id
                ) AS decision_exists,
                lifecycle.active_phase_kind AS active_phase,
                lifecycle.approval_tool_request_id
           FROM tool_approval_judge_model_call AS judge
           JOIN turn_lifecycle AS lifecycle
             ON lifecycle.turn_id = judge.turn_id
            AND lifecycle.session_id = judge.session_id
          WHERE judge.model_call_id = $1",
    )
    .bind(prepared.call().into_uuid())
    .fetch_one(&pool)
    .await?;

    assert_eq!(prepared.session_context().goal(), Some(&statement));
    assert_goal_transition_applied(&achieved);
    assert_eq!(outcome, CompleteApprovalJudgeOutcome::EscalatedToHuman);
    assert_eq!(parked.judge_state, "terminal");
    assert_eq!(parked.recommendation, "escalate_to_human");
    assert!(!parked.decision_exists);
    assert_eq!(parked.active_phase, "awaiting_tool_approval");
    assert_eq!(parked.approval_tool_request_id, request.into_uuid());

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn approval_judge_prepared_insert_rejects_estimated_usage_provenance()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0x7eb0;
    let (fixture, _, _, requests) = checkpoint_tool_batch_with_approval(
        &pool,
        seed,
        APPROVAL_PROPOSAL,
        InitialToolApproval::Delegated,
    )
    .await?;
    let [request] = requests.as_slice() else {
        panic!("the fixture has one delegated request")
    };
    let error = sqlx::query(
        "INSERT INTO tool_approval_judge_model_call
            (model_call_id, request_id, session_id, turn_id,
             direct_model_selection_id, resolved_provider_model_identity_id,
             credential_reference, state_kind, usage_provenance_kind)
         VALUES ($1, $2, $3, $4, $5, $6, $7, 'prepared', $8)",
    )
    .bind(Uuid::from_u128(seed + 0xe1))
    .bind(request.into_uuid())
    .bind(fixture.session.into_uuid())
    .bind(fixture.turn.into_uuid())
    .bind(Uuid::from_u128(seed + 0xe2))
    .bind(Uuid::from_u128(seed + 0xe3))
    .bind(APPROVAL_JUDGE_CREDENTIAL)
    .bind(APPROVAL_JUDGE_ESTIMATED_PROVENANCE)
    .execute(&pool)
    .await
    .expect_err("a prepared judge cannot be born with estimated usage provenance");
    assert_eq!(
        database_constraint(&error),
        Some("tool_approval_judge_prepared_usage_is_reported")
    );

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn approval_judge_preparation_serializes_a_concurrent_user_decision()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0x7ec0;
    let (fixture, _, _, request) =
        checkpoint_confirmed_tool_round(&pool, seed, APPROVAL_TOOL_NAME, APPROVAL_ARGUMENTS)
            .await?;
    let mut judge_transaction = pool.begin().await?;
    let (_, judge_call) =
        insert_prepared_judge(&mut judge_transaction, &fixture, request, seed + 0xe0).await?;
    let repository = PostgresToolLoopRepository::new(pool.clone());
    let decision_task = tokio::spawn(async move {
        repository
            .decide(
                decide_tool_request(
                    DurableCommandId::from_uuid(Uuid::from_u128(seed + 0xe8)),
                    request,
                    ToolApprovalDecision::Approve,
                ),
                || TurnAttemptId::from_uuid(Uuid::from_u128(seed + 0xe9)),
            )
            .await
    });
    assert!(
        blocked_backends_reached(&pool, 1).await?,
        "the user decision must wait for judge preparation"
    );

    judge_transaction.commit().await?;
    let decision_error = decision_task
        .await?
        .expect_err("an unfinished judge prevents a concurrent user decision");
    let ToolLoopRepositoryError::Database { source, .. } = decision_error else {
        panic!("the unfinished-judge rejection remains a database constraint")
    };
    assert_eq!(
        database_constraint(&source),
        Some("tool_approval_decision_requires_terminal_judge")
    );
    let durable_state: ApprovalJudgeDurableState = sqlx::query_as(
        "SELECT
            EXISTS (
                SELECT 1 FROM tool_approval_judge_model_call
                 WHERE model_call_id = $1 AND state_kind = 'prepared'
            ) AS prepared_judge_exists,
            EXISTS (
                SELECT 1 FROM tool_approval_decision WHERE request_id = $2
            ) AS decision_exists,
            EXISTS (
                SELECT 1 FROM turn_lifecycle
                 WHERE turn_id = $3 AND session_id = $4
                   AND state_kind = 'active'
                   AND active_phase_kind = 'awaiting_tool_approval'
                   AND approval_tool_request_id = $2
            ) AS active_wait_exists",
    )
    .bind(judge_call)
    .bind(request.into_uuid())
    .bind(fixture.turn.into_uuid())
    .bind(fixture.session.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert!(durable_state.prepared_judge_exists);
    assert!(!durable_state.decision_exists);
    assert!(durable_state.active_wait_exists);

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn approval_decision_insert_serializes_a_concurrent_judge_preparation()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0x7ed0;
    let (fixture, _, _, request) =
        checkpoint_confirmed_tool_round(&pool, seed, APPROVAL_TOOL_NAME, APPROVAL_ARGUMENTS)
            .await?;
    let mut decision_transaction = pool.begin().await?;
    sqlx::query(
        "INSERT INTO tool_approval_decision
            (request_id, decision_kind, decision_source)
         VALUES ($1, 'approve', 'policy_auto')",
    )
    .bind(request.into_uuid())
    .execute(&mut *decision_transaction)
    .await?;
    let judge_pool = pool.clone();
    let judge_task = tokio::spawn(async move {
        let mut judge_transaction = judge_pool.begin().await?;
        let prepared =
            insert_prepared_judge(&mut judge_transaction, &fixture, request, seed + 0xe0).await?;
        judge_transaction.commit().await?;
        Ok::<_, sqlx::Error>(prepared)
    });
    assert!(
        blocked_backends_reached(&pool, 1).await?,
        "judge preparation must wait for the decision's request lock"
    );

    decision_transaction.rollback().await?;
    let (_, judge_call) = judge_task.await??;
    let durable_state: ApprovalJudgeDecisionDurableState = sqlx::query_as(
        "SELECT
            EXISTS (
                SELECT 1 FROM tool_approval_judge_model_call
                 WHERE model_call_id = $1 AND state_kind = 'prepared'
            ) AS prepared_judge_exists,
            EXISTS (
                SELECT 1 FROM tool_approval_decision WHERE request_id = $2
            ) AS decision_exists",
    )
    .bind(judge_call)
    .bind(request.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert!(durable_state.prepared_judge_exists);
    assert!(!durable_state.decision_exists);

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn automatic_policy_decision_requires_no_explicit_event_effect() -> Result<(), Box<dyn Error>>
{
    let (container, pool, _database_url) = migrated_postgres().await?;
    let (_fixture, _repository, _observation, requests) = checkpoint_tool_batch_with_approval(
        &pool,
        APPROVAL_FIXTURE_SEED,
        &[(APPROVAL_TOOL_NAME, APPROVAL_ARGUMENTS)],
        InitialToolApproval::PolicyAuto,
    )
    .await?;
    let [request] = requests.as_slice() else {
        panic!("the automatic fixture returns one request")
    };
    let state: AutomaticApprovalEventState = sqlx::query_as(
        "SELECT
            EXISTS (
                SELECT 1 FROM tool_approval_decision
                 WHERE request_id = $1 AND decision_source = 'policy_auto'
            ) AS decision_exists,
            EXISTS (
                SELECT 1 FROM tool_approval_decided_outbox_event
                 WHERE request_id = $1
            ) AS decided_event_exists",
    )
    .bind(request.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert!(state.decision_exists);
    assert!(!state.decided_event_exists);

    pool.close().await;
    drop(container);
    Ok(())
}

/// S10 / INV-020 / INV-035: a credential-suppressed proposal commits as an
/// inert request plus a fixed runtime-safety denial and leaves the turn running.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s10_inv020_inv035_suppressed_tool_request_is_denied_and_continues()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let (fixture, request) =
        checkpoint_suppressed_tool_round(&pool, APPROVAL_FIXTURE_SEED + 0x90, APPROVAL_TOOL_NAME)
            .await?;
    let stored: (String, String, String, String) = sqlx::query_as(
        "SELECT request.arguments_text, request.approval_posture,
                decision.decision_source, decision.denial_reason
           FROM tool_request AS request
           JOIN tool_approval_decision AS decision USING (request_id)
          WHERE request.request_id = $1",
    )
    .bind(request.into_uuid())
    .fetch_one(&pool)
    .await?;
    let active_phase: String = sqlx::query_scalar(
        "SELECT active_phase_kind FROM turn_lifecycle
          WHERE session_id = $1 AND turn_id = $2",
    )
    .bind(fixture.session.into_uuid())
    .bind(fixture.turn.into_uuid())
    .fetch_one(&pool)
    .await?;

    assert_eq!(stored.0, r#"{"redacted":"[redacted]"}"#);
    assert_eq!(stored.1, "auto");
    assert_eq!(stored.2, "runtime_safety");
    assert_eq!(
        stored.3,
        "Tool arguments were suppressed by the credential boundary"
    );
    assert_eq!(active_phase, "running");

    pool.close().await;
    drop(container);
    Ok(())
}

/// S10 / INV-020: runtime-safety provenance cannot be attached to ordinary
/// provider arguments or a request that retained human approval posture.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s10_inv020_runtime_safety_denial_requires_suppressed_arguments()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let (_fixture, _repository, _observation, request) = checkpoint_confirmed_tool_round(
        &pool,
        APPROVAL_FIXTURE_SEED + 0xa0,
        APPROVAL_TOOL_NAME,
        APPROVAL_ARGUMENTS,
    )
    .await?;
    let error = sqlx::query(
        "INSERT INTO tool_approval_decision
            (request_id, decision_kind, decision_source, denial_reason)
         VALUES ($1, 'deny', 'runtime_safety',
                 'Tool arguments were suppressed by the credential boundary')",
    )
    .bind(request.into_uuid())
    .execute(&pool)
    .await
    .expect_err("ordinary arguments cannot claim credential-boundary suppression");

    assert_eq!(
        database_constraint(&error),
        Some("tool_approval_runtime_safety_requires_suppressed_arguments")
    );

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn approval_guard_automatic_decision_cannot_widen_a_human_request()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let (_fixture, _, _, request) = checkpoint_confirmed_tool_round(
        &pool,
        APPROVAL_FIXTURE_SEED,
        APPROVAL_TOOL_NAME,
        APPROVAL_ARGUMENTS,
    )
    .await?;
    let mut transaction = pool.begin().await?;
    sqlx::query(
        "INSERT INTO tool_approval_decision
            (request_id, decision_kind, decision_source)
         VALUES ($1, 'approve', 'policy_auto')",
    )
    .bind(request.into_uuid())
    .execute(&mut *transaction)
    .await?;
    let error = transaction
        .commit()
        .await
        .expect_err("human posture rejects auto authority");
    assert_eq!(
        database_constraint(&error),
        Some("tool_approval_automatic_requires_auto_posture")
    );

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn approval_guard_judge_completion_respects_posture() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let (human, _, _, human_request) = checkpoint_confirmed_tool_round(
        &pool,
        APPROVAL_FIXTURE_SEED,
        APPROVAL_TOOL_NAME,
        APPROVAL_ARGUMENTS,
    )
    .await?;
    let mut connection = pool.acquire().await?;
    let posture_error = insert_completed_judge(
        &mut connection,
        &human,
        human_request,
        APPROVAL_JUDGE_SEED,
        APPROVAL_RECOMMENDATION,
        None,
        None,
    )
    .await
    .expect_err("a judge cannot approve human-only authority");
    assert_eq!(
        database_constraint(&posture_error),
        Some("tool_approval_judge_recommendation_within_posture")
    );

    drop(connection);
    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn approval_guard_completed_judge_requires_atomic_decision_effect()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let (fixture, _, _, requests) = checkpoint_tool_batch_with_approval(
        &pool,
        APPROVAL_FIXTURE_SEED,
        APPROVAL_PROPOSAL,
        InitialToolApproval::Delegated,
    )
    .await?;
    let [request] = requests.as_slice() else {
        panic!("the fixture has one delegated request")
    };
    let mut transaction = pool.begin().await?;
    insert_completed_judge(
        &mut transaction,
        &fixture,
        *request,
        APPROVAL_JUDGE_SEED,
        APPROVAL_RECOMMENDATION,
        None,
        None,
    )
    .await?;
    let error = transaction
        .commit()
        .await
        .expect_err("completed approve requires its decision, event, and lifecycle effect");
    assert_eq!(
        database_constraint(&error),
        Some("tool_approval_judge_completed_requires_decision_effect")
    );

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn approval_guard_delegate_decision_requires_event_and_lifecycle_effect()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let (fixture, _, _, requests) = checkpoint_tool_batch_with_approval(
        &pool,
        APPROVAL_FIXTURE_SEED,
        APPROVAL_PROPOSAL,
        InitialToolApproval::Delegated,
    )
    .await?;
    let request = requests[0];
    let mut transaction = pool.begin().await?;
    let (selection, call) = insert_completed_judge(
        &mut transaction,
        &fixture,
        request,
        APPROVAL_JUDGE_SEED,
        APPROVAL_RECOMMENDATION,
        None,
        None,
    )
    .await?;
    sqlx::query(
        "INSERT INTO tool_approval_decision
            (request_id, decision_kind, decision_source,
             delegate_model_selection_id, delegate_model_call_id, rationale)
         VALUES ($1, 'approve', 'delegate', $2, $3, $4)",
    )
    .bind(request.into_uuid())
    .bind(selection)
    .bind(call)
    .bind(APPROVAL_JUDGE_RATIONALE)
    .execute(&mut *transaction)
    .await?;
    let error = transaction
        .commit()
        .await
        .expect_err("delegate approval requires its event and advanced lifecycle");
    assert_eq!(
        database_constraint(&error),
        Some("tool_approval_explicit_requires_atomic_effect")
    );

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn approval_guard_user_decision_requires_event_and_lifecycle_effect()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let (_fixture, _, _, request) = checkpoint_confirmed_tool_round(
        &pool,
        APPROVAL_FIXTURE_SEED,
        APPROVAL_TOOL_NAME,
        APPROVAL_ARGUMENTS,
    )
    .await?;
    let command = Uuid::from_u128(APPROVAL_COMMAND_SEED);
    let mut transaction = pool.begin().await?;
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
                 'approve', NULL, 'applied', NULL, NULL)",
    )
    .bind(command)
    .bind(request.into_uuid())
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO tool_approval_decision
            (request_id, decision_kind, decision_source, user_command_id)
         VALUES ($1, 'approve', 'user_command', $2)",
    )
    .bind(request.into_uuid())
    .bind(command)
    .execute(&mut *transaction)
    .await?;
    let error = transaction
        .commit()
        .await
        .expect_err("user approval requires its event and advanced lifecycle");
    assert_eq!(
        database_constraint(&error),
        Some("tool_approval_explicit_requires_atomic_effect")
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// S10 / INV-019: a later request cannot gain a decision while an earlier
/// request in the same proposal batch still owns the approval wait.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s10_inv019_approval_guard_rejects_decision_for_later_request() -> Result<(), Box<dyn Error>>
{
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = APPROVAL_FIXTURE_SEED + 0x100;
    let (fixture, _, _, requests) = checkpoint_confirmed_tool_batch(
        &pool,
        seed,
        &[
            (APPROVAL_TOOL_NAME, APPROVAL_ARGUMENTS),
            ("second-tool", "{}"),
        ],
    )
    .await?;
    let [first_request, later_request] = requests.as_slice() else {
        panic!("the fixture has two ordered approval requests")
    };
    let waiting_request: Uuid = sqlx::query_scalar(
        "SELECT approval_tool_request_id
           FROM turn_lifecycle
          WHERE turn_id = $1 AND session_id = $2",
    )
    .bind(fixture.turn.into_uuid())
    .bind(fixture.session.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(waiting_request, first_request.into_uuid());

    let command = Uuid::from_u128(seed + 0xd0);
    let mut transaction = pool.begin().await?;
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
                 'approve', NULL, 'applied', NULL, NULL)",
    )
    .bind(command)
    .bind(later_request.into_uuid())
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO tool_approval_decision
            (request_id, decision_kind, decision_source, user_command_id)
         VALUES ($1, 'approve', 'user_command', $2)",
    )
    .bind(later_request.into_uuid())
    .bind(command)
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "WITH header AS (
            INSERT INTO outbox_event
                (event_kind, storage_version, session_id)
            VALUES ('tool_approval_decided', 1, $1)
            RETURNING event_sequence, event_kind, storage_version, session_id
         )
         INSERT INTO tool_approval_decided_outbox_event
            (event_sequence, event_kind, storage_version, session_id,
             turn_id, request_id)
         SELECT event_sequence, event_kind, storage_version, session_id,
                $2, $3
           FROM header",
    )
    .bind(fixture.session.into_uuid())
    .bind(fixture.turn.into_uuid())
    .bind(later_request.into_uuid())
    .execute(&mut *transaction)
    .await?;
    let error = transaction
        .commit()
        .await
        .expect_err("a later decision cannot bypass the active approval wait");
    assert_eq!(
        database_constraint(&error),
        Some("tool_approval_explicit_requires_atomic_effect")
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// S10 / INV-019: one transaction cannot collapse multiple explicit approval
/// waits from the same proposal into a single final continuation transition.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s10_inv019_approval_guard_rejects_multiple_decisions_in_one_transaction()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = APPROVAL_FIXTURE_SEED + 0x300;
    let (fixture, _, _, requests) = checkpoint_confirmed_tool_batch(
        &pool,
        seed,
        &[
            (APPROVAL_TOOL_NAME, APPROVAL_ARGUMENTS),
            ("second-tool", "{}"),
        ],
    )
    .await?;
    let [first_request, second_request] = requests.as_slice() else {
        panic!("the fixture has two ordered approval requests")
    };
    let continuation_attempt = TurnAttemptId::from_uuid(Uuid::from_u128(seed + 0xd2));
    let mut transaction = pool.begin().await?;
    insert_user_approval_decision_event(
        &mut transaction,
        &fixture,
        *first_request,
        Uuid::from_u128(seed + 0xd0),
    )
    .await?;
    sqlx::query("SAVEPOINT second_approval")
        .execute(&mut *transaction)
        .await?;
    insert_user_approval_decision_event(
        &mut transaction,
        &fixture,
        *second_request,
        Uuid::from_u128(seed + 0xd1),
    )
    .await?;
    sqlx::query("RELEASE SAVEPOINT second_approval")
        .execute(&mut *transaction)
        .await?;
    sqlx::query(
        "INSERT INTO turn_attempt
            (turn_attempt_id, turn_id, session_id,
             continued_from_attempt_id, state_kind)
         VALUES ($1, $2, $3, $4, 'prepared')",
    )
    .bind(continuation_attempt.into_uuid())
    .bind(fixture.turn.into_uuid())
    .bind(fixture.session.into_uuid())
    .bind(fixture.attempt.into_uuid())
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "UPDATE turn_lifecycle
            SET active_phase_kind = 'running', current_attempt_id = $1,
                approval_tool_request_id = NULL
          WHERE turn_id = $2 AND session_id = $3
            AND state_kind = 'active'
            AND active_phase_kind = 'awaiting_tool_approval'
            AND approval_tool_request_id = $4
            AND active_tool_round_call_id = $5",
    )
    .bind(continuation_attempt.into_uuid())
    .bind(fixture.turn.into_uuid())
    .bind(fixture.session.into_uuid())
    .bind(first_request.into_uuid())
    .bind(fixture.call.into_uuid())
    .execute(&mut *transaction)
    .await?;

    let error = transaction
        .commit()
        .await
        .expect_err("one transaction cannot skip an intermediate approval wait");
    assert_eq!(
        database_constraint(&error),
        Some("tool_approval_explicit_requires_atomic_effect")
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// S10 / INV-019: recovery remains the sole active gate after an earlier
/// automatic request becomes ambiguous; a later human request cannot acquire
/// a decision and event while that recovery wait owns the turn.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s10_inv019_approval_guard_rejects_decision_during_recovery() -> Result<(), Box<dyn Error>>
{
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = APPROVAL_FIXTURE_SEED + 0x200;
    let (fixture, _, _, requests) = checkpoint_tool_batch_with_approval(
        &pool,
        seed,
        &[
            (APPROVAL_TOOL_NAME, APPROVAL_ARGUMENTS),
            ("later-human-tool", "{}"),
        ],
        InitialToolApproval::PolicyAuto,
    )
    .await?;
    let [automatic_request, later_request] = requests.as_slice() else {
        panic!("the recovery fixture has two ordered requests")
    };
    let repository = PostgresToolLoopRepository::new(pool.clone());
    let attempt = ToolAttemptId::from_uuid(Uuid::from_u128(seed + 0xd0));
    repository
        .prepare_next_attempt(
            fixture.session,
            fixture.turn,
            attempt,
            ToolEffectClass::ExternalEffect,
        )
        .await?
        .expect("the automatic request is ready to execute");
    let authorized = repository
        .authorize_attempt(fixture.session, fixture.turn, attempt)
        .await?;
    assert_eq!(authorized.request().id(), *automatic_request);
    let mut recovery_ids = FixedStartupScanIds::new([], []);
    let StartupScanSessionOutcome::RecoveredToolAttempt(outcome) =
        PostgresStartupScanRepository::new(pool.clone())
            .recover(
                fixture.session,
                signalbox_domain::AcceptedInputTurnFailureIdentities::new(
                    SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 0xd2)),
                    ContextFrontierId::from_uuid(Uuid::from_u128(seed + 0xd3)),
                ),
                &mut recovery_ids,
            )
            .await?
    else {
        panic!("the in-flight external attempt becomes a recovery wait")
    };
    let ToolAttemptCrashOutcome::Ambiguous(ended) = *outcome else {
        panic!("the external effect is ambiguous after restart")
    };
    assert_eq!(ended.request(), *automatic_request);
    assert_eq!(ended.end(), &ToolAttemptEnd::Ambiguous);
    let recovery_attempt: Uuid = sqlx::query_scalar(
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
    assert_eq!(recovery_attempt, attempt.into_uuid());

    let mut stale_state = pool.begin().await?;
    sqlx::query("ALTER TABLE tool_approval_decision DISABLE TRIGGER ALL")
        .execute(&mut *stale_state)
        .await?;
    sqlx::query("DELETE FROM tool_approval_decision WHERE request_id = $1")
        .bind(later_request.into_uuid())
        .execute(&mut *stale_state)
        .await?;
    sqlx::query("ALTER TABLE tool_approval_decision ENABLE TRIGGER ALL")
        .execute(&mut *stale_state)
        .await?;
    sqlx::query("ALTER TABLE tool_request DISABLE TRIGGER ALL")
        .execute(&mut *stale_state)
        .await?;
    sqlx::query(
        "UPDATE tool_request
            SET approval_posture = 'human'
          WHERE request_id = $1",
    )
    .bind(later_request.into_uuid())
    .execute(&mut *stale_state)
    .await?;
    sqlx::query("ALTER TABLE tool_request ENABLE TRIGGER ALL")
        .execute(&mut *stale_state)
        .await?;
    stale_state.commit().await?;

    let command = Uuid::from_u128(seed + 0xd1);
    let mut transaction = pool.begin().await?;
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
                 'approve', NULL, 'applied', NULL, NULL)",
    )
    .bind(command)
    .bind(later_request.into_uuid())
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO tool_approval_decision
            (request_id, decision_kind, decision_source, user_command_id)
         VALUES ($1, 'approve', 'user_command', $2)",
    )
    .bind(later_request.into_uuid())
    .bind(command)
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "WITH header AS (
            INSERT INTO outbox_event
                (event_kind, storage_version, session_id)
            VALUES ('tool_approval_decided', 1, $1)
            RETURNING event_sequence, event_kind, storage_version, session_id
         )
         INSERT INTO tool_approval_decided_outbox_event
            (event_sequence, event_kind, storage_version, session_id,
             turn_id, request_id)
         SELECT event_sequence, event_kind, storage_version, session_id,
                $2, $3
           FROM header",
    )
    .bind(fixture.session.into_uuid())
    .bind(fixture.turn.into_uuid())
    .bind(later_request.into_uuid())
    .execute(&mut *transaction)
    .await?;
    let error = transaction
        .commit()
        .await
        .expect_err("a later approval cannot bypass the active recovery wait");
    assert_eq!(
        database_constraint(&error),
        Some("tool_approval_explicit_requires_atomic_effect")
    );

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn approval_guard_unsent_judge_call_rejects_usage() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let (fixture, _, _, requests) = checkpoint_tool_batch_with_approval(
        &pool,
        APPROVAL_FIXTURE_SEED,
        APPROVAL_PROPOSAL,
        InitialToolApproval::Delegated,
    )
    .await?;
    let [request] = requests.as_slice() else {
        panic!("the fixture has one delegated request")
    };
    let mut connection = pool.acquire().await?;
    let (_, call) =
        insert_prepared_judge(&mut connection, &fixture, *request, APPROVAL_JUDGE_SEED).await?;
    let error = sqlx::query(
        "UPDATE tool_approval_judge_model_call
            SET state_kind = 'terminal', terminal_disposition_kind = 'known_failed',
                input_tokens = 1
          WHERE model_call_id = $1",
    )
    .bind(call)
    .execute(&mut *connection)
    .await
    .expect_err("an unsent judge call cannot report provider usage");
    assert_eq!(
        database_constraint(&error),
        Some("tool_approval_judge_unsent_has_no_usage")
    );

    drop(connection);
    pool.close().await;
    drop(container);
    Ok(())
}

/// INV-006: cancelled approval-judge calls never retain provider usage.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv006_cancelled_approval_judge_usage_is_unreported() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let (fixture, _, _, requests) = checkpoint_tool_batch_with_approval(
        &pool,
        APPROVAL_FIXTURE_SEED,
        APPROVAL_PROPOSAL,
        InitialToolApproval::Delegated,
    )
    .await?;
    let [request] = requests.as_slice() else {
        panic!("the fixture has one delegated request")
    };
    let mut connection = pool.acquire().await?;
    let (_, call) =
        insert_prepared_judge(&mut connection, &fixture, *request, APPROVAL_JUDGE_SEED).await?;
    sqlx::query(
        "UPDATE tool_approval_judge_model_call SET state_kind = 'in_flight'
          WHERE model_call_id = $1",
    )
    .bind(call)
    .execute(&mut *connection)
    .await?;
    let error = sqlx::query(
        "UPDATE tool_approval_judge_model_call
            SET state_kind = 'terminal', terminal_disposition_kind = 'cancelled',
                input_tokens = 1
          WHERE model_call_id = $1",
    )
    .bind(call)
    .execute(&mut *connection)
    .await
    .expect_err("a cancelled judge call cannot report provider usage");
    assert_eq!(
        database_constraint(&error),
        Some("tool_approval_judge_call_cancelled_usage_is_unreported")
    );

    drop(connection);
    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn approval_guard_judge_usage_respects_u64_bounds() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let (delegated, _, _, requests) = checkpoint_tool_batch_with_approval(
        &pool,
        APPROVAL_FIXTURE_SEED,
        APPROVAL_PROPOSAL,
        InitialToolApproval::Delegated,
    )
    .await?;
    let [delegated_request] = requests.as_slice() else {
        panic!("the fixture has one delegated request")
    };
    let mut connection = pool.acquire().await?;
    let too_large = Decimal::from(u64::MAX) + Decimal::ONE;
    let usage_error = insert_completed_judge(
        &mut connection,
        &delegated,
        *delegated_request,
        APPROVAL_JUDGE_SEED,
        APPROVAL_RECOMMENDATION,
        Some(too_large),
        None,
    )
    .await
    .expect_err("judge usage above u64 cannot commit");
    assert_eq!(
        database_constraint(&usage_error),
        Some("tool_approval_judge_call_usage_u64_range")
    );

    drop(connection);
    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn approval_guard_judge_usage_rejects_fractional_counts() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let (delegated, _, _, requests) = checkpoint_tool_batch_with_approval(
        &pool,
        APPROVAL_FIXTURE_SEED,
        APPROVAL_PROPOSAL,
        InitialToolApproval::Delegated,
    )
    .await?;
    let [delegated_request] = requests.as_slice() else {
        panic!("the fixture has one delegated request")
    };
    let mut connection = pool.acquire().await?;
    let usage_error = insert_completed_judge(
        &mut connection,
        &delegated,
        *delegated_request,
        APPROVAL_JUDGE_SEED,
        APPROVAL_RECOMMENDATION,
        Some(Decimal::new(15, 1)),
        None,
    )
    .await
    .expect_err("fractional judge usage cannot be rounded into storage");
    assert_eq!(
        database_constraint(&usage_error),
        Some("tool_approval_judge_call_usage_u64_range")
    );

    drop(connection);
    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn approval_guard_user_cannot_decide_delegated_request_before_escalation()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let (_fixture, _, _, requests) = checkpoint_tool_batch_with_approval(
        &pool,
        APPROVAL_FIXTURE_SEED,
        APPROVAL_PROPOSAL,
        InitialToolApproval::Delegated,
    )
    .await?;
    let [request] = requests.as_slice() else {
        panic!("the fixture has one delegated request")
    };
    let error = PostgresToolLoopRepository::new(pool.clone())
        .decide(
            decide_tool_request(
                DurableCommandId::from_uuid(Uuid::from_u128(APPROVAL_COMMAND_SEED)),
                *request,
                ToolApprovalDecision::Approve,
            ),
            || TurnAttemptId::from_uuid(Uuid::from_u128(APPROVAL_NEXT_ATTEMPT_SEED)),
        )
        .await
        .expect_err("delegated authority requires recorded escalation");
    let ToolLoopRepositoryError::Database { source, .. } = error else {
        panic!("the authority guard returns its database constraint")
    };
    assert_eq!(
        database_constraint(&source),
        Some("tool_approval_user_requires_human_authority")
    );

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn explicit_tool_decision_dispatches_full_user_provenance() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let (fixture, _, _, request) = checkpoint_confirmed_tool_round(
        &pool,
        APPROVAL_FIXTURE_SEED,
        APPROVAL_TOOL_NAME,
        APPROVAL_ARGUMENTS,
    )
    .await?;
    let command = DurableCommandId::from_uuid(Uuid::from_u128(APPROVAL_COMMAND_SEED));
    PostgresToolLoopRepository::new(pool.clone())
        .decide(
            decide_tool_request(command, request, ToolApprovalDecision::Approve),
            || TurnAttemptId::from_uuid(Uuid::from_u128(APPROVAL_NEXT_ATTEMPT_SEED)),
        )
        .await?;

    let (event_turn, approval) = dispatched_tool_approval_decision(&pool, request)
        .await?
        .expect("the explicit decision appends its typed outbox event");
    assert_eq!(event_turn, fixture.turn);
    assert_eq!(approval.request(), request);
    assert_eq!(approval.decision(), &ToolApprovalDecision::Approve);
    assert_eq!(
        approval.decider(),
        Some(&ToolApprovalDecider::User { command })
    );
    assert_eq!(approval.rationale(), None);
    let snapshot = ProcessReadRepository::new(pool.clone())
        .read_transcript(fixture.session)
        .await?
        .expect("the decided session has a transcript");
    let projected = process_tool_approval(&snapshot, request)
        .expect("the assistant tool entry retains the explicit decision");
    assert_eq!(projected.decision(), &ToolApprovalDecision::Approve);
    assert_eq!(projected.decider(), ToolApprovalDecider::User { command });
    assert_eq!(projected.rationale(), None);

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn explicit_tool_decision_rejects_sentinel_user_provenance() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let (fixture, _, _, request) = checkpoint_confirmed_tool_round(
        &pool,
        APPROVAL_FIXTURE_SEED,
        APPROVAL_TOOL_NAME,
        APPROVAL_ARGUMENTS,
    )
    .await?;
    PostgresToolLoopRepository::new(pool.clone())
        .decide(
            decide_tool_request(
                DurableCommandId::from_uuid(Uuid::from_u128(APPROVAL_COMMAND_SEED)),
                request,
                ToolApprovalDecision::Approve,
            ),
            || TurnAttemptId::from_uuid(Uuid::from_u128(APPROVAL_NEXT_ATTEMPT_SEED)),
        )
        .await?;
    sqlx::query("ALTER TABLE tool_approval_decision DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query(
        "UPDATE tool_approval_decision
            SET user_command_id = $1
          WHERE request_id = $2",
    )
    .bind(Uuid::nil())
    .bind(request.into_uuid())
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE tool_approval_decision ENABLE TRIGGER ALL")
        .execute(&pool)
        .await?;

    let error = ProcessReadRepository::new(pool.clone())
        .read_transcript(fixture.session)
        .await
        .expect_err("sentinel command provenance fails closed");
    let ProcessReadError::Corruption(corruption) = error else {
        panic!("sentinel command provenance is projection corruption")
    };
    assert_eq!(
        corruption,
        ProcessReadCorruption::Inconsistent("tool approval user command")
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// Drives one delegated request through its judge denial and the
/// continuation that materializes the terminal `tool_denied` result, then
/// checkpoints the next model call.
///
/// Returns the fixture, the repository, the denied request, the checkpointed
/// continuation call, and the denying judge call.
async fn terminal_delegate_denial(
    pool: &PgPool,
    seed: u128,
) -> Result<
    (
        RestartModelCallFixture,
        PostgresModelCallRepository,
        ToolRequestId,
        ModelCallId,
        Uuid,
    ),
    Box<dyn Error>,
> {
    let (fixture, model_repository, _, requests) = checkpoint_tool_batch_with_approval(
        pool,
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
    let judge_call = persist_delegated_denial_fixture(
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

    let continuation_call = ModelCallId::from_uuid(Uuid::from_u128(seed + 0xd4));
    let continuation = model_repository
        .tool_loop_repository()
        .prepare_continuation(
            fixture.session,
            fixture.turn,
            fixture.call,
            signalbox_application::ToolContinuationIdentities::new(
                vec![SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(
                    seed + 0xd2,
                ))],
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 0xd3)),
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
    Ok((
        fixture,
        model_repository,
        *request,
        continuation_call,
        judge_call,
    ))
}

/// The override verification predicate over durable evidence: a delegate
/// denial still resolving inside its round rejects the command, and the
/// same denial admits it once the continuation materializes the terminal
/// denied result; the applied command durably links the denied request, the
/// session, the command, and the denying judge call.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn override_command_records_only_a_terminal_delegate_denial() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0x8e00;
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
    let judge_call = persist_delegated_denial_fixture(
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

    let repository = model_repository.tool_loop_repository();
    let command_id = DurableCommandId::from_uuid(Uuid::from_u128(seed + 0xf0));
    let still_resolving = OverrideDeniedToolRequest::try_new(command_id, fixture.session, *request)
        .expect("the fixture command identity is admitted");
    let rejected = repository.override_denied(still_resolving).await?;
    assert_eq!(
        rejected.result(),
        &OverrideDeniedToolRequestResult::Rejected(
            OverrideDeniedToolRequestRejectedResult::NotTerminallyDenied {
                denied_request: *request,
            }
        )
    );

    let continuation_call = ModelCallId::from_uuid(Uuid::from_u128(seed + 0xd4));
    let continuation = repository
        .prepare_continuation(
            fixture.session,
            fixture.turn,
            fixture.call,
            signalbox_application::ToolContinuationIdentities::new(
                vec![SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(
                    seed + 0xd2,
                ))],
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 0xd3)),
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

    let override_command_id = DurableCommandId::from_uuid(Uuid::from_u128(seed + 0xf1));
    let override_command =
        OverrideDeniedToolRequest::try_new(override_command_id, fixture.session, *request)
            .expect("the fixture command identity is admitted");
    let applied = repository.override_denied(override_command).await?;
    let OverrideDeniedToolRequestResult::Applied(applied_result) = applied.result() else {
        panic!("a terminal delegate denial admits the override")
    };
    let recorded = applied_result.recorded();
    assert_eq!(recorded.command(), override_command_id);
    assert_eq!(recorded.session(), fixture.session);
    assert_eq!(recorded.denied_request(), *request);
    assert_eq!(recorded.judge_call().into_uuid(), judge_call);
    assert_eq!(recorded.tool().as_str(), APPROVAL_TOOL_NAME);
    assert_eq!(recorded.arguments().as_str(), APPROVAL_ARGUMENTS);

    let stored: (Uuid, Uuid, Uuid, String) = sqlx::query_as(
        "SELECT recorded.session_id, recorded.command_id, recorded.judge_model_call_id,
                command.result_kind
           FROM tool_approval_user_override AS recorded
           JOIN override_denied_tool_request_command AS command
             ON command.command_id = recorded.command_id
          WHERE recorded.denied_request_id = $1",
    )
    .bind(request.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(stored.0, fixture.session.into_uuid());
    assert_eq!(stored.1, override_command_id.into_uuid());
    assert_eq!(stored.2, judge_call);
    assert_eq!(stored.3, "applied");

    pool.close().await;
    drop(container);
    Ok(())
}

/// INV-012: an equal override replay returns the recorded receipt, and a
/// distinct fresh command against the same denial records the
/// already-overridden rejection.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv012_override_command_replay_returns_the_recorded_receipt() -> Result<(), Box<dyn Error>>
{
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0x8e40;
    let (fixture, model_repository, request, _, _) = terminal_delegate_denial(&pool, seed).await?;
    let repository = model_repository.tool_loop_repository();
    let command_id = DurableCommandId::from_uuid(Uuid::from_u128(seed + 0xf0));
    let override_command = OverrideDeniedToolRequest::try_new(command_id, fixture.session, request)
        .expect("the fixture command identity is admitted");
    let applied = repository.override_denied(override_command.clone()).await?;
    assert!(matches!(
        applied.result(),
        OverrideDeniedToolRequestResult::Applied(_)
    ));

    let replayed = repository.override_denied(override_command).await?;
    assert_eq!(replayed, applied);
    let reloaded = repository
        .load_recorded_override(command_id)
        .await?
        .expect("the claimed override command loads its recorded receipt");
    assert_eq!(reloaded, applied);

    let second_command_id = DurableCommandId::from_uuid(Uuid::from_u128(seed + 0xf1));
    let second = OverrideDeniedToolRequest::try_new(second_command_id, fixture.session, request)
        .expect("the fixture command identity is admitted");
    let rejected = repository.override_denied(second).await?;
    assert_eq!(
        rejected.result(),
        &OverrideDeniedToolRequestResult::Rejected(
            OverrideDeniedToolRequestRejectedResult::AlreadyOverridden {
                denied_request: request,
            }
        )
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// The continuation that first carries a denial to the model freezes an empty
/// override inventory. This is the designed boundary of the feature, not a
/// defect: that continuation is checkpointed by the very transaction that
/// materializes the terminal `tool_denied` result, so at the instant its
/// inventory freezes no override for that denial can exist — the user has not
/// yet been shown the denial to disagree with. The override the user then
/// records takes effect at the next prepared call, one round later.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn denial_continuation_freezes_an_empty_override_inventory_by_design()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0x8e80;
    let (fixture, model_repository, request, _, _) = terminal_delegate_denial(&pool, seed).await?;
    let repository = model_repository.tool_loop_repository();
    let command_id = DurableCommandId::from_uuid(Uuid::from_u128(seed + 0xf0));
    let override_command = OverrideDeniedToolRequest::try_new(command_id, fixture.session, request)
        .expect("the fixture command identity is admitted");
    let applied = repository.override_denied(override_command).await?;
    assert!(matches!(
        applied.result(),
        OverrideDeniedToolRequestResult::Applied(_)
    ));

    let PrepareInitialModelCallOutcome::Ready {
        recorded_user_overrides,
        ..
    } = model_repository
        .prepare_initial_call(
            fixture.session,
            ModelCallId::from_uuid(Uuid::from_u128(seed + 0xf2)),
            FailedModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 0xf3)),
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 0xf4)),
            ),
            ContextFrontierId::from_uuid(Uuid::from_u128(seed + 0xf5)),
            |_| panic!("the fixture has no pending steering"),
        )
        .await?
    else {
        panic!("the checkpointed continuation call reloads as Ready")
    };
    assert!(
        recorded_user_overrides.is_empty(),
        "the continuation that materialized the denial froze its inventory before any override for that denial could exist"
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// Records a user override of a terminal delegate denial and then reaches the
/// first model call checkpointed after that override exists, so the call's
/// frozen inventory carries it.
///
/// The route is the failed-call retry path: the continuation that carried the
/// denial fails, a fresh input opens the next turn, and the call prepared there
/// is the first checkpointed second. Returns the fixture, the model-call
/// repository, the denied request, the override the applied command recorded,
/// the retry turn, and the freshly checkpointed call.
///
/// Uses seed offsets `0x110` through `0x11b` on top of those
/// `terminal_delegate_denial` reserves.
async fn recorded_override_before_a_fresh_call(
    pool: &PgPool,
    seed: u128,
) -> Result<
    (
        RestartModelCallFixture,
        PostgresModelCallRepository,
        ToolRequestId,
        RecordedUserOverride,
        TurnId,
        ModelCallId,
    ),
    Box<dyn Error>,
> {
    let (fixture, model_repository, request, continuation_call, _) =
        terminal_delegate_denial(pool, seed).await?;
    let repository = model_repository.tool_loop_repository();
    let command = DurableCommandId::from_uuid(Uuid::from_u128(seed + 0xf0));
    let override_command = OverrideDeniedToolRequest::try_new(command, fixture.session, request)
        .expect("the fixture command identity is admitted");
    let applied = repository.override_denied(override_command).await?;
    let OverrideDeniedToolRequestResult::Applied(applied_result) = applied.result() else {
        panic!("the fixture's terminal delegate denial admits the override")
    };
    let recorded = applied_result.recorded().clone();

    // The continuation checkpointed with the denial froze an empty inventory,
    // so it can never consume this override. Fail it, so the retry path
    // checkpoints a call after the override was recorded.
    let AuthorizeModelCallOutcome::Authorized(authorized_continuation) = model_repository
        .authorize_send(fixture.session, continuation_call)
        .await?
    else {
        panic!("the checkpointed continuation call authorizes")
    };
    let failure = authorized_continuation
        .observation_correlation()
        .bind_terminal_observation(ModelCallTerminalObservation::KnownFailed);
    assert!(
        matches!(
            model_repository
                .apply_terminal_observation(
                    fixture.session,
                    failure,
                    ModelCallTerminalIdentities::Failed(FailedModelCallTurnIdentities::new(
                        SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 0x116)),
                        ContextFrontierId::from_uuid(Uuid::from_u128(seed + 0x117)),
                    )),
                    |_| panic!("the fixture has no pending steering to reclassify"),
                )
                .await?,
            ModelCallTerminalOutcome::Failed(_)
        ),
        "the continuation carrying the denial must fail before the retry prepares its call"
    );

    let retry_turn = TurnId::from_uuid(Uuid::from_u128(seed + 0x112));
    assert!(matches!(
        SubmitInputRepository::new(pool.clone())
            .handle(
                start_input(
                    seed + 0x110,
                    seed + 1,
                    "retry after the failed continuation",
                    1,
                    ModelSelectionOverride::UseSessionDefault,
                ),
                AcceptedInputId::from_uuid(Uuid::from_u128(seed + 0x111)),
                Some(retry_turn),
            )
            .await?,
        SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Applied(
            SubmitInputAppliedResult::TurnOrigin(_)
        ))
    ));
    let activated = activate_earliest_queued_turn(
        pool,
        EarliestQueuedTurnActivation {
            session: fixture.session.into_uuid(),
            origin_entry: Uuid::from_u128(seed + 0x113),
            starting_frontier: Uuid::from_u128(seed + 0x114),
            initial_attempt: Uuid::from_u128(seed + 0x115),
        },
    )
    .await?;
    assert_eq!(activated.turn(), retry_turn);

    let consuming_call = ModelCallId::from_uuid(Uuid::from_u128(seed + 0x118));
    assert!(matches!(
        model_repository
            .prepare_initial_call(
                fixture.session,
                consuming_call,
                FailedModelCallTurnIdentities::new(
                    SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 0x119)),
                    ContextFrontierId::from_uuid(Uuid::from_u128(seed + 0x11a)),
                ),
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 0x11b)),
                |_| panic!("the retry turn has no pending steering"),
            )
            .await?,
        PrepareInitialModelCallOutcome::Checkpointed(checkpointed) if checkpointed == consuming_call
    ));
    Ok((
        fixture,
        model_repository,
        request,
        recorded,
        retry_turn,
        consuming_call,
    ))
}

/// S10 / INV-020: an override recorded before a call is checkpointed is frozen
/// into that call, the consuming proposal records approval under
/// `user_override` provenance naming the overridden denial, and the
/// consumption dispatches one decided event carrying that provenance.
///
/// The ordering is the whole point, so the consuming call is reached through
/// the failed-call retry path: the override is recorded first and the call that
/// consumes it is checkpointed second, which is the exact ordering a frozen
/// inventory admits.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn recorded_override_pre_approves_a_call_prepared_after_it() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0x8f40;
    let (fixture, model_repository, request, recorded, retry_turn, consuming_call) =
        recorded_override_before_a_fresh_call(&pool, seed).await?;

    let PrepareInitialModelCallOutcome::Ready {
        recorded_user_overrides,
        ..
    } = model_repository
        .prepare_initial_call(
            fixture.session,
            ModelCallId::from_uuid(Uuid::from_u128(seed + 0x11c)),
            FailedModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 0x11d)),
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 0x11e)),
            ),
            ContextFrontierId::from_uuid(Uuid::from_u128(seed + 0x11f)),
            |_| panic!("the retry turn has no pending steering"),
        )
        .await?
    else {
        panic!("the retry call checkpointed after the override reloads as Ready")
    };
    assert_eq!(
        recorded_user_overrides.as_ref(),
        std::slice::from_ref(&recorded),
        "the call checkpointed after the override freezes exactly the override the command recorded"
    );

    let AuthorizeModelCallOutcome::Authorized(authorized) = model_repository
        .authorize_send(fixture.session, consuming_call)
        .await?
    else {
        panic!("the retry call checkpointed after the override authorizes")
    };
    let response =
        ToolUsingAssistantResponse::try_from_parts(vec![AssistantResponsePart::ToolCall(
            ToolCallProposal::new(
                ToolName::try_new(String::from(APPROVAL_TOOL_NAME)).expect("fixture name is valid"),
                NormalizedToolArguments::try_from_provider_text(String::from(APPROVAL_ARGUMENTS))
                    .expect("fixture arguments are valid"),
            ),
        )])
        .expect("the proposal forms a tool-using response");
    let observation = authorized
        .observation_correlation()
        .bind_terminal_observation(ModelCallTerminalObservation::CompletedWithTools { response });
    let consuming_request = ToolRequestId::from_uuid(Uuid::from_u128(seed + 0x48));
    let outcome = model_repository
        .apply_terminal_observation(
            fixture.session,
            observation,
            ModelCallTerminalIdentities::ToolRound(ToolRoundModelCallIdentities::new(
                vec![ToolResponsePartIdentity::tool_call(
                    SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 0x88)),
                    consuming_request,
                    InitialToolApproval::UserOverride {
                        command: recorded.command(),
                        denied_request: request,
                    },
                )],
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 0xc2)),
                Some(TurnAttemptId::from_uuid(Uuid::from_u128(seed + 0xc3))),
            )),
            |_| panic!("the fixture has no pending steering to reclassify"),
        )
        .await?;
    let ModelCallTerminalOutcome::ToolRound(round) = outcome else {
        panic!("the consuming response reaches a tool round")
    };
    let [consumed] = round.automatic_approvals() else {
        panic!("the consuming round records one proposal-time approval")
    };
    assert_eq!(consumed.source(), ToolDecisionSource::UserOverride);
    assert_eq!(
        consumed.decider(),
        Some(&ToolApprovalDecider::UserOverride {
            command: recorded.command(),
            denied_request: request,
        })
    );

    let stored: (String, String, Uuid) = sqlx::query_as(
        "SELECT decision_kind, decision_source, override_denied_request_id
           FROM tool_approval_decision
          WHERE request_id = $1",
    )
    .bind(consuming_request.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(stored.0, "approve");
    assert_eq!(stored.1, "user_override");
    assert_eq!(stored.2, request.into_uuid());

    let (event_turn, approval) = dispatched_tool_approval_decision(&pool, consuming_request)
        .await?
        .expect("the consuming approval appends its typed outbox event");
    assert_eq!(event_turn, retry_turn);
    assert_eq!(approval.decision(), &ToolApprovalDecision::Approve);
    assert_eq!(
        approval.decider(),
        Some(&ToolApprovalDecider::UserOverride {
            command: recorded.command(),
            denied_request: request,
        })
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// A proposal-time `user_override` approval on a later request in the same
/// batch leaves the earlier delegated request's judge completion final, so an
/// ambiguous replay must still check the persisted continuation identity.
///
/// The nonfinality probe reads a later request as evidence of a subsequent
/// round only when its decision landed after the batch was proposed. The
/// proposing transaction records a `user_override` approval itself, consuming
/// the one-shot override from the producing call's frozen inventory, so
/// counting it as a later decision would classify this completion as nonfinal
/// and make the replay accept whatever continuation identity it is handed —
/// masking exactly the mismatch this test supplies.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn judge_completion_replay_rejects_a_mismatch_behind_a_user_override_approval()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0x8f80;
    let (fixture, model_repository, request, recorded, retry_turn, consuming_call) =
        recorded_override_before_a_fresh_call(&pool, seed).await?;

    // One batch, two requests: the earlier one parks for the judge, the later
    // one consumes the recorded override at proposal time.
    let AuthorizeModelCallOutcome::Authorized(authorized) = model_repository
        .authorize_send(fixture.session, consuming_call)
        .await?
    else {
        panic!("the retry call checkpointed after the override authorizes")
    };
    let response = ToolUsingAssistantResponse::try_from_parts(vec![
        AssistantResponsePart::ToolCall(ToolCallProposal::new(
            ToolName::try_new(String::from(JUDGED_TOOL_NAME)).expect("fixture name is valid"),
            NormalizedToolArguments::try_from_provider_text(String::from(APPROVAL_ARGUMENTS))
                .expect("fixture arguments are valid"),
        )),
        AssistantResponsePart::ToolCall(ToolCallProposal::new(
            ToolName::try_new(String::from(APPROVAL_TOOL_NAME)).expect("fixture name is valid"),
            NormalizedToolArguments::try_from_provider_text(String::from(APPROVAL_ARGUMENTS))
                .expect("fixture arguments are valid"),
        )),
    ])
    .expect("the two proposals form a tool-using response");
    let observation = authorized
        .observation_correlation()
        .bind_terminal_observation(ModelCallTerminalObservation::CompletedWithTools { response });
    let judged_request = ToolRequestId::from_uuid(Uuid::from_u128(seed + 0x120));
    let overridden_request = ToolRequestId::from_uuid(Uuid::from_u128(seed + 0x121));
    let outcome = model_repository
        .apply_terminal_observation(
            fixture.session,
            observation,
            ModelCallTerminalIdentities::ToolRound(ToolRoundModelCallIdentities::new(
                vec![
                    ToolResponsePartIdentity::tool_call(
                        SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 0x122)),
                        judged_request,
                        InitialToolApproval::Delegated,
                    ),
                    ToolResponsePartIdentity::tool_call(
                        SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 0x123)),
                        overridden_request,
                        InitialToolApproval::UserOverride {
                            command: recorded.command(),
                            denied_request: request,
                        },
                    ),
                ],
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 0x124)),
                None,
            )),
            |_| panic!("the retry turn has no pending steering to reclassify"),
        )
        .await?;
    let ModelCallTerminalOutcome::ToolRound(round) = outcome else {
        panic!("the mixed batch reaches a tool round")
    };
    assert_eq!(
        round.next_phase(),
        &ActiveTurnPhase::AwaitingApproval {
            request: judged_request,
        },
        "the earlier delegated request is the one the judge must decide"
    );
    let overridden_source: String = sqlx::query_scalar(
        "SELECT decision_source FROM tool_approval_decision WHERE request_id = $1",
    )
    .bind(overridden_request.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        overridden_source, "user_override",
        "the later request must carry the proposal-time source this probe has to recognize"
    );

    let judge = model_repository.approval_judge_repository();
    let prepared = ready_approval_judge(
        judge
            .prepare(
                fixture.session,
                retry_turn,
                ModelCallId::from_uuid(Uuid::from_u128(seed + 0x125)),
                None,
            )
            .await?,
    );
    drop(authorized_approval_judge(judge.authorize(&prepared).await?));
    let rationale = ToolDecisionRationale::try_new(String::from(APPROVAL_JUDGE_RATIONALE))?;
    let persisted_attempt = TurnAttemptId::from_uuid(Uuid::from_u128(seed + 0x126));
    let conflicting_attempt = TurnAttemptId::from_uuid(Uuid::from_u128(seed + 0x127));
    assert_eq!(
        judge
            .complete(
                &prepared,
                DelegateApprovalRecommendation::Approve,
                rationale.clone(),
                ProviderReportedTokenUsage::unreported(),
                approval_judge_completion_identities(
                    seed,
                    persisted_attempt.into_uuid().as_u128(),
                ),
                approval_judge_closed_result_entry,
            )
            .await?,
        CompleteApprovalJudgeOutcome::Decided
    );
    let error = judge
        .complete(
            &prepared,
            DelegateApprovalRecommendation::Approve,
            rationale,
            ProviderReportedTokenUsage::unreported(),
            approval_judge_completion_identities(
                seed + 0x10,
                conflicting_attempt.into_uuid().as_u128(),
            ),
            approval_judge_closed_result_entry,
        )
        .await
        .expect_err("a replay behind a proposal-time override cannot substitute another continuation identity");
    assert_eq!(
        error.operator_failure_class(),
        OperatorFailureClass::FailClosedCorruption,
        "unexpected replay error: {error:?}"
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// The UNIQUE consumption column is the durable one-shot boundary: a second
/// decision row naming the same recorded override cannot exist under any
/// interleaving.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn user_override_consumption_is_unique_per_recorded_override() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0x8ec0;
    let (fixture, model_repository, request, _, _) = terminal_delegate_denial(&pool, seed).await?;
    let repository = model_repository.tool_loop_repository();
    let command_id = DurableCommandId::from_uuid(Uuid::from_u128(seed + 0xf0));
    let override_command = OverrideDeniedToolRequest::try_new(command_id, fixture.session, request)
        .expect("the fixture command identity is admitted");
    let applied = repository.override_denied(override_command).await?;
    assert!(matches!(
        applied.result(),
        OverrideDeniedToolRequestResult::Applied(_)
    ));

    let mut transaction = pool.begin().await?;
    sqlx::query(
        "INSERT INTO tool_request
            (request_id, session_id, turn_id, producing_model_call_id,
             request_ordinal, tool_name, arguments_kind, arguments_text,
             approval_posture)
         VALUES ($1, $2, $3, $4, 1, $5, 'json', $6, 'delegated')",
    )
    .bind(Uuid::from_u128(seed + 0x49))
    .bind(fixture.session.into_uuid())
    .bind(fixture.turn.into_uuid())
    .bind(fixture.call.into_uuid())
    .bind(APPROVAL_TOOL_NAME)
    .bind(APPROVAL_ARGUMENTS)
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO tool_request
            (request_id, session_id, turn_id, producing_model_call_id,
             request_ordinal, tool_name, arguments_kind, arguments_text,
             approval_posture)
         VALUES ($1, $2, $3, $4, 2, $5, 'json', $6, 'delegated')",
    )
    .bind(Uuid::from_u128(seed + 0x4a))
    .bind(fixture.session.into_uuid())
    .bind(fixture.turn.into_uuid())
    .bind(fixture.call.into_uuid())
    .bind(APPROVAL_TOOL_NAME)
    .bind(APPROVAL_ARGUMENTS)
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO tool_approval_decision
            (request_id, decision_kind, decision_source,
             override_denied_request_id)
         VALUES ($1, 'approve', 'user_override', $2)",
    )
    .bind(Uuid::from_u128(seed + 0x49))
    .bind(request.into_uuid())
    .execute(&mut *transaction)
    .await?;
    let error = sqlx::query(
        "INSERT INTO tool_approval_decision
            (request_id, decision_kind, decision_source,
             override_denied_request_id)
         VALUES ($1, 'approve', 'user_override', $2)",
    )
    .bind(Uuid::from_u128(seed + 0x4a))
    .bind(request.into_uuid())
    .execute(&mut *transaction)
    .await
    .expect_err("a second consumption of one recorded override is impossible");
    assert_eq!(
        database_constraint(&error),
        Some("tool_approval_decision_override_denied_request_id_key")
    );
    transaction.rollback().await?;

    pool.close().await;
    drop(container);
    Ok(())
}

/// The override guard requires a terminal delegate denial: a denial the judge
/// recorded whose denied result is still unmaterialized cannot carry an
/// recorded override even when its command rows are fabricated directly.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn user_override_guard_requires_a_terminal_delegate_denial() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0x8f00;
    let (fixture, _, _, requests) = checkpoint_tool_batch_with_approval(
        &pool,
        seed,
        APPROVAL_PROPOSAL,
        InitialToolApproval::Delegated,
    )
    .await?;
    let [request] = requests.as_slice() else {
        panic!("the fixture has one delegated request")
    };
    let mut denial = pool.begin().await?;
    let judge_call = persist_delegated_denial_fixture(
        &mut denial,
        &fixture,
        *request,
        seed + 0xe0,
        TurnAttemptId::from_uuid(Uuid::from_u128(seed + 0xd1)),
        None,
        None,
    )
    .await?;
    denial.commit().await?;

    let mut transaction = pool.begin().await?;
    let command = Uuid::from_u128(seed + 0xf0);
    sqlx::query(
        "INSERT INTO durable_command
            (command_id, command_kind, storage_version, claimed_at, issuer_kind)
         VALUES ($1, 'override_denied_tool_request', 1, transaction_timestamp(), 'operator')",
    )
    .bind(command)
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO override_denied_tool_request_command
            (command_id, command_kind, storage_version, session_id,
             request_id, result_kind, rejection_kind)
         VALUES ($1, 'override_denied_tool_request', 1, $2, $3, 'applied', NULL)",
    )
    .bind(command)
    .bind(fixture.session.into_uuid())
    .bind(request.into_uuid())
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO tool_approval_user_override
            (denied_request_id, session_id, command_id, judge_model_call_id)
         VALUES ($1, $2, $3, $4)",
    )
    .bind(request.into_uuid())
    .bind(fixture.session.into_uuid())
    .bind(command)
    .bind(judge_call)
    .execute(&mut *transaction)
    .await?;
    let error = transaction
        .commit()
        .await
        .expect_err("an undenied request cannot carry a recorded override");
    assert_eq!(
        database_constraint(&error),
        Some("tool_approval_user_override_requires_terminal_denial")
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// Drives the whole sequence in which another authority lets the denied command
/// through before any call can consume the override: a terminal delegate
/// denial, a recorded override, the immutable first post-denial call
/// re-proposing that exact command, and the judge deciding the re-proposal on
/// its own.
///
/// The re-proposal is `Delegated` rather than `UserOverride` because the call
/// carrying it froze an empty inventory by design, so nothing pre-approves it
/// and the judge is the only authority that can decide it. An approved
/// re-proposal then executes and resolves its round; a denied one materializes
/// its denied result directly. Either way the round continues into a freshly
/// checkpointed call, whose frozen inventory is what the callers assert on.
///
/// Returns the fixture, the model-call repository, the denied request, the
/// re-proposal of it, the override the applied command recorded, and the
/// freshly checkpointed call.
///
/// Uses seed offsets `0xf0` and `0x120` through `0x12b` on top of those
/// `terminal_delegate_denial` reserves.
async fn judged_reproposal_after_a_recorded_override(
    pool: &PgPool,
    seed: u128,
    recommendation: DelegateApprovalRecommendation,
) -> Result<
    (
        RestartModelCallFixture,
        PostgresModelCallRepository,
        ToolRequestId,
        ToolRequestId,
        RecordedUserOverride,
        ModelCallId,
    ),
    Box<dyn Error>,
> {
    let (fixture, model_repository, request, continuation_call, _) =
        terminal_delegate_denial(pool, seed).await?;
    let tool_repository = model_repository.tool_loop_repository();
    let command = DurableCommandId::from_uuid(Uuid::from_u128(seed + 0xf0));
    let override_command = OverrideDeniedToolRequest::try_new(command, fixture.session, request)
        .expect("the fixture command identity is admitted");
    let applied = tool_repository.override_denied(override_command).await?;
    let OverrideDeniedToolRequestResult::Applied(applied_result) = applied.result() else {
        panic!("the fixture's terminal delegate denial admits the override")
    };
    let recorded = applied_result.recorded().clone();

    // The call that carried the denial froze an empty inventory, so its
    // re-proposal of the denied command parks for the judge like any other.
    let AuthorizeModelCallOutcome::Authorized(authorized) = model_repository
        .authorize_send(fixture.session, continuation_call)
        .await?
    else {
        panic!("the checkpointed continuation call authorizes")
    };
    let response =
        ToolUsingAssistantResponse::try_from_parts(vec![AssistantResponsePart::ToolCall(
            ToolCallProposal::new(
                ToolName::try_new(String::from(APPROVAL_TOOL_NAME)).expect("fixture name is valid"),
                NormalizedToolArguments::try_from_provider_text(String::from(APPROVAL_ARGUMENTS))
                    .expect("fixture arguments are valid"),
            ),
        )])
        .expect("the re-proposal forms a tool-using response");
    let observation = authorized
        .observation_correlation()
        .bind_terminal_observation(ModelCallTerminalObservation::CompletedWithTools { response });
    let reproposal = ToolRequestId::from_uuid(Uuid::from_u128(seed + 0x120));
    let outcome = model_repository
        .apply_terminal_observation(
            fixture.session,
            observation,
            ModelCallTerminalIdentities::ToolRound(ToolRoundModelCallIdentities::new(
                vec![ToolResponsePartIdentity::tool_call(
                    SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 0x121)),
                    reproposal,
                    InitialToolApproval::Delegated,
                )],
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 0x122)),
                None,
            )),
            |_| panic!("the fixture has no pending steering to reclassify"),
        )
        .await?;
    let ModelCallTerminalOutcome::ToolRound(round) = outcome else {
        panic!("the re-proposal reaches a tool round")
    };
    assert_eq!(
        round.next_phase(),
        &ActiveTurnPhase::AwaitingApproval {
            request: reproposal,
        },
        "an empty frozen inventory leaves the re-proposal parked for the judge"
    );

    let judge = model_repository.approval_judge_repository();
    let prepared = ready_approval_judge(
        judge
            .prepare(
                fixture.session,
                fixture.turn,
                ModelCallId::from_uuid(Uuid::from_u128(seed + 0x123)),
                None,
            )
            .await?,
    );
    assert_eq!(prepared.request().id(), reproposal);
    drop(authorized_approval_judge(judge.authorize(&prepared).await?));
    let rationale = ToolDecisionRationale::try_new(String::from(APPROVAL_JUDGE_RATIONALE))?;
    assert_eq!(
        judge
            .complete(
                &prepared,
                recommendation,
                rationale,
                ProviderReportedTokenUsage::unreported(),
                approval_judge_completion_identities(seed, seed + 0x124),
                approval_judge_closed_result_entry,
            )
            .await?,
        CompleteApprovalJudgeOutcome::Decided
    );
    if recommendation == DelegateApprovalRecommendation::Approve {
        let attempt = ToolAttemptId::from_uuid(Uuid::from_u128(seed + 0x125));
        tool_repository
            .prepare_next_attempt(
                fixture.session,
                fixture.turn,
                attempt,
                ToolEffectClass::EffectFree,
            )
            .await?;
        let authority = tool_repository
            .authorize_attempt(fixture.session, fixture.turn, attempt)
            .await?;
        tool_repository
            .commit_observation(
                authority
                    .executor_fence()
                    .bind(ToolAttemptObservation::Completed {
                        result: ToolResultContent::Text(
                            ToolResultText::try_new(String::from("2026-08-16T12:00:00Z"))
                                .expect("bounded fixture result"),
                        ),
                    }),
            )
            .await?;
    }

    let next_call = ModelCallId::from_uuid(Uuid::from_u128(seed + 0x126));
    assert_eq!(
        tool_repository
            .prepare_continuation(
                fixture.session,
                fixture.turn,
                continuation_call,
                signalbox_application::ToolContinuationIdentities::new(
                    vec![SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(
                        seed + 0x127,
                    ))],
                    ContextFrontierId::from_uuid(Uuid::from_u128(seed + 0x128)),
                    next_call,
                    FailedModelCallTurnIdentities::new(
                        SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 0x129)),
                        ContextFrontierId::from_uuid(Uuid::from_u128(seed + 0x12a)),
                    ),
                    ContextFrontierId::from_uuid(Uuid::from_u128(seed + 0x12b)),
                ),
                |_| panic!("the fixture has no pending steering"),
            )
            .await?,
        signalbox_application::PrepareToolContinuationOutcome::Checkpointed(next_call)
    );
    Ok((
        fixture,
        model_repository,
        request,
        reproposal,
        recorded,
        next_call,
    ))
}

/// Reads the decision recorded against one request as its kind, its source, and
/// the recorded override it consumed.
async fn stored_tool_approval_decision(
    pool: &PgPool,
    request: ToolRequestId,
) -> Result<(String, String, Option<Uuid>), sqlx::Error> {
    sqlx::query_as(
        "SELECT decision_kind, decision_source, override_denied_request_id
           FROM tool_approval_decision
          WHERE request_id = $1",
    )
    .bind(request.into_uuid())
    .fetch_one(pool)
    .await
}

/// An override is retired once the command it authorizes has been let through
/// since the denial by some other authority, not only by the consuming
/// `user_override` approval that names it.
///
/// The immutable first post-denial call cannot carry the override, so its
/// re-proposal of the denied command is decided by the judge alone and the
/// resulting approval names no override. Retaining the override past that
/// approval would let the next prepared call pre-approve yet another identical
/// proposal, repeating a side-effecting command the session has already run
/// once — so the call prepared after that approval must freeze nothing.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn an_approved_matching_request_after_the_denial_retires_the_override()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0x9000;
    let (fixture, model_repository, _, reproposal, _, next_call) =
        judged_reproposal_after_a_recorded_override(
            &pool,
            seed,
            DelegateApprovalRecommendation::Approve,
        )
        .await?;

    assert_eq!(
        stored_tool_approval_decision(&pool, reproposal).await?,
        (String::from("approve"), String::from("delegate"), None),
        "the judge, not the override, is what let the re-proposal through"
    );
    let frozen: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM model_call_user_override WHERE model_call_id = $1",
    )
    .bind(next_call.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        frozen, 0,
        "the call prepared after the approval freezes no override"
    );

    let PrepareInitialModelCallOutcome::Ready {
        recorded_user_overrides,
        ..
    } = model_repository
        .prepare_initial_call(
            fixture.session,
            ModelCallId::from_uuid(Uuid::from_u128(seed + 0x12c)),
            FailedModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 0x12d)),
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 0x12e)),
            ),
            ContextFrontierId::from_uuid(Uuid::from_u128(seed + 0x12f)),
            |_| panic!("the fixture has no pending steering"),
        )
        .await?
    else {
        panic!("the checkpointed continuation call reloads as Ready")
    };
    assert!(
        recorded_user_overrides.is_empty(),
        "an override whose command another authority already let through cannot pre-approve a repeat"
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// A matching request denied again after the denial leaves the override
/// standing: the user's disagreement is still unsatisfied, so the next prepared
/// call must freeze it.
///
/// This is the boundary the retirement scope must not cross. The sequence is
/// the approval test's, differing only in the judge's recommendation, so a
/// retirement rule keyed on anything looser than an approval recorded after the
/// denial would retire this override too and break the feature outright.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_denied_matching_request_after_the_denial_keeps_the_override()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0x9040;
    let (fixture, model_repository, _, reproposal, recorded, next_call) =
        judged_reproposal_after_a_recorded_override(
            &pool,
            seed,
            DelegateApprovalRecommendation::Deny,
        )
        .await?;

    assert_eq!(
        stored_tool_approval_decision(&pool, reproposal).await?,
        (String::from("deny"), String::from("delegate"), None),
        "the judge denied the re-proposal a second time"
    );
    let frozen: Vec<Uuid> = sqlx::query_scalar(
        "SELECT denied_request_id FROM model_call_user_override WHERE model_call_id = $1",
    )
    .bind(next_call.into_uuid())
    .fetch_all(&pool)
    .await?;
    assert_eq!(
        frozen,
        vec![recorded.denied_request().into_uuid()],
        "the call prepared after the second denial still freezes the override"
    );

    let PrepareInitialModelCallOutcome::Ready {
        recorded_user_overrides,
        ..
    } = model_repository
        .prepare_initial_call(
            fixture.session,
            ModelCallId::from_uuid(Uuid::from_u128(seed + 0x12c)),
            FailedModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 0x12d)),
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 0x12e)),
            ),
            ContextFrontierId::from_uuid(Uuid::from_u128(seed + 0x12f)),
            |_| panic!("the fixture has no pending steering"),
        )
        .await?
    else {
        panic!("the checkpointed continuation call reloads as Ready")
    };
    assert_eq!(
        recorded_user_overrides.as_ref(),
        std::slice::from_ref(&recorded),
        "a command denied again leaves the user's override with work left to do"
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// One `injection_settled` receipt as stored.
#[derive(Debug, PartialEq, sqlx::FromRow)]
struct InjectionReceipt {
    outcome_kind: String,
    rejection_kind: Option<String>,
    delivered_turn_id: Option<Uuid>,
}

impl InjectionReceipt {
    fn delivered(turn: TurnId) -> Self {
        Self {
            outcome_kind: String::from("delivered"),
            rejection_kind: None,
            delivered_turn_id: Some(turn.into_uuid()),
        }
    }

    fn not_delivered() -> Self {
        Self {
            outcome_kind: String::from("not_delivered"),
            rejection_kind: None,
            delivered_turn_id: None,
        }
    }

    fn rejected(kind: &str) -> Self {
        Self {
            outcome_kind: String::from("rejected"),
            rejection_kind: Some(String::from(kind)),
            delivered_turn_id: None,
        }
    }
}

async fn injection_receipt(
    pool: &PgPool,
    command: DurableCommandId,
) -> Result<Option<InjectionReceipt>, sqlx::Error> {
    sqlx::query_as(
        "SELECT outcome_kind, rejection_kind, delivered_turn_id
           FROM injection_settled_outbox_event
          WHERE command_id = $1",
    )
    .bind(command.into_uuid())
    .fetch_optional(pool)
    .await
}

/// An approval decision is a durable injection. It settles `delivered`
/// to the request's turn, and a restart scan leaves the decided round intact
/// for the ordinary scheduler to resume.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn approval_decision_survives_restart_and_settles_delivered() -> Result<(), Box<dyn Error>> {
    let (container, pool, database_url) = migrated_postgres().await?;
    let seed = APPROVAL_FIXTURE_SEED + 0x700;
    let (fixture, _, _, request) =
        checkpoint_confirmed_tool_round(&pool, seed, APPROVAL_TOOL_NAME, APPROVAL_ARGUMENTS)
            .await?;
    let command = DurableCommandId::from_uuid(Uuid::from_u128(seed + 0xd0));
    PostgresToolLoopRepository::new(pool.clone())
        .decide(
            decide_tool_request(command, request, ToolApprovalDecision::Approve),
            || TurnAttemptId::from_uuid(Uuid::from_u128(seed + 0xd1)),
        )
        .await?;
    assert_eq!(
        injection_receipt(&pool, command).await?,
        Some(InjectionReceipt::delivered(fixture.turn))
    );

    pool.close().await;
    let restarted_pool = PgPoolOptions::new()
        .max_connections(4)
        .connect_with(local_test_connection_options(&database_url)?)
        .await?;
    let outcome = PostgresStartupScanRepository::new(restarted_pool.clone())
        .recover(
            fixture.session,
            signalbox_domain::AcceptedInputTurnFailureIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 0xd2)),
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 0xd3)),
            ),
            &mut signalbox_application::UuidV7StartupScanIdGenerator,
        )
        .await?;
    assert_eq!(
        outcome,
        StartupScanSessionOutcome::ResumableToolBatch { turn: fixture.turn }
    );
    let decided: (String, String) = sqlx::query_as(
        "SELECT decision.decision_kind, turn.state_kind
           FROM tool_approval_decision AS decision
           JOIN tool_request AS request USING (request_id)
           JOIN turn_lifecycle AS turn ON turn.turn_id = request.turn_id
          WHERE decision.request_id = $1",
    )
    .bind(request.into_uuid())
    .fetch_one(&restarted_pool)
    .await?;
    assert_eq!(decided, (String::from("approve"), String::from("active")));

    restarted_pool.close().await;
    drop(container);
    Ok(())
}

/// A drain that cuts a decision mid-transaction leaves no partial claim,
/// so the same command applies after restart; decisions committed before the
/// drain are all still there. Zero approvals are lost either way.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn drain_then_restart_loses_no_approvals() -> Result<(), Box<dyn Error>> {
    let (container, pool, database_url) = migrated_postgres().await?;
    let first = parked_approval(&pool, APPROVAL_FIXTURE_SEED + 0x800).await?;
    let second = parked_approval(&pool, APPROVAL_FIXTURE_SEED + 0x900).await?;
    let cut = parked_approval(&pool, APPROVAL_FIXTURE_SEED + 0xa00).await?;
    let repository = PostgresToolLoopRepository::new(pool.clone());
    approve(&repository, &first).await?;
    approve(&repository, &second).await?;
    // The drain interrupts this decision after it claimed its command and
    // before it committed: the transaction is dropped, not committed.
    let mut interrupted = pool.begin().await?;
    sqlx::query(
        "INSERT INTO durable_command
            (command_id, command_kind, storage_version, claimed_at, issuer_kind)
         VALUES ($1, 'decide_tool_request', 1, transaction_timestamp(), 'operator')",
    )
    .bind(cut.command.into_uuid())
    .execute(&mut *interrupted)
    .await?;
    drop(interrupted);
    drop(repository);
    pool.close().await;

    let restarted_pool = PgPoolOptions::new()
        .max_connections(4)
        .connect_with(local_test_connection_options(&database_url)?)
        .await?;
    let mut scan = StartupScanService::new(
        signalbox_application::UuidV7StartupScanIdGenerator,
        PostgresStartupScanRepository::new(restarted_pool.clone()),
    );
    assert_eq!(scan.execute().await?.recovered_turn_count(), 0);
    let replayed = approve(
        &PostgresToolLoopRepository::new(restarted_pool.clone()),
        &cut,
    )
    .await?;
    assert!(matches!(
        replayed.result(),
        DecideToolRequestResult::Applied(_)
    ));
    assert_approved_and_delivered(&restarted_pool, &first).await?;
    assert_approved_and_delivered(&restarted_pool, &second).await?;
    assert_approved_and_delivered(&restarted_pool, &cut).await?;

    restarted_pool.close().await;
    drop(container);
    Ok(())
}

/// One session parked on a single approval request, with the identities its
/// decision will use.
struct ParkedApproval {
    fixture: RestartModelCallFixture,
    request: ToolRequestId,
    command: DurableCommandId,
    next_attempt: TurnAttemptId,
}

async fn parked_approval(pool: &PgPool, seed: u128) -> Result<ParkedApproval, Box<dyn Error>> {
    let (fixture, _, _, request) =
        checkpoint_confirmed_tool_round(pool, seed, APPROVAL_TOOL_NAME, APPROVAL_ARGUMENTS).await?;
    Ok(ParkedApproval {
        fixture,
        request,
        command: DurableCommandId::from_uuid(Uuid::from_u128(seed + 0xd0)),
        next_attempt: TurnAttemptId::from_uuid(Uuid::from_u128(seed + 0xd1)),
    })
}

async fn approve(
    repository: &PostgresToolLoopRepository,
    parked: &ParkedApproval,
) -> Result<signalbox_domain::PreparedDecideToolRequest, ToolLoopRepositoryError> {
    repository
        .decide(
            decide_tool_request(
                parked.command,
                parked.request,
                ToolApprovalDecision::Approve,
            ),
            || parked.next_attempt,
        )
        .await
}

async fn assert_approved_and_delivered(
    pool: &PgPool,
    parked: &ParkedApproval,
) -> Result<(), Box<dyn Error>> {
    let decided: (String, String) = sqlx::query_as(
        "SELECT decision.decision_kind, turn.active_phase_kind
           FROM tool_approval_decision AS decision
           JOIN tool_request AS request USING (request_id)
           JOIN turn_lifecycle AS turn ON turn.turn_id = request.turn_id
          WHERE decision.request_id = $1",
    )
    .bind(parked.request.into_uuid())
    .fetch_one(pool)
    .await?;
    assert_eq!(decided, (String::from("approve"), String::from("running")));
    assert_eq!(
        injection_receipt(pool, parked.command).await?,
        Some(InjectionReceipt::delivered(parked.fixture.turn))
    );
    Ok(())
}

/// A decision arriving after its request was decided settles
/// `not_delivered` and is never applied to a different request.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn late_decision_settles_not_delivered() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = APPROVAL_FIXTURE_SEED + 0xb00;
    let (_, _, _, request) =
        checkpoint_confirmed_tool_round(&pool, seed, APPROVAL_TOOL_NAME, APPROVAL_ARGUMENTS)
            .await?;
    let repository = PostgresToolLoopRepository::new(pool.clone());
    let first = DurableCommandId::from_uuid(Uuid::from_u128(seed + 0xd0));
    repository
        .decide(
            decide_tool_request(first, request, ToolApprovalDecision::Approve),
            || TurnAttemptId::from_uuid(Uuid::from_u128(seed + 0xd1)),
        )
        .await?;
    let late = DurableCommandId::from_uuid(Uuid::from_u128(seed + 0xd2));
    let outcome = repository
        .decide(
            decide_tool_request(
                late,
                request,
                ToolApprovalDecision::Deny {
                    reason: Some(
                        ToolDenialReason::try_new(String::from("too late"))
                            .expect("fixture denial reason is admitted"),
                    ),
                },
            ),
            || panic!("a late decision opens no attempt"),
        )
        .await?;
    assert_eq!(
        outcome.result(),
        &DecideToolRequestResult::Rejected(
            signalbox_domain::DecideToolRequestRejectedResult::AlreadyResolved { request }
        )
    );
    assert_eq!(
        injection_receipt(&pool, late).await?,
        Some(InjectionReceipt::not_delivered())
    );
    let decisions: i64 =
        sqlx::query_scalar("SELECT count(*) FROM tool_approval_decision WHERE request_id = $1")
            .bind(request.into_uuid())
            .fetch_one(&pool)
            .await?;
    assert_eq!(decisions, 1);

    pool.close().await;
    drop(container);
    Ok(())
}

/// The correlation contract stands. A decision naming a later request
/// settles `rejected`, and one naming no request records its typed rejection
/// with no session to carry a receipt.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn decision_correlation_mismatches_stay_typed_rejections() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = APPROVAL_FIXTURE_SEED + 0xc00;
    let (_, _, _, requests) = checkpoint_confirmed_tool_batch(
        &pool,
        seed,
        &[
            (APPROVAL_TOOL_NAME, APPROVAL_ARGUMENTS),
            ("second-tool", "{}"),
        ],
    )
    .await?;
    let [earliest, later] = requests.as_slice() else {
        panic!("the fixture has two ordered approval requests")
    };
    let repository = PostgresToolLoopRepository::new(pool.clone());
    let out_of_order = DurableCommandId::from_uuid(Uuid::from_u128(seed + 0xd0));
    let outcome = repository
        .decide(
            decide_tool_request(out_of_order, *later, ToolApprovalDecision::Approve),
            || panic!("a rejected decision opens no attempt"),
        )
        .await?;
    assert_eq!(
        outcome.result(),
        &DecideToolRequestResult::Rejected(
            signalbox_domain::DecideToolRequestRejectedResult::NotEarliestUndecided {
                request: *later,
                earliest: *earliest,
            }
        )
    );
    assert_eq!(
        injection_receipt(&pool, out_of_order).await?,
        Some(InjectionReceipt::rejected("not_earliest_undecided"))
    );

    let unknown = DurableCommandId::from_uuid(Uuid::from_u128(seed + 0xd2));
    let missing = ToolRequestId::from_uuid(Uuid::from_u128(seed + 0xd3));
    let outcome = repository
        .decide(
            decide_tool_request(unknown, missing, ToolApprovalDecision::Approve),
            || panic!("a rejected decision opens no attempt"),
        )
        .await?;
    assert_eq!(
        outcome.result(),
        &DecideToolRequestResult::Rejected(
            signalbox_domain::DecideToolRequestRejectedResult::RequestNotFound { request: missing }
        )
    );
    assert_eq!(injection_receipt(&pool, unknown).await?, None);

    pool.close().await;
    drop(container);
    Ok(())
}
