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
    ImportConversationError, ImportConversationOutcome, ImportConversationService,
    ImportedConversationConverter, InProcessEligibilityNudge, InProcessToolDispatchGate,
    ListSessionMetadataService, LoadSessionMetadataService, ReplaceSessionDefaultsOutcome,
    ReplaceSessionDefaultsRequest, ReplaceSessionDefaultsService, ReplaceSessionMetadataOutcome,
    ReplaceSessionMetadataRequest, ReplaceSessionMetadataService, SessionMetadataListItem,
    SessionMetadataListQuery, SubmitInputOutcome, SubmitInputRequest, SubmitInputService,
    SubmitInputTransaction, UuidV7ImportedConversationIdGenerator, UuidV7SessionIdGenerator,
    UuidV7SubmitInputIdGenerator,
};
use signalbox_conversation_import_claude_code::ClaudeCodeJsonlConverter;
use signalbox_conversation_import_codex::CodexRolloutJsonlConverter;
use signalbox_domain::{
    AcceptedInputId, Actor, CancelledModelCallTurnIdentities, DangerousToolAutoApproval,
    DeliveryRequest, DirectModelSelection, DurableCommandId, ModelAlias, ModelSelectionOverride,
    ModelSelectionRequest, PerInputConfigurationChoices, ReplaceSessionDefaultsRejectedResult,
    ReplaceSessionDefaultsResult, ReplaceSessionMetadataRejectedResult,
    ReplaceSessionMetadataResult, SessionConfigurationDefaults,
    SessionConfigurationDefaultsVersion, SessionId, SessionMetadataContent,
    SessionMetadataLastWriter, SessionMetadataSnapshot, SubmitInput, SubmitInputAppliedResult,
    SubmitInputRejectedResult, SubmitInputResult, TurnId, UserContent,
};
use signalbox_persistence::{
    conversation_import::{
        ImportedConversationIdentityCollision, ImportedConversationRepository,
        ImportedConversationRepositoryError,
    },
    create_session::{CreateSessionRepository, CreateSessionRepositoryError},
    outbox::{
        DispatchedModelCallDisposition, DispatchedModelCallState, DispatchedOutboxEvent,
        DispatchedOutboxEventKind, DispatchedReconciliationOperation, DispatchedToolBatchState,
        OutboxDeliveryDecision, OutboxDispatchError, OutboxDispatchOutcome, OutboxDispatcher,
    },
    process_read::{
        ProcessCurrentModelCallState, ProcessFailedModelCallDisposition,
        ProcessImportedContentKind, ProcessImportedSourceSpeaker,
        ProcessModelCallRecoveryPrecondition, ProcessModelSelection, ProcessReadError,
        ProcessReadRepository, ProcessReconciliationOperation, ProcessSessionAncestry,
        ProcessSessionDefaultsRead, ProcessTranscriptEntry, ProcessTranscriptItem,
        ProcessTranscriptTurn, ProcessTurnState,
    },
    replace_session_defaults::{
        ReplaceSessionDefaultsRepository, ReplaceSessionDefaultsRepositoryError,
    },
    session_metadata::{SessionMetadataRepository, SessionMetadataRepositoryError},
    submit_input::{SubmitInputHandlingOutcome, SubmitInputRepository, SubmitInputRepositoryError},
};
use signalbox_process_protocol::{
    CanonicalU64, CanonicalUuid, ClientRequest, ConversationImportFormat, CurrentModelCall,
    CurrentModelCallState, ErrorCode, ErrorDetail, FailedModelCallDisposition,
    FailedTerminalModelCall, FrameDecodeErrorKind, FrameEncodeError,
    IMPORTED_TRANSCRIPT_PROTOCOL_VERSION, ImportedContentKind, ImportedSourceSpeaker,
    ImportedSpeaker, InputContent, MAX_FRAME_BYTES, MetadataActor, MetadataLastWriter,
    ModelCallDisposition, ModelCallState, ModelSelection as WireModelSelection, ProtocolVersion,
    RejectionDetail, RequestId, SESSION_SYSTEM_PROMPT_PROTOCOL_VERSION, ServerFrame, ServerMessage,
    SessionEvent, SessionMetadata as WireSessionMetadata, SystemPromptMember, SystemPromptText,
    ToolBatchState, TranscriptEntry, TranscriptTextEntry, TurnState, content_fragments,
    decode_client_line, encode_server_line, recover_bounded_client_protocol_version,
    recover_bounded_client_request_id,
};
use sqlx::PgPool;
use tokio::{
    io::{
        AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncSeekExt, AsyncWrite, AsyncWriteExt,
        BufReader,
    },
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
const MAX_CONCURRENT_IMPORTS: usize = 1;
const INBOUND_READ_AHEAD_BYTES: usize = 8 * 1024;
const MAX_SUBMITTED_INPUT_BYTES: usize = 1024 * 1024;
const RESERVED_POOL_CONNECTIONS_OUTSIDE_SNAPSHOTS: u32 = 2;

#[derive(Clone, Debug)]
struct ConnectionServices {
    pool: PgPool,
    eligibility_nudge: InProcessEligibilityNudge,
    tool_dispatch_gate: InProcessToolDispatchGate,
    model_configuration: Arc<HubModelConfiguration>,
    updates: broadcast::Sender<ProcessUpdate>,
    inbound_frame_budget: Arc<Semaphore>,
    import_budget: Arc<Semaphore>,
    snapshot_reader_budget: Arc<Semaphore>,
}

/// The hub-owned local protocol runtime: one outbox dispatcher, one bounded
/// fan-out, and one guarded Unix listener.
#[derive(Debug)]
pub struct ProcessRuntime {
    listener: LocalProcessListener,
    pool: PgPool,
    eligibility_nudge: InProcessEligibilityNudge,
    tool_dispatch_gate: InProcessToolDispatchGate,
    model_configuration: HubModelConfiguration,
}

impl ProcessRuntime {
    /// Composes the guarded listener, fenced database, nudge, and static models.
    pub const fn new(
        listener: LocalProcessListener,
        pool: PgPool,
        eligibility_nudge: InProcessEligibilityNudge,
        tool_dispatch_gate: InProcessToolDispatchGate,
        model_configuration: HubModelConfiguration,
    ) -> Self {
        Self {
            listener,
            pool,
            eligibility_nudge,
            tool_dispatch_gate,
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
            self.tool_dispatch_gate,
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
    tool_dispatch_gate: InProcessToolDispatchGate,
    model_configuration: HubModelConfiguration,
    updates: broadcast::Sender<ProcessUpdate>,
    mut shutdown: watch::Receiver<bool>,
) -> Result<(), ProcessRuntimeError> {
    let snapshot_reader_capacity = snapshot_reader_capacity(pool.options().get_max_connections())
        .ok_or(ProcessRuntimeError::InsufficientPoolCapacity)?;
    let services = ConnectionServices {
        pool,
        eligibility_nudge,
        tool_dispatch_gate,
        model_configuration: Arc::new(model_configuration),
        updates,
        inbound_frame_budget: Arc::new(Semaphore::new(MAX_BUFFERED_INBOUND_FRAMES)),
        import_budget: Arc::new(Semaphore::new(MAX_CONCURRENT_IMPORTS)),
        snapshot_reader_budget: Arc::new(Semaphore::new(snapshot_reader_capacity)),
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
        Some(Ok(Err(ProcessConnectionError::PeerIo(error)))) => {
            drop(error);
            Ok(())
        }
        Some(Ok(Err(ProcessConnectionError::SpoolIo(error)))) => {
            Err(ProcessRuntimeError::SpoolIo(error))
        }
        Some(Ok(Err(ProcessConnectionError::Encode(FrameEncodeError::OversizedFrame)))) => Ok(()),
        Some(Ok(Err(ProcessConnectionError::Encode(error)))) => {
            Err(ProcessRuntimeError::Encode(error))
        }
        Some(Ok(Err(ProcessConnectionError::EncodeInvariant))) => {
            Err(ProcessRuntimeError::EncodeInvariant)
        }
        Some(Ok(Err(ProcessConnectionError::MessageRequiresVersion(_)))) => {
            Err(ProcessRuntimeError::EncodeInvariant)
        }
        Some(Ok(Err(ProcessConnectionError::InboundFrameBudgetClosed))) => {
            Err(ProcessRuntimeError::InboundFrameBudgetClosed)
        }
        Some(Ok(Err(ProcessConnectionError::SnapshotReaderBudgetClosed))) => {
            Err(ProcessRuntimeError::SnapshotReaderBudgetClosed)
        }
        Some(Ok(Err(ProcessConnectionError::ImportBudgetClosed))) => {
            Err(ProcessRuntimeError::ImportBudgetClosed)
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
    let mut reader = BufReader::with_capacity(INBOUND_READ_AHEAD_BYTES, reader);

    loop {
        if shutdown_requested(&shutdown) {
            return Ok(());
        }
        let Some(frame_buffer_permit) = acquire_inbound_frame_permit_after_input(
            &mut reader,
            Arc::clone(&services.inbound_frame_budget),
            &mut shutdown,
        )
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
                    let admitted_version = line
                        .strip_suffix(b"\n")
                        .and_then(recover_bounded_client_protocol_version);
                    let code = match error.kind() {
                        FrameDecodeErrorKind::UnsupportedVersion => ErrorCode::UnsupportedVersion,
                        FrameDecodeErrorKind::OversizedFrame
                        | FrameDecodeErrorKind::MalformedFrame => ErrorCode::MalformedFrame,
                    };
                    write_error(
                        &mut writer,
                        admitted_version.unwrap_or(ProtocolVersion::One),
                        error.request_id(),
                        ProtocolError::without_detail(code),
                    )
                    .await?;
                    return Ok(());
                }
            },
            IncomingLine::Oversized {
                request_id,
                admitted_version,
            } => {
                write_error(
                    &mut writer,
                    admitted_version.unwrap_or(ProtocolVersion::One),
                    request_id,
                    ProtocolError::without_detail(ErrorCode::MalformedFrame),
                )
                .await?;
                return Ok(());
            }
        };
        let (version, request_id, request) = frame.into_parts();
        let follows = matches!(request, ClientRequest::FollowSession { .. });
        let import_permit = if matches!(&request, ClientRequest::ImportConversation { .. }) {
            let Some(permit) =
                acquire_import_permit(Arc::clone(&services.import_budget), &mut shutdown).await?
            else {
                return Ok(());
            };
            Some(permit)
        } else {
            None
        };
        drop(frame_buffer_permit);
        handle_request(
            &mut writer,
            version,
            request_id,
            request,
            import_permit,
            &services,
            shutdown.clone(),
        )
        .await?;
        if follows {
            return Ok(());
        }
    }
}

async fn acquire_inbound_frame_permit_after_input<Reader>(
    reader: &mut Reader,
    budget: Arc<Semaphore>,
    shutdown: &mut watch::Receiver<bool>,
) -> Result<Option<OwnedSemaphorePermit>, ProcessConnectionError>
where
    Reader: AsyncBufRead + Unpin,
{
    let input_ready = tokio::select! {
        () = wait_for_shutdown(shutdown) => false,
        available = reader.fill_buf() => !available?.is_empty(),
    };
    if !input_ready {
        return Ok(None);
    }
    acquire_inbound_frame_permit(budget, shutdown).await
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

async fn acquire_snapshot_reader_permit(
    budget: Arc<Semaphore>,
    shutdown: &mut watch::Receiver<bool>,
) -> Result<Option<OwnedSemaphorePermit>, ProcessConnectionError> {
    tokio::select! {
        () = wait_for_shutdown(shutdown) => Ok(None),
        permit = budget.acquire_owned() => permit
            .map(Some)
            .map_err(|_| ProcessConnectionError::SnapshotReaderBudgetClosed),
    }
}

async fn acquire_import_permit(
    budget: Arc<Semaphore>,
    shutdown: &mut watch::Receiver<bool>,
) -> Result<Option<OwnedSemaphorePermit>, ProcessConnectionError> {
    tokio::select! {
        () = wait_for_shutdown(shutdown) => Ok(None),
        permit = budget.acquire_owned() => permit
            .map(Some)
            .map_err(|_| ProcessConnectionError::ImportBudgetClosed),
    }
}

fn snapshot_reader_capacity(max_pool_connections: u32) -> Option<usize> {
    let available =
        max_pool_connections.checked_sub(RESERVED_POOL_CONNECTIONS_OUTSIDE_SNAPSHOTS)?;
    if available == 0 {
        return None;
    }
    usize::try_from(available).ok()
}

async fn handle_request<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    request: ClientRequest,
    import_permit: Option<OwnedSemaphorePermit>,
    services: &ConnectionServices,
    mut shutdown: watch::Receiver<bool>,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    match request {
        ClientRequest::CreateSession {
            command_id,
            initial_model_selection,
            system_prompt,
        } => {
            handle_create_session(
                writer,
                version,
                request_id,
                command_id.into_uuid(),
                initial_model_selection,
                system_prompt,
                &services.pool,
            )
            .await
        }
        ClientRequest::ListSessions {} => {
            let Some(snapshot_permit) = acquire_snapshot_reader_permit(
                Arc::clone(&services.snapshot_reader_budget),
                &mut shutdown,
            )
            .await?
            else {
                return Ok(());
            };
            handle_list_sessions(writer, version, request_id, &services.pool, snapshot_permit).await
        }
        ClientRequest::SubmitInput {
            command_id,
            session_id,
            content,
            expected_defaults_version,
        } => {
            handle_submit_input(
                writer,
                version,
                request_id,
                command_id.into_uuid(),
                session_id,
                content,
                expected_defaults_version,
                &services.pool,
                &services.eligibility_nudge,
                &services.tool_dispatch_gate,
                services.model_configuration.as_ref(),
            )
            .await
        }
        ClientRequest::ReconcileTurn {
            command_id,
            session_id,
            expected_active_turn_id,
            content,
            expected_defaults_version,
        } => {
            handle_reconcile_turn(
                writer,
                version,
                request_id,
                command_id.into_uuid(),
                session_id,
                expected_active_turn_id,
                content,
                expected_defaults_version,
                &services.pool,
                &services.eligibility_nudge,
                &services.tool_dispatch_gate,
                services.model_configuration.as_ref(),
            )
            .await
        }
        ClientRequest::ReadTranscript { session_id } => {
            let Some(snapshot_permit) = acquire_snapshot_reader_permit(
                Arc::clone(&services.snapshot_reader_budget),
                &mut shutdown,
            )
            .await?
            else {
                return Ok(());
            };
            handle_read_transcript(
                writer,
                version,
                request_id,
                session_id,
                &services.pool,
                snapshot_permit,
            )
            .await
        }
        ClientRequest::FollowSession { session_id } => {
            let Some(snapshot_permit) = acquire_snapshot_reader_permit(
                Arc::clone(&services.snapshot_reader_budget),
                &mut shutdown,
            )
            .await?
            else {
                return Ok(());
            };
            handle_follow_session(
                writer,
                version,
                request_id,
                session_id,
                &services.pool,
                &services.updates,
                shutdown,
                snapshot_permit,
            )
            .await
        }
        ClientRequest::ListSessionMetadata {
            required_tags,
            title_contains,
            include_archived,
            page_size,
            after_session_id,
        } => {
            let Some(snapshot_permit) = acquire_snapshot_reader_permit(
                Arc::clone(&services.snapshot_reader_budget),
                &mut shutdown,
            )
            .await?
            else {
                return Ok(());
            };
            handle_list_session_metadata(
                writer,
                version,
                request_id,
                WireMetadataPageRequest {
                    required_tags,
                    title_contains,
                    include_archived,
                    page_size,
                    after_session_id,
                },
                &services.pool,
                snapshot_permit,
            )
            .await
        }
        ClientRequest::ReadSessionMetadata { session_id } => {
            handle_read_session_metadata(writer, version, request_id, session_id, &services.pool)
                .await
        }
        ClientRequest::ReplaceSessionMetadata {
            command_id,
            session_id,
            metadata,
        } => {
            handle_replace_session_metadata(
                writer,
                version,
                request_id,
                command_id.into_uuid(),
                session_id,
                metadata,
                &services.pool,
            )
            .await
        }
        ClientRequest::ReplaceSessionDefaults {
            command_id,
            session_id,
            expected_defaults_version,
            model_selection,
            dangerous_tool_auto_approval,
            system_prompt,
        } => {
            handle_replace_session_defaults(
                writer,
                version,
                request_id,
                command_id.into_uuid(),
                session_id,
                expected_defaults_version,
                model_selection,
                dangerous_tool_auto_approval,
                system_prompt,
                &services.pool,
                services.model_configuration.as_ref(),
            )
            .await
        }
        ClientRequest::ReadSessionDefaults {
            session_id,
            defaults_version,
        } => {
            handle_read_session_defaults(
                writer,
                version,
                request_id,
                session_id,
                defaults_version,
                &services.pool,
            )
            .await
        }
        ClientRequest::ImportConversation { format, source } => {
            let import_permit = import_permit.ok_or(ProcessConnectionError::ImportBudgetClosed)?;
            handle_import_conversation(
                writer,
                version,
                request_id,
                format,
                source.into_bytes(),
                &services.pool,
                import_permit,
            )
            .await
        }
    }
}

async fn handle_import_conversation<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    format: ConversationImportFormat,
    source: Vec<u8>,
    pool: &PgPool,
    import_permit: OwnedSemaphorePermit,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let outcome = match format {
        ConversationImportFormat::ClaudeCodeSessionJsonlV2 => {
            execute_import(ClaudeCodeJsonlConverter, source, pool.clone()).await
        }
        ConversationImportFormat::CodexRolloutJsonlV1 => {
            execute_import(CodexRolloutJsonlConverter, source, pool.clone()).await
        }
    };
    drop(import_permit);
    match outcome {
        Ok(ImportConversationOutcome::Inserted { conversation }) => {
            write_message(
                writer,
                version,
                request_id,
                ServerMessage::ConversationImportInserted {
                    imported_conversation_id: wire_uuid(conversation.into_uuid()),
                },
            )
            .await
        }
        Ok(ImportConversationOutcome::AlreadyImported { conversation }) => {
            write_message(
                writer,
                version,
                request_id,
                ServerMessage::ConversationImportAlreadyImported {
                    imported_conversation_id: wire_uuid(conversation.into_uuid()),
                },
            )
            .await
        }
        Err(OperationalImportError::InvalidSource) => {
            write_error(
                writer,
                version,
                request_id,
                ProtocolError::without_detail(ErrorCode::InvalidRequest),
            )
            .await
        }
        Err(OperationalImportError::Database) => {
            write_error(
                writer,
                version,
                request_id,
                ProtocolError::without_detail(ErrorCode::CommitAmbiguous),
            )
            .await
        }
        Err(OperationalImportError::Internal) => {
            write_error(
                writer,
                version,
                request_id,
                ProtocolError::without_detail(ErrorCode::Internal),
            )
            .await
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OperationalImportError {
    InvalidSource,
    Database,
    Internal,
}

async fn execute_import<Converter>(
    converter: Converter,
    source: Vec<u8>,
    pool: PgPool,
) -> Result<ImportConversationOutcome, OperationalImportError>
where
    Converter: ImportedConversationConverter + Send + 'static,
{
    let runtime = tokio::runtime::Handle::current();
    tokio::task::spawn_blocking(move || {
        runtime.block_on(async move {
            let mut service = ImportConversationService::new(
                UuidV7ImportedConversationIdGenerator,
                converter,
                ImportedConversationRepository::new(pool),
            );
            service.execute(&source).await.map_err(|error| match error {
                ImportConversationError::Conversion(_) => OperationalImportError::InvalidSource,
                ImportConversationError::Store(ImportedConversationRepositoryError::Database(
                    _,
                )) => OperationalImportError::Database,
                ImportConversationError::Store(
                    ImportedConversationRepositoryError::IdentityCollision(
                        ImportedConversationIdentityCollision::Conversation
                        | ImportedConversationIdentityCollision::TranscriptEntry,
                    )
                    | ImportedConversationRepositoryError::Corruption(_),
                )
                | ImportConversationError::ConverterIdentityMismatch { .. }
                | ImportConversationError::ConverterFormatMismatch { .. }
                | ImportConversationError::ConverterEntryIdentitySequenceMismatch
                | ImportConversationError::StoreSourceDigestMismatch { .. }
                | ImportConversationError::StoreInsertedIdentityMismatch { .. } => {
                    OperationalImportError::Internal
                }
            })
        })
    })
    .await
    .map_err(|_| OperationalImportError::Internal)?
}

async fn handle_create_session<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    command_id: uuid::Uuid,
    initial_model_selection: WireModelSelection,
    system_prompt: SystemPromptMember,
    pool: &PgPool,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let Ok(system_prompt) = domain_system_prompt(system_prompt) else {
        return write_error(
            writer,
            version,
            request_id,
            ProtocolError::without_detail(ErrorCode::InvalidRequest),
        )
        .await;
    };
    let request = CreateSessionRequest::try_new(
        DurableCommandId::from_uuid(command_id),
        SessionConfigurationDefaults::complete(
            domain_model_selection(initial_model_selection),
            DangerousToolAutoApproval::Disabled,
            system_prompt,
        ),
    );
    let Ok(request) = request else {
        return write_error(
            writer,
            version,
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
                version,
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
                version,
                request_id,
                ProtocolError::without_detail(ErrorCode::ConflictingReuse),
            )
            .await
        }
        Err(CreateSessionError::Transaction(CreateSessionRepositoryError::Database(_))) => {
            write_error(
                writer,
                version,
                request_id,
                ProtocolError::mutation_unavailable(false),
            )
            .await
        }
        Err(CreateSessionError::Transaction(CreateSessionRepositoryError::CommitAmbiguous(_))) => {
            write_error(
                writer,
                version,
                request_id,
                ProtocolError::mutation_unavailable(true),
            )
            .await
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
                version,
                request_id,
                ProtocolError::without_detail(ErrorCode::Internal),
            )
            .await
        }
    }
}

async fn handle_list_sessions<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    pool: &PgPool,
    snapshot_permit: OwnedSemaphorePermit,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let spool_result = spool_session_summaries(
        ProcessReadRepository::new(pool.clone()),
        version,
        request_id,
    )
    .await;
    drop(snapshot_permit);
    let mut spool = match spool_result {
        Ok(spool) => spool,
        Err(SessionListSpoolError::Read(error)) => {
            return write_process_read_error(writer, version, request_id, error).await;
        }
        Err(SessionListSpoolError::Spool(error)) => {
            return write_snapshot_spool_error(writer, version, request_id, error).await;
        }
    };
    write_spooled_file(writer, &mut spool.file).await
}

