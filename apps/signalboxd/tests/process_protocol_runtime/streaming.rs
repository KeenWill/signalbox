//! Streaming coverage.

use super::*;

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn process_runtime_follow_survives_idleness_beyond_client_deadlines()
-> Result<(), Box<dyn Error>> {
    let configuration = support::parse_model_configuration(MODEL_CONFIGURATION)?;
    let bounds = configuration.numeric_bounds();
    let idle = bounds
        .duration("client_frame_deadline")
        .flatten()
        .unwrap()
        .max(
            bounds
                .duration("client_write_progress_deadline")
                .flatten()
                .unwrap(),
        )
        + Duration::from_secs(1);
    let runtime = RunningRuntime::start().await?;
    let mut commands = Connection::connect(runtime.socket()).await?;
    let session = create_alias_session(&mut commands).await?;
    let mut follow =
        attach_empty_follower(runtime.socket(), ProtocolVersion::One, 1, session).await?;
    let mut partial = UnixStream::connect(runtime.socket()).await?;
    partial.write_all(b"{").await?;
    assert!(
        timeout(idle, follow.response()).await.is_err(),
        "idle follow stays connected"
    );
    let mut closed = Vec::new();
    assert_eq!(
        timeout(RESPONSE_ALLOWANCE, partial.read_to_end(&mut closed)).await??,
        0
    );
    let (accepted, _) =
        submit_first_input(&mut commands, session, "wake follower".to_owned()).await?;
    let settings = response_within(&mut follow).await?;
    let settings = turn_model_settings_resolved_event_facts(settings.message());
    assert_eq!(settings.session_id, session);
    assert_eq!(settings.accepted_input_id, accepted);
    let event = response_within(&mut follow).await?;
    let event = input_accepted_event_facts(event.message());
    assert_eq!(event.accepted_input_id, accepted);
    drop(follow);
    drop(commands);
    runtime.stop().await
}

pub(crate) const STREAMING_DELTA_COUNT: usize = 192;
pub(crate) const STREAMING_DELTA_BYTES: usize = 8 * 1024;
pub(crate) struct StreamedFollowOutcome {
    pub(crate) delta_count: usize,
    pub(crate) text: String,
}

pub(crate) async fn follow_streamed_turn_to_completion(
    mut follow: Connection,
    session_id: CanonicalUuid,
    turn_id: CanonicalUuid,
) -> Result<StreamedFollowOutcome, Box<dyn Error>> {
    let mut delta_count = 0usize;
    let mut text = String::new();
    loop {
        match response_within(&mut follow).await?.message() {
            ServerMessage::ProviderTextDelta {
                session_id: delta_session,
                turn_id: delta_turn,
                content,
                ..
            } if *delta_session == session_id && *delta_turn == turn_id => {
                delta_count += 1;
                text.push_str(content.as_str());
            }
            ServerMessage::SessionEvent {
                session_id: event_session,
                event:
                    SessionEvent::TurnCompleted {
                        turn_id: completed, ..
                    },
                ..
            } if *event_session == session_id && *completed == turn_id => {
                return Ok(StreamedFollowOutcome { delta_count, text });
            }
            ServerMessage::Error {
                code: ErrorCode::ResyncRequired,
                ..
            } => {
                return Err(io::Error::other("a draining follower unexpectedly lagged").into());
            }
            _ => {}
        }
    }
}

pub(crate) async fn receive_resync(mut follow: Connection) -> Result<usize, Box<dyn Error>> {
    let mut delta_count = 0usize;
    loop {
        match response_within(&mut follow).await?.message() {
            ServerMessage::ProviderTextDelta { .. } => delta_count += 1,
            ServerMessage::Error {
                code: ErrorCode::ResyncRequired,
                ..
            } => return Ok(delta_count),
            _ => {}
        }
    }
}

