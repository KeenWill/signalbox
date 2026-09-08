use std::{error::Error, io, time::Duration};

use signalbox_process_protocol::{
    CanonicalU64, CanonicalUuid, ClientFrame, ClientRequest, ServerFrame, ServerMessage, TurnState,
    decode_client_line, encode_server_line,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, unix::OwnedWriteHalf},
    time::timeout,
};
use uuid::Uuid;

use crate::{
    ProcessClient, TurnTerminal, await_turn_terminal, chat,
    deployment_limits::ClientDeploymentLimits, presentation::Output,
};

async fn receive_follow(
    listener: &UnixListener,
    session: CanonicalUuid,
) -> io::Result<(ClientFrame, OwnedWriteHalf)> {
    let (reader, writer) = listener.accept().await?.0.into_split();
    let mut reader = BufReader::new(reader);
    let mut line = Vec::new();
    reader.read_until(b'\n', &mut line).await?;
    let request = decode_client_line(&line).map_err(io::Error::other)?;
    assert_eq!(
        request.request(),
        &ClientRequest::FollowSession {
            session_id: session
        }
    );
    Ok((request, writer))
}

async fn receive_chat_follow(
    listener: &UnixListener,
    session: CanonicalUuid,
    limits: ClientDeploymentLimits,
) -> io::Result<(ClientFrame, OwnedWriteHalf)> {
    let (reader, mut writer) = listener.accept().await?.0.into_split();
    let mut reader = BufReader::new(reader);
    let mut line = Vec::new();
    reader.read_until(b'\n', &mut line).await?;
    let limits_request = decode_client_line(&line).map_err(io::Error::other)?;
    assert_eq!(
        limits_request.request(),
        &ClientRequest::ReadDeploymentLimits {}
    );
    let response = ServerFrame::try_new_for_version(
        limits_request.version(),
        limits_request.request_id(),
        ServerMessage::DeploymentLimits {
            max_message_utf8_bytes: limits
                .max_message_utf8_bytes
                .map(|value| CanonicalU64::new(value as u64)),
            max_system_prompt_utf8_bytes: limits
                .max_system_prompt_utf8_bytes
                .map(|value| CanonicalU64::new(value as u64)),
            terminal_input_channel_capacity: limits
                .terminal_input_channel_capacity
                .map(|value| CanonicalU64::new(value as u64)),
            min_metadata_page_size: limits.min_metadata_page_size.map(CanonicalU64::new),
            max_metadata_page_size: limits.max_metadata_page_size.map(CanonicalU64::new),
            max_review_findings_per_run: limits.max_review_findings_per_run.map(CanonicalU64::new),
        },
    )
    .map_err(io::Error::other)?;
    writer
        .write_all(&encode_server_line(&response).map_err(io::Error::other)?)
        .await?;
    line.clear();
    reader.read_until(b'\n', &mut line).await?;
    let request = decode_client_line(&line).map_err(io::Error::other)?;
    assert_ne!(limits_request.request_id(), request.request_id());
    assert_eq!(
        request.request(),
        &ClientRequest::FollowSession {
            session_id: session
        }
    );
    Ok((request, writer))
}

fn snapshot(
    request: &ClientFrame,
    session_id: CanonicalUuid,
    turn: Option<(CanonicalUuid, TurnState)>,
) -> io::Result<Vec<u8>> {
    let turn_count = u64::from(turn.is_some());
    let mut messages = vec![ServerMessage::TranscriptSnapshotStart {
        repository_watch: None,
        session_id,
        cursor: CanonicalU64::new(1),
        runner: None,
    }];
    if let Some((turn_id, state)) = turn {
        messages.push(ServerMessage::TranscriptTurn {
            turn_id,
            acceptance_position: CanonicalU64::new(1),
            model_settings: None,
            state,
        });
    }
    messages.push(ServerMessage::TranscriptModelCallsEnd {
        model_call_count: CanonicalU64::new(0),
    });
    messages.push(ServerMessage::TranscriptSnapshotEnd {
        session_id,
        cursor: CanonicalU64::new(1),
        turn_count: CanonicalU64::new(turn_count),
        entry_count: CanonicalU64::new(0),
    });
    let mut encoded = Vec::new();
    for message in messages {
        let frame =
            ServerFrame::try_new_for_version(request.version(), request.request_id(), message)
                .map_err(io::Error::other)?;
        encoded.extend(encode_server_line(&frame).map_err(io::Error::other)?);
    }
    Ok(encoded)
}

