//! Local process-protocol serving and durable outbox fan-out.

use std::{
    error::Error,
    fmt,
    future::Future,
    io::{self, SeekFrom},
    sync::Arc,
    time::Duration,
};

use signalbox_application::{
    CreateSessionError, CreateSessionOutcome, CreateSessionRequest, CreateSessionService,
    InProcessEligibilityNudge, SubmitInputOutcome, SubmitInputRequest, SubmitInputService,
    SubmitInputTransaction, UuidV7SessionIdGenerator, UuidV7SubmitInputIdGenerator,
};
use signalbox_domain::{
    AcceptedInputId, CancelledModelCallTurnIdentities, DeliveryRequest, DirectModelSelection,
    DurableCommandId, ModelAlias, ModelSelectionOverride, ModelSelectionRequest,
    PerInputConfigurationChoices, SessionConfigurationDefaults,
    SessionConfigurationDefaultsVersion, SessionId, SubmitInput, SubmitInputAppliedResult,
    SubmitInputRejectedResult, SubmitInputResult, TurnId, UserContent,
};
use signalbox_persistence::{
    create_session::{CreateSessionRepository, CreateSessionRepositoryError},
    outbox::{
        DispatchedModelCallDisposition, DispatchedModelCallState, DispatchedOutboxEvent,
        DispatchedOutboxEventKind, OutboxDeliveryDecision, OutboxDispatchError,
        OutboxDispatchOutcome, OutboxDispatcher,
    },
    process_read::{
        ProcessCurrentModelCallState, ProcessFailedModelCallDisposition, ProcessModelSelection,
        ProcessReadError, ProcessReadRepository, ProcessTranscriptEntry, ProcessTranscriptItem,
        ProcessTranscriptTurn, ProcessTurnState,
    },
    submit_input::{SubmitInputHandlingOutcome, SubmitInputRepository, SubmitInputRepositoryError},
};
use signalbox_process_protocol::{
    CanonicalU64, CanonicalUuid, ClientRequest, CurrentModelCall, CurrentModelCallState, ErrorCode,
    ErrorDetail, FailedModelCallDisposition, FailedTerminalModelCall, FrameDecodeErrorKind,
    FrameEncodeError, InputContent, MAX_FRAME_BYTES, ModelCallDisposition, ModelCallState,
    ModelSelection as WireModelSelection, RejectionDetail, RequestId, ServerFrame, ServerMessage,
    SessionEvent, TranscriptEntry, TranscriptTextEntry, TurnState, content_fragments,
    decode_client_line, encode_server_line, recover_bounded_client_request_id,
};
use sqlx::PgPool;
use tokio::{
    io::{AsyncBufRead, AsyncBufReadExt, AsyncSeekExt, AsyncWrite, AsyncWriteExt, BufReader},
    net::UnixStream,
    sync::{OwnedSemaphorePermit, Semaphore, broadcast, watch},
    task::{JoinError, JoinSet},
    time::sleep,
};

use crate::{HubModelConfiguration, LocalProcessListener, LocalSocketError};

const OUTBOX_IDLE_POLL_INTERVAL: Duration = Duration::from_millis(50);
const PROCESS_UPDATE_CAPACITY: usize = 64;
const MAX_ACTIVE_CONNECTIONS: usize = 128;
const MAX_BUFFERED_INBOUND_FRAMES: usize = 8;
const MAX_SUBMITTED_INPUT_BYTES: usize = 1024 * 1024;

#[derive(Clone, Debug)]
struct ConnectionServices {
    pool: PgPool,
    eligibility_nudge: InProcessEligibilityNudge,
    model_configuration: HubModelConfiguration,
    updates: broadcast::Sender<ProcessUpdate>,
    inbound_frame_budget: Arc<Semaphore>,
}

/// The hub-owned local protocol runtime: one outbox dispatcher, one bounded
/// fan-out, and one guarded Unix listener.
#[derive(Debug)]
pub struct ProcessRuntime {
    listener: LocalProcessListener,
    pool: PgPool,
    eligibility_nudge: InProcessEligibilityNudge,
    model_configuration: HubModelConfiguration,
}

impl ProcessRuntime {
    /// Composes the guarded listener, fenced database, nudge, and static models.
    pub const fn new(
        listener: LocalProcessListener,
        pool: PgPool,
        eligibility_nudge: InProcessEligibilityNudge,
        model_configuration: HubModelConfiguration,
    ) -> Self {
        Self {
            listener,
            pool,
            eligibility_nudge,
            model_configuration,
        }
    }

    /// Serves requests and dispatches durable updates until `shutdown` changes
    /// to true or its sender closes.
    pub async fn run(self, shutdown: watch::Receiver<bool>) -> Result<(), ProcessRuntimeError> {
        let (updates, _) = broadcast::channel(PROCESS_UPDATE_CAPACITY);
        let server = serve_connections(
            &self.listener,
            self.pool.clone(),
            self.eligibility_nudge,
            self.model_configuration,
            updates.clone(),
            shutdown.clone(),
        );
        let dispatcher = dispatch_updates(self.pool, updates, shutdown);
        let result = tokio::try_join!(server, dispatcher);
        let cleanup = self.listener.cleanup();

        result?;
        cleanup.map_err(ProcessRuntimeError::CleanupSocket)
    }
}

async fn dispatch_updates(
    pool: PgPool,
    updates: broadcast::Sender<ProcessUpdate>,
    mut shutdown: watch::Receiver<bool>,
) -> Result<(), ProcessRuntimeError> {
    let dispatcher = OutboxDispatcher::new(pool);
    loop {
        if shutdown_requested(&shutdown) {
            return Ok(());
        }
        let outcome = dispatcher
            .dispatch_next(|event| {
                let update = ProcessUpdate::from(event);
                let _ = updates.send(update);
                OutboxDeliveryDecision::Delivered
            })
            .await
            .map_err(ProcessRuntimeError::Dispatch)?;
        match outcome {
            OutboxDispatchOutcome::Delivered { .. } => {}
            OutboxDispatchOutcome::Idle => {
                tokio::select! {
                    () = wait_for_shutdown(&mut shutdown) => return Ok(()),
                    () = sleep(OUTBOX_IDLE_POLL_INTERVAL) => {}
                }
            }
            OutboxDispatchOutcome::Retry { .. } => {
                return Err(ProcessRuntimeError::UnexpectedDispatcherRetry);
            }
        }
    }
}

async fn serve_connections(
    listener: &LocalProcessListener,
    pool: PgPool,
    eligibility_nudge: InProcessEligibilityNudge,
    model_configuration: HubModelConfiguration,
    updates: broadcast::Sender<ProcessUpdate>,
    mut shutdown: watch::Receiver<bool>,
) -> Result<(), ProcessRuntimeError> {
    let services = ConnectionServices {
        pool,
        eligibility_nudge,
        model_configuration,
        updates,
        inbound_frame_budget: Arc::new(Semaphore::new(MAX_BUFFERED_INBOUND_FRAMES)),
    };
    let mut connections = JoinSet::new();
    loop {
        if shutdown_requested(&shutdown) {
            break;
        }
        tokio::select! {
            () = wait_for_shutdown(&mut shutdown) => break,
            accepted = listener.accept(), if connections.len() < MAX_ACTIVE_CONNECTIONS => {
                let (stream, _) = accepted.map_err(ProcessRuntimeError::Accept)?;
                connections.spawn(serve_connection(
                    stream,
                    services.clone(),
                    shutdown.clone(),
                ));
            }
            completed = connections.join_next(), if !connections.is_empty() => {
                inspect_connection_completion(completed)?;
            }
        }
    }

    while let Some(completed) = connections.join_next().await {
        inspect_connection_completion(Some(completed))?;
    }
    Ok(())
}

fn inspect_connection_completion(
    completed: Option<Result<Result<(), ProcessConnectionError>, JoinError>>,
) -> Result<(), ProcessRuntimeError> {
    match completed {
        None | Some(Ok(Ok(()))) => Ok(()),
        Some(Ok(Err(ProcessConnectionError::Io(error)))) => {
            drop(error);
            Ok(())
        }
        Some(Ok(Err(ProcessConnectionError::Encode(FrameEncodeError::OversizedFrame)))) => Ok(()),
        Some(Ok(Err(ProcessConnectionError::Encode(error)))) => {
            Err(ProcessRuntimeError::Encode(error))
        }
        Some(Ok(Err(ProcessConnectionError::EncodeInvariant))) => {
            Err(ProcessRuntimeError::EncodeInvariant)
        }
        Some(Ok(Err(ProcessConnectionError::InboundFrameBudgetClosed))) => {
            Err(ProcessRuntimeError::InboundFrameBudgetClosed)
        }
        Some(Err(error)) => Err(ProcessRuntimeError::ConnectionTask(error)),
    }
}