struct SessionListSpool {
    file: tokio::fs::File,
}

enum SessionListSpoolError {
    Read(ProcessReadError),
    Spool(SnapshotSpoolError),
}

#[derive(Debug)]
enum SnapshotSpoolError {
    Io(io::Error),
    MessageRequiresVersion(u64),
    Encode(FrameEncodeError),
    EncodeInvariant,
}

impl SnapshotSpoolError {
    fn from_connection(error: ProcessConnectionError) -> Self {
        match error {
            ProcessConnectionError::PeerIo(error) | ProcessConnectionError::SpoolIo(error) => {
                Self::Io(error)
            }
            ProcessConnectionError::MessageRequiresVersion(required) => {
                Self::MessageRequiresVersion(required)
            }
            ProcessConnectionError::Encode(error) => Self::Encode(error),
            ProcessConnectionError::EncodeInvariant
            | ProcessConnectionError::InboundFrameBudgetClosed
            | ProcessConnectionError::ImportBudgetClosed
            | ProcessConnectionError::SnapshotReaderBudgetClosed => Self::EncodeInvariant,
        }
    }
}

async fn write_snapshot_spool_error<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    error: SnapshotSpoolError,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    match error {
        SnapshotSpoolError::Io(error) => {
            tracing::warn!(error = %error, "process snapshot spooling failed before response");
            write_error(
                writer,
                version,
                request_id,
                ProtocolError::without_detail(ErrorCode::Unavailable),
            )
            .await
        }
        SnapshotSpoolError::MessageRequiresVersion(required) => {
            write_error(
                writer,
                version,
                request_id,
                ProtocolError::unsupported_version(required),
            )
            .await
        }
        SnapshotSpoolError::Encode(error) => Err(ProcessConnectionError::Encode(error)),
        SnapshotSpoolError::EncodeInvariant => Err(ProcessConnectionError::EncodeInvariant),
    }
}

async fn spool_session_summaries(
    repository: ProcessReadRepository,
    version: ProtocolVersion,
    request_id: RequestId,
) -> Result<SessionListSpool, SessionListSpoolError> {
    let mut reader = repository
        .open_session_summaries()
        .await
        .map_err(SessionListSpoolError::Read)?;
    let standard_file = tempfile::tempfile()
        .map_err(SnapshotSpoolError::Io)
        .map_err(SessionListSpoolError::Spool)?;
    let mut file = tokio::fs::File::from_std(standard_file);
    write_spool_message(
        &mut file,
        version,
        request_id,
        ServerMessage::SessionsStart {},
    )
    .await
    .map_err(SessionListSpoolError::Spool)?;
    while let Some(summary) = reader
        .next_summary()
        .await
        .map_err(SessionListSpoolError::Read)?
    {
        write_spool_message(
            &mut file,
            version,
            request_id,
            ServerMessage::SessionSummary {
                session_id: wire_uuid(summary.session().into_uuid()),
                defaults_version: CanonicalU64::new(summary.defaults_version()),
                model_selection: wire_model_selection(summary.model_selection()),
            },
        )
        .await
        .map_err(SessionListSpoolError::Spool)?;
    }
    let session_count = reader
        .summary_count()
        .ok_or(SnapshotSpoolError::EncodeInvariant)
        .map_err(SessionListSpoolError::Spool)?;
    write_spool_message(
        &mut file,
        version,
        request_id,
        ServerMessage::SessionsEnd {
            session_count: CanonicalU64::new(session_count),
        },
    )
    .await
    .map_err(SessionListSpoolError::Spool)?;
    file.flush()
        .await
        .map_err(SnapshotSpoolError::Io)
        .map_err(SessionListSpoolError::Spool)?;
    file.seek(SeekFrom::Start(0))
        .await
        .map_err(SnapshotSpoolError::Io)
        .map_err(SessionListSpoolError::Spool)?;
    Ok(SessionListSpool { file })
}

struct WireMetadataPageRequest {
    required_tags: Vec<String>,
    title_contains: Option<String>,
    include_archived: bool,
    page_size: CanonicalU64,
    after_session_id: Option<CanonicalUuid>,
}

async fn handle_list_session_metadata<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    request: WireMetadataPageRequest,
    pool: &PgPool,
    snapshot_permit: OwnedSemaphorePermit,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let query = SessionMetadataListQuery::try_new(
        request.required_tags,
        request.title_contains,
        request.include_archived,
        request.page_size.value(),
        request
            .after_session_id
            .map(|value| SessionId::from_uuid(value.into_uuid())),
    );
    let Ok(query) = query else {
        drop(snapshot_permit);
        return write_error(
            writer,
            version,
            request_id,
            ProtocolError::without_detail(ErrorCode::InvalidRequest),
        )
        .await;
    };
    let spool_result = spool_session_metadata_page(
        SessionMetadataRepository::new(pool.clone()),
        query,
        version,
        request_id,
    )
    .await;
    drop(snapshot_permit);
    let mut spool = match spool_result {
        Ok(spool) => spool,
        Err(MetadataPageSpoolError::Read(error)) => {
            return write_session_metadata_read_error(writer, version, request_id, error).await;
        }
        Err(MetadataPageSpoolError::Spool(error)) => {
            return write_snapshot_spool_error(writer, version, request_id, error).await;
        }
    };
    write_spooled_file(writer, &mut spool.file).await
}

enum MetadataPageSpoolError {
    Read(SessionMetadataRepositoryError),
    Spool(SnapshotSpoolError),
}

