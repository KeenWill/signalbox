//! Delegated creation through the versioned process socket.

use super::*;
use signalbox_domain::{ToolAttemptId, ToolEffectClass};
use signalbox_persistence::tool_loop::PostgresToolLoopRepository;
use signalbox_process_protocol::DelegationPolicy;

/// The process request executes only after authorization and replays the same child.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn spawn_session_requires_dispatch_and_replays_its_child() -> Result<(), Box<dyn Error>> {
    const TASK: &str = "Inspect the delegated workspace";
    let runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let session_id = create_alias_session(&mut connection).await?;
    submit_first_input(&mut connection, session_id, TASK.into()).await?;
    let session = SessionId::from_uuid(session_id.into_uuid());
    let (calls, authorized, _) = authorize_issued_model_call(&runtime.pool, session_id).await?;
    let turn = authorized.observation_correlation().turn();
    let tool_request_id = CanonicalUuid::from_uuid(Uuid::now_v7());
    let response =
        ToolUsingAssistantResponse::try_from_parts(vec![AssistantResponsePart::ToolCall(
            ToolCallProposal::new(
                ToolName::try_new("spawn_session".into()).expect("spawn tool name"),
                NormalizedToolArguments::try_from_provider_text(
                    serde_json::json!({"task":TASK,"relationship":{"kind":"background"}})
                        .to_string(),
                )
                .expect("bounded spawn arguments"),
            ),
        )])
        .expect("spawn tool response");
    let observation = authorized
        .observation_correlation()
        .bind_terminal_observation(ModelCallTerminalObservation::CompletedWithTools {
            response,
            retained_input_tokens: None,
            retained_output_tokens: None,
        });
    calls
        .apply_terminal_observation(
            session,
            observation,
            ModelCallTerminalIdentities::ToolRound(ToolRoundModelCallIdentities::new(
                vec![ToolResponsePartIdentity::tool_call(
                    SemanticTranscriptEntryId::from_uuid(Uuid::now_v7()),
                    ToolRequestId::from_uuid(tool_request_id.into_uuid()),
                    InitialToolApproval::PolicyAuto,
                )],
                ContextFrontierId::from_uuid(Uuid::now_v7()),
                Some(signalbox_domain::TurnAttemptId::from_uuid(Uuid::now_v7())),
            )),
            |_| panic!("fixture has no steering"),
        )
        .await?;
    let request = ClientRequest::SpawnSession {
        session_id,
        turn_id: CanonicalUuid::from_uuid(turn.into_uuid()),
        tool_request_id,
        task: TASK.into(),
        relationship: DelegationPolicy::Background {},
    };
    connection
        .request_version(ProtocolVersion::One, 3, request.clone())
        .await?;
    assert_eq!(
        protocol_error_code(response_within(&mut connection).await?.message()),
        ErrorCode::Rejected
    );
    let tools = PostgresToolLoopRepository::new(runtime.pool.clone());
    let attempt = ToolAttemptId::from_uuid(Uuid::now_v7());
    tools
        .prepare_next_attempt(session, turn, attempt, ToolEffectClass::ExternalEffect)
        .await?
        .expect("spawn is next");
    tools.authorize_attempt(session, turn, attempt).await?;
    connection
        .request_version(ProtocolVersion::One, 4, request.clone())
        .await?;
    let recorded = response_within(&mut connection).await?;
    assert!(
        matches!(recorded.message(), ServerMessage::SessionSpawned { tool_request_id: recorded_request, relationship: DelegationPolicy::Background {}, .. } if *recorded_request == tool_request_id),
        "spawn must return its correlated child: {recorded:?}"
    );
    connection
        .request_version(ProtocolVersion::One, 5, request)
        .await?;
    assert_eq!(
        response_within(&mut connection).await?.message(),
        recorded.message(),
        "a retry must return the stored child"
    );
    drop(connection);
    runtime.stop().await
}

