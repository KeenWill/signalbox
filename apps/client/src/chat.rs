use std::{
    fmt,
    io::{self, BufRead as _},
    pin::Pin,
    task::{Context, Poll},
};

#[cfg(test)]
use signalbox_process_protocol::CanonicalU64;
use signalbox_process_protocol::{
    CanonicalUuid, ClientRequest, ErrorCode, InputContent, ModelSelection, ServerMessage,
    SessionEvent, ToolDecision, TurnState,
};
use tokio::{
    io::{AsyncBufRead, AsyncBufReadExt as _, AsyncRead, ReadBuf},
    sync::mpsc,
};
use uuid::Uuid;

use crate::{
    MAX_INPUT_CONTENT_BYTES, ModelSystemPromptChoice, SubmitInputReceipt, command_identity,
    connection::ProcessClient,
    decide,
    error::ClientError,
    presentation::{ChatTurnStatus, Output},
    read_snapshot, replace_session_model, resolve_defaults_version, steer, stop_turn, submit_input,
    terminal_snapshot_selection, transcript,
    transcript::SnapshotIdentitySet,
};

const MAX_CHAT_LINE_BYTES: usize = MAX_INPUT_CONTENT_BYTES + ":steer ".len();
const TERMINAL_INPUT_CHANNEL_CAPACITY: usize = 1;

const COMMANDS: &str = ":stop TEXT | :steer TEXT | :approve ID | :deny ID REASON | \
    :transcript | :model ALIAS-UUID | :quit";

pub(crate) struct TerminalInput {
    receiver: mpsc::Receiver<io::Result<Vec<u8>>>,
    chunk: Vec<u8>,
    consumed: usize,
}

pub(crate) fn terminal_input() -> io::Result<TerminalInput> {
    let (sender, receiver) = mpsc::channel(TERMINAL_INPUT_CHANNEL_CAPACITY);
    std::thread::Builder::new()
        .name(String::from("signalbox-chat-stdin"))
        .spawn(move || read_terminal_input(sender))?;

    Ok(TerminalInput {
        receiver,
        chunk: Vec::new(),
        consumed: 0,
    })
}

fn read_terminal_input(sender: mpsc::Sender<io::Result<Vec<u8>>>) {
    let stdin = io::stdin();
    let mut input = stdin.lock();
    loop {
        let available = match input.fill_buf() {
            Ok(available) => available,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => {
                let _ = sender.blocking_send(Err(error));
                return;
            }
        };
        if available.is_empty() {
            return;
        }
        let consumed = available.len();
        let chunk = available.to_vec();
        input.consume(consumed);
        if sender.blocking_send(Ok(chunk)).is_err() {
            return;
        }
    }
}

impl AsyncRead for TerminalInput {
    fn poll_read(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let available = match AsyncBufRead::poll_fill_buf(self.as_mut(), context) {
            Poll::Ready(Ok(available)) => available,
            Poll::Ready(Err(error)) => return Poll::Ready(Err(error)),
            Poll::Pending => return Poll::Pending,
        };
        let copied = available.len().min(buffer.remaining());
        buffer.put_slice(&available[..copied]);
        AsyncBufRead::consume(self, copied);
        Poll::Ready(Ok(()))
    }
}

impl AsyncBufRead for TerminalInput {
    fn poll_fill_buf(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<&[u8]>> {
        let this = self.get_mut();
        loop {
            if this.consumed < this.chunk.len() {
                return Poll::Ready(Ok(&this.chunk[this.consumed..]));
            }
            match this.receiver.poll_recv(context) {
                Poll::Ready(Some(Ok(chunk))) => {
                    this.chunk = chunk;
                    this.consumed = 0;
                }
                Poll::Ready(Some(Err(error))) => return Poll::Ready(Err(error)),
                Poll::Ready(None) => return Poll::Ready(Ok(&[])),
                Poll::Pending => return Poll::Pending,
            }
        }
    }

    fn consume(self: Pin<&mut Self>, amount: usize) {
        let this = self.get_mut();
        this.consumed = this.consumed.saturating_add(amount).min(this.chunk.len());
    }
}
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
    Steer(String),
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

#[derive(Debug, Default)]
struct ChatTurns {
    awaited_turn: Option<CanonicalUuid>,
    active_turn: Option<CanonicalUuid>,
}

impl ChatTurns {
    fn status(&self) -> Option<ChatTurnStatus> {
        if let Some(turn_id) = self.active_turn {
            return Some(ChatTurnStatus::Active(turn_id));
        }
        self.awaited_turn.map(ChatTurnStatus::Queued)
    }

