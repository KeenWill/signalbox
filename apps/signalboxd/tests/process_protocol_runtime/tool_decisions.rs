//! Tool decisions coverage.

use super::*;

/// a decision naming a later request while an earlier one is undecided records the exact
/// proposal-order rejection.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn decide_tool_request_refuses_a_later_request_first() -> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let session_id = create_alias_session(&mut connection).await?;
    submit_first_input(&mut connection, session_id, String::from("first request")).await?;
    let first_request_id = CanonicalUuid::from_uuid(Uuid::from_u128(0xE1));
    let second_request_id = CanonicalUuid::from_uuid(Uuid::from_u128(0xE2));
    park_turn_on_tool_approval(
        &runtime.pool,
        session_id,
        &[first_request_id, second_request_id],
    )
    .await?;

    connection
        .request_version(
            ProtocolVersion::One,
            3,
            ClientRequest::DecideToolRequest {
                command_id: command()?,
                session_id,
                tool_request_id: second_request_id,
                decision: ToolDecision::Approve {},
            },
        )
        .await?;
    assert_eq!(
        rejected_detail(response_within(&mut connection).await?.message()),
        RejectionDetail::ToolRequestNotEarliestUndecided {
            tool_request_id: second_request_id,
            earliest_tool_request_id: first_request_id,
        }
    );

    drop(connection);
    runtime.stop().await
}

/// an unknown logical request records the exact absent-request rejection.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn decide_tool_request_reports_an_unknown_request() -> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let session_id = create_alias_session(&mut connection).await?;
    submit_first_input(&mut connection, session_id, String::from("first request")).await?;
    let pending_request_id = CanonicalUuid::from_uuid(Uuid::from_u128(0xE1));
    park_turn_on_tool_approval(&runtime.pool, session_id, &[pending_request_id]).await?;

    let unknown_request_id = CanonicalUuid::from_uuid(Uuid::from_u128(0xE3));
    connection
        .request_version(
            ProtocolVersion::One,
            3,
            ClientRequest::DecideToolRequest {
                command_id: command()?,
                session_id,
                tool_request_id: unknown_request_id,
                decision: ToolDecision::Approve {},
            },
        )
        .await?;
    assert_eq!(
        rejected_detail(response_within(&mut connection).await?.message()),
        RejectionDetail::ToolRequestNotFound {
            tool_request_id: unknown_request_id,
        }
    );

    drop(connection);
    runtime.stop().await
}

/// the session-correlation precondition refuses a decision whose named session does not own the
/// named request, before any durable command is recorded.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn decide_tool_request_refuses_a_misrouted_session_without_recording()
-> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let session_id = create_alias_session(&mut connection).await?;
    submit_first_input(&mut connection, session_id, String::from("first request")).await?;
    let pending_request_id = CanonicalUuid::from_uuid(Uuid::from_u128(0xE1));
    park_turn_on_tool_approval(&runtime.pool, session_id, &[pending_request_id]).await?;

    let mut foreign = Connection::connect(runtime.socket()).await?;
    let foreign_session_id = create_alias_session(&mut foreign).await?;
    let misrouted_command = command()?;
    foreign
        .request_version(
            ProtocolVersion::One,
            2,
            ClientRequest::DecideToolRequest {
                command_id: misrouted_command,
                session_id: foreign_session_id,
                tool_request_id: pending_request_id,
                decision: ToolDecision::Approve {},
            },
        )
        .await?;
    assert_eq!(
        rejected_detail(response_within(&mut foreign).await?.message()),
        RejectionDetail::ToolRequestNotInSession {
            session_id: foreign_session_id,
            tool_request_id: pending_request_id,
        }
    );
    let misrouted_claim_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM durable_command WHERE command_id = $1")
            .bind(misrouted_command.into_uuid())
            .fetch_one(&runtime.pool)
            .await?;
    assert_eq!(
        misrouted_claim_count, 0,
        "the session-correlation refusal must record no durable command"
    );

    drop(foreign);
    drop(connection);
    runtime.stop().await
}