pub(crate) async fn read_completed_assistant(
    socket: &Path,
    request_id: u64,
    session_id: CanonicalUuid,
    turn_id: CanonicalUuid,
) -> Result<String, Box<dyn Error>> {
    let mut connection = Connection::connect(socket).await?;
    connection
        .request_version(
            ProtocolVersion::One,
            request_id,
            ClientRequest::ReadTranscript { session_id },
        )
        .await?;
    let mut assistant_index = None;
    let mut assistant = String::new();
    let mut completion_seen = false;
    loop {
        match response_within(&mut connection).await?.message() {
            ServerMessage::TranscriptTextEntry {
                entry_index,
                entry:
                    TranscriptTextEntry::Assistant {
                        turn_id: assistant_turn,
                        ..
                    },
                ..
            } if *assistant_turn == turn_id => assistant_index = Some(entry_index.value()),
            ServerMessage::TranscriptContent {
                entry_index,
                content_fragment,
                ..
            } if assistant_index == Some(entry_index.value()) => {
                assistant.push_str(content_fragment.as_str());
            }
            ServerMessage::TranscriptEntry {
                entry:
                    TranscriptEntry::TurnCompleted {
                        turn_id: completed_turn,
                    },
                ..
            } if *completed_turn == turn_id => completion_seen = true,
            ServerMessage::TranscriptSnapshotEnd {
                session_id: snapshot_session,
                ..
            } if *snapshot_session == session_id => {
                assert!(completion_seen);
                return Ok(assistant);
            }
            _ => {}
        }
    }
}

pub(crate) fn streamed_script(delta_count: usize, delta: String) -> (Script, String) {
    let assistant = delta.repeat(delta_count);
    let script = std::iter::repeat_n(
        ObservationFact::TextDelta {
            index: 0,
            text: delta,
        },
        delta_count,
    )
    .fold(
        Script::delivering(TerminalEvidence::Completed(CompletionEvidence {
            exchange: ExchangeFacts::default(),
            message_id: None,
            reported_model: Some(ProviderReportedModel::new("fixture-model")),
            finish: CompletionFinish::EndTurn,
            content: vec![AssistantPart::Text(assistant.clone())],
            usage: TokenUsage::unreported(),
        }))
        .observing(ObservationFact::SendCommenced),
        Script::observing,
    );
    (script, assistant)
}

#[track_caller]
pub(crate) fn submitted_input_identity(
    message: &ServerMessage,
    expected_session: CanonicalUuid,
    expected_position: u64,
) -> CanonicalUuid {
    match message {
        ServerMessage::InputSubmitted {
            session_id,
            accepted_input_id,
            acceptance_position,
            ..
        } if *session_id == expected_session
            && acceptance_position.value() == expected_position =>
        {
            *accepted_input_id
        }
        message => panic!("fixture expected input-submitted receipt, got {message:?}"),
    }
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) struct TranscriptSnapshotEndFacts {
    pub(crate) session_id: CanonicalUuid,
    pub(crate) cursor: u64,
    pub(crate) turn_count: u64,
    pub(crate) entry_count: u64,
}

#[track_caller]
pub(crate) fn transcript_snapshot_end_facts(message: &ServerMessage) -> TranscriptSnapshotEndFacts {
    match message {
        ServerMessage::TranscriptSnapshotEnd {
            session_id,
            cursor,
            turn_count,
            entry_count,
        } => TranscriptSnapshotEndFacts {
            session_id: *session_id,
            cursor: cursor.value(),
            turn_count: turn_count.value(),
            entry_count: entry_count.value(),
        },
        message => panic!("fixture expected transcript-snapshot end, got {message:?}"),
    }
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) struct InputAcceptedEventFacts {
    pub(crate) cursor: u64,
    pub(crate) session_id: CanonicalUuid,
    pub(crate) accepted_input_id: CanonicalUuid,
    pub(crate) acceptance_position: u64,
    pub(crate) content: UserInputContent,
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) struct TurnModelSettingsResolvedEventFacts {
    pub(crate) cursor: u64,
    pub(crate) session_id: CanonicalUuid,
    pub(crate) accepted_input_id: CanonicalUuid,
}

