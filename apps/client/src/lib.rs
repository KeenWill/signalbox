//! Terminal client for the closed local Signalbox process protocol.

use std::{
    ffi::OsString,
    io::{BufRead, BufReader, Read, Seek, SeekFrom, Write},
    path::PathBuf,
    process::ExitCode,
};

use arguments::{Command, ParseOutcome};
use connection::ProcessClient;
use error::ClientError;
use presentation::{Output, SnapshotSelection};
use signalbox_process_protocol::{
    CanonicalU64, CanonicalUuid, ClientRequest, CommandId, ErrorCode, InputContent,
    ModelCallDisposition, ModelCallState, ModelSelection, ServerFrame, ServerMessage, SessionEvent,
    TurnState, decode_server_line, encode_server_line,
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
        .mutation_request(ClientRequest::CreateSession {
            command_id,
            initial_model_selection: selection,
        })
        .await?;
    match connection.message().await.map_err(ClientError::mutation)? {
        ServerMessage::SessionCreated { session_id } => {
            output.session_created(session_id)?;
            Ok(())
        }
        ServerMessage::Error {
            code,
            message,
            detail,
        } => Err(ClientError::remote(code, message, detail).mutation()),
        _ => Err(ClientError::Protocol("create returned an unexpected response").mutation()),
    }
}

async fn list(client: &mut ProcessClient, output: &mut Output<'_>) -> Result<(), ClientError> {
    let mut spool = tempfile::tempfile()?;
    read_session_summaries(client, |_, frame| {
        spool.write_all(&encode_server_line(frame)?)?;
        Ok(())
    })
    .await?;
    spool.seek(SeekFrom::Start(0))?;
    let mut reader = BufReader::new(spool);
    let mut line = Vec::new();
    while reader.read_until(b'\n', &mut line)? != 0 {
        match decode_server_line(&line)?.message() {
            ServerMessage::SessionSummary {
                session_id,
                defaults_version,
                model_selection,
            } => output.session_summary(
                *session_id,
                defaults_version.value(),
                &selection_display(*model_selection),
            )?,
            _ => {
                return Err(ClientError::Protocol(
                    "session-summary spool contained a non-summary frame",
                ));
            }
        }
        line.clear();
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
        None => {
            let mut selected = None;
            read_session_summaries(client, |summary, _| {
                if summary.session_id == session_id {
                    selected = Some(CanonicalU64::new(summary.defaults_version));
                }
                Ok(())
            })
            .await?;
            selected.ok_or(ClientError::Input("the selected session was not listed"))?
        }
    };
    output.recovery_value("defaults_version", &defaults_version.value().to_string())?;

    let turn_id = submit_input(
        client,
        command_id,
        session_id,
        InputContent::new(content),
        defaults_version,
    )
    .await?;

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
        TurnTerminal::Cancelled => Err(ClientError::TurnCancelled),
        TurnTerminal::ReconciliationRequired => Err(ClientError::TurnReconciliationRequired),
    }
}