async fn spool_session_metadata_page(
    repository: SessionMetadataRepository,
    query: SessionMetadataListQuery,
    version: ProtocolVersion,
    request_id: RequestId,
) -> Result<SessionListSpool, MetadataPageSpoolError> {
    let mut page = ListSessionMetadataService::new(repository)
        .execute(query)
        .await
        .map_err(MetadataPageSpoolError::Read)?;
    let standard_file = tempfile::tempfile()
        .map_err(SnapshotSpoolError::Io)
        .map_err(MetadataPageSpoolError::Spool)?;
    let mut file = tokio::fs::File::from_std(standard_file);
    write_spool_message(
        &mut file,
        version,
        request_id,
        ServerMessage::SessionMetadataPageStart {},
    )
    .await
    .map_err(MetadataPageSpoolError::Spool)?;
    let mut session_count = 0_u64;
    while let Some(item) = page
        .next_item()
        .await
        .map_err(MetadataPageSpoolError::Read)?
    {
        let (title, tags, last_writer) = wire_list_metadata(&item)
            .ok_or(SnapshotSpoolError::EncodeInvariant)
            .map_err(MetadataPageSpoolError::Spool)?;
        write_spool_message(
            &mut file,
            version,
            request_id,
            ServerMessage::SessionMetadataSummary {
                session_id: wire_uuid(item.session().into_uuid()),
                defaults_version: CanonicalU64::new(item.defaults_version().as_u64()),
                model_selection: wire_domain_model_selection(item.model_selection()),
                dangerous_tool_auto_approval: matches!(
                    item.dangerous_tool_auto_approval(),
                    DangerousToolAutoApproval::ApproveAll
                ),
                title,
                tags,
                archived: item.archived(),
                last_writer,
            },
        )
        .await
        .map_err(MetadataPageSpoolError::Spool)?;
        session_count = session_count
            .checked_add(1)
            .ok_or(SnapshotSpoolError::EncodeInvariant)
            .map_err(MetadataPageSpoolError::Spool)?;
    }
    let next_after_session_id = page
        .next_after_session()
        .map(|session| wire_uuid(session.into_uuid()));
    write_spool_message(
        &mut file,
        version,
        request_id,
        ServerMessage::SessionMetadataPageEnd {
            session_count: CanonicalU64::new(session_count),
            next_after_session_id,
        },
    )
    .await
    .map_err(MetadataPageSpoolError::Spool)?;
    file.flush()
        .await
        .map_err(SnapshotSpoolError::Io)
        .map_err(MetadataPageSpoolError::Spool)?;
    file.seek(SeekFrom::Start(0))
        .await
        .map_err(SnapshotSpoolError::Io)
        .map_err(MetadataPageSpoolError::Spool)?;
    Ok(SessionListSpool { file })
}

async fn handle_read_session_metadata<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    session_id: CanonicalUuid,
    pool: &PgPool,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let service = LoadSessionMetadataService::new(SessionMetadataRepository::new(pool.clone()));
    match service
        .execute(SessionId::from_uuid(session_id.into_uuid()))
        .await
    {
        Ok(Some(snapshot)) => {
            let (metadata, last_writer) =
                wire_metadata_snapshot(&snapshot).ok_or(ProcessConnectionError::EncodeInvariant)?;
            write_message(
                writer,
                version,
                request_id,
                ServerMessage::SessionMetadata {
                    session_id,
                    metadata,
                    last_writer,
                },
            )
            .await
        }
        Ok(None) => {
            write_error(
                writer,
                version,
                request_id,
                ProtocolError::without_detail(ErrorCode::NotFound),
            )
            .await
        }
        Err(error) => write_session_metadata_read_error(writer, version, request_id, error).await,
    }
}

async fn handle_read_session_defaults<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    session_id: CanonicalUuid,
    defaults_version: Option<CanonicalU64>,
    pool: &PgPool,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let named_version = match defaults_version {
        None => None,
        Some(value) => match SessionConfigurationDefaultsVersion::try_from_u64(value.value()) {
            Some(version) => Some(version),
            None => {
                return write_error(
                    writer,
                    version,
                    request_id,
                    ProtocolError::without_detail(ErrorCode::InvalidRequest),
                )
                .await;
            }
        },
    };
    let repository = ProcessReadRepository::new(pool.clone());
    match repository
        .read_session_defaults(SessionId::from_uuid(session_id.into_uuid()), named_version)
        .await
    {
        Ok(ProcessSessionDefaultsRead::Read(read)) => {
            let system_prompt = match wire_system_prompt(read.defaults().system_prompt()) {
                Some(system_prompt) => system_prompt,
                None => return Err(ProcessConnectionError::EncodeInvariant),
            };
            write_message_via_spool(
                writer,
                version,
                request_id,
                ServerMessage::SessionDefaults {
                    session_id,
                    defaults_version: CanonicalU64::new(read.version().as_u64()),
                    model_selection: wire_domain_model_selection(read.defaults().model()),
                    dangerous_tool_auto_approval: matches!(
                        read.defaults().dangerous_tool_auto_approval(),
                        DangerousToolAutoApproval::ApproveAll
                    ),
                    system_prompt,
                },
            )
            .await
        }
        Ok(ProcessSessionDefaultsRead::SessionNotFound) => {
            write_error(
                writer,
                version,
                request_id,
                ProtocolError::without_detail(ErrorCode::NotFound),
            )
            .await
        }
        Ok(ProcessSessionDefaultsRead::VersionNotFound) => {
            write_error(
                writer,
                version,
                request_id,
                ProtocolError::defaults_epoch_not_found(),
            )
            .await
        }
        Err(error) => write_process_read_error(writer, version, request_id, error).await,
    }
}

async fn handle_replace_session_metadata<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    command_id: uuid::Uuid,
    session_id: CanonicalUuid,
    metadata: WireSessionMetadata,
    pool: &PgPool,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let replacement = SessionMetadataContent::try_new(
        metadata.title().map(str::to_owned),
        metadata.tags().map(str::to_owned).collect(),
        metadata
            .attributes()
            .map(|(key, value)| (key.to_owned(), value.to_owned()))
            .collect(),
        metadata.archived(),
    );
    let Ok(replacement) = replacement else {
        return write_error(
            writer,
            version,
            request_id,
            ProtocolError::without_detail(ErrorCode::InvalidRequest),
        )
        .await;
    };
    let request = ReplaceSessionMetadataRequest::try_new(
        DurableCommandId::from_uuid(command_id),
        SessionId::from_uuid(session_id.into_uuid()),
        replacement,
    );
    let Ok(request) = request else {
        return write_error(
            writer,
            version,
            request_id,
            ProtocolError::without_detail(ErrorCode::InvalidRequest),
        )
        .await;
    };
    let mut service =
        ReplaceSessionMetadataService::new(SessionMetadataRepository::new(pool.clone()));
    match service.execute(request).await {
        Ok(ReplaceSessionMetadataOutcome::Recorded(ReplaceSessionMetadataResult::Applied(
            applied,
        ))) => {
            let (metadata, last_writer) = wire_metadata_snapshot(applied.snapshot())
                .ok_or(ProcessConnectionError::EncodeInvariant)?;
            let last_writer = last_writer.ok_or(ProcessConnectionError::EncodeInvariant)?;
            write_message(
                writer,
                version,
                request_id,
                ServerMessage::SessionMetadataReplaced {
                    session_id,
                    metadata,
                    last_writer,
                },
            )
            .await
        }
        Ok(ReplaceSessionMetadataOutcome::Recorded(ReplaceSessionMetadataResult::Rejected(
            ReplaceSessionMetadataRejectedResult::SessionNotFound(rejected),
        ))) => {
            write_error(
                writer,
                version,
                request_id,
                ProtocolError::rejected(RejectionDetail::SessionNotFound {
                    session_id: wire_uuid(rejected.session().into_uuid()),
                }),
            )
            .await
        }
        Ok(ReplaceSessionMetadataOutcome::ConflictingReuse { .. }) => {
            write_error(
                writer,
                version,
                request_id,
                ProtocolError::without_detail(ErrorCode::ConflictingReuse),
            )
            .await
        }
        Err(SessionMetadataRepositoryError::Database(_)) => {
            write_error(
                writer,
                version,
                request_id,
                ProtocolError::mutation_unavailable(false),
            )
            .await
        }
        Err(SessionMetadataRepositoryError::CommitAmbiguous(_)) => {
            write_error(
                writer,
                version,
                request_id,
                ProtocolError::mutation_unavailable(true),
            )
            .await
        }
        Err(
            SessionMetadataRepositoryError::DifferentCommandKind { .. }
            | SessionMetadataRepositoryError::Corruption(_),
        ) => {
            write_error(
                writer,
                version,
                request_id,
                ProtocolError::without_detail(ErrorCode::Internal),
            )
            .await
        }
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "the complete defaults replacement remains explicit at the wire adapter"
)]
async fn handle_replace_session_defaults<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    command_id: uuid::Uuid,
    session_id: CanonicalUuid,
    expected_defaults_version: CanonicalU64,
    model_selection: WireModelSelection,
    dangerous_tool_auto_approval: bool,
    system_prompt: SystemPromptMember,
    pool: &PgPool,
    model_configuration: &HubModelConfiguration,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let Some(expected_version) =
        SessionConfigurationDefaultsVersion::try_from_u64(expected_defaults_version.value())
    else {
        return write_error(
            writer,
            version,
            request_id,
            ProtocolError::without_detail(ErrorCode::InvalidRequest),
        )
        .await;
    };
    let prompt_member_is_absent = system_prompt.value().is_none();
    let Ok(system_prompt) = domain_system_prompt(system_prompt) else {
        return write_error(
            writer,
            version,
            request_id,
            ProtocolError::without_detail(ErrorCode::InvalidRequest),
        )
        .await;
    };
    let replacement_model = domain_model_selection(model_selection);
    let replacement = SessionConfigurationDefaults::complete(
        replacement_model,
        if dangerous_tool_auto_approval {
            DangerousToolAutoApproval::ApproveAll
        } else {
            DangerousToolAutoApproval::Disabled
        },
        system_prompt,
    );
    let durable_command_id = DurableCommandId::from_uuid(command_id);
    let request = ReplaceSessionDefaultsRequest::try_new(
        durable_command_id,
        SessionId::from_uuid(session_id.into_uuid()),
        expected_version,
        replacement,
    );
    let Ok(request) = request else {
        return write_error(
            writer,
            version,
            request_id,
            ProtocolError::without_detail(ErrorCode::InvalidRequest),
        )
        .await;
    };
    let repository = ReplaceSessionDefaultsRepository::new(pool.clone());
    let command_is_claimed = match repository.load(durable_command_id).await {
        Ok(Some(_)) | Err(ReplaceSessionDefaultsRepositoryError::DifferentCommandKind { .. }) => {
            true
        }
        Ok(None) => false,
        Err(ReplaceSessionDefaultsRepositoryError::Database { .. }) => {
            return write_error(
                writer,
                version,
                request_id,
                ProtocolError::mutation_unavailable(false),
            )
            .await;
        }
        Err(ReplaceSessionDefaultsRepositoryError::Corruption(_)) => {
            return write_error(
                writer,
                version,
                request_id,
                ProtocolError::without_detail(ErrorCode::Internal),
            )
            .await;
        }
    };
    if !replacement_model_is_admitted(command_is_claimed, replacement_model, model_configuration) {
        return write_error(
            writer,
            version,
            request_id,
            ProtocolError::without_detail(ErrorCode::InvalidRequest),
        )
        .await;
    }
    // A replacement below version nine cannot carry the prompt member, so on
    // a session whose current epoch has a present prompt it would silently
    // clear a fact its version cannot represent. Gate it before any command
    // is recorded; a claimed identity replays its recorded result
    // unconditionally, and an absent session is left to the transaction's
    // recorded session_not_found.
    if !command_is_claimed
        && prompt_member_is_absent
        && version.as_u64() < SESSION_SYSTEM_PROMPT_PROTOCOL_VERSION
    {
        let read = ProcessReadRepository::new(pool.clone());
        match read
            .read_session_defaults(SessionId::from_uuid(session_id.into_uuid()), None)
            .await
        {
            Ok(ProcessSessionDefaultsRead::Read(current))
                if current.defaults().system_prompt().is_some() =>
            {
                return write_error(
                    writer,
                    version,
                    request_id,
                    ProtocolError::unsupported_version(SESSION_SYSTEM_PROMPT_PROTOCOL_VERSION),
                )
                .await;
            }
            Ok(
                ProcessSessionDefaultsRead::Read(_)
                | ProcessSessionDefaultsRead::SessionNotFound
                | ProcessSessionDefaultsRead::VersionNotFound,
            ) => {}
            Err(error) => {
                return write_process_read_error(writer, version, request_id, error).await;
            }
        }
    }
    let mut service = ReplaceSessionDefaultsService::new(repository);
    match service.execute(request).await {
        Ok(ReplaceSessionDefaultsOutcome::Recorded(ReplaceSessionDefaultsResult::Applied(
            applied,
        ))) => {
            let installed = applied.installed();
            let system_prompt = if version.as_u64() < SESSION_SYSTEM_PROMPT_PROTOCOL_VERSION {
                SystemPromptMember::absent()
            } else {
                SystemPromptMember::present(
                    wire_system_prompt(installed.defaults().system_prompt())
                        .ok_or(ProcessConnectionError::EncodeInvariant)?,
                )
            };
            write_message_via_spool(
                writer,
                version,
                request_id,
                ServerMessage::SessionDefaultsReplaced {
                    session_id,
                    defaults_version: CanonicalU64::new(installed.version().as_u64()),
                    model_selection: wire_domain_model_selection(installed.defaults().model()),
                    dangerous_tool_auto_approval: matches!(
                        installed.defaults().dangerous_tool_auto_approval(),
                        DangerousToolAutoApproval::ApproveAll
                    ),
                    system_prompt,
                },
            )
            .await
        }
        Ok(ReplaceSessionDefaultsOutcome::Recorded(ReplaceSessionDefaultsResult::Rejected(
            rejected,
        ))) => {
            let detail = match rejected {
                ReplaceSessionDefaultsRejectedResult::SessionNotFound(rejected) => {
                    RejectionDetail::SessionNotFound {
                        session_id: wire_uuid(rejected.session().into_uuid()),
                    }
                }
                ReplaceSessionDefaultsRejectedResult::CurrentVersionMismatch(rejected) => {
                    RejectionDetail::DefaultsVersionMismatch {
                        session_id: wire_uuid(rejected.session().into_uuid()),
                        expected: CanonicalU64::new(rejected.expected().as_u64()),
                        current: CanonicalU64::new(rejected.current().as_u64()),
                    }
                }
                ReplaceSessionDefaultsRejectedResult::VersionExhausted(rejected) => {
                    RejectionDetail::DefaultsVersionExhausted {
                        session_id: wire_uuid(rejected.session().into_uuid()),
                        current: CanonicalU64::new(rejected.current().as_u64()),
                    }
                }
            };
            write_error(writer, version, request_id, ProtocolError::rejected(detail)).await
        }
        Ok(ReplaceSessionDefaultsOutcome::ConflictingReuse { .. }) => {
            write_error(
                writer,
                version,
                request_id,
                ProtocolError::without_detail(ErrorCode::ConflictingReuse),
            )
            .await
        }
        Err(ReplaceSessionDefaultsRepositoryError::Database {
            commit_ambiguous, ..
        }) => {
            write_error(
                writer,
                version,
                request_id,
                ProtocolError::mutation_unavailable(commit_ambiguous),
            )
            .await
        }
        Err(
            ReplaceSessionDefaultsRepositoryError::DifferentCommandKind { .. }
            | ReplaceSessionDefaultsRepositoryError::Corruption(_),
        ) => {
            write_error(
                writer,
                version,
                request_id,
                ProtocolError::without_detail(ErrorCode::Internal),
            )
            .await
        }
    }
}