#[track_caller]
pub(crate) fn turn_model_settings_resolved_event_facts(
    message: &ServerMessage,
) -> TurnModelSettingsResolvedEventFacts {
    match message {
        ServerMessage::SessionEvent {
            cursor,
            session_id,
            event:
                SessionEvent::TurnModelSettingsResolved {
                    accepted_input_id, ..
                },
        } => TurnModelSettingsResolvedEventFacts {
            cursor: cursor.value(),
            session_id: *session_id,
            accepted_input_id: *accepted_input_id,
        },
        message => panic!("fixture expected turn-settings resolution event, got {message:?}"),
    }
}

#[track_caller]
pub(crate) fn input_accepted_event_facts(message: &ServerMessage) -> InputAcceptedEventFacts {
    match message {
        ServerMessage::SessionEvent {
            cursor,
            session_id,
            event:
                SessionEvent::InputAccepted {
                    accepted_input_id,
                    acceptance_position,
                    content,
                    ..
                },
        } => InputAcceptedEventFacts {
            cursor: cursor.value(),
            session_id: *session_id,
            accepted_input_id: *accepted_input_id,
            acceptance_position: acceptance_position.value(),
            content: content.clone(),
        },
        message => panic!("fixture expected input-accepted event, got {message:?}"),
    }
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn process_runtime_reads_one_queued_transcript_snapshot() -> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let session_id = create_alias_session(&mut connection).await?;
    let content = "queued input".to_owned();
    let expected_snapshot_cursor = 5;
    let (accepted_input, turn) =
        submit_first_input(&mut connection, session_id, content.clone()).await?;

    connection
        .request(3, ClientRequest::ReadTranscript { session_id })
        .await?;

    let start = response_within(&mut connection).await?;
    assert_eq!(
        transcript_snapshot_start_cursor(start.message(), session_id),
        expected_snapshot_cursor
    );
    let queued_turn = response_within(&mut connection).await?;
    let (projected_turn, projected_position, projected_state) =
        transcript_turn_projection(queued_turn.message());
    assert_eq!(projected_turn, turn);
    assert_eq!(projected_position, 1);
    assert_eq!(
        projected_state,
        TurnState::Queued {
            accepted_input_id: accepted_input,
            content: UserInputContent::text(content),
        }
    );
    let model_calls_end = response_within(&mut connection).await?;
    assert_eq!(transcript_model_call_count(model_calls_end.message()), 0);
    let end = response_within(&mut connection).await?;
    assert_eq!(
        transcript_snapshot_end_facts(end.message()),
        TranscriptSnapshotEndFacts {
            session_id,
            cursor: expected_snapshot_cursor,
            turn_count: 1,
            entry_count: 0,
        }
    );

    drop(connection);
    runtime.stop().await
}

/// a follow subscription formed before its snapshot observes the next committed outbox event
/// strictly above that snapshot's cursor.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn process_runtime_follow_snapshot_handoff_has_no_race() -> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let mut commands = Connection::connect(runtime.socket()).await?;
    let session_id = create_alias_session(&mut commands).await?;
    let first_content = "x".repeat(MAX_SUBMITTED_INPUT_BYTES);
    let (first_accepted_input, first_turn) =
        submit_first_input(&mut commands, session_id, first_content.clone()).await?;
    let mut follow = Connection::connect(runtime.socket()).await?;
    follow
        .request(5, ClientRequest::FollowSession { session_id })
        .await?;
    let follow_start = follow.response().await?;
    let follow_cursor = transcript_snapshot_start_cursor(follow_start.message(), session_id);

    // The exact-limit queued content keeps the snapshot writer blocked after
    // its start frame. Commit the next update before draining the snapshot so
    // only a subscription formed before snapshot transmission can retain it.
    let second_position = 2;
    let second_content = UserInputContent::text(String::from("second input"));
    commands
        .request(
            6,
            ClientRequest::SubmitInput {
                command_id: command()?,
                session_id,
                content: second_content.clone(),
                expected_defaults_version: Some(CanonicalU64::new(1)),
                model_settings: ModelSettingsOverlay::inherit_all(),
                delivery: None,
            },
        )
        .await?;
    let second_submit = commands.response().await?;
    let second_accepted_input =
        submitted_input_identity(second_submit.message(), session_id, second_position);

    let queued_turn = response_within(&mut follow).await?;
    let (projected_turn, projected_position, projected_state) =
        transcript_turn_projection(queued_turn.message());
    assert_eq!(projected_turn, first_turn);
    assert_eq!(projected_position, 1);
    assert_eq!(
        projected_state,
        TurnState::Queued {
            accepted_input_id: first_accepted_input,
            content: UserInputContent::text(first_content),
        }
    );
    let model_calls_end = response_within(&mut follow).await?;
    assert_eq!(transcript_model_call_count(model_calls_end.message()), 0);
    let snapshot_end = response_within(&mut follow).await?;
    assert_eq!(
        transcript_snapshot_end_facts(snapshot_end.message()),
        TranscriptSnapshotEndFacts {
            session_id,
            cursor: follow_cursor,
            turn_count: 1,
            entry_count: 0,
        }
    );

    let settings_followed = response_within(&mut follow).await?;
    let settings_event = turn_model_settings_resolved_event_facts(settings_followed.message());
    assert!(settings_event.cursor > follow_cursor);
    assert_eq!(settings_event.session_id, session_id);
    assert_eq!(settings_event.accepted_input_id, second_accepted_input);

    let followed = response_within(&mut follow).await?;
    let event = input_accepted_event_facts(followed.message());
    assert!(event.cursor > settings_event.cursor);
    assert_eq!(event.session_id, session_id);
    assert_eq!(event.accepted_input_id, second_accepted_input);
    assert_eq!(event.acceptance_position, second_position);
    assert_eq!(event.content, second_content);

    drop(commands);
    drop(follow);
    runtime.stop().await
}