async fn serve_connection(
    stream: UnixStream,
    services: ConnectionServices,
    mut shutdown: watch::Receiver<bool>,
) -> Result<(), ProcessConnectionError> {
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);

    loop {
        if shutdown_requested(&shutdown) {
            return Ok(());
        }
        let Some(frame_buffer_permit) =
            acquire_inbound_frame_permit(Arc::clone(&services.inbound_frame_budget), &mut shutdown)
                .await?
        else {
            return Ok(());
        };
        let line = tokio::select! {
            () = wait_for_shutdown(&mut shutdown) => return Ok(()),
            line = read_frame_line(&mut reader) => line?,
        };
        let Some(line) = line else {
            return Ok(());
        };
        let frame = match line {
            IncomingLine::Complete(line) => match decode_client_line(&line) {
                Ok(frame) => frame,
                Err(error) => {
                    let code = match error.kind() {
                        FrameDecodeErrorKind::UnsupportedVersion => ErrorCode::UnsupportedVersion,
                        FrameDecodeErrorKind::OversizedFrame
                        | FrameDecodeErrorKind::MalformedFrame => ErrorCode::MalformedFrame,
                    };
                    write_error(
                        &mut writer,
                        error.request_id(),
                        ProtocolError::without_detail(code),
                    )
                    .await?;
                    return Ok(());
                }
            },
            IncomingLine::Oversized(request_id) => {
                write_error(
                    &mut writer,
                    request_id,
                    ProtocolError::without_detail(ErrorCode::MalformedFrame),
                )
                .await?;
                return Ok(());
            }
        };
        drop(frame_buffer_permit);
        let request_id = frame.request_id();
        let request = frame.request().clone();
        let follows = matches!(request, ClientRequest::FollowSession { .. });
        handle_request(
            &mut writer,
            request_id,
            request,
            &services,
            shutdown.clone(),
        )
        .await?;
        if follows {
            return Ok(());
        }
    }
}

async fn acquire_inbound_frame_permit(
    budget: Arc<Semaphore>,
    shutdown: &mut watch::Receiver<bool>,
) -> Result<Option<OwnedSemaphorePermit>, ProcessConnectionError> {
    tokio::select! {
        () = wait_for_shutdown(shutdown) => Ok(None),
        permit = budget.acquire_owned() => permit
            .map(Some)
            .map_err(|_| ProcessConnectionError::InboundFrameBudgetClosed),
    }
}

async fn handle_request<Writer>(
    writer: &mut Writer,
    request_id: RequestId,
    request: ClientRequest,
    services: &ConnectionServices,
    shutdown: watch::Receiver<bool>,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    match request {
        ClientRequest::CreateSession {
            command_id,
            initial_model_selection,
        } => {
            handle_create_session(
                writer,
                request_id,
                command_id.into_uuid(),
                initial_model_selection,
                &services.pool,
            )
            .await
        }
        ClientRequest::ListSessions {} => {
            handle_list_sessions(writer, request_id, &services.pool).await
        }
        ClientRequest::SubmitInput {
            command_id,
            session_id,
            content,
            expected_defaults_version,
        } => {
            handle_submit_input(
                writer,
                request_id,
                command_id.into_uuid(),
                session_id,
                content,
                expected_defaults_version,
                &services.pool,
                &services.eligibility_nudge,
                &services.model_configuration,
            )
            .await
        }
        ClientRequest::ReadTranscript { session_id } => {
            handle_read_transcript(writer, request_id, session_id, &services.pool).await
        }
        ClientRequest::FollowSession { session_id } => {
            handle_follow_session(
                writer,
                request_id,
                session_id,
                &services.pool,
                &services.updates,
                shutdown,
            )
            .await
        }
    }
}

async fn handle_create_session<Writer>(
    writer: &mut Writer,
    request_id: RequestId,
    command_id: uuid::Uuid,
    initial_model_selection: WireModelSelection,
    pool: &PgPool,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let request = CreateSessionRequest::try_new(
        DurableCommandId::from_uuid(command_id),
        SessionConfigurationDefaults::new(domain_model_selection(initial_model_selection)),
    );
    let Ok(request) = request else {
        return write_error(
            writer,
            request_id,
            ProtocolError::without_detail(ErrorCode::InvalidRequest),
        )
        .await;
    };
    let mut service = CreateSessionService::new(
        UuidV7SessionIdGenerator,
        CreateSessionRepository::new(pool.clone()),
    );
    match service.execute(request).await {
        Ok(CreateSessionOutcome::Applied(result)) => {
            write_message(
                writer,
                request_id,
                ServerMessage::SessionCreated {
                    session_id: wire_uuid(result.session().into_uuid()),
                },
            )
            .await
        }
        Ok(CreateSessionOutcome::ConflictingReuse { .. }) => {
            write_error(
                writer,
                request_id,
                ProtocolError::without_detail(ErrorCode::ConflictingReuse),
            )
            .await
        }
        Err(CreateSessionError::Transaction(CreateSessionRepositoryError::Database(_))) => {
            write_error(writer, request_id, ProtocolError::mutation_unavailable()).await
        }
        Err(
            CreateSessionError::Preparation(_)
            | CreateSessionError::Transaction(
                CreateSessionRepositoryError::DifferentCommandKind { .. }
                | CreateSessionRepositoryError::Corruption(_),
            ),
        ) => {
            write_error(
                writer,
                request_id,
                ProtocolError::without_detail(ErrorCode::Internal),
            )
            .await
        }
    }
}

async fn handle_list_sessions<Writer>(
    writer: &mut Writer,
    request_id: RequestId,
    pool: &PgPool,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let summaries = match ProcessReadRepository::new(pool.clone())
        .list_sessions()
        .await
    {
        Ok(summaries) => summaries,
        Err(error) => return write_process_read_error(writer, request_id, error).await,
    };
    write_message(writer, request_id, ServerMessage::SessionsStart {}).await?;
    for summary in &summaries {
        write_message(
            writer,
            request_id,
            ServerMessage::SessionSummary {
                session_id: wire_uuid(summary.session().into_uuid()),
                defaults_version: CanonicalU64::new(summary.defaults_version()),
                model_selection: wire_model_selection(summary.model_selection()),
            },
        )
        .await?;
    }
    let session_count =
        u64::try_from(summaries.len()).map_err(|_| ProcessConnectionError::EncodeInvariant)?;
    write_message(
        writer,
        request_id,
        ServerMessage::SessionsEnd {
            session_count: CanonicalU64::new(session_count),
        },
    )
    .await
}

#[derive(Debug)]
struct ConfiguredSubmitInputTransaction<'configuration> {
    repository: SubmitInputRepository,
    model_configuration: &'configuration HubModelConfiguration,
}