/// a denial reason outside the domain contract is refused as an invalid request before any durable
/// command is recorded.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn decide_tool_request_refuses_an_unsafe_denial_reason_before_recording()
-> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let session_id = create_alias_session(&mut connection).await?;
    submit_first_input(&mut connection, session_id, String::from("first request")).await?;
    let pending_request_id = CanonicalUuid::from_uuid(Uuid::from_u128(0xE1));
    park_turn_on_tool_approval(&runtime.pool, session_id, &[pending_request_id]).await?;

    let unsafe_reason_command = command()?;
    connection
        .request_version(
            ProtocolVersion::One,
            3,
            ClientRequest::DecideToolRequest {
                command_id: unsafe_reason_command,
                session_id,
                tool_request_id: pending_request_id,
                decision: ToolDecision::Deny {
                    reason: String::from(" padded "),
                },
            },
        )
        .await?;
    assert!(matches!(
        response_within(&mut connection).await?.message(),
        ServerMessage::Error {
            code: ErrorCode::InvalidRequest,
            ..
        }
    ));
    let unsafe_claim_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM durable_command WHERE command_id = $1")
            .bind(unsafe_reason_command.into_uuid())
            .fetch_one(&runtime.pool)
            .await?;
    assert_eq!(unsafe_claim_count, 0);

    drop(connection);
    runtime.stop().await
}

/// one durable decision identity has one recorded meaning — an equal
/// replay returns the exact recorded receipt, a different payload under the
/// same identity is conflicting reuse, and reusing an identity claimed by
/// another command kind is conflicting reuse too. The steps share one recorded
/// command, so they are asserted against the same durable state in one
/// execution.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn decide_tool_request_replays_equally_and_refuses_conflicting_reuse()
-> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let session_id = create_alias_session(&mut connection).await?;
    submit_first_input(&mut connection, session_id, String::from("first request")).await?;
    let pending_request_id = CanonicalUuid::from_uuid(Uuid::from_u128(0xE1));
    park_turn_on_tool_approval(&runtime.pool, session_id, &[pending_request_id]).await?;

    let denial_command = command()?;
    let denial = ClientRequest::DecideToolRequest {
        command_id: denial_command,
        session_id,
        tool_request_id: pending_request_id,
        decision: ToolDecision::Deny {
            reason: String::from("writes outside the workspace"),
        },
    };
    connection
        .request_version(ProtocolVersion::One, 3, denial.clone())
        .await?;
    assert_eq!(
        decided_receipt(response_within(&mut connection).await?.message()),
        (
            pending_request_id,
            ToolDecision::Deny {
                reason: String::from("writes outside the workspace"),
            }
        )
    );

    connection
        .request_version(ProtocolVersion::One, 4, denial)
        .await?;
    assert_eq!(
        decided_receipt(response_within(&mut connection).await?.message()),
        (
            pending_request_id,
            ToolDecision::Deny {
                reason: String::from("writes outside the workspace"),
            }
        ),
        "an equal decision replay returns its exact recorded receipt"
    );

    connection
        .request_version(
            ProtocolVersion::One,
            5,
            ClientRequest::DecideToolRequest {
                command_id: denial_command,
                session_id,
                tool_request_id: pending_request_id,
                decision: ToolDecision::Approve {},
            },
        )
        .await?;
    assert!(matches!(
        response_within(&mut connection).await?.message(),
        ServerMessage::Error {
            code: ErrorCode::ConflictingReuse,
            ..
        }
    ));

    let submit_command = command()?;
    connection
        .request_version(
            ProtocolVersion::One,
            6,
            ClientRequest::SubmitInput {
                command_id: submit_command,
                session_id,
                content: UserInputContent::text(String::from("claims a submit identity")),
                expected_defaults_version: Some(CanonicalU64::new(1)),
                model_settings: ModelSettingsOverlay::inherit_all(),
                delivery: None,
            },
        )
        .await?;
    assert!(matches!(
        rejected_detail(response_within(&mut connection).await?.message()),
        RejectionDetail::ActiveTurnPresent { .. }
    ));
    connection
        .request_version(
            ProtocolVersion::One,
            7,
            ClientRequest::DecideToolRequest {
                command_id: submit_command,
                session_id,
                tool_request_id: pending_request_id,
                decision: ToolDecision::Approve {},
            },
        )
        .await?;
    assert!(
        matches!(
            response_within(&mut connection).await?.message(),
            ServerMessage::Error {
                code: ErrorCode::ConflictingReuse,
                ..
            }
        ),
        "an identity claimed by another command kind is conflicting reuse"
    );

    drop(connection);
    runtime.stop().await
}