/// followers receive the ephemeral provider-text stream.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn inherits_provider_text_streaming() -> Result<(), Box<dyn Error>> {
    let mut runtime = RunningRuntime::start().await?;
    let mut commands = Connection::connect(runtime.socket()).await?;
    let session_id = create_alias_session(&mut commands).await?;
    let follow =
        attach_empty_follower(runtime.socket(), ProtocolVersion::One, 9, session_id).await?;
    let expected_delta_count = 1;
    let (script, assistant) =
        streamed_script(expected_delta_count, String::from("already [redacted]"));
    let (_, turn_id) = submit_first_input(
        &mut commands,
        session_id,
        String::from("retain streamed provider text"),
    )
    .await?;

    let probe = execute_streamed_turn(
        &mut runtime,
        ScriptedModel::single(script),
        session_id,
        turn_id,
    )
    .await?;
    let followed = follow_streamed_turn_to_completion(follow, session_id, turn_id).await?;
    let operations = probe.received_operations();

    assert_eq!(followed.delta_count, expected_delta_count);
    assert_eq!(followed.text, assistant);
    assert_eq!(operations.len(), 1);
    assert_eq!(operations[0].delivery, DeliveryMode::Streamed);

    drop(commands);
    runtime.stop().await
}

/// the provider bridge asks the scripted runtime for streamed delivery, and three already-attached
/// followers each observe the exact already-redacted deltas before durable terminal entries expose
/// the same complete assistant reply.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn streamed_reply_reaches_three_followers_then_durable_truth() -> Result<(), Box<dyn Error>> {
    let mut runtime = RunningRuntime::start().await?;
    let mut commands = Connection::connect(runtime.socket()).await?;
    let session_id = create_alias_session(&mut commands).await?;
    let first_follow =
        attach_empty_follower(runtime.socket(), ProtocolVersion::One, 10, session_id).await?;
    let second_follow =
        attach_empty_follower(runtime.socket(), ProtocolVersion::One, 11, session_id).await?;
    let third_follow =
        attach_empty_follower(runtime.socket(), ProtocolVersion::One, 12, session_id).await?;
    let expected_delta_count = 2;
    let (script, assistant) =
        streamed_script(expected_delta_count, String::from("already [redacted] "));
    let (_, turn_id) =
        submit_first_input(&mut commands, session_id, String::from("stream this reply")).await?;

    let probe = execute_streamed_turn(
        &mut runtime,
        ScriptedModel::single(script),
        session_id,
        turn_id,
    )
    .await?;
    let first = follow_streamed_turn_to_completion(first_follow, session_id, turn_id).await?;
    let second = follow_streamed_turn_to_completion(second_follow, session_id, turn_id).await?;
    let third = follow_streamed_turn_to_completion(third_follow, session_id, turn_id).await?;
    let first_durable = read_completed_assistant(runtime.socket(), 13, session_id, turn_id).await?;
    let second_durable =
        read_completed_assistant(runtime.socket(), 14, session_id, turn_id).await?;
    let third_durable = read_completed_assistant(runtime.socket(), 15, session_id, turn_id).await?;
    let operations = probe.received_operations();

    assert_eq!(first.delta_count, expected_delta_count);
    assert_eq!(first.text, assistant);
    assert_eq!(second.delta_count, expected_delta_count);
    assert_eq!(second.text, assistant);
    assert_eq!(third.delta_count, expected_delta_count);
    assert_eq!(third.text, assistant);
    assert_eq!(first_durable, assistant);
    assert_eq!(second_durable, assistant);
    assert_eq!(third_durable, assistant);
    assert_eq!(operations.len(), 1);
    assert_eq!(operations[0].delivery, DeliveryMode::Streamed);

    drop(commands);
    runtime.stop().await
}