fn replacement_model_is_admitted(
    command_is_claimed: bool,
    replacement_model: ModelSelectionRequest,
    model_configuration: &HubModelConfiguration,
) -> bool {
    command_is_claimed
        || match replacement_model {
            ModelSelectionRequest::Direct(selection) => {
                model_configuration.contains_selection(selection)
            }
            ModelSelectionRequest::Alias(alias) => {
                model_configuration.resolve_alias(alias).is_some()
            }
        }
}

async fn write_session_metadata_read_error<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    error: SessionMetadataRepositoryError,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let code = match error {
        SessionMetadataRepositoryError::Database(_)
        | SessionMetadataRepositoryError::CommitAmbiguous(_) => ErrorCode::Unavailable,
        SessionMetadataRepositoryError::DifferentCommandKind { .. }
        | SessionMetadataRepositoryError::Corruption(_) => ErrorCode::Internal,
    };
    write_error(
        writer,
        version,
        request_id,
        ProtocolError::without_detail(code),
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

    async fn handle<NextTurn, NextToolCancellation>(
        &mut self,
        command: SubmitInput,
        accepted_input: AcceptedInputId,
        turn: Option<TurnId>,
        cancellation_identities: CancelledModelCallTurnIdentities,
        next_reclassified_turn: NextTurn,
        next_tool_cancellation: NextToolCancellation,
    ) -> Result<SubmitInputOutcome, Self::Error>
    where
        NextTurn: FnMut(AcceptedInputId) -> TurnId + Send,
        NextToolCancellation: FnMut(
                &[signalbox_domain::ToolRequestId],
            ) -> (
                Vec<signalbox_domain::SemanticTranscriptEntryId>,
                signalbox_domain::ContextFrontierId,
            ) + Send,
    {
        let outcome = self
            .repository
            .handle_with_candidates_and_alias_resolver(
                command,
                accepted_input,
                turn,
                cancellation_identities,
                next_reclassified_turn,
                next_tool_cancellation,
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
    version: ProtocolVersion,
    request_id: RequestId,
    command_id: uuid::Uuid,
    session_id: CanonicalUuid,
    content: InputContent,
    expected_defaults_version: CanonicalU64,
    pool: &PgPool,
    eligibility_nudge: &InProcessEligibilityNudge,
    tool_dispatch_gate: &InProcessToolDispatchGate,
    model_configuration: &HubModelConfiguration,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let session = SessionId::from_uuid(session_id.into_uuid());
    let command_id = DurableCommandId::from_uuid(command_id);
    let repository = SubmitInputRepository::new(pool.clone());
    let command_is_claimed = match repository.load(command_id).await {
        Ok(Some(_)) | Err(SubmitInputRepositoryError::DifferentCommandKind { .. }) => true,
        Ok(None) => false,
        Err(error) => {
            return write_submit_input_repository_error(writer, version, request_id, error).await;
        }
    };
    if !command_is_claimed {
        match selected_session_required_protocol_version(version, pool, session).await {
            Ok(Some(required_version)) => {
                return write_error(
                    writer,
                    version,
                    request_id,
                    ProtocolError::unsupported_version(required_version),
                )
                .await;
            }
            Ok(None) => {}
            Err(error) => {
                return write_process_read_error(writer, version, request_id, error).await;
            }
        }
    }
    let Some(expected_version) =
        SessionConfigurationDefaultsVersion::try_from_u64(expected_defaults_version.value())
    else {
        return write_error(
            writer,
            version,
            request_id,
            ProtocolError::without_detail(ErrorCode::InvalidRequest),
        )
        .await;
    };
    let Ok(content) = admitted_user_content(content) else {
        return write_error(
            writer,
            version,
            request_id,
            ProtocolError::without_detail(ErrorCode::InvalidRequest),
        )
        .await;
    };
    let request = SubmitInputRequest::try_new(
        command_id,
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
            version,
            request_id,
            ProtocolError::without_detail(ErrorCode::InvalidRequest),
        )
        .await;
    };
    run_submit_input(
        writer,
        version,
        request_id,
        session_id,
        request,
        repository,
        eligibility_nudge,
        tool_dispatch_gate,
        model_configuration,
    )
    .await
}

/// Reconciles the exact active turn parked on an ambiguous model call.
///
/// The parked turn's terminal disposition is proof-bearing, so the owner
/// supplies the interrupt authority the accepted lifecycle already defines and
/// the successor input the session continues with. The narrow precondition read
/// keeps this verb from becoming a general active-turn cancellation surface;
/// the authoritative transaction still revalidates the exact expected active
/// turn under the session lock.
#[expect(
    clippy::too_many_arguments,
    reason = "the closed reconciliation request is kept explicit at this wire-to-application adapter"
)]
async fn handle_reconcile_turn<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    command_id: uuid::Uuid,
    session_id: CanonicalUuid,
    expected_active_turn_id: CanonicalUuid,
    content: InputContent,
    expected_defaults_version: CanonicalU64,
    pool: &PgPool,
    eligibility_nudge: &InProcessEligibilityNudge,
    tool_dispatch_gate: &InProcessToolDispatchGate,
    model_configuration: &HubModelConfiguration,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let session = SessionId::from_uuid(session_id.into_uuid());
    let expected_active_turn = TurnId::from_uuid(expected_active_turn_id.into_uuid());
    let command_id = DurableCommandId::from_uuid(command_id);
    let repository = SubmitInputRepository::new(pool.clone());
    // A command identity that already names durable intent must reach the
    // replay boundary unconditionally (INV-012): the first handling already
    // released the wait, so re-applying the current-state precondition would
    // answer a retry of a committed decision with a refusal instead of its
    // recorded result.
    let command_is_claimed = match repository.load(command_id).await {
        Ok(Some(_)) | Err(SubmitInputRepositoryError::DifferentCommandKind { .. }) => true,
        Ok(None) => false,
        Err(error) => {
            return write_submit_input_repository_error(writer, version, request_id, error).await;
        }
    };
    let Some(expected_version) =
        SessionConfigurationDefaultsVersion::try_from_u64(expected_defaults_version.value())
    else {
        return write_error(
            writer,
            version,
            request_id,
            ProtocolError::without_detail(ErrorCode::InvalidRequest),
        )
        .await;
    };
    let Ok(content) = admitted_user_content(content) else {
        return write_error(
            writer,
            version,
            request_id,
            ProtocolError::without_detail(ErrorCode::InvalidRequest),
        )
        .await;
    };
    if !command_is_claimed {
        match ProcessReadRepository::new(pool.clone())
            .model_call_recovery_precondition(session)
            .await
        {
            // An absent session is left to the authoritative transaction, whose
            // recorded `SessionNotFound` the version-seven contract promises.
            Ok(ProcessModelCallRecoveryPrecondition::SessionAbsent) => {}
            Ok(ProcessModelCallRecoveryPrecondition::Parked { turn })
                if turn == expected_active_turn => {}
            Ok(
                ProcessModelCallRecoveryPrecondition::NoParkedTurn
                | ProcessModelCallRecoveryPrecondition::Parked { .. },
            ) => {
                // The claim probe and this read are separate statements, so an
                // equal-identity request that overlapped ours can have released
                // the wait in between. Rechecking the claim before refusing
                // keeps the loser of that race on the replay boundary instead
                // of answering a committed decision with a refusal (INV-012).
                match repository.load(command_id).await {
                    Ok(Some(_)) | Err(SubmitInputRepositoryError::DifferentCommandKind { .. }) => {}
                    Ok(None) => {
                        return write_error(
                            writer,
                            version,
                            request_id,
                            ProtocolError::rejected(
                                RejectionDetail::TurnNotAwaitingReconciliation {
                                    session_id,
                                    turn_id: expected_active_turn_id,
                                },
                            ),
                        )
                        .await;
                    }
                    Err(error) => {
                        return write_submit_input_repository_error(
                            writer, version, request_id, error,
                        )
                        .await;
                    }
                }
            }
            Err(error) => {
                return write_process_read_error(writer, version, request_id, error).await;
            }
        }
    }
    let request = SubmitInputRequest::try_new(
        command_id,
        session,
        content,
        DeliveryRequest::Interrupt {
            expected_active_turn,
            configuration: PerInputConfigurationChoices::new(
                expected_version,
                ModelSelectionOverride::UseSessionDefault,
            ),
        },
    );
    let Ok(request) = request else {
        return write_error(
            writer,
            version,
            request_id,
            ProtocolError::without_detail(ErrorCode::InvalidRequest),
        )
        .await;
    };
    run_submit_input(
        writer,
        version,
        request_id,
        session_id,
        request,
        repository,
        eligibility_nudge,
        tool_dispatch_gate,
        model_configuration,
    )
    .await
}

#[expect(
    clippy::too_many_arguments,
    reason = "the shared submit-input execution keeps its wire and application collaborators explicit"
)]
async fn run_submit_input<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    session_id: CanonicalUuid,
    request: SubmitInputRequest,
    repository: SubmitInputRepository,
    eligibility_nudge: &InProcessEligibilityNudge,
    tool_dispatch_gate: &InProcessToolDispatchGate,
    model_configuration: &HubModelConfiguration,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let mut service = SubmitInputService::new(
        UuidV7SubmitInputIdGenerator,
        ConfiguredSubmitInputTransaction {
            repository,
            model_configuration,
        },
        eligibility_nudge.clone(),
        tool_dispatch_gate.clone(),
    );
    match service.execute(request).await {
        Ok(SubmitInputOutcome::Recorded(SubmitInputResult::Applied(
            SubmitInputAppliedResult::TurnOrigin(result),
        ))) => {
            write_message(
                writer,
                version,
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
                version,
                request_id,
                ProtocolError::rejected(map_rejection(rejected)?),
            )
            .await
        }
        Ok(SubmitInputOutcome::ConflictingReuse { .. }) => {
            write_error(
                writer,
                version,
                request_id,
                ProtocolError::without_detail(ErrorCode::ConflictingReuse),
            )
            .await
        }
        Ok(SubmitInputOutcome::Recorded(SubmitInputResult::Applied(
            SubmitInputAppliedResult::PendingSteering(_),
        ))) => {
            write_error(
                writer,
                version,
                request_id,
                ProtocolError::without_detail(ErrorCode::Internal),
            )
            .await
        }
        Err(error) => write_submit_input_repository_error(writer, version, request_id, error).await,
    }
}

async fn write_submit_input_repository_error<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    error: SubmitInputRepositoryError,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let protocol_error = match error {
        SubmitInputRepositoryError::Database(_) => ProtocolError::mutation_unavailable(false),
        SubmitInputRepositoryError::CommitAmbiguous(_) => ProtocolError::mutation_unavailable(true),
        SubmitInputRepositoryError::ModelExecution(error) => match error.as_ref() {
            signalbox_persistence::model_execution::ModelCallRepositoryError::Database {
                commit_ambiguous,
                ..
            } => ProtocolError::mutation_unavailable(*commit_ambiguous),
            _ => ProtocolError::without_detail(ErrorCode::Internal),
        },
        SubmitInputRepositoryError::DifferentCommandKind { .. }
        | SubmitInputRepositoryError::AcceptedInputIdentityCollision { .. }
        | SubmitInputRepositoryError::Corruption(_) => {
            ProtocolError::without_detail(ErrorCode::Internal)
        }
    };
    write_error(writer, version, request_id, protocol_error).await
}

fn admitted_user_content(content: InputContent) -> Result<UserContent, ()> {
    let content = content.into_string();
    if content.len() > MAX_SUBMITTED_INPUT_BYTES {
        return Err(());
    }
    UserContent::try_text(content).map_err(|_| ())
}

async fn selected_session_required_protocol_version(
    version: ProtocolVersion,
    pool: &PgPool,
    session: SessionId,
) -> Result<Option<u64>, ProcessReadError> {
    if version.as_u64() >= ProtocolVersion::Six.as_u64() {
        return Ok(None);
    }
    let repository = ProcessReadRepository::new(pool.clone());
    let has_model_identity_history = repository
        .session_has_model_identity_history(session)
        .await?;
    let has_tool_history = if version.as_u64() < ProtocolVersion::Three.as_u64() {
        repository.session_has_tool_history(session).await?
    } else {
        false
    };
    let ancestry = if version.as_u64() < IMPORTED_TRANSCRIPT_PROTOCOL_VERSION {
        repository.session_ancestry(session).await?
    } else {
        None
    };
    Ok(required_protocol_version_for_selected_session(
        version,
        SelectedSessionRepresentationFacts {
            has_model_identity_history,
            has_tool_history,
            ancestry,
        },
    ))
}

