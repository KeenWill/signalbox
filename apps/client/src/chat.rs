//! The interactive terminal chat loop: reads stdin on a background thread,
//! dispatches `:stop`, `:steer`, `:approve`, `:deny`, `:transcript`, `:model`,
//! and `:quit` commands, and renders the session's live turn and delegation
//! events as they arrive over `ProcessClient`.

use std::{
    fmt,
    future::Future,
    io::{self, BufRead as _},
    pin::Pin,
    task::{Context, Poll},
};

use signalbox_process_protocol::{
    CanonicalU64, CanonicalUuid, ClientRequest, CommandId, DescendantTerminationScope, ErrorCode,
    InputContent, ModelSelection, ServerMessage, SessionEvent, SystemPromptMember,
    SystemPromptText, ToolDecision, TurnState,
};
use tokio::{
    io::{AsyncBufRead, AsyncBufReadExt as _, AsyncRead, ReadBuf},
    signal::unix::{Signal, SignalKind, signal},
    sync::mpsc,
};
use uuid::Uuid;

use crate::{
    ClientDeploymentLimits, MAX_INPUT_CONTENT_FRAME_BYTES, ObservedSessionDefaults,
    SubmitInputReceipt, child_lifecycle_terminalization, command_identity,
    connection::ProcessClient,
    error::ClientError,
    presentation::{ChatTurnStatus, Output},
    read_session_defaults, read_session_summaries, read_snapshot, selection_display, stop_turn,
    submit_input, terminal_snapshot_selection, transcript,
    transcript::SnapshotIdentitySet,
};

// numeric-bound: guard - one unterminated terminal line exhausting input memory
const MAX_CHAT_LINE_BYTES: usize = MAX_INPUT_CONTENT_FRAME_BYTES + ":steer ".len();

const COMMANDS: &str = ":stop TEXT | :steer TEXT | :approve ID | :deny ID REASON | \
    :transcript | :model ALIAS-UUID | :quit";

pub(crate) struct TerminalInput {
    receiver: TerminalInputReceiver,
    chunk: Vec<u8>,
    consumed: usize,
}

enum TerminalInputSender {
    Bounded(mpsc::Sender<io::Result<Vec<u8>>>),
    Unbounded(mpsc::UnboundedSender<io::Result<Vec<u8>>>),
}

impl TerminalInputSender {
    fn send(&self, value: io::Result<Vec<u8>>) -> Result<(), ()> {
        match self {
            Self::Bounded(sender) => sender.blocking_send(value).map_err(|_| ()),
            Self::Unbounded(sender) => sender.send(value).map_err(|_| ()),
        }
    }
}

enum TerminalInputReceiver {
    Bounded(mpsc::Receiver<io::Result<Vec<u8>>>),
    Unbounded(mpsc::UnboundedReceiver<io::Result<Vec<u8>>>),
}

impl TerminalInputReceiver {
    fn poll_recv(&mut self, context: &mut Context<'_>) -> Poll<Option<io::Result<Vec<u8>>>> {
        match self {
            Self::Bounded(receiver) => receiver.poll_recv(context),
            Self::Unbounded(receiver) => receiver.poll_recv(context),
        }
    }
}

pub(crate) fn terminal_input(channel_capacity: Option<usize>) -> io::Result<TerminalInput> {
    let (sender, receiver) = match channel_capacity {
        Some(0) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "terminal input channel capacity must be positive or unbounded",
            ));
        }
        Some(capacity) => {
            if capacity > tokio::sync::Semaphore::MAX_PERMITS {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "terminal input channel capacity is not representable",
                ));
            }
            let (sender, receiver) = mpsc::channel(capacity);
            (
                TerminalInputSender::Bounded(sender),
                TerminalInputReceiver::Bounded(receiver),
            )
        }
        None => {
            let (sender, receiver) = mpsc::unbounded_channel();
            (
                TerminalInputSender::Unbounded(sender),
                TerminalInputReceiver::Unbounded(receiver),
            )
        }
    };
    std::thread::Builder::new()
        .name(String::from("signalbox-chat-stdin"))
        .spawn(move || read_terminal_input(sender))?;

    Ok(TerminalInput {
        receiver,
        chunk: Vec::new(),
        consumed: 0,
    })
}

