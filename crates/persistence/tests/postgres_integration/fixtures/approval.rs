//! Approval decisions and judge projections.

use crate::*;

pub(crate) const APPROVAL_FIXTURE_SEED: u128 = 0x7e00;
pub(crate) const APPROVAL_JUDGE_SEED: u128 = 0x7e50;
pub(crate) const APPROVAL_COMMAND_SEED: u128 = 0x7e80;
pub(crate) const APPROVAL_NEXT_ATTEMPT_SEED: u128 = 0x7e81;
pub(crate) const APPROVAL_TOOL_NAME: &str = "current_time";
pub(crate) const APPROVAL_ARGUMENTS: &str = "{}";
pub(crate) const APPROVAL_PROPOSAL: &[(&str, &str)] = &[(APPROVAL_TOOL_NAME, APPROVAL_ARGUMENTS)];
pub(crate) const APPROVAL_RECOMMENDATION: &str = "approve";
pub(crate) const APPROVAL_DENIAL: &str = "deny";
pub(crate) const APPROVAL_JUDGE_CREDENTIAL: &str = "fixture-credential";
pub(crate) const APPROVAL_JUDGE_RATIONALE: &str = "fixture rationale";
pub(crate) const APPROVAL_JUDGE_ESTIMATED_PROVENANCE: &str = "estimated";
pub(crate) const APPROVAL_DELEGATE_SOURCE: &str = "delegate";
pub(crate) const APPROVAL_GOAL_STATEMENT: &str = "finish the commissioned approval task";

pub(crate) fn applied_tool_decision(
    prepared: &signalbox_domain::PreparedDecideToolRequest,
) -> &signalbox_domain::DecideToolRequestAppliedResult {
    match prepared.result() {
        DecideToolRequestResult::Applied(applied) => applied,
        DecideToolRequestResult::Rejected(_) => {
            panic!("the escalated delegated request admits a user decision")
        }
    }
}

#[derive(Debug, PartialEq, sqlx::FromRow)]
pub(crate) struct ApprovalJudgeDurableState {
    pub(crate) prepared_judge_exists: bool,
    pub(crate) decision_exists: bool,
    pub(crate) active_wait_exists: bool,
}

#[derive(Debug, PartialEq, sqlx::FromRow)]
pub(crate) struct ApprovalJudgeDecisionDurableState {
    pub(crate) prepared_judge_exists: bool,
    pub(crate) decision_exists: bool,
}

#[derive(Debug, PartialEq, sqlx::FromRow)]
pub(crate) struct AutomaticApprovalEventState {
    pub(crate) decision_exists: bool,
    pub(crate) decided_event_exists: bool,
}

#[derive(Debug, PartialEq, sqlx::FromRow)]
pub(crate) struct AppliedApprovalJudgeProjection {
    pub(crate) judge_state: String,
    pub(crate) recommendation: String,
    pub(crate) decision_source: String,
    pub(crate) delegate_model_selection_id: Uuid,
    pub(crate) delegate_model_call_id: Uuid,
    pub(crate) rationale: String,
    pub(crate) active_phase: String,
}

#[derive(Debug, PartialEq, sqlx::FromRow)]
pub(crate) struct DeniedApprovalJudgeProjection {
    pub(crate) judge_state: String,
    pub(crate) recommendation: String,
    pub(crate) decision_kind: String,
    pub(crate) decision_source: String,
    pub(crate) denial_reason: Option<String>,
    pub(crate) rationale: String,
    pub(crate) active_phase: String,
}

#[derive(Debug, PartialEq, sqlx::FromRow)]
pub(crate) struct EscalatedApprovalJudgeProjection {
    pub(crate) judge_state: String,
    pub(crate) recommendation: String,
    pub(crate) decision_exists: bool,
    pub(crate) active_phase: String,
    pub(crate) approval_tool_request_id: Uuid,
}

pub(crate) async fn dispatched_tool_approval_decision(
    pool: &PgPool,
    expected_request: ToolRequestId,
) -> Result<Option<(TurnId, ToolApprovalResolution)>, OutboxDispatchError> {
    let mut found = None;
    drain_outbox(pool, |event| {
        if let DispatchedOutboxEventKind::ToolApprovalDecided { turn, approval, .. } = event.kind()
            && approval.request() == expected_request
        {
            found = Some((*turn, approval.clone()));
        }
    })
    .await?;
    Ok(found)
}

pub(crate) async fn checkpoint_suppressed_tool_round(
    pool: &PgPool,
    seed: u128,
    tool_name: &str,
) -> Result<(RestartModelCallFixture, signalbox_domain::ToolRequestId), Box<dyn Error>> {
    let (fixture, model_repository, authorized) =
        authorize_checkpointed_model_call(pool, seed).await?;
    let request = signalbox_domain::ToolRequestId::from_uuid(Uuid::from_u128(seed + 0x40));
    let response =
        ToolUsingAssistantResponse::try_from_parts(vec![AssistantResponsePart::ToolCall(
            ToolCallProposal::suppressed(
                ToolName::try_new(String::from(tool_name)).expect("valid fixture tool name"),
            ),
        )])
        .expect("the suppressed proposal forms one inert tool response");
    let observation = authorized
        .observation_correlation()
        .bind_terminal_observation(ModelCallTerminalObservation::CompletedWithTools {
            response,
            retained_input_tokens: None,
            retained_output_tokens: None,
        });
    let outcome = model_repository
        .apply_terminal_observation(
            fixture.session,
            observation,
            ModelCallTerminalIdentities::ToolRound(ToolRoundModelCallIdentities::new(
                vec![ToolResponsePartIdentity::tool_call(
                    SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 0x80)),
                    request,
                    InitialToolApproval::RuntimeSafetyDeny,
                )],
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 0xc0)),
                Some(TurnAttemptId::from_uuid(Uuid::from_u128(seed + 0xc1))),
            )),
            |_| panic!("the fixture has no pending steering to reclassify"),
        )
        .await?;
    let ModelCallTerminalOutcome::ToolRound(round) = outcome else {
        panic!("the suppressed fixture reaches an automatically denied tool round")
    };
    assert!(matches!(
        round.next_phase(),
        ActiveTurnPhase::Running { .. }
    ));
    Ok((fixture, request))
}

