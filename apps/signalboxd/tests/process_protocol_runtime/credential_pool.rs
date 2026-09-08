//! Pool exhaustion survives the socket snapshot, event, and policy read boundaries.
use super::*;

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn pool_projection_snapshot_event_and_policy_agree_over_the_socket()
-> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let session_id = create_alias_session(&mut connection).await?;
    let (_, turn_id) =
        submit_first_input(&mut connection, session_id, String::from("exhausted pool")).await?;
    let session = SessionId::from_uuid(session_id.into_uuid());
    activate_turn(&runtime.pool, session).await?;
    let mut follower = Connection::connect(runtime.socket()).await?;
    follower
        .request_version(
            ProtocolVersion::One,
            1,
            ClientRequest::FollowSession { session_id },
        )
        .await?;
    loop {
        if matches!(
            response_within(&mut follower).await?.message(),
            ServerMessage::TranscriptSnapshotEnd { .. }
        ) {
            break;
        }
    }
    sqlx::query("INSERT INTO credential_exclusion(kind,profile,origin) VALUES ('profile_quarantine','anthropic-primary','codex_home')").execute(&runtime.pool).await?;
    let configuration = support::parse_model_configuration(&MODEL_CONFIGURATION.replace(
        "on_pool_exhausted = \"park\"",
        "on_pool_exhausted = \"fail\"",
    ))?;
    let repository = PostgresModelCallRepository::new(
        runtime.pool.clone(),
        configuration.target_catalog(),
        ModelCallCredentialReference::new("unused-fallback"),
    )
    .with_credential_pools(configuration.credential_pool_runtime_catalog());
    let outcome = repository
        .prepare_initial_call(
            session,
            ModelCallId::from_uuid(Uuid::now_v7()),
            FailedModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::now_v7()),
                ContextFrontierId::from_uuid(Uuid::now_v7()),
            ),
            ContextFrontierId::from_uuid(Uuid::now_v7()),
            |_| panic!("fixture has no steering"),
        )
        .await?;
    assert!(matches!(
        outcome,
        PrepareInitialModelCallOutcome::PoolExhausted(_)
    ));
    let live = loop {
        let frame = response_within(&mut follower).await?;
        if let ServerMessage::SessionEvent {
            event: event @ SessionEvent::TurnCredentialPoolExhausted { .. },
            ..
        } = frame.message()
        {
            break event.clone();
        }
    };
    let messages = read_transcript_messages(&mut connection, 3, session_id).await?;
    let TurnState::FailedCredentialPoolExhausted {
        terminal_frontier_id,
        terminal_attempt_id,
        failure_entry_id,
        pool_policy_id,
        policy_members,
        members,
    } = turn_state_of(&messages, turn_id)
    else {
        panic!("typed exhaustion snapshot");
    };
    assert_eq!(policy_members, [String::from("anthropic-primary")]);
    assert_eq!(
        live,
        SessionEvent::TurnCredentialPoolExhausted {
            turn_id,
            terminal_frontier_id,
            terminal_attempt_id,
            failure_entry_id,
            pool_policy_id,
            policy_members: policy_members.clone(),
            members: members.clone()
        }
    );
    connection
        .request_version(
            ProtocolVersion::One,
            4,
            ClientRequest::ReadCredentialPoolPolicy {
                session_id,
                turn_id,
                pool_policy_id,
            },
        )
        .await?;
    assert_eq!(
        response_within(&mut connection).await?.message(),
        &ServerMessage::CredentialPoolPolicy {
            pool_policy_id,
            policy_members: policy_members.clone()
        }
    );
    let wrong_turn = CanonicalUuid::from_uuid(Uuid::now_v7());
    connection
        .request_version(
            ProtocolVersion::One,
            5,
            ClientRequest::ReadCredentialPoolPolicy {
                session_id,
                turn_id: wrong_turn,
                pool_policy_id,
            },
        )
        .await?;
    let rejection = response_within(&mut connection).await?;
    let ServerMessage::Error { detail, .. } = rejection.message() else {
        panic!("foreign turn must be rejected");
    };
    assert_eq!(
        *detail,
        signalbox_process_protocol::ErrorDetail::rejected(
            signalbox_process_protocol::RejectionDetail::UnknownPoolPolicy {
                session_id,
                turn_id: wrong_turn,
                pool_policy_id
            }
        )
    );
    drop(follower);
    drop(connection);
    runtime.stop().await
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn pool_projection_parked_wait_stays_readable_and_followable() -> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let session_id = create_alias_session(&mut connection).await?;
    let (_, turn_id) = submit_first_input(
        &mut connection,
        session_id,
        String::from("parked credential admission"),
    )
    .await?;
    let session = SessionId::from_uuid(session_id.into_uuid());
    activate_turn(&runtime.pool, session).await?;
    sqlx::query("INSERT INTO credential_exclusion(kind,profile,origin) VALUES ('profile_quarantine','anthropic-primary','codex_home')").execute(&runtime.pool).await?;
    let configuration = support::parse_model_configuration(MODEL_CONFIGURATION)?;
    let repository = PostgresModelCallRepository::new(
        runtime.pool.clone(),
        configuration.target_catalog(),
        ModelCallCredentialReference::new("unused-fallback"),
    )
    .with_credential_pools(configuration.credential_pool_runtime_catalog());
    let PrepareInitialModelCallOutcome::CredentialWait(wait) = repository
        .prepare_initial_call(
            session,
            ModelCallId::from_uuid(Uuid::now_v7()),
            FailedModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::now_v7()),
                ContextFrontierId::from_uuid(Uuid::now_v7()),
            ),
            ContextFrontierId::from_uuid(Uuid::now_v7()),
            |_| panic!("fixture has no steering"),
        )
        .await?
    else {
        panic!("clearable quarantine parks")
    };
    let expected = TurnState::ActiveAwaitingCredentialAvailability {
        wait_attempt_id: CanonicalUuid::from_uuid(wait.attempt().into_uuid()),
        cause: signalbox_process_protocol::CredentialAvailabilityWaitCause::Exhausted,
    };
    let snapshot = read_transcript_messages(&mut connection, 3, session_id).await?;
    assert_eq!(turn_state_of(&snapshot, turn_id), expected);
    let mut follower = Connection::connect(runtime.socket()).await?;
    follower
        .request_version(
            ProtocolVersion::One,
            1,
            ClientRequest::FollowSession { session_id },
        )
        .await?;
    let mut messages = Vec::new();
    loop {
        let frame = response_within(&mut follower).await?;
        let ended = matches!(frame.message(), ServerMessage::TranscriptSnapshotEnd { .. });
        messages.push(frame.message().clone());
        if ended {
            break;
        }
    }
    assert_eq!(turn_state_of(&messages, turn_id), expected);
    runtime.stop().await?;
    Ok(())
}