/// a follower that cannot keep up with ephemeral provider deltas receives the existing
/// resynchronization error, loses some deltas, and recovers the exact completed assistant reply
/// from durable transcript truth without any delta persistence or replay.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn streaming_lag_resync_loses_deltas_and_reads_complete_transcript()
-> Result<(), Box<dyn Error>> {
    let mut runtime = RunningRuntime::start().await?;
    let mut commands = Connection::connect(runtime.socket()).await?;
    let session_id = create_alias_session(&mut commands).await?;
    let lagging_follow =
        attach_empty_follower(runtime.socket(), ProtocolVersion::One, 20, session_id).await?;
    let (script, assistant) =
        streamed_script(STREAMING_DELTA_COUNT, "x".repeat(STREAMING_DELTA_BYTES));
    let (_, turn_id) = submit_first_input(
        &mut commands,
        session_id,
        String::from("force follower resynchronization"),
    )
    .await?;

    let probe = execute_streamed_turn(
        &mut runtime,
        ScriptedModel::single(script),
        session_id,
        turn_id,
    )
    .await?;
    let observed_delta_count = receive_resync(lagging_follow).await?;
    let durable = read_completed_assistant(runtime.socket(), 21, session_id, turn_id).await?;
    let operations = probe.received_operations();

    assert!(observed_delta_count < STREAMING_DELTA_COUNT);
    assert_eq!(durable, assistant);
    assert_eq!(operations.len(), 1);
    assert_eq!(operations[0].delivery, DeliveryMode::Streamed);

    drop(commands);
    runtime.stop().await
}

/// followers receive the ephemeral delta stream.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn followers_inherit_the_streamed_deltas() -> Result<(), Box<dyn Error>> {
    let mut runtime = RunningRuntime::start().await?;
    let mut commands = Connection::connect(runtime.socket()).await?;
    let session_id = create_alias_session(&mut commands).await?;
    let follow =
        attach_empty_follower(runtime.socket(), ProtocolVersion::One, 40, session_id).await?;
    let expected_delta_count = 3;
    let (script, assistant) =
        streamed_script(expected_delta_count, String::from("already [redacted] "));
    let (_, turn_id) = submit_first_input(
        &mut commands,
        session_id,
        String::from("stream to the follower"),
    )
    .await?;

    let probe = execute_streamed_turn(
        &mut runtime,
        ScriptedModel::single(script),
        session_id,
        turn_id,
    )
    .await?;
    let followed = follow_streamed_turn_to_completion(follow, session_id, turn_id).await?;
    let durable = read_completed_assistant(runtime.socket(), 41, session_id, turn_id).await?;
    let operations = probe.received_operations();

    assert_eq!(followed.delta_count, expected_delta_count);
    assert_eq!(followed.text, assistant);
    assert_eq!(durable, assistant);
    assert_eq!(operations.len(), 1);
    assert_eq!(operations[0].delivery, DeliveryMode::Streamed);

    drop(commands);
    runtime.stop().await
}