#[tokio::test]
async fn send_wait_reconnects_to_the_original_accepted_turn() -> Result<(), Box<dyn Error>> {
    let directory = tempfile::tempdir()?;
    let socket = directory.path().join("client.sock");
    let listener = UnixListener::bind(&socket)?;
    let session = CanonicalUuid::from_uuid(Uuid::from_u128(1));
    let accepted_turn = CanonicalUuid::from_uuid(Uuid::from_u128(2));
    let attempt = CanonicalUuid::from_uuid(Uuid::from_u128(3));
    let server = tokio::spawn(async move {
        let (first, mut writer) = receive_follow(&listener, session).await?;
        writer
            .write_all(&snapshot(
                &first,
                session,
                Some((
                    accepted_turn,
                    TurnState::ActiveRunning {
                        current_attempt_id: attempt,
                        current_model_call: None,
                    },
                )),
            )?)
            .await?;
        drop(writer);
        let (second, mut writer) = receive_follow(&listener, session).await?;
        assert_ne!(first.request_id(), second.request_id());
        writer
            .write_all(&snapshot(
                &second,
                session,
                Some((
                    accepted_turn,
                    TurnState::Cancelled {
                        terminal_frontier_id: CanonicalUuid::from_uuid(Uuid::from_u128(4)),
                        terminal_attempt_id: attempt,
                        terminal_model_call_id: None,
                    },
                )),
            )?)
            .await?;
        Ok::<_, io::Error>(())
    });
    let mut client = ProcessClient::new(socket);
    let result = timeout(
        Duration::from_secs(5),
        await_turn_terminal(&mut client, session, accepted_turn),
    )
    .await??;
    assert_eq!(result, TurnTerminal::Cancelled);
    server.await??;
    Ok(())
}

#[tokio::test]
async fn chat_reconnects_after_snapshot_loss_with_unrelated_limit_changes()
-> Result<(), Box<dyn Error>> {
    let directory = tempfile::tempdir()?;
    let socket = directory.path().join("client.sock");
    let listener = UnixListener::bind(&socket)?;
    let session = CanonicalUuid::from_uuid(Uuid::from_u128(8));
    let (input, mut input_writer) = tokio::io::duplex(64);
    let (done, finished) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (_, writer) =
            receive_chat_follow(&listener, session, ClientDeploymentLimits::unbounded()).await?;
        drop(writer);
        let (request, mut writer) = receive_chat_follow(
            &listener,
            session,
            ClientDeploymentLimits {
                // The smallest positive values distinguish these unrelated limits from unbounded.
                max_system_prompt_utf8_bytes: Some(1),
                min_metadata_page_size: Some(1),
                max_metadata_page_size: Some(1),
                max_review_findings_per_run: Some(1),
                ..ClientDeploymentLimits::unbounded()
            },
        )
        .await?;
        writer
            .write_all(&snapshot(&request, session, None)?)
            .await?;
        input_writer.write_all(b":quit\n").await?;
        let _ = finished.await;
        Ok::<_, io::Error>(())
    });
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut output = Output::new(&mut stdout, &mut stderr, false);
    let mut client = ProcessClient::new(socket);
    timeout(
        Duration::from_secs(5),
        chat::run(&mut client, &mut output, session, |_| {
            Ok(BufReader::new(input))
        }),
    )
    .await??;
    let _ = done.send(());
    server.await??;
    Ok(())
}

#[tokio::test]
async fn chat_rejects_changed_message_limit_on_reconnect() -> Result<(), Box<dyn Error>> {
    chat_rejects_changed_limits_on_reconnect(ClientDeploymentLimits {
        // A finite message budget differs from the original unbounded budget.
        max_message_utf8_bytes: Some(1),
        ..ClientDeploymentLimits::unbounded()
    })
    .await
}

#[tokio::test]
async fn chat_rejects_changed_input_capacity_on_reconnect() -> Result<(), Box<dyn Error>> {
    chat_rejects_changed_limits_on_reconnect(ClientDeploymentLimits {
        // A rendezvous channel differs from the original unbounded channel.
        terminal_input_channel_capacity: Some(0),
        ..ClientDeploymentLimits::unbounded()
    })
    .await
}