fn required_protocol_version_for_selected_session(
    version: ProtocolVersion,
    facts: SelectedSessionRepresentationFacts,
) -> Option<u64> {
    if facts.has_model_identity_history && version.as_u64() < ProtocolVersion::Six.as_u64() {
        Some(ProtocolVersion::Six.as_u64())
    } else if facts.has_tool_history && version.as_u64() < ProtocolVersion::Three.as_u64() {
        Some(ProtocolVersion::Three.as_u64())
    } else if version == ProtocolVersion::One
        && matches!(
            facts.ancestry,
            Some(ProcessSessionAncestry::ImportedConversation)
        )
    {
        Some(IMPORTED_TRANSCRIPT_PROTOCOL_VERSION)
    } else {
        None
    }
}

#[derive(Clone, Copy)]
struct SelectedSessionRepresentationFacts {
    has_model_identity_history: bool,
    has_tool_history: bool,
    ancestry: Option<ProcessSessionAncestry>,
}

async fn handle_read_transcript<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    session_id: CanonicalUuid,
    pool: &PgPool,
    snapshot_permit: OwnedSemaphorePermit,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let selected_session = SessionId::from_uuid(session_id.into_uuid());
    match selected_session_required_protocol_version(version, pool, selected_session).await {
        Ok(Some(required_version)) => {
            return write_error(
                writer,
                version,
                request_id,
                ProtocolError::unsupported_version(required_version),
            )
            .await;
        }
        Ok(None) => {}
        Err(error) => {
            return write_process_read_error(writer, version, request_id, error).await;
        }
    }
    let spool_result = spool_transcript(
        ProcessReadRepository::new(pool.clone()),
        selected_session,
        version,
        request_id,
    )
    .await;
    drop(snapshot_permit);
    let spool = match spool_result {
        Ok(Some(spool)) => spool,
        Ok(None) => {
            return write_error(
                writer,
                version,
                request_id,
                ProtocolError::without_detail(ErrorCode::NotFound),
            )
            .await;
        }
        Err(TranscriptSpoolError::Read(error)) => {
            return write_process_read_error(writer, version, request_id, error).await;
        }
        Err(TranscriptSpoolError::Spool(error)) => {
            return write_snapshot_spool_error(writer, version, request_id, error).await;
        }
    };
    write_spooled_transcript(writer, spool).await.map(|_| ())
}

#[expect(
    clippy::too_many_arguments,
    reason = "the versioned follow stream keeps each protocol and runtime boundary explicit"
)]
async fn handle_follow_session<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    session_id: CanonicalUuid,
    pool: &PgPool,
    updates: &broadcast::Sender<ProcessUpdate>,
    mut shutdown: watch::Receiver<bool>,
    snapshot_permit: OwnedSemaphorePermit,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let selected_session = SessionId::from_uuid(session_id.into_uuid());
    match selected_session_required_protocol_version(version, pool, selected_session).await {
        Ok(Some(required_version)) => {
            return write_error(
                writer,
                version,
                request_id,
                ProtocolError::unsupported_version(required_version),
            )
            .await;
        }
        Ok(None) => {}
        Err(error) => {
            return write_process_read_error(writer, version, request_id, error).await;
        }
    }
    let mut subscription = updates.subscribe();
    let snapshot_result = run_until_shutdown(
        &mut shutdown,
        spool_transcript(
            ProcessReadRepository::new(pool.clone()),
            selected_session,
            version,
            request_id,
        ),
    )
    .await;
    drop(snapshot_permit);
    let Some(snapshot_result) = snapshot_result else {
        return Ok(());
    };
    let spool = match snapshot_result {
        Ok(Some(spool)) => spool,
        Ok(None) => {
            return run_until_shutdown(
                &mut shutdown,
                write_error(
                    writer,
                    version,
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
                write_process_read_error(writer, version, request_id, error),
            )
            .await
            .unwrap_or(Ok(()));
        }
        Err(TranscriptSpoolError::Spool(error)) => {
            return write_snapshot_spool_error(writer, version, request_id, error).await;
        }
    };
    let Some(snapshot_write) =
        run_until_shutdown(&mut shutdown, write_spooled_transcript(writer, spool)).await
    else {
        return Ok(());
    };
    let mut observed_cursor = snapshot_write?;

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
                        version,
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
        let message = ServerMessage::SessionEvent {
            cursor: CanonicalU64::new(update.cursor),
            session_id,
            event: update.event.wire(),
        };
        if version.as_u64() < message.minimum_protocol_version() {
            return run_until_shutdown(
                &mut shutdown,
                write_error(
                    writer,
                    version,
                    request_id,
                    ProtocolError::unsupported_version(message.minimum_protocol_version()),
                ),
            )
            .await
            .unwrap_or(Ok(()));
        }
        let Some(event_write) = run_until_shutdown(
            &mut shutdown,
            write_message(writer, version, request_id, message),
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
    Spool(SnapshotSpoolError),
}

async fn spool_transcript(
    repository: ProcessReadRepository,
    session: SessionId,
    version: ProtocolVersion,
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
        .map_err(SnapshotSpoolError::Io)
        .map_err(TranscriptSpoolError::Spool)?;
    let mut file = tokio::fs::File::from_std(standard_file);
    let session_id = wire_uuid(reader.session().into_uuid());
    let cursor = CanonicalU64::new(reader.cursor());
    write_spool_message(
        &mut file,
        version,
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
                write_transcript_turn(&mut file, version, request_id, &turn)
                    .await
                    .map_err(SnapshotSpoolError::from_connection)
                    .map_err(TranscriptSpoolError::Spool)?;
            }
            ProcessTranscriptItem::Entry(entry) => {
                write_transcript_entry(&mut file, version, request_id, &entry)
                    .await
                    .map_err(SnapshotSpoolError::from_connection)
                    .map_err(TranscriptSpoolError::Spool)?;
            }
        }
    }
    let summary = reader
        .summary()
        .ok_or(SnapshotSpoolError::EncodeInvariant)
        .map_err(TranscriptSpoolError::Spool)?;
    write_spool_message(
        &mut file,
        version,
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
        .map_err(SnapshotSpoolError::Io)
        .map_err(TranscriptSpoolError::Spool)?;
    file.seek(SeekFrom::Start(0))
        .await
        .map_err(SnapshotSpoolError::Io)
        .map_err(TranscriptSpoolError::Spool)?;
    Ok(Some(TranscriptSpool {
        file,
        cursor: summary.cursor(),
    }))
}

async fn write_spooled_transcript<Writer>(
    writer: &mut Writer,
    mut spool: TranscriptSpool,
) -> Result<u64, ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    write_spooled_file(writer, &mut spool.file).await?;
    Ok(spool.cursor)
}

async fn write_spooled_file<Writer>(
    writer: &mut Writer,
    file: &mut tokio::fs::File,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let mut buffer = [0_u8; 8 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .await
            .map_err(ProcessConnectionError::SpoolIo)?;
        if read == 0 {
            return Ok(());
        }
        writer
            .write_all(&buffer[..read])
            .await
            .map_err(ProcessConnectionError::PeerIo)?;
    }
}