impl SubmitInputTransaction for ConfiguredSubmitInputTransaction<'_> {
    type Error = SubmitInputRepositoryError;

    async fn handle<NextTurn>(
        &mut self,
        command: SubmitInput,
        accepted_input: AcceptedInputId,
        turn: Option<TurnId>,
        cancellation_identities: CancelledModelCallTurnIdentities,
        next_reclassified_turn: NextTurn,
    ) -> Result<SubmitInputOutcome, Self::Error>
    where
        NextTurn: FnMut(AcceptedInputId) -> TurnId + Send,
    {
        let outcome = self
            .repository
            .handle_with_candidates_and_alias_resolver(
                command,
                accepted_input,
                turn,
                cancellation_identities,
                next_reclassified_turn,
                |alias| self.model_configuration.resolve_alias(alias),
            )
            .await?;

        Ok(match outcome {
            SubmitInputHandlingOutcome::Recorded(result) => SubmitInputOutcome::Recorded(result),
            SubmitInputHandlingOutcome::ConflictingReuse { command_id } => {
                SubmitInputOutcome::ConflictingReuse { command_id }
            }
        })
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "the closed submit request is kept explicit at this wire-to-application adapter"
)]
async fn handle_submit_input<Writer>(
    writer: &mut Writer,
    request_id: RequestId,
    command_id: uuid::Uuid,
    session_id: CanonicalUuid,
    content: InputContent,
    expected_defaults_version: CanonicalU64,
    pool: &PgPool,
    eligibility_nudge: &InProcessEligibilityNudge,
    model_configuration: &HubModelConfiguration,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let session = SessionId::from_uuid(session_id.into_uuid());
    let Some(expected_version) =
        SessionConfigurationDefaultsVersion::try_from_u64(expected_defaults_version.value())
    else {
        return write_error(
            writer,
            request_id,
            ProtocolError::without_detail(ErrorCode::InvalidRequest),
        )
        .await;
    };
    let Ok(content) = admitted_user_content(&content) else {
        return write_error(
            writer,
            request_id,
            ProtocolError::without_detail(ErrorCode::InvalidRequest),
        )
        .await;
    };
    let request = SubmitInputRequest::try_new(
        DurableCommandId::from_uuid(command_id),
        session,
        content,
        DeliveryRequest::StartWhenNoActiveTurn {
            configuration: PerInputConfigurationChoices::new(
                expected_version,
                ModelSelectionOverride::UseSessionDefault,
            ),
        },
    );
    let Ok(request) = request else {
        return write_error(
            writer,
            request_id,
            ProtocolError::without_detail(ErrorCode::InvalidRequest),
        )
        .await;
    };
    let mut service = SubmitInputService::new(
        UuidV7SubmitInputIdGenerator,
        ConfiguredSubmitInputTransaction {
            repository: SubmitInputRepository::new(pool.clone()),
            model_configuration,
        },
        eligibility_nudge.clone(),
    );
    match service.execute(request).await {
        Ok(SubmitInputOutcome::Recorded(SubmitInputResult::Applied(
            SubmitInputAppliedResult::TurnOrigin(result),
        ))) => {
            write_message(
                writer,
                request_id,
                ServerMessage::InputSubmitted {
                    session_id,
                    accepted_input_id: wire_uuid(result.accepted_input().into_uuid()),
                    acceptance_position: CanonicalU64::new(result.acceptance_position().as_u64()),
                    turn_id: wire_uuid(result.turn().into_uuid()),
                },
            )
            .await
        }
        Ok(SubmitInputOutcome::Recorded(SubmitInputResult::Rejected(rejected))) => {
            write_error(
                writer,
                request_id,
                ProtocolError::rejected(map_rejection(rejected)?),
            )
            .await
        }
        Ok(SubmitInputOutcome::ConflictingReuse { .. }) => {
            write_error(
                writer,
                request_id,
                ProtocolError::without_detail(ErrorCode::ConflictingReuse),
            )
            .await
        }
        Err(SubmitInputRepositoryError::Database(_)) => {
            write_error(writer, request_id, ProtocolError::mutation_unavailable()).await
        }
        Err(SubmitInputRepositoryError::ModelExecution(error))
            if matches!(
                error.as_ref(),
                signalbox_persistence::model_execution::ModelCallRepositoryError::Database { .. }
            ) =>
        {
            write_error(writer, request_id, ProtocolError::mutation_unavailable()).await
        }
        Ok(SubmitInputOutcome::Recorded(SubmitInputResult::Applied(
            SubmitInputAppliedResult::PendingSteering(_),
        )))
        | Err(
            SubmitInputRepositoryError::DifferentCommandKind { .. }
            | SubmitInputRepositoryError::AcceptedInputIdentityCollision { .. }
            | SubmitInputRepositoryError::Corruption(_)
            | SubmitInputRepositoryError::ModelExecution(_),
        ) => {
            write_error(
                writer,
                request_id,
                ProtocolError::without_detail(ErrorCode::Internal),
            )
            .await
        }
    }
}

fn admitted_user_content(content: &InputContent) -> Result<UserContent, ()> {
    if content.as_str().len() > MAX_SUBMITTED_INPUT_BYTES {
        return Err(());
    }
    UserContent::try_text(content.as_str().to_owned()).map_err(|_| ())
}

async fn handle_read_transcript<Writer>(
    writer: &mut Writer,
    request_id: RequestId,
    session_id: CanonicalUuid,
    pool: &PgPool,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let mut spool = match spool_transcript(
        ProcessReadRepository::new(pool.clone()),
        SessionId::from_uuid(session_id.into_uuid()),
        request_id,
    )
    .await
    {
        Ok(Some(spool)) => spool,
        Ok(None) => {
            return write_error(
                writer,
                request_id,
                ProtocolError::without_detail(ErrorCode::NotFound),
            )
            .await;
        }
        Err(TranscriptSpoolError::Read(error)) => {
            return write_process_read_error(writer, request_id, error).await;
        }
        Err(TranscriptSpoolError::Spool(error)) => return Err(error),
    };
    write_spooled_transcript(writer, &mut spool).await
}

async fn handle_follow_session<Writer>(
    writer: &mut Writer,
    request_id: RequestId,
    session_id: CanonicalUuid,
    pool: &PgPool,
    updates: &broadcast::Sender<ProcessUpdate>,
    mut shutdown: watch::Receiver<bool>,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let mut subscription = updates.subscribe();
    let selected_session = SessionId::from_uuid(session_id.into_uuid());
    let snapshot_result = run_until_shutdown(
        &mut shutdown,
        spool_transcript(
            ProcessReadRepository::new(pool.clone()),
            selected_session,
            request_id,
        ),
    )
    .await;
    let Some(snapshot_result) = snapshot_result else {
        return Ok(());
    };
    let mut spool = match snapshot_result {
        Ok(Some(spool)) => spool,
        Ok(None) => {
            return run_until_shutdown(
                &mut shutdown,
                write_error(
                    writer,
                    request_id,
                    ProtocolError::without_detail(ErrorCode::NotFound),
                ),
            )
            .await
            .unwrap_or(Ok(()));
        }
        Err(TranscriptSpoolError::Read(error)) => {
            return run_until_shutdown(
                &mut shutdown,
                write_process_read_error(writer, request_id, error),
            )
            .await
            .unwrap_or(Ok(()));
        }
        Err(TranscriptSpoolError::Spool(error)) => return Err(error),
    };
    let Some(snapshot_write) =
        run_until_shutdown(&mut shutdown, write_spooled_transcript(writer, &mut spool)).await
    else {
        return Ok(());
    };
    snapshot_write?;
    let mut observed_cursor = spool.cursor;

    loop {
        let update = tokio::select! {
            () = wait_for_shutdown(&mut shutdown) => return Ok(()),
            update = subscription.recv() => update,
        };
        let update = match update {
            Ok(update) => update,
            Err(broadcast::error::RecvError::Lagged(_)) => {
                return run_until_shutdown(
                    &mut shutdown,
                    write_error(
                        writer,
                        request_id,
                        ProtocolError::without_detail(ErrorCode::ResyncRequired),
                    ),
                )
                .await
                .unwrap_or(Ok(()));
            }
            Err(broadcast::error::RecvError::Closed) => return Ok(()),
        };
        if update.cursor <= observed_cursor {
            continue;
        }
        observed_cursor = update.cursor;
        if update.session != selected_session {
            continue;
        }
        let Some(event_write) = run_until_shutdown(
            &mut shutdown,
            write_message(
                writer,
                request_id,
                ServerMessage::SessionEvent {
                    cursor: CanonicalU64::new(update.cursor),
                    session_id,
                    event: update.event.wire(),
                },
            ),
        )
        .await
        else {
            return Ok(());
        };
        event_write?;
    }
}

struct TranscriptSpool {
    file: tokio::fs::File,
    cursor: u64,
}

enum TranscriptSpoolError {
    Read(ProcessReadError),
    Spool(ProcessConnectionError),
}