async fn submit_input(
    client: &mut ProcessClient,
    command_id: CommandId,
    session_id: CanonicalUuid,
    content: InputContent,
    defaults_version: CanonicalU64,
) -> Result<CanonicalUuid, ClientError> {
    let mut connection = client
        .mutation_request(ClientRequest::SubmitInput {
            command_id,
            session_id,
            content,
            expected_defaults_version: defaults_version,
        })
        .await?;
    match connection.message().await.map_err(ClientError::mutation)? {
        ServerMessage::InputSubmitted {
            session_id: submitted_session,
            turn_id,
            ..
        } if submitted_session == session_id => Ok(turn_id),
        ServerMessage::Error {
            code,
            message,
            detail,
        } => Err(ClientError::remote(code, message, detail).mutation()),
        _ => Err(ClientError::Protocol("submit returned an unexpected response").mutation()),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TurnTerminal {
    Completed,
    Failed,
    Refused,
    Cancelled,
    ReconciliationRequired,
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
        Some(TurnState::Cancelled { .. }) => Ok(Some(TurnTerminal::Cancelled)),
        Some(TurnState::ReconciliationRequired { .. }) => {
            Ok(Some(TurnTerminal::ReconciliationRequired))
        }
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
        SessionEvent::TurnCancelled { turn_id, .. } if *turn_id == selected_turn => {
            Some(TurnTerminal::Cancelled)
        }
        SessionEvent::TurnReconciliationRequired { turn_id, .. } if *turn_id == selected_turn => {
            Some(TurnTerminal::ReconciliationRequired)
        }
        SessionEvent::SessionCreated {}
        | SessionEvent::InputAccepted { .. }
        | SessionEvent::TurnActivated { .. }
        | SessionEvent::ModelCallTransition { .. }
        | SessionEvent::TurnCompleted { .. }
        | SessionEvent::TurnFailed { .. }
        | SessionEvent::TurnRefused { .. }
        | SessionEvent::TurnCancelled { .. }
        | SessionEvent::TurnReconciliationRequired { .. } => None,
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
            completion_entry_id,
            ..
        } => Some(SnapshotSelection::Completed {
            turn_id: *turn_id,
            model_call_id: *model_call_id,
            terminal_entry_id: *completion_entry_id,
        }),
        SessionEvent::TurnFailed {
            turn_id,
            failure_entry_id,
            ..
        } => Some(SnapshotSelection::Failed {
            turn_id: *turn_id,
            terminal_entry_id: *failure_entry_id,
        }),
        SessionEvent::TurnCancelled {
            turn_id,
            cancellation_entry_id,
            ..
        } => Some(SnapshotSelection::Cancelled {
            turn_id: *turn_id,
            terminal_entry_id: *cancellation_entry_id,
        }),
        SessionEvent::TurnRefused { .. } | SessionEvent::TurnReconciliationRequired { .. } => None,
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
}

async fn read_session_summaries(
    client: &mut ProcessClient,
    mut consume: impl FnMut(SessionSummary, &ServerFrame) -> Result<(), ClientError>,
) -> Result<(), ClientError> {
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
    let mut prior_session = None;
    let mut summary_count = 0_u64;
    loop {
        let frame = connection.frame().await?;
        match frame.message() {
            ServerMessage::SessionSummary {
                session_id,
                defaults_version,
                ..
            } => {
                if prior_session
                    .is_some_and(|prior: CanonicalUuid| prior.into_uuid() >= session_id.into_uuid())
                {
                    return Err(ClientError::Protocol(
                        "session summaries were not strictly ordered",
                    ));
                }
                let summary = SessionSummary {
                    session_id: *session_id,
                    defaults_version: defaults_version.value(),
                };
                consume(summary, &frame)?;
                prior_session = Some(*session_id);
                summary_count = summary_count
                    .checked_add(1)
                    .ok_or(ClientError::Protocol("session summary count overflowed"))?;
            }
            ServerMessage::SessionsEnd { session_count }
                if session_count.value() == summary_count =>
            {
                return Ok(());
            }
            ServerMessage::Error {
                code,
                message,
                detail,
            } => return Err(ClientError::remote(*code, message.clone(), *detail)),
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
    use std::{
        error::Error,
        ffi::OsString,
        io::{self, Cursor},
        path::PathBuf,
        process::ExitCode,
        time::Duration,
    };

    use signalbox_process_protocol::{
        CanonicalU64, CanonicalUuid, ClientRequest, CommandId, InputContent, ModelCallDisposition,
        ModelCallState, ModelSelection, ServerFrame, ServerMessage, SessionEvent, TurnState,
        decode_client_line, encode_server_line,
    };
    use tokio::{
        io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
        net::UnixListener,
        time::timeout,
    };
    use uuid::Uuid;

    use super::{
        MAX_INPUT_CONTENT_BYTES, ProcessClient, SnapshotSelection, TurnTerminal, create,
        model_call_recovery_transition, read_input, run, socket_path, submit_input,
        terminal_event_state, terminal_snapshot_selection, terminal_snapshot_state,
    };
    use crate::{error::ClientError, presentation::Output};

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
    fn send_classifies_cancelled_snapshot_truth() {
        let state = TurnState::Cancelled {
            terminal_frontier_id: CanonicalUuid::from_uuid(Uuid::from_u128(1)),
            terminal_attempt_id: CanonicalUuid::from_uuid(Uuid::from_u128(2)),
            terminal_model_call_id: None,
        };

        assert_eq!(
            terminal_snapshot_state(Some(&state))
                .expect("cancelled state is terminal protocol truth"),
            Some(TurnTerminal::Cancelled)
        );
    }

    #[test]
    fn send_classifies_reconciliation_required_snapshot_truth() {
        let state = TurnState::ReconciliationRequired {
            terminal_frontier_id: CanonicalUuid::from_uuid(Uuid::from_u128(1)),
            terminal_attempt_id: CanonicalUuid::from_uuid(Uuid::from_u128(2)),
            terminal_model_call_id: CanonicalUuid::from_uuid(Uuid::from_u128(3)),
        };

        assert_eq!(
            terminal_snapshot_state(Some(&state))
                .expect("reconciliation state is terminal protocol truth"),
            Some(TurnTerminal::ReconciliationRequired)
        );
    }

    #[test]
    fn send_classifies_cancelled_event_for_its_turn() {
        let selected_turn = CanonicalUuid::from_uuid(Uuid::from_u128(1));
        let event = SessionEvent::TurnCancelled {
            turn_id: selected_turn,
            cancellation_entry_id: CanonicalUuid::from_uuid(Uuid::from_u128(2)),
            terminal_frontier_id: CanonicalUuid::from_uuid(Uuid::from_u128(3)),
        };

        assert_eq!(
            terminal_event_state(&event, selected_turn),
            Some(TurnTerminal::Cancelled)
        );
    }

    #[test]
    fn send_classifies_reconciliation_required_event_for_its_turn() {
        let selected_turn = CanonicalUuid::from_uuid(Uuid::from_u128(1));
        let event = SessionEvent::TurnReconciliationRequired {
            turn_id: selected_turn,
            model_call_id: CanonicalUuid::from_uuid(Uuid::from_u128(2)),
            terminal_frontier_id: CanonicalUuid::from_uuid(Uuid::from_u128(3)),
        };

        assert_eq!(
            terminal_event_state(&event, selected_turn),
            Some(TurnTerminal::ReconciliationRequired)
        );
    }

    #[test]
    fn cli_socket_path_must_be_absolute() {
        assert!(matches!(
            socket_path(Some(PathBuf::from("relative.sock")), None),
            Err(ClientError::Input(
                "the local process socket path must be absolute"
            ))
        ));
    }

    #[test]
    fn environment_socket_path_must_be_absolute() {
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

    #[test]
    fn refused_terminal_event_requests_no_side_reread() {
        assert!(
            terminal_snapshot_selection(&SessionEvent::TurnRefused {
                turn_id: CanonicalUuid::from_uuid(Uuid::from_u128(1)),
                model_call_id: CanonicalUuid::from_uuid(Uuid::from_u128(2)),
                terminal_frontier_id: CanonicalUuid::from_uuid(Uuid::from_u128(3)),
            })
            .is_none()
        );
    }

    #[test]
    fn cancellation_event_selects_its_exact_marker_for_reread() {
        let turn_id = CanonicalUuid::from_uuid(Uuid::from_u128(1));

        assert!(matches!(
            terminal_snapshot_selection(&SessionEvent::TurnCancelled {
                turn_id,
                cancellation_entry_id: CanonicalUuid::from_uuid(Uuid::from_u128(2)),
                terminal_frontier_id: CanonicalUuid::from_uuid(Uuid::from_u128(3)),
            }),
            Some(SnapshotSelection::Cancelled {
                turn_id: selected,
                terminal_entry_id,
            }) if selected == turn_id && terminal_entry_id == CanonicalUuid::from_uuid(Uuid::from_u128(2))
        ));
    }

    #[test]
    fn reconciliation_event_selects_no_semantic_material_for_reread() {
        let turn_id = CanonicalUuid::from_uuid(Uuid::from_u128(1));

        assert!(
            terminal_snapshot_selection(&SessionEvent::TurnReconciliationRequired {
                turn_id,
                model_call_id: CanonicalUuid::from_uuid(Uuid::from_u128(2)),
                terminal_frontier_id: CanonicalUuid::from_uuid(Uuid::from_u128(3)),
            })
            .is_none()
        );
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

    #[tokio::test]
    async fn create_connection_failure_is_definitely_uncommitted() -> Result<(), Box<dyn Error>> {
        let directory = tempfile::tempdir()?;
        let mut client = ProcessClient::new(directory.path().join("missing.sock"));
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut output = Output::new(&mut stdout, &mut stderr, false);

        let result = create(
            &mut client,
            &mut output,
            ModelSelection::Direct {
                selection_id: CanonicalUuid::from_uuid(Uuid::from_u128(1)),
            },
            Some(CommandId::try_from_uuid(Uuid::from_u128(2))?),
        )
        .await;

        assert!(matches!(result, Err(ClientError::Io(_))));
        Ok(())
    }

    #[tokio::test]
    async fn submit_connection_failure_is_definitely_uncommitted() -> Result<(), Box<dyn Error>> {
        let directory = tempfile::tempdir()?;
        let mut client = ProcessClient::new(directory.path().join("missing.sock"));

        let result = submit_input(
            &mut client,
            CommandId::try_from_uuid(Uuid::from_u128(1))?,
            CanonicalUuid::from_uuid(Uuid::from_u128(2)),
            InputContent::new(String::from("queued content")),
            CanonicalU64::new(1),
        )
        .await;

        assert!(matches!(result, Err(ClientError::Io(_))));
        Ok(())
    }

    #[tokio::test]
    async fn submit_input_releases_its_connection_after_acceptance() -> Result<(), Box<dyn Error>> {
        let directory = tempfile::tempdir()?;
        let socket = directory.path().join("client.sock");
        let listener = UnixListener::bind(&socket)?;
        let session_id = CanonicalUuid::from_uuid(Uuid::from_u128(1));
        let turn_id = CanonicalUuid::from_uuid(Uuid::from_u128(2));
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await?;
            let (reader, mut writer) = stream.into_split();
            let mut reader = BufReader::new(reader);
            let mut line = Vec::new();
            reader.read_until(b'\n', &mut line).await?;
            let request = decode_client_line(&line).map_err(io::Error::other)?;
            assert!(matches!(
                request.request(),
                ClientRequest::SubmitInput {
                    session_id: requested_session,
                    ..
                } if *requested_session == session_id
            ));
            let response = ServerFrame::try_new(
                request.request_id(),
                ServerMessage::InputSubmitted {
                    session_id,
                    accepted_input_id: CanonicalUuid::from_uuid(Uuid::from_u128(3)),
                    acceptance_position: CanonicalU64::new(1),
                    turn_id,
                },
            )
            .map_err(io::Error::other)?;
            writer
                .write_all(&encode_server_line(&response).map_err(io::Error::other)?)
                .await?;

            let mut byte = [0_u8; 1];
            let read = timeout(Duration::from_secs(1), reader.read(&mut byte))
                .await
                .map_err(io::Error::other)??;
            assert_eq!(read, 0);
            Ok::<(), io::Error>(())
        });

        let mut client = ProcessClient::new(socket);
        let submitted_turn = submit_input(
            &mut client,
            CommandId::try_from_uuid(Uuid::from_u128(4))?,
            session_id,
            InputContent::new(String::from("queued content")),
            CanonicalU64::new(1),
        )
        .await?;
        assert_eq!(submitted_turn, turn_id);
        server.await??;
        Ok(())
    }
}