async fn chat_rejects_changed_limits_on_reconnect(
    current_limits: ClientDeploymentLimits,
) -> Result<(), Box<dyn Error>> {
    let directory = tempfile::tempdir()?;
    let socket = directory.path().join("client.sock");
    let listener = UnixListener::bind(&socket)?;
    let session = CanonicalUuid::from_uuid(Uuid::from_u128(9));
    let (input, _input_writer) = tokio::io::duplex(64);
    let server = tokio::spawn(async move {
        let (_, writer) =
            receive_chat_follow(&listener, session, ClientDeploymentLimits::unbounded()).await?;
        drop(writer);
        let (_, _writer) = receive_chat_follow(&listener, session, current_limits).await?;
        Ok::<_, io::Error>(())
    });
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut output = Output::new(&mut stdout, &mut stderr, false);
    let mut client = ProcessClient::new(socket);
    let error = timeout(
        Duration::from_secs(5),
        chat::run(&mut client, &mut output, session, |_| {
            Ok(BufReader::new(input))
        }),
    )
    .await?
    .expect_err("changed policy requires a fresh input queue");
    assert_eq!(
        error.to_string(),
        "daemon deployment limits changed; restart chat to apply them"
    );
    server.await??;
    Ok(())
}

async fn disconnect_after_chat_limits(listener: &UnixListener) -> io::Result<()> {
    let stream = listener.accept().await?.0.into_std()?;
    let shutdown = stream.try_clone()?;
    let mut stream = BufReader::new(tokio::net::UnixStream::from_std(stream)?);
    let mut line = Vec::new();
    stream.read_until(b'\n', &mut line).await?;
    let request = decode_client_line(&line).map_err(io::Error::other)?;
    assert_eq!(request.request(), &ClientRequest::ReadDeploymentLimits {});
    // Reject FollowSession writes even though the complete limits response is readable.
    shutdown.shutdown(std::net::Shutdown::Read)?;
    let response = ServerFrame::try_new_for_version(
        request.version(),
        request.request_id(),
        ServerMessage::DeploymentLimits {
            max_message_utf8_bytes: None,
            max_system_prompt_utf8_bytes: None,
            terminal_input_channel_capacity: Some(CanonicalU64::new(0)),
            min_metadata_page_size: None,
            max_metadata_page_size: None,
            max_review_findings_per_run: None,
        },
    )
    .map_err(io::Error::other)?;
    stream
        .write_all(&encode_server_line(&response).map_err(io::Error::other)?)
        .await
}

#[tokio::test]
async fn chat_retries_initial_follow_before_creating_input() -> Result<(), Box<dyn Error>> {
    let directory = tempfile::tempdir()?;
    let socket = directory.path().join("client.sock");
    let listener = UnixListener::bind(&socket)?;
    let session = CanonicalUuid::from_uuid(Uuid::now_v7());
    let (input, mut input_writer) = tokio::io::duplex(64);
    let (done, finished) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        disconnect_after_chat_limits(&listener).await?;
        let (request, mut writer) = receive_chat_follow(
            &listener,
            session,
            ClientDeploymentLimits {
                terminal_input_channel_capacity: Some(1),
                ..ClientDeploymentLimits::unbounded()
            },
        )
        .await?;
        writer
            .write_all(&snapshot(&request, session, None)?)
            .await?;
        input_writer.write_all(b":quit\n").await?;
        let _ = finished.await;
        Ok::<_, io::Error>(())
    });
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut output = Output::new(&mut stdout, &mut stderr, false);
    let mut client = ProcessClient::new(socket);
    timeout(
        Duration::from_secs(5),
        chat::run(&mut client, &mut output, session, |capacity| {
            assert_eq!(
                capacity,
                Some(1),
                "input uses the first successful follow's limits"
            );
            Ok(BufReader::new(input))
        }),
    )
    .await??;
    let _ = done.send(());
    server.await??;
    Ok(())
}

#[tokio::test]
async fn chat_startup_disconnects_share_the_follow_retry_budget() -> Result<(), Box<dyn Error>> {
    let directory = tempfile::tempdir()?;
    let socket = directory.path().join("client.sock");
    let listener = UnixListener::bind(&socket)?;
    let session = CanonicalUuid::from_uuid(Uuid::now_v7());
    let server = tokio::spawn(async move {
        for _ in 0..3 {
            disconnect_after_chat_limits(&listener).await?;
        }
        let (_, writer) =
            receive_chat_follow(&listener, session, ClientDeploymentLimits::unbounded()).await?;
        drop(writer);
        Ok::<_, io::Error>(())
    });
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut output = Output::new(&mut stdout, &mut stderr, false);
    let mut client = ProcessClient::new(socket);
    let error = timeout(
        Duration::from_secs(5),
        chat::run(&mut client, &mut output, session, |_| {
            Ok(BufReader::new(tokio::io::empty()))
        }),
    )
    .await?
    .expect_err("startup failures must consume the same three follow retries");
    assert!(matches!(error, crate::ClientError::ConnectionClosed));
    server.await??;
    Ok(())
}