async fn spool_transcript(
    repository: ProcessReadRepository,
    session: SessionId,
    request_id: RequestId,
) -> Result<Option<TranscriptSpool>, TranscriptSpoolError> {
    let Some(mut reader) = repository
        .open_transcript(session)
        .await
        .map_err(TranscriptSpoolError::Read)?
    else {
        return Ok(None);
    };
    let standard_file = tempfile::tempfile()
        .map_err(ProcessConnectionError::from)
        .map_err(TranscriptSpoolError::Spool)?;
    let mut file = tokio::fs::File::from_std(standard_file);
    let session_id = wire_uuid(reader.session().into_uuid());
    let cursor = CanonicalU64::new(reader.cursor());
    write_message(
        &mut file,
        request_id,
        ServerMessage::TranscriptSnapshotStart { session_id, cursor },
    )
    .await
    .map_err(TranscriptSpoolError::Spool)?;
    while let Some(item) = reader
        .next_item()
        .await
        .map_err(TranscriptSpoolError::Read)?
    {
        match item {
            ProcessTranscriptItem::Turn(turn) => {
                write_transcript_turn(&mut file, request_id, &turn)
                    .await
                    .map_err(TranscriptSpoolError::Spool)?;
            }
            ProcessTranscriptItem::Entry(entry) => {
                write_transcript_entry(&mut file, request_id, &entry)
                    .await
                    .map_err(TranscriptSpoolError::Spool)?;
            }
        }
    }
    let summary = reader
        .summary()
        .ok_or(ProcessConnectionError::EncodeInvariant)
        .map_err(TranscriptSpoolError::Spool)?;
    write_message(
        &mut file,
        request_id,
        ServerMessage::TranscriptSnapshotEnd {
            session_id,
            cursor,
            turn_count: CanonicalU64::new(summary.turn_count()),
            entry_count: CanonicalU64::new(summary.entry_count()),
        },
    )
    .await
    .map_err(TranscriptSpoolError::Spool)?;
    file.flush()
        .await
        .map_err(ProcessConnectionError::from)
        .map_err(TranscriptSpoolError::Spool)?;
    file.seek(SeekFrom::Start(0))
        .await
        .map_err(ProcessConnectionError::from)
        .map_err(TranscriptSpoolError::Spool)?;
    Ok(Some(TranscriptSpool {
        file,
        cursor: summary.cursor(),
    }))
}

async fn write_spooled_transcript<Writer>(
    writer: &mut Writer,
    spool: &mut TranscriptSpool,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    tokio::io::copy(&mut spool.file, writer).await?;
    Ok(())
}

async fn write_transcript_turn<Writer>(
    writer: &mut Writer,
    request_id: RequestId,
    turn: &ProcessTranscriptTurn,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    write_message(
        writer,
        request_id,
        ServerMessage::TranscriptTurn {
            turn_id: wire_uuid(turn.turn().into_uuid()),
            acceptance_position: CanonicalU64::new(turn.acceptance_position()),
            state: wire_turn_state(turn.state()),
        },
    )
    .await
}

async fn write_transcript_entry<Writer>(
    writer: &mut Writer,
    request_id: RequestId,
    entry: &ProcessTranscriptEntry,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    match entry {
        ProcessTranscriptEntry::User {
            entry_index,
            source_session,
            entry,
            accepted_input,
            turn,
            content,
        } => {
            write_message(
                writer,
                request_id,
                ServerMessage::TranscriptTextEntry {
                    entry_index: CanonicalU64::new(*entry_index),
                    source_session_id: wire_uuid(source_session.into_uuid()),
                    entry_id: wire_uuid(entry.into_uuid()),
                    entry: TranscriptTextEntry::User {
                        accepted_input_id: wire_uuid(accepted_input.into_uuid()),
                        turn_id: wire_uuid(turn.into_uuid()),
                    },
                },
            )
            .await?;
            write_content(writer, request_id, *entry_index, content).await
        }
        ProcessTranscriptEntry::Assistant {
            entry_index,
            source_session,
            entry,
            turn,
            model_call,
            content,
        } => {
            write_message(
                writer,
                request_id,
                ServerMessage::TranscriptTextEntry {
                    entry_index: CanonicalU64::new(*entry_index),
                    source_session_id: wire_uuid(source_session.into_uuid()),
                    entry_id: wire_uuid(entry.into_uuid()),
                    entry: TranscriptTextEntry::Assistant {
                        turn_id: wire_uuid(turn.into_uuid()),
                        model_call_id: wire_uuid(model_call.into_uuid()),
                    },
                },
            )
            .await?;
            write_content(writer, request_id, *entry_index, content).await
        }
        ProcessTranscriptEntry::TurnFailed {
            entry_index,
            source_session,
            entry,
            turn,
        } => {
            write_message(
                writer,
                request_id,
                ServerMessage::TranscriptEntry {
                    entry_index: CanonicalU64::new(*entry_index),
                    source_session_id: wire_uuid(source_session.into_uuid()),
                    entry_id: wire_uuid(entry.into_uuid()),
                    entry: TranscriptEntry::TurnFailed {
                        turn_id: wire_uuid(turn.into_uuid()),
                    },
                },
            )
            .await
        }
        ProcessTranscriptEntry::TurnCompleted {
            entry_index,
            source_session,
            entry,
            turn,
        } => {
            write_message(
                writer,
                request_id,
                ServerMessage::TranscriptEntry {
                    entry_index: CanonicalU64::new(*entry_index),
                    source_session_id: wire_uuid(source_session.into_uuid()),
                    entry_id: wire_uuid(entry.into_uuid()),
                    entry: TranscriptEntry::TurnCompleted {
                        turn_id: wire_uuid(turn.into_uuid()),
                    },
                },
            )
            .await
        }
        ProcessTranscriptEntry::TurnCancelled {
            entry_index,
            source_session,
            entry,
            turn,
        } => {
            write_message(
                writer,
                request_id,
                ServerMessage::TranscriptEntry {
                    entry_index: CanonicalU64::new(*entry_index),
                    source_session_id: wire_uuid(source_session.into_uuid()),
                    entry_id: wire_uuid(entry.into_uuid()),
                    entry: TranscriptEntry::TurnCancelled {
                        turn_id: wire_uuid(turn.into_uuid()),
                    },
                },
            )
            .await
        }
    }
}

async fn write_content<Writer>(
    writer: &mut Writer,
    request_id: RequestId,
    entry_index: u64,
    content: &str,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let mut fragments = content_fragments(content).peekable();
    let mut fragment_index = 0_u64;
    while let Some(fragment) = fragments.next() {
        let final_fragment = fragments.peek().is_none();
        write_message(
            writer,
            request_id,
            ServerMessage::TranscriptContent {
                entry_index: CanonicalU64::new(entry_index),
                fragment_index: CanonicalU64::new(fragment_index),
                final_fragment,
                content_fragment: fragment,
            },
        )
        .await?;
        if !final_fragment {
            fragment_index = fragment_index
                .checked_add(1)
                .ok_or(ProcessConnectionError::EncodeInvariant)?;
        }
    }
    Ok(())
}

fn map_rejection(
    rejected: SubmitInputRejectedResult,
) -> Result<RejectionDetail, ProcessConnectionError> {
    Ok(match rejected {
        SubmitInputRejectedResult::SessionNotFound { session } => {
            RejectionDetail::SessionNotFound {
                session_id: wire_uuid(session.into_uuid()),
            }
        }
        SubmitInputRejectedResult::ActiveTurnPresent {
            session,
            active_turn,
        } => RejectionDetail::ActiveTurnPresent {
            session_id: wire_uuid(session.into_uuid()),
            active_turn_id: wire_uuid(active_turn.into_uuid()),
        },
        SubmitInputRejectedResult::SessionDefaultsVersionMismatch {
            session,
            expected,
            current,
        } => RejectionDetail::DefaultsVersionMismatch {
            session_id: wire_uuid(session.into_uuid()),
            expected: CanonicalU64::new(expected.as_u64()),
            current: CanonicalU64::new(current.as_u64()),
        },
        SubmitInputRejectedResult::UnknownModelAlias { session, alias } => {
            RejectionDetail::UnknownModelAlias {
                session_id: wire_uuid(session.into_uuid()),
                alias_id: wire_uuid(alias.into_uuid()),
            }
        }
        SubmitInputRejectedResult::AcceptancePositionExhausted { session, last } => {
            RejectionDetail::AcceptancePositionExhausted {
                session_id: wire_uuid(session.into_uuid()),
                last: CanonicalU64::new(last.as_u64()),
            }
        }
        SubmitInputRejectedResult::NoActiveTurn { .. }
        | SubmitInputRejectedResult::ActiveTurnMismatch { .. }
        | SubmitInputRejectedResult::SafePointUnavailableWhileStopping { .. }
        | SubmitInputRejectedResult::InterruptAlreadyApplied { .. } => {
            return Err(ProcessConnectionError::EncodeInvariant);
        }
    })
}