async fn write_transcript_turn<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    turn: &ProcessTranscriptTurn,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    write_message(
        writer,
        version,
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
    version: ProtocolVersion,
    request_id: RequestId,
    entry: &ProcessTranscriptEntry,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    match entry {
        ProcessTranscriptEntry::ModelIdentityChanged {
            entry_index,
            source_session,
            entry,
            turn,
            defaults_version,
            selected,
        } => {
            write_message(
                writer,
                version,
                request_id,
                ServerMessage::TranscriptEntry {
                    entry_index: CanonicalU64::new(*entry_index),
                    source_session_id: wire_uuid(source_session.into_uuid()),
                    entry_id: wire_uuid(entry.into_uuid()),
                    entry: TranscriptEntry::ModelIdentityChanged {
                        turn_id: wire_uuid(turn.into_uuid()),
                        defaults_version: CanonicalU64::new(*defaults_version),
                        selected_model_id: wire_uuid(selected.into_uuid()),
                    },
                },
            )
            .await
        }
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
                version,
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
            write_content(writer, version, request_id, *entry_index, content).await
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
                version,
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
            write_content(writer, version, request_id, *entry_index, content).await
        }
        ProcessTranscriptEntry::AssistantToolUse {
            entry_index,
            source_session,
            entry,
            turn,
            model_call,
            request,
            name,
            arguments,
        } => {
            write_message(
                writer,
                version,
                request_id,
                ServerMessage::TranscriptEntry {
                    entry_index: CanonicalU64::new(*entry_index),
                    source_session_id: wire_uuid(source_session.into_uuid()),
                    entry_id: wire_uuid(entry.into_uuid()),
                    entry: TranscriptEntry::AssistantToolUse {
                        turn_id: wire_uuid(turn.into_uuid()),
                        model_call_id: wire_uuid(model_call.into_uuid()),
                        tool_request_id: wire_uuid(request.into_uuid()),
                        tool_name: name.clone(),
                        arguments: arguments.clone(),
                    },
                },
            )
            .await
        }
        ProcessTranscriptEntry::ToolExecutionResult {
            entry_index,
            source_session,
            entry,
            request,
            attempt,
            content,
        } => {
            write_message(
                writer,
                version,
                request_id,
                ServerMessage::TranscriptEntry {
                    entry_index: CanonicalU64::new(*entry_index),
                    source_session_id: wire_uuid(source_session.into_uuid()),
                    entry_id: wire_uuid(entry.into_uuid()),
                    entry: TranscriptEntry::ToolExecutionResult {
                        tool_request_id: wire_uuid(request.into_uuid()),
                        tool_attempt_id: wire_uuid(attempt.into_uuid()),
                        content: content.clone(),
                    },
                },
            )
            .await
        }
        ProcessTranscriptEntry::ToolDenied {
            entry_index,
            source_session,
            entry,
            request,
            content,
        } => {
            write_message(
                writer,
                version,
                request_id,
                ServerMessage::TranscriptEntry {
                    entry_index: CanonicalU64::new(*entry_index),
                    source_session_id: wire_uuid(source_session.into_uuid()),
                    entry_id: wire_uuid(entry.into_uuid()),
                    entry: TranscriptEntry::ToolDenied {
                        tool_request_id: wire_uuid(request.into_uuid()),
                        content: content.clone(),
                    },
                },
            )
            .await
        }
        ProcessTranscriptEntry::ToolClosed {
            entry_index,
            source_session,
            entry,
            request,
            content,
        } => {
            write_message(
                writer,
                version,
                request_id,
                ServerMessage::TranscriptEntry {
                    entry_index: CanonicalU64::new(*entry_index),
                    source_session_id: wire_uuid(source_session.into_uuid()),
                    entry_id: wire_uuid(entry.into_uuid()),
                    entry: TranscriptEntry::ToolClosed {
                        tool_request_id: wire_uuid(request.into_uuid()),
                        content: content.clone(),
                    },
                },
            )
            .await
        }
        ProcessTranscriptEntry::TurnFailed {
            entry_index,
            source_session,
            entry,
            turn,
        } => {
            write_message(
                writer,
                version,
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
                version,
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
                version,
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
        ProcessTranscriptEntry::ImportedText {
            entry_index,
            source_session,
            entry,
            imported_conversation,
            imported_entry,
            source_speaker,
            content,
        } => {
            write_message(
                writer,
                version,
                request_id,
                ServerMessage::TranscriptTextEntry {
                    entry_index: CanonicalU64::new(*entry_index),
                    source_session_id: wire_uuid(source_session.into_uuid()),
                    entry_id: wire_uuid(entry.into_uuid()),
                    entry: TranscriptTextEntry::Imported {
                        imported_conversation_id: wire_uuid(imported_conversation.into_uuid()),
                        imported_entry_id: wire_uuid(imported_entry.into_uuid()),
                        source_speaker: wire_imported_source_speaker(*source_speaker),
                    },
                },
            )
            .await?;
            write_content(writer, version, request_id, *entry_index, content).await
        }
        ProcessTranscriptEntry::Imported {
            entry_index,
            source_session,
            entry,
            imported_conversation,
            imported_entry,
            source_speaker,
            content_kind,
        } => {
            write_message(
                writer,
                version,
                request_id,
                ServerMessage::TranscriptEntry {
                    entry_index: CanonicalU64::new(*entry_index),
                    source_session_id: wire_uuid(source_session.into_uuid()),
                    entry_id: wire_uuid(entry.into_uuid()),
                    entry: TranscriptEntry::Imported {
                        imported_conversation_id: wire_uuid(imported_conversation.into_uuid()),
                        imported_entry_id: wire_uuid(imported_entry.into_uuid()),
                        source_speaker: wire_imported_source_speaker(*source_speaker),
                        content_kind: wire_imported_content_kind(*content_kind),
                    },
                },
            )
            .await
        }
    }
}

async fn write_content<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
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
            version,
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
        SubmitInputRejectedResult::ActiveTurnMismatch {
            session,
            expected_active_turn,
            actual_active_turn,
        } => RejectionDetail::ActiveTurnMismatch {
            session_id: wire_uuid(session.into_uuid()),
            expected_active_turn_id: wire_uuid(expected_active_turn.into_uuid()),
            active_turn_id: wire_uuid(actual_active_turn.into_uuid()),
        },
        SubmitInputRejectedResult::NoActiveTurn {
            session,
            expected_active_turn,
        } => RejectionDetail::NoActiveTurn {
            session_id: wire_uuid(session.into_uuid()),
            expected_active_turn_id: wire_uuid(expected_active_turn.into_uuid()),
        },
        SubmitInputRejectedResult::SafePointUnavailableWhileStopping { .. }
        | SubmitInputRejectedResult::InterruptAlreadyApplied { .. }
        | SubmitInputRejectedResult::InterruptUnavailableWhileAwaitingApproval { .. } => {
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

fn wire_domain_model_selection(selection: ModelSelectionRequest) -> WireModelSelection {
    match selection {
        ModelSelectionRequest::Direct(selection) => WireModelSelection::Direct {
            selection_id: wire_uuid(selection.into_uuid()),
        },
        ModelSelectionRequest::Alias(alias) => WireModelSelection::Alias {
            alias_id: wire_uuid(alias.into_uuid()),
        },
    }
}

/// Maps the presence-checked wire member into the domain's optional bounded
/// prompt. Frame validation already bounds the text; construction failure is
/// a fail-closed invalid request rather than a panic.
fn domain_system_prompt(
    member: SystemPromptMember,
) -> Result<Option<signalbox_domain::SessionSystemPrompt>, ()> {
    match member.value() {
        None | Some(None) => Ok(None),
        Some(Some(text)) => {
            signalbox_domain::SessionSystemPrompt::try_new(text.as_str().to_owned())
                .map(Some)
                .map_err(|_| ())
        }
    }
}

/// Maps the domain's optional bounded prompt onto the wire text type.
///
/// The domain admission is strictly at least as strict as the wire's, so a
/// `None` here is fail-closed encode-invariant evidence.
fn wire_system_prompt(
    prompt: Option<&signalbox_domain::SessionSystemPrompt>,
) -> Option<Option<SystemPromptText>> {
    match prompt {
        None => Some(None),
        Some(value) => SystemPromptText::try_new(value.as_str().to_owned())
            .ok()
            .map(Some),
    }
}

fn wire_list_metadata(
    item: &SessionMetadataListItem,
) -> Option<(Option<String>, Vec<String>, Option<MetadataLastWriter>)> {
    let last_writer = match item.last_writer() {
        Some(writer) => Some(wire_metadata_last_writer(writer)?),
        None => None,
    };
    Some((
        item.title().map(str::to_owned),
        item.tags().map(str::to_owned).collect(),
        last_writer,
    ))
}

fn wire_metadata_snapshot(
    snapshot: &SessionMetadataSnapshot,
) -> Option<(WireSessionMetadata, Option<MetadataLastWriter>)> {
    let content = snapshot.content();
    let metadata = WireSessionMetadata::try_new(
        content.title().map(str::to_owned),
        content.tags().map(str::to_owned).collect(),
        content
            .attributes()
            .map(|(key, value)| (key.to_owned(), value.to_owned()))
            .collect(),
        content.archived(),
    )
    .ok()?;
    let last_writer = match snapshot.last_writer() {
        Some(writer) => Some(wire_metadata_last_writer(writer)?),
        None => None,
    };
    Some((metadata, last_writer))
}

fn wire_metadata_last_writer(writer: SessionMetadataLastWriter) -> Option<MetadataLastWriter> {
    let actor = match writer.actor() {
        Actor::Owner => MetadataActor::Owner {},
        Actor::Recovery | Actor::Model { .. } | Actor::Tool { .. } => return None,
    };
    Some(MetadataLastWriter::new(
        CanonicalU64::new(writer.updated_at().as_unix_micros()),
        actor,
    ))
}

const fn wire_imported_source_speaker(
    source: ProcessImportedSourceSpeaker,
) -> ImportedSourceSpeaker {
    match source {
        ProcessImportedSourceSpeaker::NotAttested => ImportedSourceSpeaker::NotAttested {},
        ProcessImportedSourceSpeaker::AttestedAbsent => ImportedSourceSpeaker::AttestedAbsent {},
        ProcessImportedSourceSpeaker::User => ImportedSourceSpeaker::Attested {
            speaker: ImportedSpeaker::User,
        },
        ProcessImportedSourceSpeaker::Assistant => ImportedSourceSpeaker::Attested {
            speaker: ImportedSpeaker::Assistant,
        },
    }
}

const fn wire_imported_content_kind(kind: ProcessImportedContentKind) -> ImportedContentKind {
    match kind {
        ProcessImportedContentKind::SourceEvent => ImportedContentKind::SourceEvent,
        ProcessImportedContentKind::SourceMessageBlock => ImportedContentKind::SourceMessageBlock,
        ProcessImportedContentKind::Text => ImportedContentKind::Text,
        ProcessImportedContentKind::ToolCall => ImportedContentKind::ToolCall,
        ProcessImportedContentKind::ToolResult => ImportedContentKind::ToolResult,
        ProcessImportedContentKind::Thinking => ImportedContentKind::Thinking,
        ProcessImportedContentKind::RedactedThinking => ImportedContentKind::RedactedThinking,
        ProcessImportedContentKind::Document => ImportedContentKind::Document,
        ProcessImportedContentKind::MessageContentAbsent => {
            ImportedContentKind::MessageContentAbsent
        }
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
        ProcessTurnState::ActiveAwaitingToolApproval { request } => {
            TurnState::ActiveAwaitingToolApproval {
                tool_request_id: wire_uuid(request.into_uuid()),
            }
        }
        ProcessTurnState::ActiveAwaitingToolRecovery {
            ended_attempt,
            recovery_attempt,
        } => TurnState::ActiveAwaitingToolRecovery {
            ended_attempt_id: wire_uuid(ended_attempt.into_uuid()),
            recovery_tool_attempt_id: wire_uuid(recovery_attempt.into_uuid()),
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
            operation,
        } => match operation {
            ProcessReconciliationOperation::ModelCall(call) => TurnState::ReconciliationRequired {
                terminal_frontier_id: wire_uuid(terminal_frontier.into_uuid()),
                terminal_attempt_id: wire_uuid(terminal_attempt.into_uuid()),
                terminal_model_call_id: wire_uuid(call.into_uuid()),
            },
            ProcessReconciliationOperation::ToolAttempt(attempt) => {
                TurnState::ToolReconciliationRequired {
                    terminal_frontier_id: wire_uuid(terminal_frontier.into_uuid()),
                    terminal_attempt_id: wire_uuid(terminal_attempt.into_uuid()),
                    terminal_tool_attempt_id: wire_uuid(attempt.into_uuid()),
                }
            }
        },
    }
}

async fn write_process_read_error<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
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
    write_error(
        writer,
        version,
        request_id,
        ProtocolError::without_detail(code),
    )
    .await
}

async fn write_error<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    error: ProtocolError,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    write_message(
        writer,
        version,
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
    version: ProtocolVersion,
    request_id: RequestId,
    message: ServerMessage,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let minimum_protocol_version = message.minimum_protocol_version();
    if version.as_u64() < minimum_protocol_version {
        return Err(ProcessConnectionError::MessageRequiresVersion(
            minimum_protocol_version,
        ));
    }
    let frame = ServerFrame::try_new_for_version(version, request_id, message)
        .map_err(FrameEncodeError::Validation)?;
    let encoded = encode_server_line(&frame)?;
    writer.write_all(&encoded).await?;
    Ok(())
}

/// Writes one system-prompt-bearing message through a temporary-file spool.
///
/// A prompt response can approach the frame cap, so the direct
/// `write_message` path would retain the complete encoded frame while a peer
/// that stops reading blocks the write. Spooling first keeps per-connection
/// heap at fixed I/O buffers, mirroring the snapshot paths
/// (docs/spec/process-protocol.md).
async fn write_message_via_spool<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    message: ServerMessage,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let standard_file = tempfile::tempfile().map_err(ProcessConnectionError::SpoolIo)?;
    let mut file = tokio::fs::File::from_std(standard_file);
    write_message(&mut file, version, request_id, message).await?;
    file.flush()
        .await
        .map_err(ProcessConnectionError::SpoolIo)?;
    file.seek(SeekFrom::Start(0))
        .await
        .map_err(ProcessConnectionError::SpoolIo)?;
    write_spooled_file(writer, &mut file).await
}

async fn write_spool_message(
    writer: &mut tokio::fs::File,
    version: ProtocolVersion,
    request_id: RequestId,
    message: ServerMessage,
) -> Result<(), SnapshotSpoolError> {
    write_message(writer, version, request_id, message)
        .await
        .map_err(SnapshotSpoolError::from_connection)
}

enum IncomingLine {
    Complete(Vec<u8>),
    Oversized {
        request_id: RequestId,
        admitted_version: Option<ProtocolVersion>,
    },
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
                let (request_id, admitted_version) = if frame_len == MAX_FRAME_BYTES + 1 {
                    line.extend_from_slice(&available[..newline]);
                    (
                        recover_bounded_client_request_id(&line),
                        recover_bounded_client_protocol_version(&line),
                    )
                } else {
                    (RequestId::uncorrelated(), None)
                };
                reader.consume(consumed);
                return Ok(Some(IncomingLine::Oversized {
                    request_id,
                    admitted_version,
                }));
            }
            line.extend_from_slice(&available[..consumed]);
            reader.consume(consumed);
            return Ok(Some(IncomingLine::Complete(line)));
        }
        if line.len().saturating_add(available.len()) > MAX_FRAME_BYTES {
            let consumed = available.len();
            reader.consume(consumed);
            return Ok(Some(IncomingLine::Oversized {
                request_id: RequestId::uncorrelated(),
                admitted_version: None,
            }));
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
    const fn unsupported_version(required_version: u64) -> Self {
        Self {
            code: ErrorCode::UnsupportedVersion,
            message: match required_version {
                2 => "the selected session requires protocol version 2",
                3 => "the selected session requires protocol version 3",
                6 => "the selected session requires protocol version 6",
                9 => "the selected session requires protocol version 9",
                _ => "the protocol version is unsupported",
            },
            detail: ErrorDetail::none(),
        }
    }

    /// The selected session exists but the named immutable defaults epoch
    /// was never installed; the wire code remains the shared `not_found`.
    const fn defaults_epoch_not_found() -> Self {
        Self {
            code: ErrorCode::NotFound,
            message: "the requested defaults epoch was not found on the selected session",
            detail: ErrorDetail::none(),
        }
    }

    const fn without_detail(code: ErrorCode) -> Self {
        Self {
            code,
            message: match code {
                ErrorCode::MalformedFrame => "the protocol frame is malformed",
                ErrorCode::UnsupportedVersion => {
                    "the protocol version is unsupported; supported versions: 1, 2, 3, 4, 5, 6, 7, 9"
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
                ErrorCode::CommitAmbiguous => {
                    "the mutation commit is ambiguous; retry the exact command"
                }
                ErrorCode::Internal => "the request failed an internal integrity check",
            },
            detail: ErrorDetail::none(),
        }
    }

    const fn mutation_unavailable(commit_ambiguous: bool) -> Self {
        if commit_ambiguous {
            Self::without_detail(ErrorCode::CommitAmbiguous)
        } else {
            Self::without_detail(ErrorCode::Unavailable)
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
    ToolBatchTransition {
        turn: signalbox_domain::TurnId,
        producing_call: signalbox_domain::ModelCallId,
        state: DispatchedToolBatchState,
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
        operation: DispatchedReconciliationOperation,
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
            DispatchedOutboxEventKind::ToolBatchTransition {
                turn,
                producing_call,
                state,
            } => Self::ToolBatchTransition {
                turn: *turn,
                producing_call: *producing_call,
                state: *state,
            },
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
                operation,
                terminal_frontier,
            } => Self::TurnReconciliationRequired {
                turn: *turn,
                operation: *operation,
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
            Self::ToolBatchTransition {
                turn,
                producing_call,
                state,
            } => SessionEvent::ToolBatchTransition {
                turn_id: wire_uuid(turn.into_uuid()),
                model_call_id: wire_uuid(producing_call.into_uuid()),
                state: match state {
                    DispatchedToolBatchState::Proposed { frontier } => ToolBatchState::Proposed {
                        frontier_id: wire_uuid(frontier.into_uuid()),
                    },
                    DispatchedToolBatchState::ResultsProjected { frontier } => {
                        ToolBatchState::ResultsProjected {
                            frontier_id: wire_uuid(frontier.into_uuid()),
                        }
                    }
                    DispatchedToolBatchState::RecoveryRequired { attempt } => {
                        ToolBatchState::RecoveryRequired {
                            tool_attempt_id: wire_uuid(attempt.into_uuid()),
                        }
                    }
                },
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
                operation,
                terminal_frontier,
            } => match operation {
                DispatchedReconciliationOperation::ModelCall(call) => {
                    SessionEvent::TurnReconciliationRequired {
                        turn_id: wire_uuid(turn.into_uuid()),
                        model_call_id: wire_uuid(call.into_uuid()),
                        terminal_frontier_id: wire_uuid(terminal_frontier.into_uuid()),
                    }
                }
                DispatchedReconciliationOperation::ToolAttempt(attempt) => {
                    SessionEvent::TurnToolReconciliationRequired {
                        turn_id: wire_uuid(turn.into_uuid()),
                        tool_attempt_id: wire_uuid(attempt.into_uuid()),
                        terminal_frontier_id: wire_uuid(terminal_frontier.into_uuid()),
                    }
                }
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
    PeerIo(io::Error),
    SpoolIo(io::Error),
    MessageRequiresVersion(u64),
    Encode(FrameEncodeError),
    EncodeInvariant,
    InboundFrameBudgetClosed,
    ImportBudgetClosed,
    SnapshotReaderBudgetClosed,
}

impl From<io::Error> for ProcessConnectionError {
    fn from(error: io::Error) -> Self {
        Self::PeerIo(error)
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
            Self::PeerIo(_) => "the local process peer I/O failed",
            Self::SpoolIo(_) => "the local process snapshot spool I/O failed",
            Self::MessageRequiresVersion(_) => {
                "the local process message requires a newer protocol version"
            }
            Self::Encode(_) => "the local process connection could not encode a frame",
            Self::EncodeInvariant => {
                "the local process connection could not represent an internal value"
            }
            Self::InboundFrameBudgetClosed => {
                "the local process connection lost its inbound frame budget"
            }
            Self::ImportBudgetClosed => {
                "the local process connection lost its conversation import budget"
            }
            Self::SnapshotReaderBudgetClosed => {
                "the local process connection lost its snapshot reader budget"
            }
        })
    }
}

impl Error for ProcessConnectionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::PeerIo(error) | Self::SpoolIo(error) => Some(error),
            Self::Encode(error) => Some(error),
            Self::MessageRequiresVersion(_)
            | Self::EncodeInvariant
            | Self::InboundFrameBudgetClosed
            | Self::ImportBudgetClosed
            | Self::SnapshotReaderBudgetClosed => None,
        }
    }
}

