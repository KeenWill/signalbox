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

fn snapshot(
    request: &ClientFrame,
    session_id: CanonicalUuid,
    turn: Option<(CanonicalUuid, TurnState)>,
) -> io::Result<Vec<u8>> {
    let turn_count = u64::from(turn.is_some());
    let mut messages = vec![ServerMessage::TranscriptSnapshotStart {
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
async fn chat_reconnects_after_snapshot_receipt_is_lost() -> Result<(), Box<dyn Error>> {
    let directory = tempfile::tempdir()?;
    let socket = directory.path().join("client.sock");
    let listener = UnixListener::bind(&socket)?;
    let session = CanonicalUuid::from_uuid(Uuid::from_u128(8));
    let (input, mut input_writer) = tokio::io::duplex(64);
    let (done, finished) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (_, writer) = receive_follow(&listener, session).await?;
        drop(writer);
        let (request, mut writer) = receive_follow(&listener, session).await?;
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
        chat::run(
            &mut client,
            &mut output,
            session,
            BufReader::new(input),
            ClientDeploymentLimits::unbounded(),
        ),
    )
    .await??;
    let _ = done.send(());
    server.await??;
    Ok(())
}
