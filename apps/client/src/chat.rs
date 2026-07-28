use std::fmt;

use signalbox_process_protocol::{
    CanonicalUuid, ClientRequest, ErrorCode, InputContent, ModelSelection, ServerMessage,
    SessionEvent, ToolDecision, TurnState,
};
use tokio::io::{AsyncBufRead, AsyncBufReadExt as _};
use uuid::Uuid;

use crate::{
    MAX_INPUT_CONTENT_BYTES, ModelSystemPromptChoice, command_identity, connection::ProcessClient,
    decide, error::ClientError, presentation::Output, read_snapshot, replace_session_model,
    resolve_defaults_version, stop_turn, submit_input, terminal_snapshot_selection, transcript,
    transcript::SnapshotIdentitySet,
};

const MAX_CHAT_LINE_BYTES: usize = MAX_INPUT_CONTENT_BYTES + ":stop ".len();

const COMMANDS: &str =
    ":stop TEXT | :approve ID | :deny ID REASON | :transcript | :model ALIAS-UUID | :quit";

#[derive(Debug, Eq, PartialEq)]
enum LineRead {
    Line(String),
    Rejected(&'static str),
    Eof,
}

struct BoundedLines<R> {
    reader: R,
    buffer: Vec<u8>,
    discarding: bool,
}

impl<R> BoundedLines<R>
where
    R: AsyncBufRead + Unpin,
{
    fn new(reader: R) -> Self {
        Self {
            reader,
            buffer: Vec::new(),
            discarding: false,
        }
    }

    async fn next_line(&mut self) -> std::io::Result<LineRead> {
        loop {
            let available = self.reader.fill_buf().await?;
            if available.is_empty() {
                if self.discarding {
                    self.discarding = false;
                    self.buffer.clear();
                    return Ok(LineRead::Rejected(
                        "interactive line exceeds the 1 MiB content bound",
                    ));
                }
                if self.buffer.is_empty() {
                    return Ok(LineRead::Eof);
                }
                return Ok(self.take_line());
            }
            let newline = available.iter().position(|byte| *byte == b'\n');
            let consumed = newline.map_or(available.len(), |position| position + 1);
            let content_length = newline.unwrap_or(available.len());
            if !self.discarding {
                let remaining = MAX_CHAT_LINE_BYTES.saturating_sub(self.buffer.len());
                let copied = content_length.min(remaining);
                self.buffer.extend_from_slice(&available[..copied]);
                if copied < content_length {
                    self.discarding = true;
                }
            }
            self.reader.consume(consumed);
            if newline.is_some() {
                if self.discarding {
                    self.discarding = false;
                    self.buffer.clear();
                    return Ok(LineRead::Rejected(
                        "interactive line exceeds the 1 MiB content bound",
                    ));
                }
                return Ok(self.take_line());
            }
        }
    }

    fn take_line(&mut self) -> LineRead {
        if self.buffer.last() == Some(&b'\r') {
            self.buffer.pop();
        }
        let bytes = std::mem::take(&mut self.buffer);
        match String::from_utf8(bytes) {
            Ok(line) => LineRead::Line(line),
            Err(_) => LineRead::Rejected("interactive input must be UTF-8"),
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
enum ChatInput {
    Submit(String),
    Stop(String),
    Approve(CanonicalUuid),
    Deny {
        tool_request_id: CanonicalUuid,
        reason: String,
    },
    Transcript,
    Model(CanonicalUuid),
    Quit,
}

#[derive(Debug, Eq, PartialEq)]
struct ChatSyntaxError(&'static str);

impl fmt::Display for ChatSyntaxError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.0)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum InterruptAction {
    OfferStop,
    ExitRunning,
    ExitIdle,
}

#[derive(Debug, Default)]
struct InterruptState {
    offered_stop: bool,
}

impl InterruptState {
    fn received(&mut self, active_turn: Option<CanonicalUuid>) -> InterruptAction {
        match (active_turn, self.offered_stop) {
            (Some(_), false) => {
                self.offered_stop = true;
                InterruptAction::OfferStop
            }
            (Some(_), true) => InterruptAction::ExitRunning,
            (None, _) => InterruptAction::ExitIdle,
        }
    }

    fn reset(&mut self) {
        self.offered_stop = false;
    }
}

pub(crate) async fn run<R>(
    client: &mut ProcessClient,
    output: &mut Output<'_>,
    session_id: CanonicalUuid,
    input: R,
) -> Result<(), ClientError>
where
    R: AsyncBufRead + Unpin,
{
    let mut lines = BoundedLines::new(input);
    let mut displayed_entries = SnapshotIdentitySet::new()?;
    let mut interrupts = InterruptState::default();

    'resubscribe: loop {
        let mut connection = client
            .request(ClientRequest::FollowSession { session_id })
            .await?;
        let mut snapshot = read_snapshot(&mut connection, session_id).await?;
        let mut active_turn = snapshot.active_turn()?;
        let mut observed_cursor = snapshot.cursor();
        output.followed_snapshot(&mut snapshot, &mut displayed_entries)?;
        output.chat_started(session_id, active_turn, COMMANDS)?;

        loop {
            tokio::select! {
                message = connection.message() => {
                    match message? {
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
                            let active_terminalized =
                                update_active_from_event(&mut active_turn, &event);
                            if let Some(selection) = terminal_snapshot_selection(&event) {
                                let mut refreshed = transcript(client, session_id).await?;
                                output.terminal_material(
                                    &mut refreshed,
                                    &mut displayed_entries,
                                    selection,
                                )?;
                                render_approval_wait(output, &mut refreshed, &event)?;
                            }
                            if active_terminalized {
                                interrupts.reset();
                                output.chat_ready(session_id)?;
                            }
                            output.flush()?;
                        }
                        ServerMessage::ProviderTextDelta {
                            session_id: delta_session,
                            turn_id,
                            model_call_id,
                            part_index,
                            content,
                        } if delta_session == session_id => {
                            output.provider_text_delta(
                                session_id,
                                turn_id,
                                model_call_id,
                                part_index.value(),
                                content.as_str(),
                            )?;
                        }
                        ServerMessage::Error {
                            code: ErrorCode::ResyncRequired,
                            ..
                        } => continue 'resubscribe,
                        ServerMessage::Error {
                            code,
                            message,
                            detail,
                        } => return Err(ClientError::remote(code, message, detail)),
                        _ => return Err(ClientError::Protocol(
                            "chat follow returned an unexpected response",
                        )),
                    }
                }
                line = lines.next_line() => {
                    let line = match line? {
                        LineRead::Line(line) => line,
                        LineRead::Rejected(message) => {
                            output.chat_usage(message, COMMANDS)?;
                            continue;
                        }
                        LineRead::Eof => {
                            output.chat_exiting(active_turn)?;
                            return Ok(());
                        }
                    };
                    let action = match parse_line(line) {
                        Ok(action) => action,
                        Err(error) => {
                            output.chat_usage(&error.to_string(), COMMANDS)?;
                            continue;
                        }
                    };
                    match action {
                        ChatInput::Submit(content) => {
                            let Some(_) = active_turn else {
                                match submit(client, output, session_id, content).await {
                                    Ok(turn_id) => {
                                        active_turn = Some(turn_id);
                                        interrupts.reset();
                                        output.chat_submitted(turn_id)?;
                                    }
                                    Err(error) => output.error(&error)?,
                                }
                                continue;
                            };
                            output.chat_usage(
                                "a turn is already active; use an in-loop command",
                                COMMANDS,
                            )?;
                        }
                        ChatInput::Stop(content) => {
                            let Some(turn_id) = active_turn else {
                                output.chat_usage("the session has no active turn to stop", COMMANDS)?;
                                continue;
                            };
                            match stop(client, output, session_id, turn_id, content).await {
                                Ok(successor_turn_id) => {
                                    active_turn = Some(successor_turn_id);
                                    interrupts.reset();
                                    output.chat_stopped(turn_id, successor_turn_id)?;
                                }
                                Err(error) => output.error(&error)?,
                            }
                        }
                        ChatInput::Approve(tool_request_id) => {
                            if let Err(error) = decide(
                                client,
                                output,
                                session_id,
                                tool_request_id,
                                None,
                                ToolDecision::Approve {},
                            )
                            .await
                            {
                                output.error(&error)?;
                            }
                        }
                        ChatInput::Deny {
                            tool_request_id,
                            reason,
                        } => {
                            if let Err(error) = decide(
                                client,
                                output,
                                session_id,
                                tool_request_id,
                                None,
                                ToolDecision::Deny { reason },
                            )
                            .await
                            {
                                output.error(&error)?;
                            }
                        }
                        ChatInput::Transcript => match transcript(client, session_id).await {
                            Ok(mut snapshot) => output.snapshot(&mut snapshot)?,
                            Err(error) => output.error(&error)?,
                        },
                        ChatInput::Model(alias_id) => {
                            if let Err(error) = replace_session_model(
                                client,
                                output,
                                session_id,
                                ModelSelection::Alias { alias_id },
                                None,
                                None,
                                None,
                                ModelSystemPromptChoice::Keep,
                            )
                            .await
                            {
                                output.error(&error)?;
                            }
                        }
                        ChatInput::Quit => {
                            output.chat_exiting(active_turn)?;
                            return Ok(());
                        }
                    }
                    output.flush()?;
                }
                interrupt = tokio::signal::ctrl_c() => {
                    interrupt.map_err(ClientError::Io)?;
                    match interrupts.received(active_turn) {
                        InterruptAction::OfferStop => output.chat_interrupt_offered(COMMANDS)?,
                        InterruptAction::ExitRunning => {
                            output.chat_exiting(active_turn)?;
                            return Ok(());
                        }
                        InterruptAction::ExitIdle => {
                            output.chat_exiting(None)?;
                            return Ok(());
                        }
                    }
                }
            }
        }
    }
}

async fn submit(
    client: &mut ProcessClient,
    output: &mut Output<'_>,
    session_id: CanonicalUuid,
    content: String,
) -> Result<CanonicalUuid, ClientError> {
    let (command_id, _) = command_identity(None)?;
    output.recovery_value(
        "command_id",
        &command_id.into_uuid().hyphenated().to_string(),
    )?;
    let defaults_version = resolve_defaults_version(client, output, session_id, None).await?;
    submit_input(
        client,
        command_id,
        session_id,
        InputContent::new(content),
        defaults_version,
    )
    .await
}

async fn stop(
    client: &mut ProcessClient,
    output: &mut Output<'_>,
    session_id: CanonicalUuid,
    active_turn: CanonicalUuid,
    content: String,
) -> Result<CanonicalUuid, ClientError> {
    let (command_id, _) = command_identity(None)?;
    output.recovery_value(
        "command_id",
        &command_id.into_uuid().hyphenated().to_string(),
    )?;
    output.recovery_value("turn", &active_turn.to_string())?;
    let defaults_version = resolve_defaults_version(client, output, session_id, None).await?;
    stop_turn(
        client,
        command_id,
        session_id,
        active_turn,
        InputContent::new(content),
        defaults_version,
    )
    .await
}

fn update_active_from_event(active_turn: &mut Option<CanonicalUuid>, event: &SessionEvent) -> bool {
    match event {
        SessionEvent::InputAccepted { turn_id, .. }
        | SessionEvent::TurnActivated { turn_id, .. }
            if active_turn.is_none() || *active_turn == Some(*turn_id) =>
        {
            *active_turn = Some(*turn_id);
            false
        }
        SessionEvent::TurnCompleted { turn_id, .. }
        | SessionEvent::TurnFailed { turn_id, .. }
        | SessionEvent::TurnRefused { turn_id, .. }
        | SessionEvent::TurnCancelled { turn_id, .. }
        | SessionEvent::TurnReconciliationRequired { turn_id, .. }
        | SessionEvent::TurnToolReconciliationRequired { turn_id, .. }
            if *active_turn == Some(*turn_id) =>
        {
            *active_turn = None;
            true
        }
        SessionEvent::InputAccepted { .. }
        | SessionEvent::TurnActivated { .. }
        | SessionEvent::SessionCreated {}
        | SessionEvent::ModelCallTransition { .. }
        | SessionEvent::ToolBatchTransition { .. }
        | SessionEvent::TurnCompleted { .. }
        | SessionEvent::TurnFailed { .. }
        | SessionEvent::TurnRefused { .. }
        | SessionEvent::TurnCancelled { .. }
        | SessionEvent::TurnReconciliationRequired { .. }
        | SessionEvent::TurnToolReconciliationRequired { .. } => false,
    }
}

fn render_approval_wait(
    output: &mut Output<'_>,
    snapshot: &mut crate::transcript::TranscriptSnapshot,
    event: &SessionEvent,
) -> Result<(), ClientError> {
    let SessionEvent::ToolBatchTransition { turn_id, .. } = event else {
        return Ok(());
    };
    if let Some(TurnState::ActiveAwaitingToolApproval { tool_request_id }) =
        snapshot.turn_state(*turn_id)?
    {
        output.chat_awaiting_approval(*turn_id, tool_request_id)?;
    }
    Ok(())
}

fn parse_line(line: String) -> Result<ChatInput, ChatSyntaxError> {
    if !line.starts_with(':') {
        validate_content(&line, "input must not be empty")?;
        return Ok(ChatInput::Submit(line));
    }
    if line == ":transcript" {
        return Ok(ChatInput::Transcript);
    }
    if line == ":quit" {
        return Ok(ChatInput::Quit);
    }
    if let Some(content) = line.strip_prefix(":stop ") {
        validate_content(content, ":stop requires successor text")?;
        return Ok(ChatInput::Stop(content.to_owned()));
    }
    if let Some(value) = line.strip_prefix(":approve ") {
        return parse_uuid(value, ":approve requires one canonical request UUID")
            .map(ChatInput::Approve);
    }
    if let Some(value) = line.strip_prefix(":model ") {
        return parse_uuid(value, ":model requires one canonical alias UUID").map(ChatInput::Model);
    }
    if let Some(arguments) = line.strip_prefix(":deny ") {
        let (request, reason) = arguments.split_once(' ').ok_or(ChatSyntaxError(
            ":deny requires a canonical request UUID and reason",
        ))?;
        if reason.is_empty() || reason.trim() != reason {
            return Err(ChatSyntaxError(
                ":deny reason must be nonempty with no surrounding whitespace",
            ));
        }
        return Ok(ChatInput::Deny {
            tool_request_id: parse_uuid(request, ":deny requires a canonical request UUID")?,
            reason: reason.to_owned(),
        });
    }
    Err(ChatSyntaxError("unknown chat command"))
}

fn validate_content(content: &str, empty_message: &'static str) -> Result<(), ChatSyntaxError> {
    if content.is_empty() {
        return Err(ChatSyntaxError(empty_message));
    }
    if content.len() > MAX_INPUT_CONTENT_BYTES {
        return Err(ChatSyntaxError("input exceeds the 1 MiB UTF-8 bound"));
    }
    if content.contains('\0') {
        return Err(ChatSyntaxError("input must not contain U+0000"));
    }
    Ok(())
}

fn parse_uuid(value: &str, message: &'static str) -> Result<CanonicalUuid, ChatSyntaxError> {
    let parsed = Uuid::parse_str(value).map_err(|_| ChatSyntaxError(message))?;
    if parsed.hyphenated().to_string() != value {
        return Err(ChatSyntaxError(message));
    }
    Ok(CanonicalUuid::from_uuid(parsed))
}

#[cfg(test)]
mod tests {
    use super::*;

    const REQUEST: &str = "00000000-0000-0000-0000-000000000123";

    #[tokio::test]
    async fn bounded_lines_admits_an_exact_bound_and_strips_its_newline() {
        let expected = "x".repeat(MAX_CHAT_LINE_BYTES);
        let input = format!("{expected}\n");
        let mut lines = BoundedLines::new(tokio::io::BufReader::new(input.as_bytes()));

        assert_eq!(
            lines.next_line().await.expect("fixture line read"),
            LineRead::Line(expected)
        );
    }

    #[tokio::test]
    async fn bounded_lines_discards_an_oversized_line_then_reads_its_successor() {
        let oversized = "x".repeat(MAX_CHAT_LINE_BYTES + 1);
        let successor = "next line";
        let input = format!("{oversized}\n{successor}\n");
        let mut lines = BoundedLines::new(tokio::io::BufReader::new(input.as_bytes()));

        assert_eq!(
            lines.next_line().await.expect("fixture line read"),
            LineRead::Rejected("interactive line exceeds the 1 MiB content bound")
        );
        assert_eq!(
            lines.next_line().await.expect("fixture line read"),
            LineRead::Line(String::from(successor))
        );
    }

    #[tokio::test]
    async fn bounded_lines_rejects_non_utf8_without_losing_the_next_line() {
        let input = [0xff, b'\n', b'o', b'k', b'\n'];
        let mut lines = BoundedLines::new(tokio::io::BufReader::new(input.as_slice()));

        assert_eq!(
            lines.next_line().await.expect("fixture line read"),
            LineRead::Rejected("interactive input must be UTF-8")
        );
        assert_eq!(
            lines.next_line().await.expect("fixture line read"),
            LineRead::Line(String::from("ok"))
        );
    }

    #[test]
    fn chat_parser_keeps_plain_input_exact() {
        assert_eq!(
            parse_line(String::from("  exact owner text  ")),
            Ok(ChatInput::Submit(String::from("  exact owner text  ")))
        );
    }

    #[test]
    fn chat_parser_maps_the_closed_control_set() {
        let request = CanonicalUuid::from_uuid(Uuid::parse_str(REQUEST).expect("fixture UUID"));

        assert_eq!(
            parse_line(String::from(":stop continue here")),
            Ok(ChatInput::Stop(String::from("continue here")))
        );
        assert_eq!(
            parse_line(format!(":approve {REQUEST}")),
            Ok(ChatInput::Approve(request))
        );
        assert_eq!(
            parse_line(format!(":deny {REQUEST} not allowed")),
            Ok(ChatInput::Deny {
                tool_request_id: request,
                reason: String::from("not allowed")
            })
        );
        assert_eq!(
            parse_line(String::from(":transcript")),
            Ok(ChatInput::Transcript)
        );
        assert_eq!(
            parse_line(format!(":model {REQUEST}")),
            Ok(ChatInput::Model(request))
        );
        assert_eq!(parse_line(String::from(":quit")), Ok(ChatInput::Quit));
    }

    #[test]
    fn chat_parser_rejects_stop_without_successor_content() {
        assert_eq!(
            parse_line(String::from(":stop")),
            Err(ChatSyntaxError("unknown chat command"))
        );
    }

    #[test]
    fn chat_parser_rejects_a_malformed_approval_identity() {
        assert_eq!(
            parse_line(String::from(":approve not-a-uuid")),
            Err(ChatSyntaxError(
                ":approve requires one canonical request UUID"
            ))
        );
    }

    #[test]
    fn chat_parser_rejects_an_unknown_command() {
        assert_eq!(
            parse_line(String::from(":later")),
            Err(ChatSyntaxError("unknown chat command"))
        );
    }

    #[test]
    fn first_interrupt_offers_stop_and_second_exits_without_stopping() {
        let active_turn = CanonicalUuid::from_uuid(Uuid::from_u128(1));
        let mut state = InterruptState::default();

        assert_eq!(
            state.received(Some(active_turn)),
            InterruptAction::OfferStop
        );
        assert_eq!(
            state.received(Some(active_turn)),
            InterruptAction::ExitRunning
        );
    }

    #[test]
    fn delayed_old_turn_events_do_not_replace_or_terminalize_a_local_successor() {
        let old_turn = CanonicalUuid::from_uuid(Uuid::from_u128(1));
        let successor_turn = CanonicalUuid::from_uuid(Uuid::from_u128(2));
        let mut active_turn = Some(successor_turn);

        assert!(!update_active_from_event(
            &mut active_turn,
            &SessionEvent::TurnActivated {
                turn_id: old_turn,
                current_attempt_id: CanonicalUuid::from_uuid(Uuid::from_u128(3)),
            }
        ));
        assert_eq!(active_turn, Some(successor_turn));
        assert!(!update_active_from_event(
            &mut active_turn,
            &SessionEvent::TurnCancelled {
                turn_id: old_turn,
                cancellation_entry_id: CanonicalUuid::from_uuid(Uuid::from_u128(4)),
                terminal_frontier_id: CanonicalUuid::from_uuid(Uuid::from_u128(5)),
            }
        ));
        assert_eq!(active_turn, Some(successor_turn));
    }
}