/// Fatal local-process runtime failure.
#[derive(Debug)]
pub enum ProcessRuntimeError {
    /// The guarded listener could not accept a connection.
    Accept(io::Error),
    /// A completed snapshot spool could not be read for transmission.
    SpoolIo(io::Error),
    /// A server frame could not satisfy the closed wire contract.
    Encode(FrameEncodeError),
    /// Runtime-owned values could not be represented by the closed wire contract.
    EncodeInvariant,
    /// The runtime-owned aggregate inbound frame budget closed unexpectedly.
    InboundFrameBudgetClosed,
    /// The runtime-owned conversation-import budget closed unexpectedly.
    ImportBudgetClosed,
    /// The runtime-owned snapshot-reader budget closed unexpectedly.
    SnapshotReaderBudgetClosed,
    /// The application pool cannot reserve capacity outside snapshot reads.
    InsufficientPoolCapacity,
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
            Self::SpoolIo(_) => "the local process server could not read a snapshot spool",
            Self::Encode(_) => "the local process server could not encode a frame",
            Self::EncodeInvariant => {
                "the local process server could not represent an internal value"
            }
            Self::InboundFrameBudgetClosed => {
                "the local process server lost its inbound frame budget"
            }
            Self::ImportBudgetClosed => {
                "the local process server lost its conversation import budget"
            }
            Self::SnapshotReaderBudgetClosed => {
                "the local process server lost its snapshot reader budget"
            }
            Self::InsufficientPoolCapacity => {
                "the local process server cannot reserve database pool capacity"
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
            Self::SpoolIo(error) => Some(error),
            Self::Encode(error) => Some(error),
            Self::ConnectionTask(error) => Some(error),
            Self::Dispatch(error) => Some(error),
            Self::CleanupSocket(error) => Some(error),
            Self::EncodeInvariant
            | Self::InboundFrameBudgetClosed
            | Self::ImportBudgetClosed
            | Self::SnapshotReaderBudgetClosed
            | Self::InsufficientPoolCapacity
            | Self::UnexpectedDispatcherRetry => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        error::Error,
        io,
        sync::{Arc, mpsc},
        thread,
    };