fn domain_model_selection(selection: WireModelSelection) -> ModelSelectionRequest {
    match selection {
        WireModelSelection::Direct { selection_id } => {
            ModelSelectionRequest::Direct(DirectModelSelection::from_uuid(selection_id.into_uuid()))
        }
        WireModelSelection::Alias { alias_id } => {
            ModelSelectionRequest::Alias(ModelAlias::from_uuid(alias_id.into_uuid()))
        }
    }
}

fn wire_model_selection(selection: ProcessModelSelection) -> WireModelSelection {
    match selection {
        ProcessModelSelection::Direct(selection) => WireModelSelection::Direct {
            selection_id: wire_uuid(selection.into_uuid()),
        },
        ProcessModelSelection::Alias(alias) => WireModelSelection::Alias {
            alias_id: wire_uuid(alias.into_uuid()),
        },
    }
}

fn wire_turn_state(state: &ProcessTurnState) -> TurnState {
    match state {
        ProcessTurnState::Queued {
            accepted_input,
            content,
        } => TurnState::Queued {
            accepted_input_id: wire_uuid(accepted_input.into_uuid()),
            content: InputContent::new(content.clone()),
        },
        ProcessTurnState::ActiveRunning {
            current_attempt,
            current_model_call,
        } => TurnState::ActiveRunning {
            current_attempt_id: wire_uuid(current_attempt.into_uuid()),
            current_model_call: current_model_call.map(|call| {
                CurrentModelCall::new(
                    wire_uuid(call.call().into_uuid()),
                    match call.state() {
                        ProcessCurrentModelCallState::Prepared => {
                            CurrentModelCallState::Prepared {}
                        }
                        ProcessCurrentModelCallState::InFlight => {
                            CurrentModelCallState::InFlight {}
                        }
                        ProcessCurrentModelCallState::CancellationRequested => {
                            CurrentModelCallState::CancellationRequested {}
                        }
                    },
                )
            }),
        },
        ProcessTurnState::ActiveAwaitingModelCallRecovery {
            ended_attempt,
            recovery_call,
        } => TurnState::ActiveAwaitingModelCallRecovery {
            ended_attempt_id: wire_uuid(ended_attempt.into_uuid()),
            recovery_model_call_id: wire_uuid(recovery_call.into_uuid()),
        },
        ProcessTurnState::Failed {
            terminal_frontier,
            terminal_attempt,
            terminal_model_call,
        } => TurnState::Failed {
            terminal_frontier_id: wire_uuid(terminal_frontier.into_uuid()),
            terminal_attempt_id: terminal_attempt.map(|attempt| wire_uuid(attempt.into_uuid())),
            terminal_model_call: terminal_model_call.map(|call| {
                FailedTerminalModelCall::new(
                    wire_uuid(call.call().into_uuid()),
                    match call.disposition() {
                        ProcessFailedModelCallDisposition::KnownFailed => {
                            FailedModelCallDisposition::KnownFailed
                        }
                        ProcessFailedModelCallDisposition::Cancelled => {
                            FailedModelCallDisposition::Cancelled
                        }
                    },
                )
            }),
        },
        ProcessTurnState::Completed {
            terminal_frontier,
            terminal_attempt,
            terminal_call,
        } => TurnState::Completed {
            terminal_frontier_id: wire_uuid(terminal_frontier.into_uuid()),
            terminal_attempt_id: wire_uuid(terminal_attempt.into_uuid()),
            terminal_model_call_id: wire_uuid(terminal_call.into_uuid()),
        },
        ProcessTurnState::Refused {
            terminal_frontier,
            terminal_attempt,
            terminal_call,
        } => TurnState::Refused {
            terminal_frontier_id: wire_uuid(terminal_frontier.into_uuid()),
            terminal_attempt_id: wire_uuid(terminal_attempt.into_uuid()),
            terminal_model_call_id: wire_uuid(terminal_call.into_uuid()),
        },
        ProcessTurnState::Cancelled {
            terminal_frontier,
            terminal_attempt,
            terminal_call,
        } => TurnState::Cancelled {
            terminal_frontier_id: wire_uuid(terminal_frontier.into_uuid()),
            terminal_attempt_id: wire_uuid(terminal_attempt.into_uuid()),
            terminal_model_call_id: terminal_call.map(|call| wire_uuid(call.into_uuid())),
        },
        ProcessTurnState::ReconciliationRequired {
            terminal_frontier,
            terminal_attempt,
            terminal_call,
        } => TurnState::ReconciliationRequired {
            terminal_frontier_id: wire_uuid(terminal_frontier.into_uuid()),
            terminal_attempt_id: wire_uuid(terminal_attempt.into_uuid()),
            terminal_model_call_id: wire_uuid(terminal_call.into_uuid()),
        },
    }
}

async fn write_process_read_error<Writer>(
    writer: &mut Writer,
    request_id: RequestId,
    error: ProcessReadError,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let code = match error {
        ProcessReadError::Database(_) => ErrorCode::Unavailable,
        ProcessReadError::Corruption(_) => ErrorCode::Internal,
    };
    write_error(writer, request_id, ProtocolError::without_detail(code)).await
}

async fn write_error<Writer>(
    writer: &mut Writer,
    request_id: RequestId,
    error: ProtocolError,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    write_message(
        writer,
        request_id,
        ServerMessage::Error {
            code: error.code,
            message: error.message.to_owned(),
            detail: error.detail,
        },
    )
    .await
}

async fn write_message<Writer>(
    writer: &mut Writer,
    request_id: RequestId,
    message: ServerMessage,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let frame = ServerFrame::try_new(request_id, message).map_err(FrameEncodeError::Validation)?;
    let encoded = encode_server_line(&frame)?;
    writer.write_all(&encoded).await?;
    Ok(())
}

enum IncomingLine {
    Complete(Vec<u8>),
    Oversized(RequestId),
}

async fn read_frame_line<Reader>(
    reader: &mut Reader,
) -> Result<Option<IncomingLine>, ProcessConnectionError>
where
    Reader: AsyncBufRead + Unpin,
{
    let mut line = Vec::new();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return if line.is_empty() {
                Ok(None)
            } else {
                Ok(Some(IncomingLine::Complete(line)))
            };
        }
        if let Some(newline) = available.iter().position(|byte| *byte == b'\n') {
            let consumed = newline + 1;
            let frame_len = line.len().saturating_add(consumed);
            if frame_len > MAX_FRAME_BYTES {
                let request_id = if frame_len == MAX_FRAME_BYTES + 1 {
                    line.extend_from_slice(&available[..newline]);
                    recover_bounded_client_request_id(&line)
                } else {
                    RequestId::uncorrelated()
                };
                reader.consume(consumed);
                return Ok(Some(IncomingLine::Oversized(request_id)));
            }
            line.extend_from_slice(&available[..consumed]);
            reader.consume(consumed);
            return Ok(Some(IncomingLine::Complete(line)));
        }
        if line.len().saturating_add(available.len()) > MAX_FRAME_BYTES {
            let consumed = available.len();
            reader.consume(consumed);
            return Ok(Some(IncomingLine::Oversized(RequestId::uncorrelated())));
        }
        line.extend_from_slice(available);
        let consumed = available.len();
        reader.consume(consumed);
    }
}

fn shutdown_requested(shutdown: &watch::Receiver<bool>) -> bool {
    *shutdown.borrow()
}

async fn wait_for_shutdown(shutdown: &mut watch::Receiver<bool>) {
    while !shutdown_requested(shutdown) {
        if shutdown.changed().await.is_err() {
            return;
        }
    }
}

async fn run_until_shutdown<Output, Operation>(
    shutdown: &mut watch::Receiver<bool>,
    operation: Operation,
) -> Option<Output>
where
    Operation: Future<Output = Output>,
{
    tokio::select! {
        () = wait_for_shutdown(shutdown) => None,
        output = operation => Some(output),
    }
}

fn wire_uuid(value: uuid::Uuid) -> CanonicalUuid {
    CanonicalUuid::from_uuid(value)
}

struct ProtocolError {
    code: ErrorCode,
    message: &'static str,
    detail: ErrorDetail,
}

impl ProtocolError {
    const fn without_detail(code: ErrorCode) -> Self {
        Self {
            code,
            message: match code {
                ErrorCode::MalformedFrame => "the protocol frame is malformed",
                ErrorCode::UnsupportedVersion => {
                    "the protocol version is unsupported; supported version: 1"
                }
                ErrorCode::InvalidRequest => "the request values are invalid",
                ErrorCode::NotFound => "the requested session was not found",
                ErrorCode::ConflictingReuse => {
                    "the command identity already names different intent"
                }
                ErrorCode::Rejected => "the command was rejected by current durable state",
                ErrorCode::ResyncRequired => {
                    "the follow stream fell behind; reconnect for a fresh snapshot"
                }
                ErrorCode::Unavailable => "the requested operation is unavailable",
                ErrorCode::Internal => "the request failed an internal integrity check",
            },
            detail: ErrorDetail::none(),
        }
    }