/// the final approval opens the executing phase.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn decide_tool_request_final_approval_opens_the_executing_phase() -> Result<(), Box<dyn Error>>
{
    let runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let session_id = create_alias_session(&mut connection).await?;
    let (_, decided_turn_id) =
        submit_first_input(&mut connection, session_id, String::from("first request")).await?;
    let pending_request_id = CanonicalUuid::from_uuid(Uuid::from_u128(0xE1));
    park_turn_on_tool_approval(&runtime.pool, session_id, &[pending_request_id]).await?;

    connection
        .request_version(
            ProtocolVersion::One,
            3,
            ClientRequest::DecideToolRequest {
                command_id: command()?,
                session_id,
                tool_request_id: pending_request_id,
                decision: ToolDecision::Approve {},
            },
        )
        .await?;
    assert_eq!(
        decided_receipt(response_within(&mut connection).await?.message()),
        (pending_request_id, ToolDecision::Approve {})
    );

    let decided = read_transcript_messages(&mut connection, 4, session_id).await?;
    assert!(
        matches!(
            turn_state_of(&decided, decided_turn_id),
            TurnState::ActiveRunning { .. }
        ),
        "the final approval opens the executing phase"
    );

    drop(connection);
    runtime.stop().await
}

/// A decision in flight when the daemon drains is either acknowledged and
/// durable or unacknowledged and unclaimed; after the restart the same command
/// replays to one applied decision with one `delivered` receipt.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn decide_tool_request_survives_a_drain_and_restart() -> Result<(), Box<dyn Error>> {
    let mut runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let session_id = create_alias_session(&mut connection).await?;
    submit_first_input(&mut connection, session_id, String::from("first request")).await?;
    let pending_request_id = CanonicalUuid::from_uuid(Uuid::from_u128(0xE1));
    park_turn_on_tool_approval(&runtime.pool, session_id, &[pending_request_id]).await?;

    let decision_command = command()?;
    let decision = ClientRequest::DecideToolRequest {
        command_id: decision_command,
        session_id,
        tool_request_id: pending_request_id,
        decision: ToolDecision::Approve {},
    };
    connection
        .request_version(ProtocolVersion::One, 3, decision.clone())
        .await?;
    runtime.shutdown.send_replace(true);
    // Whether the drain let this reply through or closed the socket first is
    // the race under test; either way the replay below settles the decision.
    let _acknowledgement = timeout(Duration::from_secs(5), connection.response()).await;
    drop(connection);
    runtime.restart().await?;

    let mut connection = Connection::connect(runtime.socket()).await?;
    connection
        .request_version(ProtocolVersion::One, 4, decision)
        .await?;
    assert_eq!(
        decided_receipt(response_within(&mut connection).await?.message()),
        (pending_request_id, ToolDecision::Approve {}),
        "the drained decision replays as one applied decision"
    );
    let settled: (i64, String) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM tool_approval_decision WHERE request_id = $1),
                receipt.outcome_kind
           FROM injection_settled_outbox_event AS receipt
          WHERE receipt.command_id = $2",
    )
    .bind(pending_request_id.into_uuid())
    .bind(decision_command.into_uuid())
    .fetch_one(&runtime.pool)
    .await?;
    assert_eq!(settled, (1, String::from("delivered")));

    drop(connection);
    runtime.stop().await
}

/// a request that already has a terminal resolution records the exact already-resolved rejection
/// for a later distinct decision.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn decide_tool_request_refuses_an_already_resolved_request() -> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let session_id = create_alias_session(&mut connection).await?;
    submit_first_input(&mut connection, session_id, String::from("first request")).await?;
    let pending_request_id = CanonicalUuid::from_uuid(Uuid::from_u128(0xE1));
    park_turn_on_tool_approval(&runtime.pool, session_id, &[pending_request_id]).await?;

    connection
        .request_version(
            ProtocolVersion::One,
            3,
            ClientRequest::DecideToolRequest {
                command_id: command()?,
                session_id,
                tool_request_id: pending_request_id,
                decision: ToolDecision::Approve {},
            },
        )
        .await?;
    assert_eq!(
        decided_receipt(response_within(&mut connection).await?.message()),
        (pending_request_id, ToolDecision::Approve {})
    );

    connection
        .request_version(
            ProtocolVersion::One,
            4,
            ClientRequest::DecideToolRequest {
                command_id: command()?,
                session_id,
                tool_request_id: pending_request_id,
                decision: ToolDecision::Approve {},
            },
        )
        .await?;
    assert_eq!(
        rejected_detail(response_within(&mut connection).await?.message()),
        RejectionDetail::ToolRequestAlreadyResolved {
            tool_request_id: pending_request_id,
        }
    );

    drop(connection);
    runtime.stop().await
}