    use signalbox_application::ImportedConversationConverter;
    use signalbox_domain::{
        ContextFrontierId, DirectModelSelection, ImportedConversation, ImportedConversationFormat,
        ImportedConversationId, ImportedTranscriptEntryId, ModelCallId, ModelSelectionRequest,
        SemanticTranscriptEntryId, SessionId, SubmitInputRejectedResult, ToolAttemptId,
        TurnAttemptId, TurnId,
    };
    use signalbox_process_protocol::{
        CanonicalU64, CanonicalUuid, ErrorCode, FrameEncodeError, ImportedContentKind,
        ImportedSourceSpeaker, ImportedSpeaker, InputContent, MAX_CONTENT_FRAGMENT_BYTES,
        ProtocolVersion, RejectionDetail, ServerFrame, ServerMessage, SessionEvent, ToolBatchState,
        TranscriptEntry, TranscriptTextEntry, TurnState, decode_server_line, encode_server_line,
    };
    use sqlx::postgres::PgPoolOptions;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt, BufReader, duplex},
        sync::{Semaphore, watch},
        time::{Duration, timeout},
    };
    use uuid::Uuid;

    use super::{
        INBOUND_READ_AHEAD_BYTES, IncomingLine, MAX_ACTIVE_CONNECTIONS,
        MAX_BUFFERED_INBOUND_FRAMES, MAX_CONCURRENT_IMPORTS, MAX_FRAME_BYTES,
        MAX_SUBMITTED_INPUT_BYTES, OperationalImportError, ProcessConnectionError,
        ProcessRuntimeError, ProcessUpdateEvent, ProtocolError,
        RESERVED_POOL_CONNECTIONS_OUTSIDE_SNAPSHOTS, RequestId, SelectedSessionRepresentationFacts,
        SnapshotSpoolError, acquire_import_permit, acquire_inbound_frame_permit,
        acquire_inbound_frame_permit_after_input, acquire_snapshot_reader_permit,
        admitted_user_content, execute_import, inspect_connection_completion, map_rejection,
        read_frame_line, replacement_model_is_admitted,
        required_protocol_version_for_selected_session, run_until_shutdown,
        snapshot_reader_capacity, wire_model_call_state, wire_turn_state, wire_uuid, write_content,
        write_snapshot_spool_error, write_transcript_entry,
    };
    use signalbox_persistence::{
        outbox::{
            DispatchedModelCallDisposition, DispatchedModelCallState, DispatchedOutboxEventKind,
            DispatchedReconciliationOperation, DispatchedToolBatchState,
        },
        process_read::{
            ProcessImportedContentKind, ProcessImportedSourceSpeaker,
            ProcessReconciliationOperation, ProcessSessionAncestry, ProcessTranscriptEntry,
            ProcessTurnState,
        },
    };
    use signalbox_process_protocol::{ModelCallDisposition, ModelCallState};

    fn server_error_code_and_message(message: &ServerMessage) -> (ErrorCode, &str) {
        match message {
            ServerMessage::Error { code, message, .. } => (*code, message),
            other => panic!("expected server error, observed {other:?}"),
        }
    }

    #[test]
    fn commit_ambiguity_selects_the_stable_process_error_code() {
        assert_eq!(
            ProtocolError::mutation_unavailable(false).code,
            ErrorCode::Unavailable
        );
        assert_eq!(
            ProtocolError::mutation_unavailable(true).code,
            ErrorCode::CommitAmbiguous
        );
        assert!(
            ProtocolError::unsupported_version(2)
                .message
                .contains("version 2")
        );
        assert!(
            ProtocolError::unsupported_version(3)
                .message
                .contains("version 3")
        );
        assert!(
            ProtocolError::unsupported_version(6)
                .message
                .contains("version 6")
        );
        assert!(
            ProtocolError::without_detail(ErrorCode::UnsupportedVersion)
                .message
                .contains("1, 2, 3, 4, 5, 6")
        );
    }

    /// INV-033 / INV-046: each retained protocol is gated by the first version
    /// that can represent the selected session's durable history.
    #[test]
    fn inv033_inv046_legacy_session_compatibility_requires_first_representable_version() {
        assert_eq!(
            required_protocol_version_for_selected_session(
                ProtocolVersion::One,
                SelectedSessionRepresentationFacts {
                    has_model_identity_history: false,
                    has_tool_history: false,
                    ancestry: Some(ProcessSessionAncestry::ImportedConversation),
                },
            ),
            Some(2)
        );
        assert_eq!(
            required_protocol_version_for_selected_session(
                ProtocolVersion::One,
                SelectedSessionRepresentationFacts {
                    has_model_identity_history: false,
                    has_tool_history: true,
                    ancestry: Some(ProcessSessionAncestry::OwnerInitiated),
                },
            ),
            Some(3),
            "tool history takes precedence over retained-version ancestry"
        );
        assert_eq!(
            required_protocol_version_for_selected_session(
                ProtocolVersion::Two,
                SelectedSessionRepresentationFacts {
                    has_model_identity_history: false,
                    has_tool_history: true,
                    ancestry: Some(ProcessSessionAncestry::OwnerInitiated),
                },
            ),
            Some(3),
            "tool history takes precedence over retained-version ancestry"
        );
        assert_eq!(
            required_protocol_version_for_selected_session(
                ProtocolVersion::Three,
                SelectedSessionRepresentationFacts {
                    has_model_identity_history: false,
                    has_tool_history: true,
                    ancestry: Some(ProcessSessionAncestry::ImportedConversation),
                },
            ),
            None
        );
        assert_eq!(
            required_protocol_version_for_selected_session(
                ProtocolVersion::Four,
                SelectedSessionRepresentationFacts {
                    has_model_identity_history: false,
                    has_tool_history: true,
                    ancestry: Some(ProcessSessionAncestry::ImportedConversation),
                },
            ),
            None
        );
        assert_eq!(
            required_protocol_version_for_selected_session(
                ProtocolVersion::Two,
                SelectedSessionRepresentationFacts {
                    has_model_identity_history: false,
                    has_tool_history: false,
                    ancestry: Some(ProcessSessionAncestry::ImportedConversation),
                },
            ),
            None
        );
        assert_eq!(
            required_protocol_version_for_selected_session(
                ProtocolVersion::Five,
                SelectedSessionRepresentationFacts {
                    has_model_identity_history: true,
                    has_tool_history: false,
                    ancestry: Some(ProcessSessionAncestry::OwnerInitiated),
                },
            ),
            Some(6)
        );
        assert_eq!(
            required_protocol_version_for_selected_session(
                ProtocolVersion::One,
                SelectedSessionRepresentationFacts {
                    has_model_identity_history: true,
                    has_tool_history: true,
                    ancestry: Some(ProcessSessionAncestry::ImportedConversation),
                },
            ),
            Some(6),
            "model-identity history requires the newest retained representation"
        );
        assert_eq!(
            required_protocol_version_for_selected_session(
                ProtocolVersion::Six,
                SelectedSessionRepresentationFacts {
                    has_model_identity_history: true,
                    has_tool_history: true,
                    ancestry: Some(ProcessSessionAncestry::ImportedConversation),
                },
            ),
            None
        );
    }

    /// INV-033: a reconciliation decision that lost its race to another
    /// decision reaches the wire as its recorded typed rejection, not as an
    /// encode invariant that closes the connection.
    #[test]
    fn inv033_racing_reconciliation_rejections_have_wire_projections() -> Result<(), Box<dyn Error>>
    {
        let session = SessionId::from_uuid(uuid::Uuid::from_u128(1));
        let expected_active_turn = TurnId::from_uuid(uuid::Uuid::from_u128(2));
        let actual_active_turn = TurnId::from_uuid(uuid::Uuid::from_u128(3));

        assert_eq!(
            map_rejection(SubmitInputRejectedResult::NoActiveTurn {
                session,
                expected_active_turn,
            })?,
            RejectionDetail::NoActiveTurn {
                session_id: wire_uuid(session.into_uuid()),
                expected_active_turn_id: wire_uuid(expected_active_turn.into_uuid()),
            }
        );
        assert_eq!(
            map_rejection(SubmitInputRejectedResult::ActiveTurnMismatch {
                session,
                expected_active_turn,
                actual_active_turn,
            })?,
            RejectionDetail::ActiveTurnMismatch {
                session_id: wire_uuid(session.into_uuid()),
                expected_active_turn_id: wire_uuid(expected_active_turn.into_uuid()),
                active_turn_id: wire_uuid(actual_active_turn.into_uuid()),
            }
        );
        Ok(())
    }

    #[test]
    fn recorded_defaults_replay_does_not_depend_on_the_current_catalog()
    -> Result<(), Box<dyn Error>> {
        let current_catalog = crate::HubModelConfiguration::parse(
            r#"
version = 1

[[models]]
selection_id = "00000000-0000-0000-0000-000000000001"
target_id = "00000000-0000-0000-0000-000000000002"
provider = "anthropic"
provider_model = "still-current"
max_output_tokens = 256
"#,
        )?;
        let removed_selection =
            ModelSelectionRequest::Direct(DirectModelSelection::from_uuid(Uuid::from_u128(4)));

        assert!(replacement_model_is_admitted(
            true,
            removed_selection,
            &current_catalog,
        ));
        assert!(!replacement_model_is_admitted(
            false,
            removed_selection,
            &current_catalog,
        ));
        Ok(())
    }

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
            Some(IncomingLine::Oversized {
                request_id,
                admitted_version: None,
            }) if request_id.value() == 0
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
            Some(IncomingLine::Oversized {
                request_id,
                admitted_version: Some(ProtocolVersion::One),
            }) if request_id.value() == 9
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

    #[tokio::test]
    async fn idle_reader_does_not_reserve_an_inbound_frame_slot() -> Result<(), Box<dyn Error>> {
        assert_eq!(
            MAX_ACTIVE_CONNECTIONS * INBOUND_READ_AHEAD_BYTES,
            1024 * 1024
        );
        let budget = Arc::new(Semaphore::new(1));
        let (mut client, server) = duplex(8);
        let mut reader = BufReader::new(server);
        let (_shutdown, mut shutdown_receiver) = watch::channel(false);
        let acquire = acquire_inbound_frame_permit_after_input(
            &mut reader,
            Arc::clone(&budget),
            &mut shutdown_receiver,
        );
        tokio::pin!(acquire);

        assert!(
            timeout(Duration::from_millis(20), &mut acquire)
                .await
                .is_err()
        );
        assert_eq!(budget.available_permits(), 1);

        client.write_all(b"{").await?;
        let permit = timeout(Duration::from_secs(1), &mut acquire)
            .await??
            .ok_or_else(|| io::Error::other("ready input must acquire a frame slot"))?;
        assert_eq!(budget.available_permits(), 0);
        drop(permit);
        Ok(())
    }

    #[tokio::test]
    async fn snapshot_reader_budget_reserves_two_pool_connections() -> Result<(), Box<dyn Error>> {
        let max_pool_connections = 10;
        let capacity = snapshot_reader_capacity(max_pool_connections)
            .ok_or_else(|| io::Error::other("the production pool must admit snapshot readers"))?;
        assert_eq!(
            capacity,
            usize::try_from(max_pool_connections - RESERVED_POOL_CONNECTIONS_OUTSIDE_SNAPSHOTS)?
        );
        assert!(snapshot_reader_capacity(2).is_none());

        let budget = Arc::new(Semaphore::new(capacity));
        let (shutdown, shutdown_receiver) = watch::channel(false);
        let mut permits = Vec::new();
        for _ in 0..capacity {
            permits.push(
                acquire_snapshot_reader_permit(Arc::clone(&budget), &mut shutdown_receiver.clone())
                    .await?
                    .ok_or_else(|| io::Error::other("the running fixture must acquire a permit"))?,
            );
        }
        assert!(
            timeout(
                Duration::from_millis(20),
                acquire_snapshot_reader_permit(Arc::clone(&budget), &mut shutdown_receiver.clone()),
            )
            .await
            .is_err(),
            "the next snapshot reader must leave two pool slots free"
        );

        shutdown.send(true)?;
        assert!(
            acquire_snapshot_reader_permit(Arc::clone(&budget), &mut shutdown_receiver.clone())
                .await?
                .is_none()
        );
        Ok(())
    }

    #[tokio::test]
    async fn import_budget_admits_one_retained_aggregate_at_a_time() -> Result<(), Box<dyn Error>> {
        assert_eq!(MAX_CONCURRENT_IMPORTS, 1);
        let budget = Arc::new(Semaphore::new(MAX_CONCURRENT_IMPORTS));
        let (_shutdown, shutdown_receiver) = watch::channel(false);
        let first = acquire_import_permit(Arc::clone(&budget), &mut shutdown_receiver.clone())
            .await?
            .ok_or_else(|| io::Error::other("the first import must acquire its permit"))?;

        assert!(
            timeout(
                Duration::from_millis(20),
                acquire_import_permit(Arc::clone(&budget), &mut shutdown_receiver.clone()),
            )
            .await
            .is_err(),
            "a second retained import aggregate must wait"
        );

        drop(first);
        let second = timeout(
            Duration::from_secs(1),
            acquire_import_permit(Arc::clone(&budget), &mut shutdown_receiver.clone()),
        )
        .await??
        .ok_or_else(|| io::Error::other("the released import permit must be acquired"))?;
        drop(second);
        Ok(())
    }

    #[tokio::test]
    async fn import_conversion_runs_off_the_async_worker() -> Result<(), Box<dyn Error>> {
        let async_worker = thread::current().id();
        let (thread_sender, thread_receiver) = mpsc::sync_channel(1);
        let pool = PgPoolOptions::new()
            .connect_lazy("postgresql://signalbox:fixture@127.0.0.1/signalbox")?;

        let outcome = execute_import(
            ThreadReportingRejectConverter(thread_sender),
            Vec::new(),
            pool,
        )
        .await;
        let conversion_worker = thread_receiver.recv_timeout(Duration::from_secs(1))?;

        assert_eq!(outcome, Err(OperationalImportError::InvalidSource));
        assert_ne!(conversion_worker, async_worker);
        Ok(())
    }

    struct ThreadReportingRejectConverter(mpsc::SyncSender<thread::ThreadId>);

    impl ImportedConversationConverter for ThreadReportingRejectConverter {
        type Error = io::Error;

        fn format(&self) -> ImportedConversationFormat {
            ImportedConversationFormat::CodexRolloutJsonlV1
        }

        fn convert<NextEntryId>(
            &mut self,
            _conversation: ImportedConversationId,
            _source: &[u8],
            _next_entry_id: NextEntryId,
        ) -> Result<ImportedConversation, Self::Error>
        where
            NextEntryId: FnMut() -> ImportedTranscriptEntryId,
        {
            self.0
                .send(thread::current().id())
                .map_err(|_| io::Error::other("the test thread receiver closed"))?;
            Err(io::Error::other("fixture conversion rejection"))
        }
    }

    #[test]
    fn process_submission_admits_the_exact_content_bound() {
        let exact = InputContent::new("\u{1}".repeat(MAX_SUBMITTED_INPUT_BYTES));
        assert!(admitted_user_content(exact).is_ok());
    }

    #[test]
    fn process_submission_rejects_content_over_the_bound() {
        assert!(
            admitted_user_content(InputContent::new("x".repeat(MAX_SUBMITTED_INPUT_BYTES + 1)))
                .is_err()
        );
    }

    #[test]
    fn accepted_input_bound_keeps_snapshot_projection_representable() -> Result<(), Box<dyn Error>>
    {
        let frame = ServerFrame::try_new(
            RequestId::try_new(u64::MAX)?,
            ServerMessage::TranscriptTurn {
                turn_id: CanonicalUuid::from_uuid(Uuid::from_u128(u128::MAX)),
                acceptance_position: CanonicalU64::new(u64::MAX),
                state: TurnState::Queued {
                    accepted_input_id: CanonicalUuid::from_uuid(Uuid::from_u128(u128::MAX - 1)),
                    content: InputContent::new("\u{1}".repeat(MAX_SUBMITTED_INPUT_BYTES)),
                },
            },
        )?;

        assert!(encode_server_line(&frame)?.len() <= MAX_FRAME_BYTES);
        Ok(())
    }

    #[test]
    fn accepted_input_bound_keeps_update_projection_representable() -> Result<(), Box<dyn Error>> {
        let frame = ServerFrame::try_new(
            RequestId::try_new(u64::MAX)?,
            ServerMessage::SessionEvent {
                cursor: CanonicalU64::new(u64::MAX),
                session_id: CanonicalUuid::from_uuid(Uuid::from_u128(u128::MAX)),
                event: SessionEvent::InputAccepted {
                    accepted_input_id: CanonicalUuid::from_uuid(Uuid::from_u128(u128::MAX - 1)),
                    turn_id: CanonicalUuid::from_uuid(Uuid::from_u128(u128::MAX - 2)),
                    acceptance_position: CanonicalU64::new(u64::MAX),
                    content: InputContent::new("\u{1}".repeat(MAX_SUBMITTED_INPUT_BYTES)),
                },
            },
        )?;

        assert!(encode_server_line(&frame)?.len() <= MAX_FRAME_BYTES);
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

    #[test]
    fn peer_io_failure_does_not_fail_the_runtime() {
        assert!(
            inspect_connection_completion(Some(Ok(Err(ProcessConnectionError::PeerIo(
                io::Error::new(io::ErrorKind::BrokenPipe, "fixture peer closed")
            )))))
            .is_ok()
        );
    }

    #[test]
    fn spool_read_failure_is_fatal_runtime_evidence() {
        let result = inspect_connection_completion(Some(Ok(Err(ProcessConnectionError::SpoolIo(
            io::Error::other("fixture spool read"),
        )))));

        assert!(matches!(result, Err(ProcessRuntimeError::SpoolIo(_))));
    }

    #[tokio::test]
    async fn pre_response_spool_io_is_reported_as_unavailable() -> Result<(), Box<dyn Error>> {
        let request_id = RequestId::try_new(9)?;
        let (mut writer, mut reader) = duplex(1_024);

        write_snapshot_spool_error(
            &mut writer,
            ProtocolVersion::One,
            request_id,
            SnapshotSpoolError::Io(io::Error::other("fixture spool write")),
        )
        .await?;
        drop(writer);
        let mut encoded = Vec::new();
        reader.read_to_end(&mut encoded).await?;

        let frame = decode_server_line(&encoded)?;
        assert!(matches!(
            frame.message(),
            ServerMessage::Error {
                code: ErrorCode::Unavailable,
                ..
            }
        ));
        Ok(())
    }

    #[tokio::test]
    async fn spool_version_race_reports_the_exact_required_version() -> Result<(), Box<dyn Error>> {
        let request_id = RequestId::try_new(10)?;
        let (mut writer, mut reader) = duplex(1_024);
        let source_session = SessionId::from_uuid(Uuid::from_u128(1));
        let model_identity = ProcessTranscriptEntry::ModelIdentityChanged {
            entry_index: 0,
            source_session,
            entry: SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(2)),
            turn: TurnId::from_uuid(Uuid::from_u128(3)),
            defaults_version: 2,
            selected: DirectModelSelection::from_uuid(Uuid::from_u128(4)),
        };

        let spool_error = write_transcript_entry(
            &mut writer,
            ProtocolVersion::Five,
            request_id,
            &model_identity,
        )
        .await
        .expect_err("a version-five spool cannot encode a model-identity boundary");
        write_snapshot_spool_error(
            &mut writer,
            ProtocolVersion::Five,
            request_id,
            SnapshotSpoolError::from_connection(spool_error),
        )
        .await?;
        drop(writer);
        let mut encoded = Vec::new();
        reader.read_to_end(&mut encoded).await?;

        let frame = decode_server_line(&encoded)?;
        let expected = ProtocolError::unsupported_version(6);
        assert_eq!(
            server_error_code_and_message(frame.message()),
            (expected.code, expected.message)
        );
        Ok(())
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
        write_content(&mut writer, ProtocolVersion::One, request_id, 3, &text).await?;
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
        write_content(&mut writer, ProtocolVersion::One, request_id, 0, "").await?;
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

    #[tokio::test]
    async fn s28_imported_entries_map_only_to_version_two_conservative_shapes()
    -> Result<(), Box<dyn Error>> {
        let request_id = RequestId::try_new(11)?;
        let source_session = SessionId::from_uuid(Uuid::from_u128(1));
        let conversation = ImportedConversationId::from_uuid(Uuid::from_u128(2));
        let imported_entry = ImportedTranscriptEntryId::from_uuid(Uuid::from_u128(3));
        let semantic_entry = SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(4));
        let (mut writer, mut reader) = duplex(4_096);

        write_transcript_entry(
            &mut writer,
            ProtocolVersion::Two,
            request_id,
            &ProcessTranscriptEntry::ImportedText {
                entry_index: 0,
                source_session,
                entry: semantic_entry,
                imported_conversation: conversation,
                imported_entry,
                source_speaker: ProcessImportedSourceSpeaker::User,
                content: String::from("source-attested"),
            },
        )
        .await?;
        write_transcript_entry(
            &mut writer,
            ProtocolVersion::Two,
            request_id,
            &ProcessTranscriptEntry::Imported {
                entry_index: 1,
                source_session,
                entry: SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(5)),
                imported_conversation: conversation,
                imported_entry: ImportedTranscriptEntryId::from_uuid(Uuid::from_u128(6)),
                source_speaker: ProcessImportedSourceSpeaker::NotAttested,
                content_kind: ProcessImportedContentKind::ToolResult,
            },
        )
        .await?;
        drop(writer);

        let mut encoded = Vec::new();
        reader.read_to_end(&mut encoded).await?;
        let mut lines = encoded.split_inclusive(|byte| *byte == b'\n');
        let text = decode_server_line(
            lines
                .next()
                .ok_or_else(|| io::Error::other("missing imported text metadata"))?,
        )?;
        assert!(matches!(
            text.message(),
            ServerMessage::TranscriptTextEntry {
                entry: TranscriptTextEntry::Imported {
                    imported_conversation_id,
                    imported_entry_id,
                    source_speaker: ImportedSourceSpeaker::Attested {
                        speaker: ImportedSpeaker::User,
                    },
                },
                ..
            } if imported_conversation_id.into_uuid() == conversation.into_uuid()
                && imported_entry_id.into_uuid() == imported_entry.into_uuid()
        ));
        assert!(matches!(
            decode_server_line(
                lines
                    .next()
                    .ok_or_else(|| io::Error::other("missing imported text content"))?
            )?
            .message(),
            ServerMessage::TranscriptContent {
                final_fragment: true,
                content_fragment,
                ..
            } if content_fragment.as_str() == "source-attested"
        ));
        assert!(matches!(
            decode_server_line(
                lines
                    .next()
                    .ok_or_else(|| io::Error::other("missing conservative imported entry"))?
            )?
            .message(),
            ServerMessage::TranscriptEntry {
                entry: TranscriptEntry::Imported {
                    source_speaker: ImportedSourceSpeaker::NotAttested {},
                    content_kind: ImportedContentKind::ToolResult,
                    ..
                },
                ..
            }
        ));
        assert!(lines.next().is_none());
        Ok(())
    }

    #[test]
    fn every_persistence_terminal_call_disposition_has_a_wire_projection() {
        assert_eq!(
            wire_model_call_state(DispatchedModelCallState::CancellationRequested),
            ModelCallState::CancellationRequested {}
        );
        assert_eq!(
            wire_model_call_state(DispatchedModelCallState::Terminal(
                DispatchedModelCallDisposition::Completed,
            )),
            ModelCallState::Terminal {
                disposition: ModelCallDisposition::Completed
            }
        );
        assert_eq!(
            wire_model_call_state(DispatchedModelCallState::Terminal(
                DispatchedModelCallDisposition::KnownFailed,
            )),
            ModelCallState::Terminal {
                disposition: ModelCallDisposition::KnownFailed
            }
        );
        assert_eq!(
            wire_model_call_state(DispatchedModelCallState::Terminal(
                DispatchedModelCallDisposition::Refused,
            )),
            ModelCallState::Terminal {
                disposition: ModelCallDisposition::Refused
            }
        );
        assert_eq!(
            wire_model_call_state(DispatchedModelCallState::Terminal(
                DispatchedModelCallDisposition::Cancelled,
            )),
            ModelCallState::Terminal {
                disposition: ModelCallDisposition::Cancelled
            }
        );
        assert_eq!(
            wire_model_call_state(DispatchedModelCallState::Terminal(
                DispatchedModelCallDisposition::Ambiguous,
            )),
            ModelCallState::Terminal {
                disposition: ModelCallDisposition::Ambiguous
            }
        );
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
                operation: ProcessReconciliationOperation::ModelCall(call),
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
                operation: DispatchedReconciliationOperation::ModelCall(call),
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
        let tool_attempt = ToolAttemptId::from_uuid(Uuid::from_u128(6));
        let recovery = ProcessUpdateEvent::from(&DispatchedOutboxEventKind::ToolBatchTransition {
            turn,
            producing_call: call,
            state: DispatchedToolBatchState::RecoveryRequired {
                attempt: tool_attempt,
            },
        });
        assert_eq!(
            recovery.wire(),
            SessionEvent::ToolBatchTransition {
                turn_id: CanonicalUuid::from_uuid(turn.into_uuid()),
                model_call_id: CanonicalUuid::from_uuid(call.into_uuid()),
                state: ToolBatchState::RecoveryRequired {
                    tool_attempt_id: CanonicalUuid::from_uuid(tool_attempt.into_uuid()),
                },
            }
        );
    }
}