fn read_terminal_input(sender: TerminalInputSender) {
    let stdin = io::stdin();
    let mut input = stdin.lock();
    loop {
        let available = match input.fill_buf() {
            Ok(available) => available,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => {
                let _ = sender.send(Err(error));
                return;
            }
        };
        if available.is_empty() {
            return;
        }
        let consumed = available.len();
        let chunk = available.to_vec();
        input.consume(consumed);
        if sender.send(Ok(chunk)).is_err() {
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
                        "interactive line exceeds the wire-frame content guard",
                    ));
                }
                if self.buffer.is_empty() {
                    return Ok(LineRead::Eof);
                }
                return Ok(self.take_line(false));
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
                        "interactive line exceeds the wire-frame content guard",
                    ));
                }
                return Ok(self.take_line(true));
            }
        }
    }

    fn take_line(&mut self, strip_carriage_return: bool) -> LineRead {
        if strip_carriage_return && self.buffer.last() == Some(&b'\r') {
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
    approval_request: Option<CanonicalUuid>,
}

impl ChatTurns {
    fn status(&self) -> Option<ChatTurnStatus> {
        if let (Some(turn_id), Some(tool_request_id)) = (self.active_turn, self.approval_request) {
            return Some(ChatTurnStatus::AwaitingApproval {
                turn_id,
                tool_request_id,
            });
        }
        if let Some(turn_id) = self.active_turn {
            return Some(ChatTurnStatus::Active(turn_id));
        }
        self.awaited_turn.map(ChatTurnStatus::Queued)
    }

    fn awaiting_reply(&self) -> bool {
        self.awaited_turn.is_some()
    }

    fn controllable_turn(&self) -> Option<CanonicalUuid> {
        if self.approval_request.is_some() {
            return None;
        }
        self.active_turn
    }

    fn approval_request(&self) -> Option<CanonicalUuid> {
        self.approval_request
    }

    fn queued(&mut self, turn_id: CanonicalUuid) {
        self.awaited_turn = Some(turn_id);
        self.active_turn = None;
        self.approval_request = None;
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
            self.approval_request = None;
            return true;
        }
        false
    }

    fn retired(&mut self, turn_id: CanonicalUuid) -> bool {
        if self.awaited_turn == Some(turn_id) && self.active_turn.is_none() {
            self.awaited_turn = None;
            self.approval_request = None;
            return true;
        }
        false
    }

    fn terminalized(&mut self, turn_id: CanonicalUuid) -> bool {
        if self.active_turn == Some(turn_id) {
            self.active_turn = None;
            self.approval_request = None;
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
                self.approval_request = match snapshot.turn_state(turn_id)? {
                    Some(TurnState::ActiveAwaitingToolApproval { tool_request_id }) => {
                        Some(tool_request_id)
                    }
                    _ => None,
                };
            }
            None => {
                self.active_turn = None;
                self.approval_request = None;
                self.awaited_turn = snapshot.first_queued_turn()?;
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum InterruptAction {
    OfferStop,
    OfferApproval(CanonicalUuid),
    ExitActive,
    ExitQueued,
    ExitIdle,
}

#[derive(Debug, Default)]
struct InterruptState {
    offered_for: Option<ChatTurnStatus>,
}

impl InterruptState {
    fn received(&mut self, status: Option<ChatTurnStatus>) -> InterruptAction {
        if self.offered_for != status {
            self.offered_for = None;
        }
        match (status, self.offered_for) {
            (Some(status @ ChatTurnStatus::Active(_)), None) => {
                self.offered_for = Some(status);
                InterruptAction::OfferStop
            }
            (
                Some(
                    status @ ChatTurnStatus::AwaitingApproval {
                        tool_request_id, ..
                    },
                ),
                None,
            ) => {
                self.offered_for = Some(status);
                InterruptAction::OfferApproval(tool_request_id)
            }
            (
                Some(ChatTurnStatus::Active(_) | ChatTurnStatus::AwaitingApproval { .. }),
                Some(_),
            ) => InterruptAction::ExitActive,
            (Some(ChatTurnStatus::Queued(_)), _) => InterruptAction::ExitQueued,
            (None, _) => InterruptAction::ExitIdle,
        }
    }

    fn reset(&mut self) {
        self.offered_for = None;
    }
}

/// One interrupt listener for the whole chat session, paired with the offer
/// state it drives. Registering a listener per `select!` iteration instead would
/// drop an interrupt that arrives between iterations, because a listener
/// delivers only the signals that arrive after its registration; a listener that
/// outlives the loop queues them.
struct ChatInterrupts {
    listener: Signal,
    state: InterruptState,
}

impl ChatInterrupts {
    fn listen() -> Result<Self, ClientError> {
        Ok(Self {
            listener: signal(SignalKind::interrupt()).map_err(ClientError::Io)?,
            state: InterruptState::default(),
        })
    }

    /// Resolves with the action the next interrupt calls for. Cancelling this
    /// future consumes no interrupt: the offer state advances in the same poll
    /// that receives one.
    async fn received(
        &mut self,
        status: Option<ChatTurnStatus>,
    ) -> Result<InterruptAction, ClientError> {
        self.listener.recv().await.ok_or_else(|| {
            ClientError::Io(io::Error::other(
                "the interrupt listener stopped delivering",
            ))
        })?;
        Ok(self.state.received(status))
    }

    fn reset(&mut self) {
        self.state.reset();
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RequestKind {
    ReadOnly,
    Mutation,
}

enum RequestWait<T> {
    Complete(Result<T, ClientError>),
    Exit,
}

async fn await_request<T, F>(
    output: &mut Output<'_>,
    interrupts: &mut ChatInterrupts,
    status: Option<ChatTurnStatus>,
    kind: RequestKind,
    future: F,
) -> Result<RequestWait<T>, ClientError>
where
    F: Future<Output = Result<T, ClientError>>,
{
    tokio::pin!(future);
    loop {
        tokio::select! {
            result = &mut future => return Ok(RequestWait::Complete(result)),
            interrupt = interrupts.received(status) => {
                match interrupt? {
                    InterruptAction::OfferStop => output.chat_interrupt_offered(COMMANDS)?,
                    InterruptAction::OfferApproval(tool_request_id) => {
                        output.chat_approval_interrupt_offered(tool_request_id, COMMANDS)?;
                    }
                    InterruptAction::ExitActive
                    | InterruptAction::ExitQueued
                    | InterruptAction::ExitIdle => {
                        if kind == RequestKind::Mutation {
                            output.chat_mutation_abandoned()?;
                        }
                        output.chat_exiting(status)?;
                        return Ok(RequestWait::Exit);
                    }
                }
            }
        }
    }
}

fn report_request_error(output: &mut Output<'_>, error: ClientError) -> Result<(), ClientError> {
    if error.is_ambiguous_mutation() {
        return Err(error);
    }
    output.error(&error)?;
    Ok(())
}

pub(crate) async fn run<R>(
    client: &mut ProcessClient,
    output: &mut Output<'_>,
    session_id: CanonicalUuid,
    input: R,
    deployment_limits: ClientDeploymentLimits,
) -> Result<(), ClientError>
where
    R: AsyncBufRead + Unpin,
{
    let mut lines = BoundedLines::new(input);
    let mut displayed_entries = SnapshotIdentitySet::new()?;
    let mut interrupts = ChatInterrupts::listen()?;
    let mut turns = ChatTurns::default();

    'resubscribe: loop {
        let mut connection = match await_request(
            output,
            &mut interrupts,
            turns.status(),
            RequestKind::ReadOnly,
            client.request(ClientRequest::FollowSession { session_id }),
        )
        .await?
        {
            RequestWait::Complete(result) => result?,
            RequestWait::Exit => return Ok(()),
        };
        let mut snapshot = match await_request(
            output,
            &mut interrupts,
            turns.status(),
            RequestKind::ReadOnly,
            read_snapshot(&mut connection, session_id),
        )
        .await?
        {
            RequestWait::Complete(result) => result?,
            RequestWait::Exit => return Ok(()),
        };
        turns.resynchronize(&mut snapshot)?;
        interrupts.reset();
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
                            let turn_effect =
                                update_turns_from_event(&mut turns, &event, session_id);
                            if let Some(selection) = terminal_snapshot_selection(&event, session_id)
                            {
                                let mut refreshed = match await_request(
                                    output,
                                    &mut interrupts,
                                    turns.status(),
                                    RequestKind::ReadOnly,
                                    transcript(client, session_id),
                                )
                                .await?
                                {
                                    RequestWait::Complete(result) => result?,
                                    RequestWait::Exit => return Ok(()),
                                };
                                output.terminal_material(
                                    &mut refreshed,
                                    &mut displayed_entries,
                                    selection,
                                )?;
                                turns.resynchronize(&mut refreshed)?;
                                render_approval_wait(&turns, output)?;
                            }
                            match turn_effect {
                                TurnEventEffect::Activated(turn_id) => output.chat_activated(turn_id)?,
                                TurnEventEffect::ApprovalDecided => {
                                    if !refresh_approval_after_decision(
                                        client,
                                        output,
                                        &mut interrupts,
                                        &mut turns,
                                        session_id,
                                    )
                                    .await?
                                    {
                                        return Ok(());
                                    }
                                }
                                TurnEventEffect::Ready => {
                                    interrupts.reset();
                                    render_chat_status(&turns, output, session_id)?;
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
                    let action = match parse_line_with_limit(
                        line,
                        deployment_limits.max_message_utf8_bytes,
                    ) {
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
                            let (command_id, _) = command_identity(None)?;
                            output.recovery_value(
                                "command_id",
                                &command_id.into_uuid().hyphenated().to_string(),
                            )?;
                            let defaults_version = match await_request(
                                output,
                                &mut interrupts,
                                turns.status(),
                                RequestKind::ReadOnly,
                                observe_defaults_version(client, session_id),
                            )
                            .await?
                            {
                                RequestWait::Complete(Ok(version)) => version,
                                RequestWait::Complete(Err(error)) => {
                                    report_request_error(output, error)?;
                                    continue;
                                }
                                RequestWait::Exit => return Ok(()),
                            };
                            output.recovery_value(
                                "defaults_version",
                                &defaults_version.value().to_string(),
                            )?;
                            match await_request(
                                output,
                                &mut interrupts,
                                turns.status(),
                                RequestKind::Mutation,
                                submit(
                                    client,
                                    command_id,
                                    session_id,
                                    content,
                                    defaults_version,
                                ),
                            )
                            .await?
                            {
                                RequestWait::Complete(Ok(turn_id)) => {
                                    turns.queued(turn_id);
                                    interrupts.reset();
                                    output.chat_queued(turn_id)?;
                                }
                                RequestWait::Complete(Err(error)) => {
                                    report_request_error(output, error)?;
                                }
                                RequestWait::Exit => return Ok(()),
                            }
                        }
                        ChatInput::Stop(content) => {
                            if let Some(tool_request_id) = turns.approval_request() {
                                output.chat_usage(
                                    &format!(
                                        "the active turn awaits approval request {tool_request_id}; decide it before stopping"
                                    ),
                                    COMMANDS,
                                )?;
                                continue;
                            }
                            let Some(turn_id) = turns.controllable_turn() else {
                                output.chat_usage("the session has no active turn to stop", COMMANDS)?;
                                continue;
                            };
                            let (command_id, _) = command_identity(None)?;
                            output.recovery_value(
                                "command_id",
                                &command_id.into_uuid().hyphenated().to_string(),
                            )?;
                            output.recovery_value("turn", &turn_id.to_string())?;
                            let defaults_version = match await_request(
                                output,
                                &mut interrupts,
                                turns.status(),
                                RequestKind::ReadOnly,
                                observe_defaults_version(client, session_id),
                            )
                            .await?
                            {
                                RequestWait::Complete(Ok(version)) => version,
                                RequestWait::Complete(Err(error)) => {
                                    report_request_error(output, error)?;
                                    continue;
                                }
                                RequestWait::Exit => return Ok(()),
                            };
                            output.recovery_value(
                                "defaults_version",
                                &defaults_version.value().to_string(),
                            )?;
                            match await_request(
                                output,
                                &mut interrupts,
                                turns.status(),
                                RequestKind::Mutation,
                                stop(
                                    client,
                                    command_id,
                                    session_id,
                                    turn_id,
                                    content,
                                    defaults_version,
                                ),
                            )
                            .await?
                            {
                                RequestWait::Complete(Ok(successor_turn_id)) => {
                                    turns.queued(successor_turn_id);
                                    interrupts.reset();
                                    output.chat_stopped(turn_id, successor_turn_id)?;
                                }
                                RequestWait::Complete(Err(error)) => {
                                    report_request_error(output, error)?;
                                }
                                RequestWait::Exit => return Ok(()),
                            }
                        }
                        ChatInput::Steer(content) => {
                            if let Some(tool_request_id) = turns.approval_request() {
                                output.chat_usage(
                                    &format!(
                                        "the active turn awaits approval request {tool_request_id}; decide it before steering"
                                    ),
                                    COMMANDS,
                                )?;
                                continue;
                            }
                            let Some(turn_id) = turns.controllable_turn() else {
                                output.chat_usage(
                                    "the session has no active turn to steer",
                                    COMMANDS,
                                )?;
                                continue;
                            };
                            let (command_id, _) = command_identity(None)?;
                            output.recovery_value(
                                "command_id",
                                &command_id.into_uuid().hyphenated().to_string(),
                            )?;
                            output.recovery_value("turn", &turn_id.to_string())?;
                            match await_request(
                                output,
                                &mut interrupts,
                                turns.status(),
                                RequestKind::Mutation,
                                steer(client, command_id, session_id, turn_id, content),
                            )
                            .await?
                            {
                                RequestWait::Complete(Ok((
                                    accepted_input_id,
                                    acceptance_position,
                                    source_turn_id,
                                ))) => output.steering_submitted(
                                    accepted_input_id,
                                    acceptance_position,
                                    source_turn_id,
                                )?,
                                RequestWait::Complete(Err(error)) => {
                                    report_request_error(output, error)?;
                                }
                                RequestWait::Exit => return Ok(()),
                            }
                        }
                        ChatInput::Approve(tool_request_id) => {
                            let decision = ToolDecision::Approve {};
                            let (command_id, _) = command_identity(None)?;
                            output.recovery_value(
                                "command_id",
                                &command_id.into_uuid().hyphenated().to_string(),
                            )?;
                            match await_request(
                                output,
                                &mut interrupts,
                                turns.status(),
                                RequestKind::Mutation,
                                decide(
                                    client,
                                    command_id,
                                    session_id,
                                    tool_request_id,
                                    decision.clone(),
                                ),
                            )
                            .await?
                            {
                                RequestWait::Complete(Ok(())) => {
                                    output.tool_request_decided(tool_request_id, &decision)?;
                                }
                                RequestWait::Complete(Err(error)) => {
                                    report_request_error(output, error)?;
                                }
                                RequestWait::Exit => return Ok(()),
                            }
                        }
                        ChatInput::Deny {
                            tool_request_id,
                            reason,
                        } => {
                            let decision = ToolDecision::Deny { reason };
                            let (command_id, _) = command_identity(None)?;
                            output.recovery_value(
                                "command_id",
                                &command_id.into_uuid().hyphenated().to_string(),
                            )?;
                            match await_request(
                                output,
                                &mut interrupts,
                                turns.status(),
                                RequestKind::Mutation,
                                decide(
                                    client,
                                    command_id,
                                    session_id,
                                    tool_request_id,
                                    decision.clone(),
                                ),
                            )
                            .await?
                            {
                                RequestWait::Complete(Ok(())) => {
                                    output.tool_request_decided(tool_request_id, &decision)?;
                                }
                                RequestWait::Complete(Err(error)) => {
                                    report_request_error(output, error)?;
                                }
                                RequestWait::Exit => return Ok(()),
                            }
                        }
                        ChatInput::Transcript => {
                            match await_request(
                                output,
                                &mut interrupts,
                                turns.status(),
                                RequestKind::ReadOnly,
                                transcript(client, session_id),
                            )
                            .await?
                            {
                                RequestWait::Complete(Ok(mut snapshot)) => {
                                    output.snapshot(&mut snapshot)?;
                                }
                                RequestWait::Complete(Err(error)) => {
                                    report_request_error(output, error)?;
                                }
                                RequestWait::Exit => return Ok(()),
                            }
                        }
                        ChatInput::Model(alias_id) => {
                            let (command_id, _) = command_identity(None)?;
                            output.recovery_value(
                                "command_id",
                                &command_id.into_uuid().hyphenated().to_string(),
                            )?;
                            let observed = match await_request(
                                output,
                                &mut interrupts,
                                turns.status(),
                                RequestKind::ReadOnly,
                                read_session_defaults(client, session_id, None),
                            )
                            .await?
                            {
                                RequestWait::Complete(Ok(observed)) => observed,
                                RequestWait::Complete(Err(error)) => {
                                    report_request_error(output, error)?;
                                    continue;
                                }
                                RequestWait::Exit => return Ok(()),
                            };
                            output.recovery_value(
                                "defaults_version",
                                &observed.version.value().to_string(),
                            )?;
                            output.recovery_value(
                                "dangerous_tool_auto_approval",
                                if observed.dangerous_tool_auto_approval {
                                    "approve-all"
                                } else {
                                    "disabled"
                                },
                            )?;
                            let selection = ModelSelection::Alias { alias_id };
                            match await_request(
                                output,
                                &mut interrupts,
                                turns.status(),
                                RequestKind::Mutation,
                                replace_model(
                                    client,
                                    command_id,
                                    session_id,
                                    selection,
                                    observed,
                                ),
                            )
                            .await?
                            {
                                RequestWait::Complete(Ok(installed_version)) => {
                                    output.session_defaults_replaced(
                                        session_id,
                                        installed_version,
                                        &selection_display(selection),
                                    )?;
                                }
                                RequestWait::Complete(Err(error)) => {
                                    report_request_error(output, error)?;
                                }
                                RequestWait::Exit => return Ok(()),
                            }
                        }
                        ChatInput::Quit => {
                            output.chat_exiting(turns.status())?;
                            return Ok(());
                        }
                    }
                    output.flush()?;
                }
                interrupt = interrupts.received(turns.status()) => {
                    match interrupt? {
                        InterruptAction::OfferStop => output.chat_interrupt_offered(COMMANDS)?,
                        InterruptAction::OfferApproval(tool_request_id) => {
                            output.chat_approval_interrupt_offered(tool_request_id, COMMANDS)?;
                        }
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

async fn observe_defaults_version(
    client: &mut ProcessClient,
    session_id: CanonicalUuid,
) -> Result<CanonicalU64, ClientError> {
    let mut selected = None;
    read_session_summaries(client, |summary, _| {
        if summary.session_id == session_id {
            selected = Some(CanonicalU64::new(summary.defaults_version));
        }
        Ok(())
    })
    .await?;
    selected.ok_or(ClientError::Input("the selected session was not listed"))
}

async fn submit(
    client: &mut ProcessClient,
    command_id: CommandId,
    session_id: CanonicalUuid,
    content: String,
    defaults_version: CanonicalU64,
) -> Result<CanonicalUuid, ClientError> {
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
    command_id: CommandId,
    session_id: CanonicalUuid,
    active_turn: CanonicalUuid,
    content: String,
    defaults_version: CanonicalU64,
) -> Result<CanonicalUuid, ClientError> {
    stop_turn(
        client,
        command_id,
        session_id,
        active_turn,
        InputContent::new(content),
        defaults_version,
        DescendantTerminationScope::ParentAlone,
    )
    .await
}

async fn steer(
    client: &mut ProcessClient,
    command_id: CommandId,
    session_id: CanonicalUuid,
    active_turn: CanonicalUuid,
    content: String,
) -> Result<(CanonicalUuid, u64, CanonicalUuid), ClientError> {
    let receipt = submit_input(
        client,
        command_id,
        session_id,
        InputContent::new(content),
        None,
        Some(signalbox_process_protocol::InputDelivery::Steer {
            expected_active_turn_id: active_turn,
        }),
    )
    .await?;
    let SubmitInputReceipt::Steering {
        accepted_input_id,
        acceptance_position,
        source_turn_id,
    } = receipt
    else {
        return Err(ClientError::Protocol("steer returned a turn-origin receipt").mutation());
    };
    if source_turn_id != active_turn {
        return Err(ClientError::Protocol("steer returned another source turn").mutation());
    }
    Ok((accepted_input_id, acceptance_position, source_turn_id))
}

async fn decide(
    client: &mut ProcessClient,
    command_id: CommandId,
    session_id: CanonicalUuid,
    tool_request_id: CanonicalUuid,
    decision: ToolDecision,
) -> Result<(), ClientError> {
    let mut connection = client
        .mutation_request(ClientRequest::DecideToolRequest {
            command_id,
            session_id,
            tool_request_id,
            decision: decision.clone(),
        })
        .await?;
    match connection.message().await.map_err(ClientError::mutation)? {
        ServerMessage::ToolRequestDecided {
            tool_request_id: decided_request,
            decision: recorded_decision,
        } if decided_request == tool_request_id && recorded_decision == decision => Ok(()),
        ServerMessage::Error {
            code,
            message,
            detail,
        } => Err(ClientError::remote(code, message, detail).mutation()),
        _ => Err(ClientError::Protocol("decision returned an unexpected receipt").mutation()),
    }
}

async fn replace_model(
    client: &mut ProcessClient,
    command_id: CommandId,
    session_id: CanonicalUuid,
    selection: ModelSelection,
    observed: ObservedSessionDefaults,
) -> Result<u64, ClientError> {
    let replacement_system_prompt = observed.system_prompt.clone();
    let mut connection = client
        .mutation_request(model_replacement_request(
            command_id,
            session_id,
            selection,
            &observed,
            replacement_system_prompt.clone(),
        ))
        .await?;
    match connection.message().await.map_err(ClientError::mutation)? {
        ServerMessage::SessionDefaultsReplaced {
            session_id: replaced_session,
            defaults_version: installed_version,
            model_selection,
            model_settings,
            dangerous_tool_auto_approval,
            system_prompt: receipt_system_prompt,
            ..
        } if replaced_session == session_id
            && model_selection == selection
            && model_settings.precedence.session == observed.model_settings
            && dangerous_tool_auto_approval == observed.dangerous_tool_auto_approval
            && receipt_system_prompt.value() == Some(&replacement_system_prompt)
            && observed
                .version
                .value()
                .checked_add(1)
                .is_some_and(|expected| installed_version.value() == expected) =>
        {
            Ok(installed_version.value())
        }
        ServerMessage::Error {
            code,
            message,
            detail,
        } => Err(ClientError::remote(code, message, detail).mutation()),
        _ => Err(
            ClientError::Protocol("model replacement returned an unexpected response").mutation(),
        ),
    }
}

fn model_replacement_request(
    command_id: CommandId,
    session_id: CanonicalUuid,
    selection: ModelSelection,
    observed: &ObservedSessionDefaults,
    replacement_system_prompt: Option<SystemPromptText>,
) -> ClientRequest {
    ClientRequest::ReplaceSessionDefaults {
        command_id,
        session_id,
        expected_defaults_version: observed.version,
        model_selection: selection,
        model_settings: observed.model_settings,
        dangerous_tool_auto_approval: observed.dangerous_tool_auto_approval,
        system_prompt: SystemPromptMember::present(replacement_system_prompt),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TurnEventEffect {
    None,
    Activated(CanonicalUuid),
    ApprovalDecided,
    Ready,
}

fn update_turns_from_event(
    turns: &mut ChatTurns,
    event: &SessionEvent,
    session_id: CanonicalUuid,
) -> TurnEventEffect {
    if child_lifecycle_terminalization(event, session_id) {
        // The cascade terminalized this session's own delegated turn without
        // naming it. The caller's refresh resynchronizes the tracked turn from
        // the authoritative snapshot, so this reports only that the chat must
        // reset its interrupt offer and render the resulting status.
        return TurnEventEffect::Ready;
    }
    match event {
        SessionEvent::InputAccepted { turn_id, .. } => {
            turns.accepted(*turn_id);
            TurnEventEffect::None
        }
        SessionEvent::GoalTurnRetired { turn_id } => {
            if turns.retired(*turn_id) {
                TurnEventEffect::Ready
            } else {
                TurnEventEffect::None
            }
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
        SessionEvent::ToolApprovalDecided { .. } => TurnEventEffect::ApprovalDecided,
        SessionEvent::SessionCreated {}
        | SessionEvent::SessionModelSettingsChanged { .. }
        | SessionEvent::TurnModelSettingsResolved { .. }
        | SessionEvent::ModelCallTransition { .. }
        | SessionEvent::ToolBatchTransition { .. }
        | SessionEvent::RunnerStateTransition { .. }
        | SessionEvent::ContextCompacted { .. }
        | SessionEvent::ChildSpawned { .. }
        | SessionEvent::ChildWaiting { .. }
        | SessionEvent::SessionMessage { .. }
        | SessionEvent::ChildResult { .. }
        | SessionEvent::ChildLifecycleDisposition { .. } => TurnEventEffect::None,
    }
}

fn render_approval_wait(turns: &ChatTurns, output: &mut Output<'_>) -> Result<(), ClientError> {
    if let Some(ChatTurnStatus::AwaitingApproval {
        turn_id,
        tool_request_id,
    }) = turns.status()
    {
        output.chat_awaiting_approval(turn_id, tool_request_id)?;
    }
    Ok(())
}

fn render_chat_status(
    turns: &ChatTurns,
    output: &mut Output<'_>,
    session_id: CanonicalUuid,
) -> Result<(), ClientError> {
    match turns.status() {
        Some(ChatTurnStatus::Queued(turn_id)) => output.chat_queued(turn_id)?,
        Some(ChatTurnStatus::Active(turn_id)) => output.chat_activated(turn_id)?,
        Some(ChatTurnStatus::AwaitingApproval {
            turn_id,
            tool_request_id,
        }) => output.chat_awaiting_approval(turn_id, tool_request_id)?,
        None => output.chat_ready(session_id)?,
    }
    Ok(())
}

async fn refresh_approval_after_decision(
    client: &mut ProcessClient,
    output: &mut Output<'_>,
    interrupts: &mut ChatInterrupts,
    turns: &mut ChatTurns,
    session_id: CanonicalUuid,
) -> Result<bool, ClientError> {
    let mut snapshot = match await_request(
        output,
        interrupts,
        turns.status(),
        RequestKind::ReadOnly,
        transcript(client, session_id),
    )
    .await?
    {
        RequestWait::Complete(Ok(snapshot)) => snapshot,
        RequestWait::Complete(Err(error)) => return Err(error),
        RequestWait::Exit => return Ok(false),
    };
    turns.resynchronize(&mut snapshot)?;
    interrupts.reset();
    render_chat_status(turns, output, session_id)?;
    Ok(true)
}

#[cfg(test)]
fn parse_line(line: String) -> Result<ChatInput, ChatSyntaxError> {
    parse_line_with_limit(line, None)
}

fn parse_line_with_limit(
    line: String,
    max_message_utf8_bytes: Option<usize>,
) -> Result<ChatInput, ChatSyntaxError> {
    if !line.starts_with(':') {
        validate_content(&line, "input must not be empty", max_message_utf8_bytes)?;
        return Ok(ChatInput::Submit(line));
    }
    if line == ":transcript" {
        return Ok(ChatInput::Transcript);
    }
    if line == ":quit" {
        return Ok(ChatInput::Quit);
    }
    if let Some(content) = line.strip_prefix(":stop ") {
        validate_content(
            content,
            ":stop requires successor text",
            max_message_utf8_bytes,
        )?;
        return Ok(ChatInput::Stop(content.to_owned()));
    }
    if let Some(content) = line.strip_prefix(":steer ") {
        validate_content(content, ":steer requires text", max_message_utf8_bytes)?;
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
        if reason.is_empty() || has_surrounding_posix_whitespace(reason) {
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

fn has_surrounding_posix_whitespace(value: &str) -> bool {
    let is_posix_whitespace = |byte| matches!(byte, b' ' | b'\t'..=b'\r');
    value
        .as_bytes()
        .first()
        .is_some_and(|byte| is_posix_whitespace(*byte))
        || value
            .as_bytes()
            .last()
            .is_some_and(|byte| is_posix_whitespace(*byte))
}

fn validate_content(
    content: &str,
    empty_message: &'static str,
    max_message_utf8_bytes: Option<usize>,
) -> Result<(), ChatSyntaxError> {
    if content.is_empty() {
        return Err(ChatSyntaxError(empty_message));
    }
    if content.len() > MAX_INPUT_CONTENT_FRAME_BYTES {
        return Err(ChatSyntaxError("input exceeds the wire-frame UTF-8 guard"));
    }
    if max_message_utf8_bytes.is_some_and(|maximum| content.len() > maximum) {
        return Err(ChatSyntaxError(
            "input exceeds the deployment UTF-8 byte limit",
        ));
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
    use signalbox_process_protocol::{
        DelegationOutcome, DelegationProvenance, DelegationReason, FastModeOverlay,
        ModelSettingsOverlay, ReasoningLevel, SettingOverlay,
    };

    const REQUEST: &str = "00000000-0000-0000-0000-000000000123";
    /// The session the chat follows. Only a delegation event addressed to this
    /// exact session projects onto the chat's own turns.
    const FOLLOWED_SESSION_IDENTITY: u128 = 0x5e5;

    fn followed_session() -> CanonicalUuid {
        CanonicalUuid::from_uuid(Uuid::from_u128(FOLLOWED_SESSION_IDENTITY))
    }

    #[test]
    fn chat_model_replacement_preserves_the_observed_session_settings() {
        let command_id = CommandId::try_from_uuid(Uuid::from_u128(1))
            .expect("fixture command identity is admitted");
        let session_id = CanonicalUuid::from_uuid(Uuid::from_u128(2));
        let selection_id = CanonicalUuid::from_uuid(Uuid::from_u128(3));
        let model_settings = ModelSettingsOverlay {
            reasoning_level: SettingOverlay::Value(ReasoningLevel::High),
            fast_mode: FastModeOverlay::Inherit,
            service_tier: SettingOverlay::Inherit,
        };
        let observed = ObservedSessionDefaults {
            version: CanonicalU64::new(4),
            model_settings,
            dangerous_tool_auto_approval: false,
            system_prompt: None,
        };

        let request = model_replacement_request(
            command_id,
            session_id,
            ModelSelection::Direct { selection_id },
            &observed,
            None,
        );

        assert_eq!(
            request,
            ClientRequest::ReplaceSessionDefaults {
                command_id,
                session_id,
                expected_defaults_version: observed.version,
                model_selection: ModelSelection::Direct { selection_id },
                model_settings,
                dangerous_tool_auto_approval: false,
                system_prompt: SystemPromptMember::present(None),
            }
        );
    }

    #[tokio::test]
    async fn terminal_input_reads_async_channel_chunks() {
        const INPUT: &str = "first line\nsecond line\n";
        const FIRST_LINE: &str = "first line";
        const SECOND_LINE: &str = "second line";
        let (sender, receiver) = mpsc::channel(1);
        sender
            .send(Ok(Vec::from(INPUT.as_bytes())))
            .await
            .expect("fixture receiver remains open");
        drop(sender);
        let input = TerminalInput {
            receiver: TerminalInputReceiver::Bounded(receiver),
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
    async fn bounded_lines_strips_carriage_return_only_for_crlf() {
        const BARE_CARRIAGE_RETURN: &[u8] = b"exact\r";
        const CRLF_LINE: &[u8] = b"terminated\r\n";
        let mut bare = BoundedLines::new(tokio::io::BufReader::new(BARE_CARRIAGE_RETURN));
        let mut terminated = BoundedLines::new(tokio::io::BufReader::new(CRLF_LINE));

        assert_eq!(
            bare.next_line().await.expect("fixture line read"),
            LineRead::Line(String::from("exact\r"))
        );
        assert_eq!(
            terminated.next_line().await.expect("fixture line read"),
            LineRead::Line(String::from("terminated"))
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
            LineRead::Rejected("interactive line exceeds the wire-frame content guard")
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
            parse_line(String::from("  exact user text  ")),
            Ok(ChatInput::Submit(String::from("  exact user text  ")))
        );
    }

    #[test]
    fn chat_parser_uses_the_learned_message_limit() {
        assert_eq!(
            parse_line_with_limit(String::from("four"), Some(3)),
            Err(ChatSyntaxError(
                "input exceeds the deployment UTF-8 byte limit"
            ))
        );
        assert_eq!(
            parse_line_with_limit(String::from("four"), None),
            Ok(ChatInput::Submit(String::from("four")))
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
    fn chat_parser_preserves_nonbreaking_space_at_denial_edges() {
        const REASON: &str = "\u{00a0}denied\u{00a0}";
        let request = CanonicalUuid::from_uuid(Uuid::parse_str(REQUEST).expect("fixture UUID"));

        assert_eq!(
            parse_line(format!(":deny {REQUEST} {REASON}")),
            Ok(ChatInput::Deny {
                tool_request_id: request,
                reason: String::from(REASON),
            })
        );
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
        const QUEUED_USER_INPUT: &str = "queued user input";
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
                    content: InputContent::new(String::from(QUEUED_USER_INPUT)),
                },
                followed_session(),
            ),
            TurnEventEffect::None
        );
        assert_eq!(turns.status(), Some(ChatTurnStatus::Queued(turn_id)));
        assert_eq!(turns.controllable_turn(), None);
        assert_eq!(
            update_turns_from_event(
                &mut turns,
                &SessionEvent::TurnActivated {
                    turn_id,
                    current_attempt_id: CanonicalUuid::from_uuid(Uuid::from_u128(ATTEMPT_IDENTITY)),
                },
                followed_session(),
            ),
            TurnEventEffect::Activated(turn_id)
        );

        assert_eq!(turns.status(), Some(ChatTurnStatus::Active(turn_id)));
    }

    #[test]
    fn retired_queued_goal_turn_admits_the_replacement_activation() {
        const RETIRED_TURN_IDENTITY: u128 = 61;
        const RETIRED_INPUT_IDENTITY: u128 = 62;
        const REPLACEMENT_TURN_IDENTITY: u128 = 63;
        const REPLACEMENT_INPUT_IDENTITY: u128 = 64;
        const REPLACEMENT_ATTEMPT_IDENTITY: u128 = 65;
        let retired_turn = CanonicalUuid::from_uuid(Uuid::from_u128(RETIRED_TURN_IDENTITY));
        let replacement_turn = CanonicalUuid::from_uuid(Uuid::from_u128(REPLACEMENT_TURN_IDENTITY));
        let mut turns = ChatTurns::default();

        assert_eq!(
            update_turns_from_event(
                &mut turns,
                &SessionEvent::InputAccepted {
                    accepted_input_id: CanonicalUuid::from_uuid(Uuid::from_u128(
                        RETIRED_INPUT_IDENTITY,
                    )),
                    turn_id: retired_turn,
                    acceptance_position: CanonicalU64::new(1),
                    content: InputContent::new(String::from("obsolete goal input")),
                },
                followed_session(),
            ),
            TurnEventEffect::None
        );
        assert_eq!(
            update_turns_from_event(
                &mut turns,
                &SessionEvent::GoalTurnRetired {
                    turn_id: retired_turn,
                },
                followed_session(),
            ),
            TurnEventEffect::Ready
        );
        assert_eq!(
            update_turns_from_event(
                &mut turns,
                &SessionEvent::InputAccepted {
                    accepted_input_id: CanonicalUuid::from_uuid(Uuid::from_u128(
                        REPLACEMENT_INPUT_IDENTITY,
                    )),
                    turn_id: replacement_turn,
                    acceptance_position: CanonicalU64::new(2),
                    content: InputContent::new(String::from("replacement goal input")),
                },
                followed_session(),
            ),
            TurnEventEffect::None
        );
        assert_eq!(
            update_turns_from_event(
                &mut turns,
                &SessionEvent::TurnActivated {
                    turn_id: replacement_turn,
                    current_attempt_id: CanonicalUuid::from_uuid(Uuid::from_u128(
                        REPLACEMENT_ATTEMPT_IDENTITY,
                    )),
                },
                followed_session(),
            ),
            TurnEventEffect::Activated(replacement_turn)
        );
        assert_eq!(
            turns.status(),
            Some(ChatTurnStatus::Active(replacement_turn))
        );
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
    fn interrupt_offer_is_rebound_after_active_turn_changes() {
        const FIRST_TURN_IDENTITY: u128 = 21;
        const SECOND_TURN_IDENTITY: u128 = 22;
        let first_turn = CanonicalUuid::from_uuid(Uuid::from_u128(FIRST_TURN_IDENTITY));
        let second_turn = CanonicalUuid::from_uuid(Uuid::from_u128(SECOND_TURN_IDENTITY));
        let mut state = InterruptState::default();

        assert_eq!(
            state.received(Some(ChatTurnStatus::Active(first_turn))),
            InterruptAction::OfferStop
        );
        assert_eq!(
            state.received(Some(ChatTurnStatus::Active(second_turn))),
            InterruptAction::OfferStop
        );
    }

    #[test]
    fn approval_interrupt_offers_the_exact_decision_before_exit() {
        const TURN_IDENTITY: u128 = 31;
        const REQUEST_IDENTITY: u128 = 32;
        let turn_id = CanonicalUuid::from_uuid(Uuid::from_u128(TURN_IDENTITY));
        let tool_request_id = CanonicalUuid::from_uuid(Uuid::from_u128(REQUEST_IDENTITY));
        let status = Some(ChatTurnStatus::AwaitingApproval {
            turn_id,
            tool_request_id,
        });
        let mut state = InterruptState::default();

        assert_eq!(
            state.received(status),
            InterruptAction::OfferApproval(tool_request_id)
        );
        assert_eq!(state.received(status), InterruptAction::ExitActive);
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
    fn snapshot_resynchronization_awaits_the_first_queued_turn() {
        const FIRST_TURN_IDENTITY: u128 = 51;
        const FIRST_INPUT_IDENTITY: u128 = 52;
        const SECOND_TURN_IDENTITY: u128 = 53;
        const SECOND_INPUT_IDENTITY: u128 = 54;
        const FIRST_POSITION: u64 = 1;
        const SECOND_POSITION: u64 = 2;
        const FIRST_CONTENT: &str = "first queued input";
        const SECOND_CONTENT: &str = "second queued input";
        let first_turn = CanonicalUuid::from_uuid(Uuid::from_u128(FIRST_TURN_IDENTITY));
        let mut snapshot = crate::transcript::TranscriptSnapshot::from_messages(
            SECOND_POSITION,
            [
                ServerMessage::TranscriptTurn {
                    turn_id: first_turn,
                    acceptance_position: CanonicalU64::new(FIRST_POSITION),
                    model_settings: None,
                    state: TurnState::Queued {
                        accepted_input_id: CanonicalUuid::from_uuid(Uuid::from_u128(
                            FIRST_INPUT_IDENTITY,
                        )),
                        content: InputContent::new(String::from(FIRST_CONTENT)),
                    },
                },
                ServerMessage::TranscriptTurn {
                    turn_id: CanonicalUuid::from_uuid(Uuid::from_u128(SECOND_TURN_IDENTITY)),
                    acceptance_position: CanonicalU64::new(SECOND_POSITION),
                    model_settings: None,
                    state: TurnState::Queued {
                        accepted_input_id: CanonicalUuid::from_uuid(Uuid::from_u128(
                            SECOND_INPUT_IDENTITY,
                        )),
                        content: InputContent::new(String::from(SECOND_CONTENT)),
                    },
                },
            ],
        )
        .expect("fixture snapshot");
        let mut turns = ChatTurns::default();

        turns
            .resynchronize(&mut snapshot)
            .expect("queued snapshot resynchronizes");

        assert_eq!(turns.status(), Some(ChatTurnStatus::Queued(first_turn)));
        assert!(turns.awaiting_reply());
    }

    #[test]
    fn approval_phase_refresh_replaces_the_exact_request_identity() {
        const TURN_IDENTITY: u128 = 41;
        const FIRST_REQUEST_IDENTITY: u128 = 42;
        const SECOND_REQUEST_IDENTITY: u128 = 43;
        const ACCEPTANCE_POSITION: u64 = 1;
        let turn_id = CanonicalUuid::from_uuid(Uuid::from_u128(TURN_IDENTITY));
        let first_request = CanonicalUuid::from_uuid(Uuid::from_u128(FIRST_REQUEST_IDENTITY));
        let second_request = CanonicalUuid::from_uuid(Uuid::from_u128(SECOND_REQUEST_IDENTITY));
        let mut first_snapshot = crate::transcript::TranscriptSnapshot::from_messages(
            ACCEPTANCE_POSITION,
            [ServerMessage::TranscriptTurn {
                turn_id,
                acceptance_position: CanonicalU64::new(ACCEPTANCE_POSITION),
                model_settings: None,
                state: TurnState::ActiveAwaitingToolApproval {
                    tool_request_id: first_request,
                },
            }],
        )
        .expect("fixture snapshot");
        let mut second_snapshot = crate::transcript::TranscriptSnapshot::from_messages(
            ACCEPTANCE_POSITION,
            [ServerMessage::TranscriptTurn {
                turn_id,
                acceptance_position: CanonicalU64::new(ACCEPTANCE_POSITION),
                model_settings: None,
                state: TurnState::ActiveAwaitingToolApproval {
                    tool_request_id: second_request,
                },
            }],
        )
        .expect("fixture snapshot");
        let mut turns = ChatTurns::default();
        turns.activated(turn_id);

        turns
            .resynchronize(&mut first_snapshot)
            .expect("first approval phase");
        assert_eq!(
            turns.status(),
            Some(ChatTurnStatus::AwaitingApproval {
                turn_id,
                tool_request_id: first_request,
            })
        );
        assert_eq!(turns.controllable_turn(), None);
        turns
            .resynchronize(&mut second_snapshot)
            .expect("second approval phase");
        assert_eq!(
            turns.status(),
            Some(ChatTurnStatus::AwaitingApproval {
                turn_id,
                tool_request_id: second_request,
            })
        );
    }

    #[test]
    fn external_approval_decision_requests_authoritative_chat_refresh() {
        const TURN_IDENTITY: u128 = 44;
        const REQUEST_IDENTITY: u128 = 45;
        const COMMAND_IDENTITY: u128 = 46;
        let mut turns = ChatTurns::default();

        assert_eq!(
            update_turns_from_event(
                &mut turns,
                &SessionEvent::ToolApprovalDecided {
                    turn_id: CanonicalUuid::from_uuid(Uuid::from_u128(TURN_IDENTITY)),
                    tool_request_id: CanonicalUuid::from_uuid(Uuid::from_u128(REQUEST_IDENTITY)),
                    decision: signalbox_process_protocol::ToolApprovalEventDecision::Approve {},
                    decider: signalbox_process_protocol::ToolApprovalEventDecider::User {
                        command_id: CanonicalUuid::from_uuid(Uuid::from_u128(COMMAND_IDENTITY)),
                    },
                    rationale: None,
                },
                followed_session(),
            ),
            TurnEventEffect::ApprovalDecided
        );
    }

    #[test]
    fn terminal_approval_refresh_renders_authoritative_ready_state() {
        const SESSION_IDENTITY: u128 = 47;
        let session_id = CanonicalUuid::from_uuid(Uuid::from_u128(SESSION_IDENTITY));
        let turns = ChatTurns::default();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut output = Output::new(&mut stdout, &mut stderr, false);

        render_chat_status(&turns, &mut output, session_id)
            .expect("the authoritative ready state renders");

        assert_eq!(
            String::from_utf8(stdout).expect("ready output is UTF-8"),
            format!("chat session={session_id} state=ready\n")
        );
        assert!(stderr.is_empty());
    }

    #[test]
    fn ambiguous_mutation_error_is_not_rendered_as_a_retriable_loop_error() {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut output = Output::new(&mut stdout, &mut stderr, false);

        let error = report_request_error(&mut output, ClientError::AmbiguousMutation)
            .expect_err("ambiguous mutation exits the loop");

        assert!(error.is_ambiguous_mutation());
        assert!(stdout.is_empty());
        assert!(stderr.is_empty());
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
                },
                followed_session(),
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
                },
                followed_session(),
            ),
            TurnEventEffect::None
        );
        assert_eq!(turns.status(), Some(ChatTurnStatus::Queued(successor_turn)));
    }

    #[test]
    fn cascade_terminalizing_this_chat_renders_authoritative_ready_state() {
        const SPAWNING_REQUEST_IDENTITY: u128 = 71;
        const PARENT_SESSION_IDENTITY: u128 = 72;
        const COMMAND_IDENTITY: u128 = 73;
        const DELEGATED_TURN_IDENTITY: u128 = 74;
        let delegated_turn = CanonicalUuid::from_uuid(Uuid::from_u128(DELEGATED_TURN_IDENTITY));
        let mut turns = ChatTurns::default();
        turns.activated(delegated_turn);

        assert_eq!(
            update_turns_from_event(
                &mut turns,
                &SessionEvent::ChildLifecycleDisposition {
                    spawning_request_id: CanonicalUuid::from_uuid(Uuid::from_u128(
                        SPAWNING_REQUEST_IDENTITY,
                    )),
                    child_session_id: followed_session(),
                    outcome: DelegationOutcome::Stopped,
                    reason: DelegationReason::ParentStopped,
                    provenance: DelegationProvenance::ParentGoalCommand {
                        parent_session_id: CanonicalUuid::from_uuid(Uuid::from_u128(
                            PARENT_SESSION_IDENTITY,
                        )),
                        goal_generation: CanonicalU64::new(1),
                        command_id: CanonicalUuid::from_uuid(Uuid::from_u128(COMMAND_IDENTITY)),
                        descendant_scope: DescendantTerminationScope::ParentAndDescendants,
                    },
                },
                followed_session(),
            ),
            TurnEventEffect::Ready
        );
    }

    #[test]
    fn cascade_terminalizing_another_child_leaves_this_chat_turn_active() {
        const SPAWNING_REQUEST_IDENTITY: u128 = 81;
        const OTHER_CHILD_SESSION_IDENTITY: u128 = 82;
        const COMMAND_IDENTITY: u128 = 83;
        const ACTIVE_TURN_IDENTITY: u128 = 84;
        let active_turn = CanonicalUuid::from_uuid(Uuid::from_u128(ACTIVE_TURN_IDENTITY));
        let mut turns = ChatTurns::default();
        turns.activated(active_turn);

        assert_eq!(
            update_turns_from_event(
                &mut turns,
                &SessionEvent::ChildLifecycleDisposition {
                    spawning_request_id: CanonicalUuid::from_uuid(Uuid::from_u128(
                        SPAWNING_REQUEST_IDENTITY,
                    )),
                    child_session_id: CanonicalUuid::from_uuid(Uuid::from_u128(
                        OTHER_CHILD_SESSION_IDENTITY,
                    )),
                    outcome: DelegationOutcome::Stopped,
                    reason: DelegationReason::ParentStopped,
                    provenance: DelegationProvenance::ParentGoalCommand {
                        parent_session_id: followed_session(),
                        goal_generation: CanonicalU64::new(1),
                        command_id: CanonicalUuid::from_uuid(Uuid::from_u128(COMMAND_IDENTITY)),
                        descendant_scope: DescendantTerminationScope::ParentAndDescendants,
                    },
                },
                followed_session(),
            ),
            TurnEventEffect::None
        );
        assert_eq!(turns.status(), Some(ChatTurnStatus::Active(active_turn)));
    }
}
