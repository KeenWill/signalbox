//! Terminal client for the closed local Signalbox process protocol.

use std::{
    ffi::OsString,
    io::{Read, Write},
    path::PathBuf,
    process::ExitCode,
};

use arguments::{Command, ParseOutcome};
use connection::ProcessClient;
use error::ClientError;
use presentation::{Output, SnapshotSelection};
use signalbox_process_protocol::{
    CanonicalU64, CanonicalUuid, ClientRequest, CommandId, ErrorCode, InputContent,
    ModelCallDisposition, ModelCallState, ModelSelection, ServerMessage, SessionEvent, TurnState,
};
use transcript::{SnapshotIdentitySet, SnapshotRecord, TranscriptSnapshot, read_snapshot};
use uuid::Uuid;

mod arguments;
mod connection;
mod error;
mod presentation;
mod transcript;

const MAX_INPUT_CONTENT_BYTES: usize = 1_048_576;

/// Parses and runs one terminal-client invocation.
pub async fn run(
    arguments: impl IntoIterator<Item = OsString>,
    socket_environment: Option<OsString>,
    stdin: &mut dyn Read,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> ExitCode {
    let parsed = match arguments::parse(arguments) {
        Ok(ParseOutcome::Help(help)) => {
            return if write!(stdout, "{help}").is_ok() {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            };
        }
        Ok(ParseOutcome::Run(arguments)) => arguments,
        Err(error) => {
            let _ = write!(stderr, "{error}");
            return ExitCode::from(2);
        }
    };
    let raw_output = parsed.raw_output;
    let result = execute(parsed, socket_environment, stdin, stdout, stderr).await;
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            let mut output = Output::new(stdout, stderr, raw_output);
            let _ = output.error(&error);
            ExitCode::FAILURE
        }
    }
}