    fn awaiting_reply(&self) -> bool {
        self.awaited_turn.is_some()
    }

    fn active_turn(&self) -> Option<CanonicalUuid> {
        self.active_turn
    }

    fn queued(&mut self, turn_id: CanonicalUuid) {
        self.awaited_turn = Some(turn_id);
        self.active_turn = None;
    }

    fn accepted(&mut self, turn_id: CanonicalUuid) {
        if self.awaited_turn.is_none() {
            self.queued(turn_id);
        }
    }

    fn activated(&mut self, turn_id: CanonicalUuid) -> bool {
        if self.awaited_turn.is_none() || self.awaited_turn == Some(turn_id) {
            self.awaited_turn = Some(turn_id);
            self.active_turn = Some(turn_id);
            return true;
        }
        false
    }

    fn terminalized(&mut self, turn_id: CanonicalUuid) -> bool {
        if self.active_turn == Some(turn_id) {
            self.active_turn = None;
        }
        if self.awaited_turn == Some(turn_id) {
            self.awaited_turn = None;
            return true;
        }
        false
    }

    fn resynchronize(
        &mut self,
        snapshot: &mut crate::transcript::TranscriptSnapshot,
    ) -> Result<(), ClientError> {
        match snapshot.active_turn()? {
            Some(turn_id) => {
                self.awaited_turn = Some(turn_id);
                self.active_turn = Some(turn_id);
            }
            None => {
                self.active_turn = None;
                if let Some(awaited_turn) = self.awaited_turn
                    && !matches!(
                        snapshot.turn_state(awaited_turn)?,
                        Some(TurnState::Queued { .. })
                    )
                {
                    self.awaited_turn = None;
                }
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum InterruptAction {
    OfferStop,
    ExitActive,
    ExitQueued,
    ExitIdle,
}

#[derive(Debug, Default)]
struct InterruptState {
    offered_stop: bool,
}

impl InterruptState {
    fn received(&mut self, status: Option<ChatTurnStatus>) -> InterruptAction {
        match (status, self.offered_stop) {
            (Some(ChatTurnStatus::Active(_)), false) => {
                self.offered_stop = true;
                InterruptAction::OfferStop
            }
            (Some(ChatTurnStatus::Active(_)), true) => InterruptAction::ExitActive,
            (Some(ChatTurnStatus::Queued(_)), _) => InterruptAction::ExitQueued,
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
    let mut turns = ChatTurns::default();

    'resubscribe: loop {
        let mut connection = client
            .request(ClientRequest::FollowSession { session_id })
            .await?;
        let mut snapshot = read_snapshot(&mut connection, session_id).await?;
        turns.resynchronize(&mut snapshot)?;
        let mut observed_cursor = snapshot.cursor();
        output.followed_snapshot(&mut snapshot, &mut displayed_entries)?;
        output.chat_started(session_id, turns.status(), COMMANDS)?;

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
                            let turn_effect = update_turns_from_event(&mut turns, &event);
                            if let Some(selection) = terminal_snapshot_selection(&event) {
                                let mut refreshed = transcript(client, session_id).await?;
                                output.terminal_material(
                                    &mut refreshed,
                                    &mut displayed_entries,
                                    selection,
                                )?;
                                render_approval_wait(output, &mut refreshed, &event)?;
                            }
                            match turn_effect {
                                TurnEventEffect::Activated(turn_id) => output.chat_activated(turn_id)?,
                                TurnEventEffect::Ready => {
                                    interrupts.reset();
                                    output.chat_ready(session_id)?;
                                }
                                TurnEventEffect::None => {}
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
                            output.chat_exiting(turns.status())?;
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
                            if turns.awaiting_reply() {
                                output.chat_usage(
                                    "a turn is queued or active; use an in-loop command",
                                    COMMANDS,
                                )?;
                                continue;
                            }
                            match submit(client, output, session_id, content).await {
                                Ok(turn_id) => {
                                    turns.queued(turn_id);
                                    interrupts.reset();
                                    output.chat_queued(turn_id)?;
                                }
                                Err(error) => output.error(&error)?,
                            }
                        }
                        ChatInput::Stop(content) => {
                            let Some(turn_id) = turns.active_turn() else {
                                output.chat_usage("the session has no active turn to stop", COMMANDS)?;
                                continue;
                            };
                            match stop(client, output, session_id, turn_id, content).await {
                                Ok(successor_turn_id) => {
                                    turns.queued(successor_turn_id);
                                    interrupts.reset();
                                    output.chat_stopped(turn_id, successor_turn_id)?;
                                }
                                Err(error) => output.error(&error)?,
                            }
                        }
                        ChatInput::Steer(content) => {
                            let Some(turn_id) = turns.active_turn() else {
                                output.chat_usage(
                                    "the session has no active turn to steer",
                                    COMMANDS,
                                )?;
                                continue;
                            };
                            if let Err(error) =
                                steer(client, output, session_id, None, Some(turn_id), content).await
                            {
                                output.error(&error)?;
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
                            output.chat_exiting(turns.status())?;
                            return Ok(());
                        }
                    }
                    output.flush()?;
                }
                interrupt = tokio::signal::ctrl_c() => {
                    interrupt.map_err(ClientError::Io)?;
                    match interrupts.received(turns.status()) {
                        InterruptAction::OfferStop => output.chat_interrupt_offered(COMMANDS)?,
                        InterruptAction::ExitActive | InterruptAction::ExitQueued => {
                            output.chat_exiting(turns.status())?;
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
    let receipt = submit_input(
        client,
        command_id,
        session_id,
        InputContent::new(content),
        Some(defaults_version),
        None,
    )
    .await?;
    let SubmitInputReceipt::Turn { turn_id } = receipt else {
        return Err(ClientError::Protocol("chat input returned a steering receipt").mutation());
    };
    Ok(turn_id)
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TurnEventEffect {
    None,
    Activated(CanonicalUuid),
    Ready,
}

fn update_turns_from_event(turns: &mut ChatTurns, event: &SessionEvent) -> TurnEventEffect {
    match event {
        SessionEvent::InputAccepted { turn_id, .. } => {
            turns.accepted(*turn_id);
            TurnEventEffect::None
        }
        SessionEvent::TurnActivated { turn_id, .. } => {
            if turns.activated(*turn_id) {
                TurnEventEffect::Activated(*turn_id)
            } else {
                TurnEventEffect::None
            }
        }
        SessionEvent::TurnCompleted { turn_id, .. }
        | SessionEvent::TurnFailed { turn_id, .. }
        | SessionEvent::TurnRefused { turn_id, .. }
        | SessionEvent::TurnCancelled { turn_id, .. }
        | SessionEvent::TurnReconciliationRequired { turn_id, .. }
        | SessionEvent::TurnToolReconciliationRequired { turn_id, .. } => {
            if turns.terminalized(*turn_id) {
                TurnEventEffect::Ready
            } else {
                TurnEventEffect::None
            }
        }
        SessionEvent::SessionCreated {}
        | SessionEvent::ModelCallTransition { .. }
        | SessionEvent::ToolBatchTransition { .. } => TurnEventEffect::None,
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
    if let Some(content) = line.strip_prefix(":steer ") {
        validate_content(content, ":steer requires text")?;
        return Ok(ChatInput::Steer(content.to_owned()));
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
    async fn terminal_input_reads_async_channel_chunks() {
        const INPUT: &str = "first line\nsecond line\n";
        const FIRST_LINE: &str = "first line";
        const SECOND_LINE: &str = "second line";
        let (sender, receiver) = mpsc::channel(TERMINAL_INPUT_CHANNEL_CAPACITY);
        sender
            .send(Ok(Vec::from(INPUT.as_bytes())))
            .await
            .expect("fixture receiver remains open");
        drop(sender);
        let input = TerminalInput {
            receiver,
            chunk: Vec::new(),
            consumed: 0,
        };
        let mut lines = BoundedLines::new(input);

        assert_eq!(
            lines.next_line().await.expect("fixture line read"),
            LineRead::Line(String::from(FIRST_LINE))
        );
        assert_eq!(
            lines.next_line().await.expect("fixture line read"),
            LineRead::Line(String::from(SECOND_LINE))
        );
        assert_eq!(
            lines.next_line().await.expect("fixture end read"),
            LineRead::Eof
        );
    }

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
            parse_line(String::from(":steer inspect the cache")),
            Ok(ChatInput::Steer(String::from("inspect the cache")))
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
    fn turn_controls_activate_only_on_turn_activated() {
        const TURN_IDENTITY: u128 = 11;
        const ACCEPTED_INPUT_IDENTITY: u128 = 12;
        const ATTEMPT_IDENTITY: u128 = 13;
        const FIRST_ACCEPTANCE_POSITION: u64 = 1;
        const QUEUED_OWNER_INPUT: &str = "queued owner input";
        let turn_id = CanonicalUuid::from_uuid(Uuid::from_u128(TURN_IDENTITY));
        let mut turns = ChatTurns::default();

        assert_eq!(
            update_turns_from_event(
                &mut turns,
                &SessionEvent::InputAccepted {
                    accepted_input_id: CanonicalUuid::from_uuid(Uuid::from_u128(
                        ACCEPTED_INPUT_IDENTITY,
                    )),
                    turn_id,
                    acceptance_position: CanonicalU64::new(FIRST_ACCEPTANCE_POSITION),
                    content: InputContent::new(String::from(QUEUED_OWNER_INPUT)),
                }
            ),
            TurnEventEffect::None
        );
        assert_eq!(turns.status(), Some(ChatTurnStatus::Queued(turn_id)));
        assert_eq!(turns.active_turn(), None);
        assert_eq!(
            update_turns_from_event(
                &mut turns,
                &SessionEvent::TurnActivated {
                    turn_id,
                    current_attempt_id: CanonicalUuid::from_uuid(Uuid::from_u128(ATTEMPT_IDENTITY)),
                }
            ),
            TurnEventEffect::Activated(turn_id)
        );

        assert_eq!(turns.status(), Some(ChatTurnStatus::Active(turn_id)));
    }
    #[test]
    fn first_interrupt_offers_stop_and_second_exits_without_stopping() {
        const ACTIVE_TURN_IDENTITY: u128 = 1;
        let active_turn = CanonicalUuid::from_uuid(Uuid::from_u128(ACTIVE_TURN_IDENTITY));
        let mut state = InterruptState::default();

        assert_eq!(
            state.received(Some(ChatTurnStatus::Active(active_turn))),
            InterruptAction::OfferStop
        );
        assert_eq!(
            state.received(Some(ChatTurnStatus::Active(active_turn))),
            InterruptAction::ExitActive
        );
    }

    #[test]
    fn queued_interrupt_exits_without_offering_stop() {
        const QUEUED_TURN_IDENTITY: u128 = 2;
        let queued_turn = CanonicalUuid::from_uuid(Uuid::from_u128(QUEUED_TURN_IDENTITY));
        let mut state = InterruptState::default();

        assert_eq!(
            state.received(Some(ChatTurnStatus::Queued(queued_turn))),
            InterruptAction::ExitQueued
        );
    }

    #[test]
    fn delayed_old_turn_events_do_not_replace_or_terminalize_a_local_successor() {
        const OLD_TURN_IDENTITY: u128 = 1;
        const SUCCESSOR_TURN_IDENTITY: u128 = 2;
        const OLD_ATTEMPT_IDENTITY: u128 = 3;
        const CANCELLATION_ENTRY_IDENTITY: u128 = 4;
        const TERMINAL_FRONTIER_IDENTITY: u128 = 5;
        let old_turn = CanonicalUuid::from_uuid(Uuid::from_u128(OLD_TURN_IDENTITY));
        let successor_turn = CanonicalUuid::from_uuid(Uuid::from_u128(SUCCESSOR_TURN_IDENTITY));
        let mut turns = ChatTurns::default();
        turns.queued(successor_turn);

        assert_eq!(
            update_turns_from_event(
                &mut turns,
                &SessionEvent::TurnActivated {
                    turn_id: old_turn,
                    current_attempt_id: CanonicalUuid::from_uuid(Uuid::from_u128(
                        OLD_ATTEMPT_IDENTITY
                    )),
                }
            ),
            TurnEventEffect::None
        );
        assert_eq!(turns.status(), Some(ChatTurnStatus::Queued(successor_turn)));
        assert_eq!(
            update_turns_from_event(
                &mut turns,
                &SessionEvent::TurnCancelled {
                    turn_id: old_turn,
                    cancellation_entry_id: CanonicalUuid::from_uuid(Uuid::from_u128(
                        CANCELLATION_ENTRY_IDENTITY,
                    )),
                    terminal_frontier_id: CanonicalUuid::from_uuid(Uuid::from_u128(
                        TERMINAL_FRONTIER_IDENTITY
                    )),
                }
            ),
            TurnEventEffect::None
        );
        assert_eq!(turns.status(), Some(ChatTurnStatus::Queued(successor_turn)));
    }
}