    const fn mutation_unavailable() -> Self {
        Self {
            code: ErrorCode::Unavailable,
            message: "the mutation outcome may be ambiguous; retry the exact command",
            detail: ErrorDetail::none(),
        }
    }

    const fn rejected(detail: RejectionDetail) -> Self {
        Self {
            code: ErrorCode::Rejected,
            message: "the command was rejected by current durable state",
            detail: ErrorDetail::rejected(detail),
        }
    }
}

#[derive(Clone, Debug)]
struct ProcessUpdate {
    cursor: u64,
    session: SessionId,
    event: ProcessUpdateEvent,
}

impl From<&DispatchedOutboxEvent> for ProcessUpdate {
    fn from(event: &DispatchedOutboxEvent) -> Self {
        Self {
            cursor: event.sequence(),
            session: event.session(),
            event: ProcessUpdateEvent::from(event.kind()),
        }
    }
}

#[derive(Clone, Debug)]
enum ProcessUpdateEvent {
    SessionCreated,
    InputAccepted {
        accepted_input: signalbox_domain::AcceptedInputId,
        turn: signalbox_domain::TurnId,
        acceptance_position: u64,
        content: String,
    },
    TurnActivated {
        turn: signalbox_domain::TurnId,
        current_attempt: signalbox_domain::TurnAttemptId,
    },
    ModelCallTransition {
        turn: signalbox_domain::TurnId,
        call: signalbox_domain::ModelCallId,
        state: DispatchedModelCallState,
    },
    TurnCompleted {
        turn: signalbox_domain::TurnId,
        call: signalbox_domain::ModelCallId,
        completion_entry: signalbox_domain::SemanticTranscriptEntryId,
        terminal_frontier: signalbox_domain::ContextFrontierId,
    },
    TurnFailed {
        turn: signalbox_domain::TurnId,
        failure_entry: signalbox_domain::SemanticTranscriptEntryId,
        terminal_frontier: signalbox_domain::ContextFrontierId,
    },
    TurnRefused {
        turn: signalbox_domain::TurnId,
        call: signalbox_domain::ModelCallId,
        terminal_frontier: signalbox_domain::ContextFrontierId,
    },
    TurnCancelled {
        turn: signalbox_domain::TurnId,
        cancellation_entry: signalbox_domain::SemanticTranscriptEntryId,
        terminal_frontier: signalbox_domain::ContextFrontierId,
    },
    TurnReconciliationRequired {
        turn: signalbox_domain::TurnId,
        call: signalbox_domain::ModelCallId,
        terminal_frontier: signalbox_domain::ContextFrontierId,
    },
}

impl From<&DispatchedOutboxEventKind> for ProcessUpdateEvent {
    fn from(event: &DispatchedOutboxEventKind) -> Self {
        match event {
            DispatchedOutboxEventKind::SessionCreated => Self::SessionCreated,
            DispatchedOutboxEventKind::InputAccepted {
                accepted_input,
                turn,
                acceptance_position,
                content,
            } => Self::InputAccepted {
                accepted_input: *accepted_input,
                turn: *turn,
                acceptance_position: acceptance_position.as_u64(),
                content: content.clone(),
            },
            DispatchedOutboxEventKind::TurnActivated {
                turn,
                current_attempt,
            } => Self::TurnActivated {
                turn: *turn,
                current_attempt: *current_attempt,
            },
            DispatchedOutboxEventKind::TurnFailed {
                turn,
                failure_entry,
                terminal_frontier,
            } => Self::TurnFailed {
                turn: *turn,
                failure_entry: *failure_entry,
                terminal_frontier: *terminal_frontier,
            },
            DispatchedOutboxEventKind::ModelCallTransition { turn, call, state } => {
                Self::ModelCallTransition {
                    turn: *turn,
                    call: *call,
                    state: *state,
                }
            }
            DispatchedOutboxEventKind::TurnCompleted {
                turn,
                call,
                completion_entry,
                terminal_frontier,
            } => Self::TurnCompleted {
                turn: *turn,
                call: *call,
                completion_entry: *completion_entry,
                terminal_frontier: *terminal_frontier,
            },
            DispatchedOutboxEventKind::TurnRefused {
                turn,
                call,
                terminal_frontier,
            } => Self::TurnRefused {
                turn: *turn,
                call: *call,
                terminal_frontier: *terminal_frontier,
            },
            DispatchedOutboxEventKind::TurnCancelled {
                turn,
                cancellation_entry,
                terminal_frontier,
            } => Self::TurnCancelled {
                turn: *turn,
                cancellation_entry: *cancellation_entry,
                terminal_frontier: *terminal_frontier,
            },
            DispatchedOutboxEventKind::TurnReconciliationRequired {
                turn,
                call,
                terminal_frontier,
            } => Self::TurnReconciliationRequired {
                turn: *turn,
                call: *call,
                terminal_frontier: *terminal_frontier,
            },
        }
    }
}

impl ProcessUpdateEvent {
    fn wire(&self) -> SessionEvent {
        match self {
            Self::SessionCreated => SessionEvent::SessionCreated {},
            Self::InputAccepted {
                accepted_input,
                turn,
                acceptance_position,
                content,
            } => SessionEvent::InputAccepted {
                accepted_input_id: wire_uuid(accepted_input.into_uuid()),
                turn_id: wire_uuid(turn.into_uuid()),
                acceptance_position: CanonicalU64::new(*acceptance_position),
                content: InputContent::new(content.clone()),
            },
            Self::TurnActivated {
                turn,
                current_attempt,
            } => SessionEvent::TurnActivated {
                turn_id: wire_uuid(turn.into_uuid()),
                current_attempt_id: wire_uuid(current_attempt.into_uuid()),
            },
            Self::ModelCallTransition { turn, call, state } => SessionEvent::ModelCallTransition {
                turn_id: wire_uuid(turn.into_uuid()),
                model_call_id: wire_uuid(call.into_uuid()),
                state: wire_model_call_state(*state),
            },
            Self::TurnCompleted {
                turn,
                call,
                completion_entry,
                terminal_frontier,
            } => SessionEvent::TurnCompleted {
                turn_id: wire_uuid(turn.into_uuid()),
                model_call_id: wire_uuid(call.into_uuid()),
                completion_entry_id: wire_uuid(completion_entry.into_uuid()),
                terminal_frontier_id: wire_uuid(terminal_frontier.into_uuid()),
            },
            Self::TurnFailed {
                turn,
                failure_entry,
                terminal_frontier,
            } => SessionEvent::TurnFailed {
                turn_id: wire_uuid(turn.into_uuid()),
                failure_entry_id: wire_uuid(failure_entry.into_uuid()),
                terminal_frontier_id: wire_uuid(terminal_frontier.into_uuid()),
            },
            Self::TurnRefused {
                turn,
                call,
                terminal_frontier,
            } => SessionEvent::TurnRefused {
                turn_id: wire_uuid(turn.into_uuid()),
                model_call_id: wire_uuid(call.into_uuid()),
                terminal_frontier_id: wire_uuid(terminal_frontier.into_uuid()),
            },
            Self::TurnCancelled {
                turn,
                cancellation_entry,
                terminal_frontier,
            } => SessionEvent::TurnCancelled {
                turn_id: wire_uuid(turn.into_uuid()),
                cancellation_entry_id: wire_uuid(cancellation_entry.into_uuid()),
                terminal_frontier_id: wire_uuid(terminal_frontier.into_uuid()),
            },
            Self::TurnReconciliationRequired {
                turn,
                call,
                terminal_frontier,
            } => SessionEvent::TurnReconciliationRequired {
                turn_id: wire_uuid(turn.into_uuid()),
                model_call_id: wire_uuid(call.into_uuid()),
                terminal_frontier_id: wire_uuid(terminal_frontier.into_uuid()),
            },
        }
    }
}