async fn execute(
    arguments: arguments::Arguments,
    socket_environment: Option<OsString>,
    stdin: &mut dyn Read,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> Result<(), ClientError> {
    let input = if matches!(arguments.command, Command::Send { .. }) {
        Some(read_input(stdin)?)
    } else {
        None
    };
    let socket = socket_path(arguments.socket, socket_environment)?;
    let mut client = ProcessClient::new(socket);
    let mut output = Output::new(stdout, stderr, arguments.raw_output);

    match arguments.command {
        Command::Create {
            selection,
            command_id,
        } => create(&mut client, &mut output, selection, command_id).await,
        Command::List => list(&mut client, &mut output).await,
        Command::Send {
            session_id,
            command_id,
            defaults_version,
        } => {
            let input = input.ok_or(ClientError::Input("send input was not read"))?;
            send(
                &mut client,
                &mut output,
                session_id,
                command_id,
                defaults_version,
                input,
            )
            .await
        }
        Command::Transcript { session_id } => {
            let mut snapshot = transcript(&mut client, session_id).await?;
            output.snapshot(&mut snapshot)?;
            Ok(())
        }
        Command::Follow { session_id } => follow(&mut client, &mut output, session_id).await,
    }
}

fn socket_path(
    override_path: Option<PathBuf>,
    socket_environment: Option<OsString>,
) -> Result<PathBuf, ClientError> {
    let path = match override_path {
        Some(path) if !path.as_os_str().is_empty() => path,
        Some(_) => return Err(ClientError::Input("--socket requires a nonempty path")),
        None => {
            let value = socket_environment.ok_or(ClientError::Input(
                "set SIGNALBOX_SOCKET_PATH or pass --socket",
            ))?;
            if value.is_empty() {
                return Err(ClientError::Input(
                    "set SIGNALBOX_SOCKET_PATH or pass --socket",
                ));
            }
            PathBuf::from(value)
        }
    };
    if !path.is_absolute() {
        return Err(ClientError::Input(
            "the local process socket path must be absolute",
        ));
    }
    Ok(path)
}

fn read_input(stdin: &mut dyn Read) -> Result<String, ClientError> {
    let mut bytes = Vec::new();
    stdin
        .take((MAX_INPUT_CONTENT_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.is_empty() {
        return Err(ClientError::Input("send input must not be empty"));
    }
    if bytes.len() > MAX_INPUT_CONTENT_BYTES {
        return Err(ClientError::Input(
            "send input exceeds the 1 MiB UTF-8 byte limit",
        ));
    }
    let text = String::from_utf8(bytes)
        .map_err(|_| ClientError::Input("send input must be valid UTF-8"))?;
    if text.contains('\0') {
        return Err(ClientError::Input("send input must not contain U+0000"));
    }
    Ok(text)
}

async fn create(
    client: &mut ProcessClient,
    output: &mut Output<'_>,
    selection: ModelSelection,
    command_id: Option<CommandId>,
) -> Result<(), ClientError> {
    let (command_id, generated) = command_identity(command_id)?;
    if generated {
        output.recovery_value(
            "command_id",
            &command_id.into_uuid().hyphenated().to_string(),
        )?;
    }
    let mut connection = client
        .request(ClientRequest::CreateSession {
            command_id,
            initial_model_selection: selection,
        })
        .await
        .map_err(ClientError::mutation)?;
    match connection.message().await.map_err(ClientError::mutation)? {
        ServerMessage::SessionCreated { session_id } => {
            output.session_created(session_id)?;
            Ok(())
        }
        ServerMessage::Error {
            code,
            message,
            detail,
        } => Err(ClientError::remote(code, message, detail)),
        _ => Err(ClientError::Protocol("create returned an unexpected response").mutation()),
    }
}

async fn list(client: &mut ProcessClient, output: &mut Output<'_>) -> Result<(), ClientError> {
    for summary in read_session_summaries(client).await? {
        output.session_summary(
            summary.session_id,
            summary.defaults_version,
            &selection_display(summary.model_selection),
        )?;
    }
    Ok(())
}

async fn send(
    client: &mut ProcessClient,
    output: &mut Output<'_>,
    session_id: CanonicalUuid,
    command_id: Option<CommandId>,
    defaults_version: Option<CanonicalU64>,
    content: String,
) -> Result<(), ClientError> {
    let (command_id, generated) = command_identity(command_id)?;
    if generated {
        output.recovery_value(
            "command_id",
            &command_id.into_uuid().hyphenated().to_string(),
        )?;
    }
    let defaults_version = match defaults_version {
        Some(version) => version,
        None => read_session_summaries(client)
            .await?
            .into_iter()
            .find(|summary| summary.session_id == session_id)
            .map(|summary| CanonicalU64::new(summary.defaults_version))
            .ok_or(ClientError::Input("the selected session was not listed"))?,
    };
    output.recovery_value("defaults_version", &defaults_version.value().to_string())?;

    let mut connection = client
        .request(ClientRequest::SubmitInput {
            command_id,
            session_id,
            content: InputContent::new(content),
            expected_defaults_version: defaults_version,
        })
        .await
        .map_err(ClientError::mutation)?;
    let turn_id = match connection.message().await.map_err(ClientError::mutation)? {
        ServerMessage::InputSubmitted {
            session_id: submitted_session,
            turn_id,
            ..
        } if submitted_session == session_id => turn_id,
        ServerMessage::Error {
            code,
            message,
            detail,
        } => return Err(ClientError::remote(code, message, detail)),
        _ => {
            return Err(ClientError::Protocol("submit returned an unexpected response").mutation());
        }
    };

    match await_turn_terminal(client, session_id, turn_id).await? {
        TurnTerminal::Completed => {
            let mut snapshot = transcript(client, session_id).await?;
            let state = snapshot.turn_state(turn_id)?;
            if !matches!(state.as_ref(), Some(TurnState::Completed { .. })) {
                return Err(ClientError::Protocol(
                    "terminal reread did not retain completed turn state",
                ));
            }
            write_assistant_texts(&mut snapshot, output, turn_id)?;
            Ok(())
        }
        TurnTerminal::Failed => Err(ClientError::TurnFailed),
        TurnTerminal::Refused => Err(ClientError::TurnRefused),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TurnTerminal {
    Completed,
    Failed,
    Refused,
}

async fn await_turn_terminal(
    client: &mut ProcessClient,
    session_id: CanonicalUuid,
    turn_id: CanonicalUuid,
) -> Result<TurnTerminal, ClientError> {
    loop {
        let mut connection = client
            .request(ClientRequest::FollowSession { session_id })
            .await?;
        let mut snapshot = read_snapshot(&mut connection, session_id).await?;
        let state = snapshot.turn_state(turn_id)?;
        if let Some(terminal) = terminal_snapshot_state(state.as_ref())? {
            return Ok(terminal);
        }
        let mut observed_cursor = snapshot.cursor();
        loop {
            match connection.message().await? {
                ServerMessage::SessionEvent {
                    cursor,
                    session_id: event_session,
                    event,
                } if event_session == session_id => {
                    if cursor.value() <= observed_cursor {
                        continue;
                    }
                    observed_cursor = cursor.value();
                    if let Some(terminal) = terminal_event_state(&event, turn_id) {
                        return Ok(terminal);
                    }
                    if model_call_recovery_transition(&event, turn_id) {
                        let mut refreshed = transcript(client, session_id).await?;
                        let refreshed_state = refreshed.turn_state(turn_id)?;
                        let Some(terminal) = terminal_snapshot_state(refreshed_state.as_ref())?
                        else {
                            return Err(ClientError::Protocol(
                                "an ambiguous model call did not produce recovery or terminal state",
                            ));
                        };
                        return Ok(terminal);
                    }
                }
                ServerMessage::Error {
                    code: ErrorCode::ResyncRequired,
                    ..
                } => break,
                ServerMessage::Error {
                    code,
                    message,
                    detail,
                } => return Err(ClientError::remote(code, message, detail)),
                _ => {
                    return Err(ClientError::Protocol(
                        "follow returned an unexpected response",
                    ));
                }
            }
        }
    }
}

fn model_call_recovery_transition(event: &SessionEvent, selected_turn: CanonicalUuid) -> bool {
    matches!(
        event,
        SessionEvent::ModelCallTransition {
            turn_id,
            state: ModelCallState::Terminal {
                disposition: ModelCallDisposition::Ambiguous,
            },
            ..
        } if *turn_id == selected_turn
    )
}

fn terminal_snapshot_state(state: Option<&TurnState>) -> Result<Option<TurnTerminal>, ClientError> {
    match state {
        Some(TurnState::Completed { .. }) => Ok(Some(TurnTerminal::Completed)),
        Some(TurnState::Failed { .. }) => Ok(Some(TurnTerminal::Failed)),
        Some(TurnState::Refused { .. }) => Ok(Some(TurnTerminal::Refused)),
        Some(TurnState::Queued { .. } | TurnState::ActiveRunning { .. }) => Ok(None),
        Some(TurnState::ActiveAwaitingModelCallRecovery { .. }) => {
            Err(ClientError::TurnRecoveryRequired)
        }
        None => Err(ClientError::Protocol(
            "follow snapshot omitted the submitted turn",
        )),
    }
}

fn terminal_event_state(
    event: &SessionEvent,
    selected_turn: CanonicalUuid,
) -> Option<TurnTerminal> {
    match event {
        SessionEvent::TurnCompleted { turn_id, .. } if *turn_id == selected_turn => {
            Some(TurnTerminal::Completed)
        }
        SessionEvent::TurnFailed { turn_id, .. } if *turn_id == selected_turn => {
            Some(TurnTerminal::Failed)
        }
        SessionEvent::TurnRefused { turn_id, .. } if *turn_id == selected_turn => {
            Some(TurnTerminal::Refused)
        }
        SessionEvent::SessionCreated {}
        | SessionEvent::InputAccepted { .. }
        | SessionEvent::TurnActivated { .. }
        | SessionEvent::ModelCallTransition { .. }
        | SessionEvent::TurnCompleted { .. }
        | SessionEvent::TurnFailed { .. }
        | SessionEvent::TurnRefused { .. } => None,
    }
}

async fn transcript(
    client: &mut ProcessClient,
    session_id: CanonicalUuid,
) -> Result<TranscriptSnapshot, ClientError> {
    let mut connection = client
        .request(ClientRequest::ReadTranscript { session_id })
        .await?;
    read_snapshot(&mut connection, session_id).await
}

async fn follow(
    client: &mut ProcessClient,
    output: &mut Output<'_>,
    session_id: CanonicalUuid,
) -> Result<(), ClientError> {
    let mut displayed_entries = SnapshotIdentitySet::new()?;
    loop {
        let mut connection = client
            .request(ClientRequest::FollowSession { session_id })
            .await?;
        let mut snapshot = read_snapshot(&mut connection, session_id).await?;
        output.followed_snapshot(&mut snapshot, &mut displayed_entries)?;
        let mut observed_cursor = snapshot.cursor();
        loop {
            match connection.message().await? {
                ServerMessage::SessionEvent {
                    cursor,
                    session_id: event_session,
                    event,
                } if event_session == session_id => {
                    if cursor.value() <= observed_cursor {
                        continue;
                    }
                    observed_cursor = cursor.value();
                    output.event(observed_cursor, session_id, &event)?;
                    if let Some(selection) = terminal_snapshot_selection(&event) {
                        let mut refreshed = transcript(client, session_id).await?;
                        output.terminal_material(
                            &mut refreshed,
                            &mut displayed_entries,
                            selection,
                        )?;
                    }
                }
                ServerMessage::Error {
                    code: ErrorCode::ResyncRequired,
                    ..
                } => break,
                ServerMessage::Error {
                    code,
                    message,
                    detail,
                } => return Err(ClientError::remote(code, message, detail)),
                _ => {
                    return Err(ClientError::Protocol(
                        "follow returned an unexpected response",
                    ));
                }
            }
        }
    }
}

fn terminal_snapshot_selection(event: &SessionEvent) -> Option<SnapshotSelection> {
    match event {
        SessionEvent::TurnCompleted {
            turn_id,
            model_call_id,
            ..
        } => Some(SnapshotSelection::Completed {
            turn_id: *turn_id,
            model_call_id: *model_call_id,
        }),
        SessionEvent::TurnFailed { turn_id, .. } => {
            Some(SnapshotSelection::Failed { turn_id: *turn_id })
        }
        SessionEvent::TurnRefused {
            turn_id,
            model_call_id,
            ..
        } => Some(SnapshotSelection::Refused {
            turn_id: *turn_id,
            model_call_id: *model_call_id,
        }),
        SessionEvent::SessionCreated {}
        | SessionEvent::InputAccepted { .. }
        | SessionEvent::TurnActivated { .. }
        | SessionEvent::ModelCallTransition { .. } => None,
    }
}

fn write_assistant_texts(
    snapshot: &mut TranscriptSnapshot,
    output: &mut Output<'_>,
    selected_turn: CanonicalUuid,
) -> Result<(), ClientError> {
    let mut selected_entry = false;
    for record in snapshot.replay()? {
        match record? {
            SnapshotRecord::Entry(entry) => {
                selected_entry = matches!(
                    entry.kind,
                    transcript::SnapshotEntryKind::Text(
                        signalbox_process_protocol::TranscriptTextEntry::Assistant {
                            turn_id,
                            ..
                        }
                    ) if turn_id == selected_turn
                );
            }
            SnapshotRecord::Content(content) if selected_entry => {
                let ends_with_newline = content.content.as_str().ends_with('\n');
                output.assistant_text_fragment(
                    content.content.as_str(),
                    content.final_fragment,
                    ends_with_newline,
                )?;
                if content.final_fragment {
                    selected_entry = false;
                }
            }
            SnapshotRecord::Turn(_) | SnapshotRecord::Content(_) => {}
        }
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SessionSummary {
    session_id: CanonicalUuid,
    defaults_version: u64,
    model_selection: ModelSelection,
}

async fn read_session_summaries(
    client: &mut ProcessClient,
) -> Result<Vec<SessionSummary>, ClientError> {
    let mut connection = client.request(ClientRequest::ListSessions {}).await?;
    match connection.message().await? {
        ServerMessage::SessionsStart {} => {}
        ServerMessage::Error {
            code,
            message,
            detail,
        } => return Err(ClientError::remote(code, message, detail)),
        _ => {
            return Err(ClientError::Protocol(
                "session list did not begin with its start frame",
            ));
        }
    }
    let mut summaries = Vec::new();
    loop {
        match connection.message().await? {
            ServerMessage::SessionSummary {
                session_id,
                defaults_version,
                model_selection,
            } => {
                if summaries.last().is_some_and(|prior: &SessionSummary| {
                    prior.session_id.into_uuid() >= session_id.into_uuid()
                }) {
                    return Err(ClientError::Protocol(
                        "session summaries were not strictly ordered",
                    ));
                }
                summaries.push(SessionSummary {
                    session_id,
                    defaults_version: defaults_version.value(),
                    model_selection,
                });
            }
            ServerMessage::SessionsEnd { session_count }
                if usize::try_from(session_count.value()) == Ok(summaries.len()) =>
            {
                return Ok(summaries);
            }
            ServerMessage::Error {
                code,
                message,
                detail,
            } => return Err(ClientError::remote(code, message, detail)),
            _ => {
                return Err(ClientError::Protocol(
                    "session list sequence or count was invalid",
                ));
            }
        }
    }
}

fn command_identity(supplied: Option<CommandId>) -> Result<(CommandId, bool), ClientError> {
    match supplied {
        Some(command_id) => Ok((command_id, false)),
        None => CommandId::try_from_uuid(Uuid::now_v7())
            .map(|command_id| (command_id, true))
            .map_err(|_| ClientError::Protocol("UUIDv7 generator produced a reserved value")),
    }
}

fn selection_display(selection: ModelSelection) -> String {
    match selection {
        ModelSelection::Direct { selection_id } => format!("model={selection_id}"),
        ModelSelection::Alias { alias_id } => format!("alias={alias_id}"),
    }
}

#[cfg(test)]
mod tests {
    use std::{ffi::OsString, io::Cursor, path::PathBuf, process::ExitCode};

    use signalbox_process_protocol::{
        CanonicalUuid, ModelCallDisposition, ModelCallState, SessionEvent, TurnState,
    };
    use uuid::Uuid;

    use super::{
        MAX_INPUT_CONTENT_BYTES, model_call_recovery_transition, read_input, run, socket_path,
        terminal_snapshot_state,
    };
    use crate::error::ClientError;

    #[test]
    fn empty_send_input_is_rejected() {
        assert!(read_input(&mut Cursor::new(Vec::<u8>::new())).is_err());
    }

    #[test]
    fn nul_in_send_input_is_rejected() {
        assert!(read_input(&mut Cursor::new(b"before\0after".to_vec())).is_err());
    }

    #[test]
    fn oversized_send_input_is_rejected() {
        assert!(read_input(&mut Cursor::new(vec![b'a'; MAX_INPUT_CONTENT_BYTES + 1])).is_err());
    }

    #[test]
    fn exact_limit_send_input_is_accepted() {
        let exact = vec![b'a'; MAX_INPUT_CONTENT_BYTES];
        assert_eq!(
            read_input(&mut Cursor::new(exact.clone()))
                .ok()
                .map(|value| value.into_bytes()),
            Some(exact)
        );
    }

    #[test]
    fn send_fails_explicitly_when_model_call_recovery_is_required() {
        let state = TurnState::ActiveAwaitingModelCallRecovery {
            ended_attempt_id: CanonicalUuid::from_uuid(Uuid::from_u128(1)),
            recovery_model_call_id: CanonicalUuid::from_uuid(Uuid::from_u128(2)),
        };

        assert!(matches!(
            terminal_snapshot_state(Some(&state)),
            Err(ClientError::TurnRecoveryRequired)
        ));
    }

    #[test]
    fn configured_socket_paths_must_be_absolute() {
        assert!(matches!(
            socket_path(Some(PathBuf::from("relative.sock")), None),
            Err(ClientError::Input(
                "the local process socket path must be absolute"
            ))
        ));
        assert!(matches!(
            socket_path(None, Some(OsString::from("relative.sock"))),
            Err(ClientError::Input(
                "the local process socket path must be absolute"
            ))
        ));
    }

    #[test]
    fn selected_turn_ambiguous_model_call_requests_recovery_reread() {
        let selected_turn = CanonicalUuid::from_uuid(Uuid::from_u128(1));
        let event = SessionEvent::ModelCallTransition {
            turn_id: selected_turn,
            model_call_id: CanonicalUuid::from_uuid(Uuid::from_u128(2)),
            state: ModelCallState::Terminal {
                disposition: ModelCallDisposition::Ambiguous,
            },
        };

        assert!(model_call_recovery_transition(&event, selected_turn));
        assert!(!model_call_recovery_transition(
            &event,
            CanonicalUuid::from_uuid(Uuid::from_u128(3))
        ));
    }

    #[tokio::test]
    async fn invalid_send_input_fails_before_a_missing_socket_is_opened() {
        let mut input = Cursor::new(Vec::<u8>::new());
        let mut output = Vec::new();
        let mut error = Vec::new();
        let exit = run(
            [
                OsString::from("--socket"),
                OsString::from("/does/not/exist"),
                OsString::from("send"),
                OsString::from("00000000-0000-0000-0000-000000000001"),
            ],
            None,
            &mut input,
            &mut output,
            &mut error,
        )
        .await;
        assert_eq!(exit, ExitCode::FAILURE);
        assert!(String::from_utf8_lossy(&error).contains("must not be empty"));
    }
}