/// Fails naming the outcome a goal transition produced instead of an applied
/// event, so a refused or misrouted fixture transition is not mistaken for
/// one that landed.
#[track_caller]
pub(crate) fn assert_goal_transition_applied(outcome: &GoalTransitionOutcome) {
    match outcome {
        GoalTransitionOutcome::Applied(_) => {}
        GoalTransitionOutcome::GoalNotAttached => {
            panic!("the goal transition found no attached goal")
        }
        GoalTransitionOutcome::Rejected(error) => {
            panic!("the goal transition was rejected: {:?}", error.failure())
        }
        GoalTransitionOutcome::NotCurrentGoalTurn => {
            panic!("the goal transition named a turn outside the current goal generation")
        }
        GoalTransitionOutcome::SessionClosing => {
            panic!("the goal transition found a pending session closure")
        }
    }
}

pub(crate) async fn insert_completed_judge(
    connection: &mut PgConnection,
    fixture: &RestartModelCallFixture,
    request: ToolRequestId,
    seed: u128,
    recommendation: &str,
    input_tokens: Option<Decimal>,
    usage_provenance: Option<&str>,
) -> Result<(Uuid, Uuid), sqlx::Error> {
    let (selection, call) = insert_prepared_judge(connection, fixture, request, seed).await?;
    sqlx::query(
        "UPDATE tool_approval_judge_model_call SET state_kind = 'in_flight'
          WHERE model_call_id = $1",
    )
    .bind(call)
    .execute(&mut *connection)
    .await?;
    sqlx::query(
        "UPDATE tool_approval_judge_model_call
            SET state_kind = 'terminal', terminal_disposition_kind = 'completed',
                recommendation_kind = $1, rationale = $2,
                input_tokens = $3,
                usage_provenance_kind = COALESCE($4, usage_provenance_kind)
          WHERE model_call_id = $5",
    )
    .bind(recommendation)
    .bind(APPROVAL_JUDGE_RATIONALE)
    .bind(input_tokens)
    .bind(usage_provenance)
    .bind(call)
    .execute(&mut *connection)
    .await?;
    Ok((selection, call))
}

pub(crate) async fn insert_prepared_judge(
    connection: &mut PgConnection,
    fixture: &RestartModelCallFixture,
    request: ToolRequestId,
    seed: u128,
) -> Result<(Uuid, Uuid), sqlx::Error> {
    let selection = Uuid::from_u128(seed + 1);
    let call = Uuid::from_u128(seed + 2);
    sqlx::query(
        "INSERT INTO tool_approval_judge_model_call
            (model_call_id, request_id, session_id, turn_id,
             direct_model_selection_id, resolved_provider_model_identity_id,
             credential_reference, state_kind)
         VALUES ($1, $2, $3, $4, $5, $6, $7, 'prepared')",
    )
    .bind(call)
    .bind(request.into_uuid())
    .bind(fixture.session.into_uuid())
    .bind(fixture.turn.into_uuid())
    .bind(selection)
    .bind(Uuid::from_u128(seed + 3))
    .bind(APPROVAL_JUDGE_CREDENTIAL)
    .execute(&mut *connection)
    .await?;
    Ok((selection, call))
}

pub(crate) async fn insert_user_approval_decision_event(
    connection: &mut PgConnection,
    fixture: &RestartModelCallFixture,
    request: ToolRequestId,
    command: Uuid,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO durable_command
            (command_id, command_kind, storage_version, claimed_at, issuer_kind)
         VALUES ($1, 'decide_tool_request', 1, transaction_timestamp(), 'operator')",
    )
    .bind(command)
    .execute(&mut *connection)
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
    .execute(&mut *connection)
    .await?;
    sqlx::query(
        "INSERT INTO tool_approval_decision
            (request_id, decision_kind, decision_source, user_command_id)
         VALUES ($1, 'approve', 'user_command', $2)",
    )
    .bind(request.into_uuid())
    .bind(command)
    .execute(&mut *connection)
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
    .bind(request.into_uuid())
    .execute(&mut *connection)
    .await?;
    Ok(())
}

pub(crate) fn process_tool_approval(
    snapshot: &ProcessTranscriptSnapshot,
    request: ToolRequestId,
) -> Option<&ProcessToolApproval> {
    snapshot.entries().iter().find_map(|entry| match entry {
        ProcessTranscriptEntry::AssistantToolUse {
            request: entry_request,
            approval,
            ..
        } if *entry_request == request => approval.as_ref(),
        _ => None,
    })
}