const fn wire_model_call_state(state: DispatchedModelCallState) -> ModelCallState {
    match state {
        DispatchedModelCallState::Prepared => ModelCallState::Prepared {},
        DispatchedModelCallState::InFlight => ModelCallState::InFlight {},
        DispatchedModelCallState::CancellationRequested => ModelCallState::CancellationRequested {},
        DispatchedModelCallState::Terminal(disposition) => ModelCallState::Terminal {
            disposition: match disposition {
                DispatchedModelCallDisposition::Completed => ModelCallDisposition::Completed,
                DispatchedModelCallDisposition::KnownFailed => ModelCallDisposition::KnownFailed,
                DispatchedModelCallDisposition::Refused => ModelCallDisposition::Refused,
                DispatchedModelCallDisposition::Cancelled => ModelCallDisposition::Cancelled,
                DispatchedModelCallDisposition::Ambiguous => ModelCallDisposition::Ambiguous,
            },
        },
    }
}

#[derive(Debug)]
enum ProcessConnectionError {
    Io(io::Error),
    Encode(FrameEncodeError),
    EncodeInvariant,
    InboundFrameBudgetClosed,
}

impl From<io::Error> for ProcessConnectionError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<FrameEncodeError> for ProcessConnectionError {
    fn from(error: FrameEncodeError) -> Self {
        Self::Encode(error)
    }
}

impl fmt::Display for ProcessConnectionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Io(_) => "the local process connection failed",
            Self::Encode(_) => "the local process connection could not encode a frame",
            Self::EncodeInvariant => {
                "the local process connection could not represent an internal value"
            }
            Self::InboundFrameBudgetClosed => {
                "the local process connection lost its inbound frame budget"
            }
        })
    }
}

impl Error for ProcessConnectionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Encode(error) => Some(error),
            Self::EncodeInvariant | Self::InboundFrameBudgetClosed => None,
        }
    }
}

/// Fatal local-process runtime failure.
#[derive(Debug)]
pub enum ProcessRuntimeError {
    /// The guarded listener could not accept a connection.
    Accept(io::Error),
    /// A server frame could not satisfy the closed wire contract.
    Encode(FrameEncodeError),
    /// Runtime-owned values could not be represented by the closed wire contract.
    EncodeInvariant,
    /// The runtime-owned aggregate inbound frame budget closed unexpectedly.
    InboundFrameBudgetClosed,
    /// A connection task panicked or was cancelled unexpectedly.
    ConnectionTask(JoinError),
    /// The durable outbox dispatcher failed.
    Dispatch(OutboxDispatchError),
    /// The single dispatcher produced an impossible retry result.
    UnexpectedDispatcherRetry,
    /// The revalidated socket path could not be cleaned up.
    CleanupSocket(LocalSocketError),
}

impl fmt::Display for ProcessRuntimeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Accept(_) => "the local process listener failed",
            Self::Encode(_) => "the local process server could not encode a frame",
            Self::EncodeInvariant => {
                "the local process server could not represent an internal value"
            }
            Self::InboundFrameBudgetClosed => {
                "the local process server lost its inbound frame budget"
            }
            Self::ConnectionTask(_) => "a local process connection task failed",
            Self::Dispatch(_) => "the durable process-update dispatcher failed",
            Self::UnexpectedDispatcherRetry => {
                "the process-update dispatcher unexpectedly requested retry"
            }
            Self::CleanupSocket(_) => "the local process socket could not be cleaned up",
        })
    }
}