/// A model tool schedules its committed child with reconciliation sweeps disabled.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn model_spawn_retains_child_hint_when_parent_fills_the_buffer() -> Result<(), Box<dyn Error>>
{
    use signalbox_application::{EligibilityNudge, EligibilityWorkSource};
    use signalbox_tools_sessions::{SessionDelegationPort, SessionDelegationPortOutcome};
    const TASK: &str = "Inspect the delegated workspace";
    let runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let session_id = create_alias_session(&mut connection).await?;
    submit_first_input(&mut connection, session_id, TASK.into()).await?;
    let session = SessionId::from_uuid(session_id.into_uuid());
    let (calls, authorized, _) = authorize_issued_model_call(&runtime.pool, session_id).await?;
    let turn = authorized.observation_correlation().turn();
    let tool_request_id = CanonicalUuid::from_uuid(Uuid::now_v7());
    let response =
        ToolUsingAssistantResponse::try_from_parts(vec![AssistantResponsePart::ToolCall(
            ToolCallProposal::new(
                ToolName::try_new("spawn_session".into()).expect("spawn tool name"),
                NormalizedToolArguments::try_from_provider_text(
                    serde_json::json!({"task":TASK,"relationship":{"kind":"background"}})
                        .to_string(),
                )
                .expect("bounded spawn arguments"),
            ),
        )])
        .expect("spawn tool response");
    let observation = authorized
        .observation_correlation()
        .bind_terminal_observation(ModelCallTerminalObservation::CompletedWithTools {
            response,
            retained_input_tokens: None,
            retained_output_tokens: None,
        });
    calls
        .apply_terminal_observation(
            session,
            observation,
            ModelCallTerminalIdentities::ToolRound(ToolRoundModelCallIdentities::new(
                vec![ToolResponsePartIdentity::tool_call(
                    SemanticTranscriptEntryId::from_uuid(Uuid::now_v7()),
                    ToolRequestId::from_uuid(tool_request_id.into_uuid()),
                    InitialToolApproval::PolicyAuto,
                )],
                ContextFrontierId::from_uuid(Uuid::now_v7()),
                Some(signalbox_domain::TurnAttemptId::from_uuid(Uuid::now_v7())),
            )),
            |_| panic!("fixture has no steering"),
        )
        .await?;

    let tools = PostgresToolLoopRepository::new(runtime.pool.clone());
    let attempt = ToolAttemptId::from_uuid(Uuid::now_v7());
    tools
        .prepare_next_attempt(session, turn, attempt, ToolEffectClass::ExternalEffect)
        .await?
        .expect("spawn is next");
    let dispatch = tools.authorize_attempt(session, turn, attempt).await?;
    let request = signalbox_domain::DelegatedSpawnRequest::parse(
        dispatch.request().clone(),
        TASK.into(),
        signalbox_domain::ChildRelationshipPolicy::Background,
    )?;
    let (nudge, mut work_source) =
        InProcessEligibilityWorkSource::with_options(NoSweep, None, std::num::NonZeroUsize::new(1));
    assert_eq!(
        nudge.nudge(session),
        signalbox_application::EligibilityNudgeOutcome::Enqueued
    );
    let mut port = signalboxd::PostgresSessionDelegationPort::new(runtime.pool.clone(), nudge);
    let SessionDelegationPortOutcome::Applied(receipt) =
        port.spawn_session(request, dispatch).await?
    else {
        panic!("model spawn must commit");
    };
    assert_eq!(
        timeout(Duration::from_secs(5), work_source.next()).await??,
        session
    );
    assert_eq!(
        timeout(Duration::from_secs(5), work_source.next()).await??,
        receipt.child(),
        "the committed child must be nudged without a sweep"
    );
    drop(connection);
    runtime.stop().await
}

struct NoSweep;
impl EligibilitySweep for NoSweep {
    type Error = std::convert::Infallible;
    async fn find_sessions(&mut self) -> Result<EligibilitySweepBatch, Self::Error> {
        pending().await
    }
}