impl Error for ProcessRuntimeError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Accept(error) => Some(error),
            Self::Encode(error) => Some(error),
            Self::ConnectionTask(error) => Some(error),
            Self::Dispatch(error) => Some(error),
            Self::CleanupSocket(error) => Some(error),
            Self::EncodeInvariant
            | Self::InboundFrameBudgetClosed
            | Self::UnexpectedDispatcherRetry => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{error::Error, io, sync::Arc};

    use signalbox_domain::{
        ContextFrontierId, ModelCallId, SemanticTranscriptEntryId, TurnAttemptId, TurnId,
    };
    use signalbox_process_protocol::{
        CanonicalU64, CanonicalUuid, FrameEncodeError, InputContent, MAX_CONTENT_FRAGMENT_BYTES,
        ServerFrame, ServerMessage, SessionEvent, TurnState, decode_server_line,
        encode_server_line,
    };
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt, BufReader, duplex},
        sync::{Semaphore, watch},
        time::{Duration, timeout},
    };
    use uuid::Uuid;

    use super::{
        IncomingLine, MAX_BUFFERED_INBOUND_FRAMES, MAX_FRAME_BYTES, MAX_SUBMITTED_INPUT_BYTES,
        ProcessConnectionError, ProcessUpdateEvent, RequestId, acquire_inbound_frame_permit,
        admitted_user_content, inspect_connection_completion, read_frame_line, run_until_shutdown,
        wire_model_call_state, wire_turn_state, write_content,
    };
    use signalbox_persistence::{
        outbox::{
            DispatchedModelCallDisposition, DispatchedModelCallState, DispatchedOutboxEventKind,
        },
        process_read::ProcessTurnState,
    };
    use signalbox_process_protocol::{ModelCallDisposition, ModelCallState};

    #[tokio::test]
    async fn inv033_frame_reader_accepts_the_exact_cap_and_rejects_the_next_byte()
    -> Result<(), Box<dyn Error>> {
        let mut exact = vec![b'x'; MAX_FRAME_BYTES];
        let Some(final_byte) = exact.last_mut() else {
            return Err(io::Error::other("the positive frame cap has no final byte").into());
        };
        *final_byte = b'\n';
        let mut exact_reader = BufReader::new(exact.as_slice());
        assert!(matches!(
            read_frame_line(&mut exact_reader).await?,
            Some(IncomingLine::Complete(line)) if line.len() == MAX_FRAME_BYTES
        ));

        let mut oversized = vec![b'x'; MAX_FRAME_BYTES + 1];
        let Some(final_byte) = oversized.last_mut() else {
            return Err(io::Error::other("the oversized frame has no final byte").into());
        };
        *final_byte = b'\n';
        let mut oversized_reader = BufReader::new(oversized.as_slice());
        assert!(matches!(
            read_frame_line(&mut oversized_reader).await?,
            Some(IncomingLine::Oversized(request_id)) if request_id.value() == 0
        ));

        let request_members = r#""request_id":"9""#;
        let mut correlated = format!(
            r#"{{"version":1,{request_members},"request":{{"type":"list_sessions","padding":""#
        )
        .into_bytes();
        let suffix = b"\"}}";
        correlated.resize(MAX_FRAME_BYTES - suffix.len(), b'x');
        correlated.extend_from_slice(suffix);
        correlated.push(b'\n');
        let mut correlated_reader = BufReader::new(correlated.as_slice());
        assert!(matches!(
            read_frame_line(&mut correlated_reader).await?,
            Some(IncomingLine::Oversized(request_id)) if request_id.value() == 9
        ));
        Ok(())
    }

    #[tokio::test]
    async fn inbound_frame_budget_bounds_raw_accumulation_and_waits_for_shutdown()
    -> Result<(), Box<dyn Error>> {
        assert_eq!(
            MAX_BUFFERED_INBOUND_FRAMES * MAX_FRAME_BYTES,
            64 * 1024 * 1024
        );
        let budget = Arc::new(Semaphore::new(MAX_BUFFERED_INBOUND_FRAMES));
        let (shutdown, shutdown_receiver) = watch::channel(false);
        let mut permits = Vec::new();
        for _ in 0..MAX_BUFFERED_INBOUND_FRAMES {
            permits.push(
                acquire_inbound_frame_permit(Arc::clone(&budget), &mut shutdown_receiver.clone())
                    .await?
                    .ok_or_else(|| io::Error::other("the running fixture must acquire a permit"))?,
            );
        }

        assert!(
            timeout(
                Duration::from_millis(20),
                acquire_inbound_frame_permit(Arc::clone(&budget), &mut shutdown_receiver.clone()),
            )
            .await
            .is_err(),
            "the ninth frame accumulator must wait"
        );

        drop(permits.pop());
        let released = timeout(
            Duration::from_secs(1),
            acquire_inbound_frame_permit(Arc::clone(&budget), &mut shutdown_receiver.clone()),
        )
        .await??
        .ok_or_else(|| io::Error::other("a released frame slot must be acquired"))?;
        permits.push(released);

        shutdown.send(true)?;
        assert!(
            acquire_inbound_frame_permit(Arc::clone(&budget), &mut shutdown_receiver.clone())
                .await?
                .is_none(),
            "a connection waiting for the full budget must stop on shutdown"
        );
        Ok(())
    }

    #[test]
    fn submitted_input_bound_keeps_reflected_frames_representable() -> Result<(), Box<dyn Error>> {
        let content = InputContent::new("\u{1}".repeat(MAX_SUBMITTED_INPUT_BYTES));
        assert!(admitted_user_content(&content).is_ok());
        assert!(
            admitted_user_content(&InputContent::new(
                "x".repeat(MAX_SUBMITTED_INPUT_BYTES + 1)
            ))
            .is_err()
        );

        let request_id = RequestId::try_new(u64::MAX)?;
        let session_id = CanonicalUuid::from_uuid(Uuid::from_u128(u128::MAX));
        let turn_id = CanonicalUuid::from_uuid(Uuid::from_u128(u128::MAX - 1));
        let accepted_input_id = CanonicalUuid::from_uuid(Uuid::from_u128(u128::MAX - 2));
        let frames = [
            ServerFrame::try_new(
                request_id,
                ServerMessage::TranscriptTurn {
                    turn_id,
                    acceptance_position: CanonicalU64::new(u64::MAX),
                    state: TurnState::Queued {
                        accepted_input_id,
                        content: content.clone(),
                    },
                },
            )?,
            ServerFrame::try_new(
                request_id,
                ServerMessage::SessionEvent {
                    cursor: CanonicalU64::new(u64::MAX),
                    session_id,
                    event: SessionEvent::InputAccepted {
                        accepted_input_id,
                        turn_id,
                        acceptance_position: CanonicalU64::new(u64::MAX),
                        content,
                    },
                },
            )?,
        ];
        for frame in frames {
            assert!(encode_server_line(&frame)?.len() <= MAX_FRAME_BYTES);
        }
        Ok(())
    }

    #[test]
    fn oversized_connection_frame_does_not_fail_the_runtime() {
        assert!(
            inspect_connection_completion(Some(Ok(Err(ProcessConnectionError::Encode(
                FrameEncodeError::OversizedFrame
            )))))
            .is_ok()
        );
    }

    #[tokio::test]
    async fn blocked_follow_write_is_cancelled_by_shutdown() -> Result<(), Box<dyn Error>> {
        let (mut writer, _reader) = duplex(1);
        writer.write_all(b"x").await?;
        let (shutdown, mut shutdown_receiver) = watch::channel(false);
        let blocked_write = tokio::spawn(async move {
            run_until_shutdown(
                &mut shutdown_receiver,
                writer.write_all(b"blocked follow output"),
            )
            .await
        });
        tokio::task::yield_now().await;

        shutdown.send(true)?;

        let outcome = timeout(Duration::from_secs(1), blocked_write).await??;
        assert!(outcome.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn runtime_content_writer_preserves_empty_and_multibyte_text()
    -> Result<(), Box<dyn Error>> {
        let request_id = RequestId::try_new(7)?;
        let text = format!(
            "{}\u{1f980}tail",
            "a".repeat(MAX_CONTENT_FRAGMENT_BYTES - 1)
        );
        let (mut writer, mut reader) = duplex(MAX_FRAME_BYTES * 2);
        write_content(&mut writer, request_id, 3, &text).await?;
        drop(writer);
        let mut encoded = Vec::new();
        reader.read_to_end(&mut encoded).await?;

        let mut reconstructed = String::new();
        let mut expected_fragment = 0_u64;
        let lines = encoded.split_inclusive(|byte| *byte == b'\n');
        for line in lines {
            let frame = decode_server_line(line)?;
            match frame.message() {
                ServerMessage::TranscriptContent {
                    entry_index,
                    fragment_index,
                    final_fragment,
                    content_fragment,
                } => {
                    assert_eq!(entry_index.value(), 3);
                    assert_eq!(fragment_index.value(), expected_fragment);
                    reconstructed.push_str(content_fragment.as_str());
                    expected_fragment += 1;
                    assert_eq!(*final_fragment, expected_fragment == 2);
                }
                message => {
                    return Err(io::Error::other(format!("unexpected message: {message:?}")).into());
                }
            }
        }
        assert_eq!(expected_fragment, 2);
        assert_eq!(reconstructed, text);

        let (mut writer, mut reader) = duplex(1_024);
        write_content(&mut writer, request_id, 0, "").await?;
        drop(writer);
        let mut encoded = Vec::new();
        reader.read_to_end(&mut encoded).await?;
        let frame = decode_server_line(&encoded)?;
        assert!(matches!(
            frame.message(),
            ServerMessage::TranscriptContent {
                fragment_index,
                final_fragment: true,
                content_fragment,
                ..
            } if fragment_index.value() == 0 && content_fragment.as_str().is_empty()
        ));
        Ok(())
    }

    #[test]
    fn every_persistence_terminal_call_disposition_has_a_wire_projection() {
        assert_eq!(
            wire_model_call_state(DispatchedModelCallState::CancellationRequested),
            ModelCallState::CancellationRequested {}
        );
        let cases = [
            (
                DispatchedModelCallDisposition::Completed,
                ModelCallDisposition::Completed,
            ),
            (
                DispatchedModelCallDisposition::KnownFailed,
                ModelCallDisposition::KnownFailed,
            ),
            (
                DispatchedModelCallDisposition::Refused,
                ModelCallDisposition::Refused,
            ),
            (
                DispatchedModelCallDisposition::Cancelled,
                ModelCallDisposition::Cancelled,
            ),
            (
                DispatchedModelCallDisposition::Ambiguous,
                ModelCallDisposition::Ambiguous,
            ),
        ];
        for (source, expected) in cases {
            assert_eq!(
                wire_model_call_state(DispatchedModelCallState::Terminal(source)),
                ModelCallState::Terminal {
                    disposition: expected
                }
            );
        }
    }

    #[test]
    fn cancellation_and_reconciliation_project_to_exact_wire_shapes() {
        let turn = TurnId::from_uuid(Uuid::from_u128(1));
        let attempt = TurnAttemptId::from_uuid(Uuid::from_u128(2));
        let call = ModelCallId::from_uuid(Uuid::from_u128(3));
        let entry = SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(4));
        let frontier = ContextFrontierId::from_uuid(Uuid::from_u128(5));

        assert_eq!(
            wire_turn_state(&ProcessTurnState::Cancelled {
                terminal_frontier: frontier,
                terminal_attempt: attempt,
                terminal_call: Some(call),
            }),
            TurnState::Cancelled {
                terminal_frontier_id: CanonicalUuid::from_uuid(frontier.into_uuid()),
                terminal_attempt_id: CanonicalUuid::from_uuid(attempt.into_uuid()),
                terminal_model_call_id: Some(CanonicalUuid::from_uuid(call.into_uuid())),
            }
        );
        assert_eq!(
            wire_turn_state(&ProcessTurnState::ReconciliationRequired {
                terminal_frontier: frontier,
                terminal_attempt: attempt,
                terminal_call: call,
            }),
            TurnState::ReconciliationRequired {
                terminal_frontier_id: CanonicalUuid::from_uuid(frontier.into_uuid()),
                terminal_attempt_id: CanonicalUuid::from_uuid(attempt.into_uuid()),
                terminal_model_call_id: CanonicalUuid::from_uuid(call.into_uuid()),
            }
        );

        let cancelled = ProcessUpdateEvent::from(&DispatchedOutboxEventKind::TurnCancelled {
            turn,
            cancellation_entry: entry,
            terminal_frontier: frontier,
        });
        assert_eq!(
            cancelled.wire(),
            SessionEvent::TurnCancelled {
                turn_id: CanonicalUuid::from_uuid(turn.into_uuid()),
                cancellation_entry_id: CanonicalUuid::from_uuid(entry.into_uuid()),
                terminal_frontier_id: CanonicalUuid::from_uuid(frontier.into_uuid()),
            }
        );
        let reconciliation =
            ProcessUpdateEvent::from(&DispatchedOutboxEventKind::TurnReconciliationRequired {
                turn,
                call,
                terminal_frontier: frontier,
            });
        assert_eq!(
            reconciliation.wire(),
            SessionEvent::TurnReconciliationRequired {
                turn_id: CanonicalUuid::from_uuid(turn.into_uuid()),
                model_call_id: CanonicalUuid::from_uuid(call.into_uuid()),
                terminal_frontier_id: CanonicalUuid::from_uuid(frontier.into_uuid()),
            }
        );
    }
}
