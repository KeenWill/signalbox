#![allow(
    clippy::expect_used,
    clippy::panic,
    reason = "the standalone integration test uses assertion panics and explicit fixture expectations"
)]

use std::{
    collections::VecDeque,
    error::Error,
    fs,
    io::{self, ErrorKind},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    time::Duration,
};

use signalbox_application::{
    AuthorizeModelCallOutcome, CreateSessionFromImportedFrontierIdGenerator,
    CreateSessionFromImportedFrontierOutcome, CreateSessionFromImportedFrontierRequest,
    CreateSessionFromImportedFrontierService, ImportConversationOutcome, ImportConversationService,
    ImportedConversationIdGenerator, InProcessAttemptDispatchGate, InProcessEligibilityWorkSource,
    InProcessToolDispatchGate, ModelCallCredentialReference, ModelCallExecutionOutcome,
    ModelCallExecutionService, SchedulerLoop, SchedulerLoopExit, ScriptedModelCallProvider,
    ScriptedModelCallStep, StartEligibleTurnOutcome, StartEligibleTurnService, StartupScanService,
    UuidV7ModelCallExecutionIdGenerator, UuidV7StartEligibleTurnIdGenerator,
    UuidV7StartupScanIdGenerator,
};
use signalbox_conversation_import_claude_code::ClaudeCodeJsonlConverter;
use signalbox_domain::{
    ActiveTurnPhase, AssistantResponsePart, AssistantText, ContextFrontierId, DirectModelSelection,
    DurableCommandId, FailedModelCallTurnIdentities, ImportedConversationFormat,
    ImportedConversationId, ImportedSessionRelationship, ImportedTranscriptEntryId,
    InitialToolApproval, ModelCallId, ModelCallTerminalIdentities, ModelCallTerminalObservation,
    ModelCallTerminalOutcome, ModelSelectionRequest, ModelTargetCatalog, NormalizedToolArguments,
    SemanticTranscriptEntryId, SessionConfigurationDefaults, SessionId, ToolCallProposal, ToolName,
    ToolRequestId, ToolResponsePartIdentity, ToolRoundModelCallIdentities,
    ToolUsingAssistantResponse, TurnId,
};
use signalbox_model_provider_runtime::RuntimeModelCallProvider;
use signalbox_model_runtime::{
    AssistantPart, CompletionEvidence, CompletionFinish, DeliveryMode, ExchangeFacts,
    ObservationFact, ProviderReportedModel, Script, ScriptedModel, TerminalEvidence, TokenUsage,
};
use signalbox_persistence::{
    conversation_import::ImportedConversationRepository,
    create_session_from_imported_frontier::ImportedSessionRepository,
    local_test_connection_options, migrate,
    model_execution::{PostgresModelCallRepository, PrepareInitialModelCallOutcome},
    scheduler::PostgresEligibilitySweep,
    start_eligible_turn::StartEligibleTurnRepository,
    startup::PostgresStartupScanRepository,
};
use signalbox_process_protocol::{
    CanonicalU64, CanonicalUuid, ClientFrame, ClientRequest, CommandId, ConversationImportFormat,
    ConversationImportSource, ConversationOriginFilter, ConversationSummary,
    CurrentModelCallState, ErrorCode, ImportedContentKind,
    ImportedConversationSourceFormat,
    ImportedSourceSpeaker, ImportedSpeaker, InputContent, MetadataActor, ModelSelection,
    ProtocolVersion, RejectionDetail, RequestId, ServerFrame, ServerMessage, SessionEvent,
    SessionMetadata, SystemPromptMember, SystemPromptText, ToolDecision, TranscriptEntry,
    TranscriptTextEntry, TurnState, decode_server_line, encode_client_line,
};
use signalboxd::{
    ActivatedTurnPass, FatalExecutionSupervisor, HubModelConfiguration, LocalProcessListener,
    PostgresProviderModelExecution, ProcessProviderTextDeltaSink, ProcessRuntime,
    ProcessRuntimeError,
};
use sqlx::{PgPool, postgres::PgPoolOptions};
use testcontainers_modules::{
    postgres::Postgres,
    testcontainers::{ContainerAsync, ImageExt, runners::AsyncRunner},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{
        UnixStream,
        unix::{OwnedReadHalf, OwnedWriteHalf},
    },
    sync::watch,
    task::JoinHandle,
    time::timeout,
};
use uuid::Uuid;

const POSTGRES_IMAGE_TAG: &str = "18.4-alpine3.23";
const DATABASE_NAME: &str = "signalbox_process_runtime";
const DATABASE_USER: &str = "signalbox";
const DATABASE_PASSWORD: &str = "signalbox-test-only";
const MAX_SUBMITTED_INPUT_BYTES: usize = 1024 * 1024;
const OVERSIZED_SUBMITTED_INPUT_BYTES: usize = MAX_SUBMITTED_INPUT_BYTES + 1;
const STREAMING_DELTA_COUNT: usize = 192;
const STREAMING_DELTA_BYTES: usize = 8 * 1024;
const MODEL_CONFIGURATION: &str = r#"
version = 1

[[models]]
selection_id = "00000000-0000-0000-0000-000000000001"
target_id = "00000000-0000-0000-0000-000000000003"
provider = "anthropic"
provider_model = "fixture-model"
max_output_tokens = 256

[[models]]
selection_id = "00000000-0000-0000-0000-000000000004"
target_id = "00000000-0000-0000-0000-000000000005"
provider = "anthropic"
provider_model = "fixture-model-next"
max_output_tokens = 256

[[aliases]]
alias_id = "00000000-0000-0000-0000-000000000002"
selection_id = "00000000-0000-0000-0000-000000000001"
"#;

async fn postgres() -> Result<(ContainerAsync<Postgres>, PgPool), Box<dyn Error>> {
    let container = Postgres::default()
        .with_db_name(DATABASE_NAME)
        .with_user(DATABASE_USER)
        .with_password(DATABASE_PASSWORD)
        .with_fsync_enabled()
        .with_tag(POSTGRES_IMAGE_TAG)
        .start()
        .await?;
    let host = container.get_host().await?;
    let port = container.get_host_port_ipv4(5432).await?;
    let database_url =
        format!("postgres://{DATABASE_USER}:{DATABASE_PASSWORD}@{host}:{port}/{DATABASE_NAME}");
    let pool = PgPoolOptions::new()
        .max_connections(8)
        .connect_with(local_test_connection_options(&database_url)?)
        .await?;
    migrate(&pool).await?;
    Ok((container, pool))
}

struct SocketDirectory {
    directory: PathBuf,
    socket: PathBuf,
}

impl SocketDirectory {
    fn create() -> Result<Self, Box<dyn Error>> {
        let directory = PathBuf::from("/tmp").join(format!("signalbox-process-{}", Uuid::now_v7()));
        fs::create_dir(&directory)?;
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
        let socket = directory.join("hub.sock");
        Ok(Self { directory, socket })
    }

    fn socket(&self) -> &Path {
        &self.socket
    }

    fn cleanup(self) -> Result<(), Box<dyn Error>> {
        let mut lock = self.socket.into_os_string();
        lock.push(".lock");
        match fs::remove_file(PathBuf::from(lock)) {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        fs::remove_dir(self.directory)?;
        Ok(())
    }
}

struct Connection {
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
}

impl Connection {
    async fn connect(path: &Path) -> Result<Self, Box<dyn Error>> {
        let stream = UnixStream::connect(path).await?;
        let (reader, writer) = stream.into_split();
        Ok(Self {
            reader: BufReader::new(reader),
            writer,
        })
    }

    async fn request(
        &mut self,
        request_id: u64,
        request: ClientRequest,
    ) -> Result<(), Box<dyn Error>> {
        self.request_version(ProtocolVersion::One, request_id, request)
            .await
    }

    async fn request_version(
        &mut self,
        version: ProtocolVersion,
        request_id: u64,
        request: ClientRequest,
    ) -> Result<(), Box<dyn Error>> {
        let frame =
            ClientFrame::try_new_for_version(version, RequestId::try_new(request_id)?, request)?;
        self.writer.write_all(&encode_client_line(&frame)?).await?;
        Ok(())
    }

    async fn raw_request(&mut self, frame: &str) -> Result<(), Box<dyn Error>> {
        self.writer.write_all(frame.as_bytes()).await?;
        Ok(())
    }

    async fn response(&mut self) -> Result<ServerFrame, Box<dyn Error>> {
        let mut line = Vec::new();
        if self.reader.read_until(b'\n', &mut line).await? == 0 {
            return Err(io::Error::new(
                ErrorKind::UnexpectedEof,
                "the process server closed before its next frame",
            )
            .into());
        }
        Ok(decode_server_line(&line)?)
    }
}

fn command() -> Result<CommandId, Box<dyn Error>> {
    Ok(CommandId::try_from_uuid(Uuid::now_v7())?)
}

#[derive(Debug)]
struct FixedImportIds {
    conversations: VecDeque<ImportedConversationId>,
    entries: VecDeque<ImportedTranscriptEntryId>,
}

impl ImportedConversationIdGenerator for FixedImportIds {
    fn next_conversation_id(&mut self) -> ImportedConversationId {
        self.conversations
            .pop_front()
            .expect("the fixture supplies one conversation identity")
    }

    fn next_entry_id(&mut self) -> ImportedTranscriptEntryId {
        self.entries
            .pop_front()
            .expect("the fixture supplies one identity per imported entry")
    }
}

#[derive(Debug)]
struct FixedImportedSessionIds {
    sessions: VecDeque<SessionId>,
    semantic_entries: VecDeque<SemanticTranscriptEntryId>,
    frontiers: VecDeque<ContextFrontierId>,
}

impl CreateSessionFromImportedFrontierIdGenerator for FixedImportedSessionIds {
    fn next_session_id(&mut self) -> SessionId {
        self.sessions
            .pop_front()
            .expect("the fixture supplies one session identity")
    }

    fn next_semantic_entry_id(&mut self) -> SemanticTranscriptEntryId {
        self.semantic_entries
            .pop_front()
            .expect("the fixture supplies one semantic identity per prefix entry")
    }

    fn next_context_frontier_id(&mut self) -> ContextFrontierId {
        self.frontiers
            .pop_front()
            .expect("the fixture supplies one seed frontier identity")
    }
}

async fn create_imported_session(pool: &PgPool) -> Result<CanonicalUuid, Box<dyn Error>> {
    let conversation = ImportedConversationId::from_uuid(Uuid::from_u128(0x100));
    let imported_entries = [
        ImportedTranscriptEntryId::from_uuid(Uuid::from_u128(0x200)),
        ImportedTranscriptEntryId::from_uuid(Uuid::from_u128(0x201)),
    ];
    let source = concat!(
        "{\"type\":\"user\",\"message\":{\"content\":\"imported user\"}}\n",
        "{\"type\":\"assistant\",\"message\":{\"content\":[",
        "{\"type\":\"tool_use\",\"id\":\"call\",\"name\":\"lookup\",",
        "\"input\":{\"query\":\"synthetic\"}}]}}"
    );
    let mut import_service = ImportConversationService::new(
        FixedImportIds {
            conversations: [conversation].into(),
            entries: imported_entries.into(),
        },
        ClaudeCodeJsonlConverter,
        ImportedConversationRepository::new(pool.clone()),
    );
    assert_eq!(
        import_service.execute(source.as_bytes()).await?,
        ImportConversationOutcome::Inserted { conversation }
    );
    let (_, _, import_repository) = import_service.into_parts();
    let stored = import_repository
        .load(conversation)
        .await?
        .expect("the synthetic imported conversation is durable");
    let frontier = stored
        .frontiers()
        .last()
        .expect("the final imported entry exposes a seed boundary");

    let session = SessionId::from_uuid(Uuid::from_u128(0x300));
    let mut create_service = CreateSessionFromImportedFrontierService::new(
        FixedImportedSessionIds {
            sessions: [session].into(),
            semantic_entries: [
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0x400)),
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0x401)),
            ]
            .into(),
            frontiers: [ContextFrontierId::from_uuid(Uuid::from_u128(0x500))].into(),
        },
        ImportedSessionRepository::new(pool.clone()),
    );
    let outcome = create_service
        .execute(CreateSessionFromImportedFrontierRequest::try_new(
            DurableCommandId::from_uuid(Uuid::from_u128(0x600)),
            frontier,
            ImportedSessionRelationship::Resume,
            SessionConfigurationDefaults::new(ModelSelectionRequest::Direct(
                DirectModelSelection::from_uuid(Uuid::from_u128(1)),
            )),
        )?)
        .await?;
    assert!(matches!(
        outcome,
        CreateSessionFromImportedFrontierOutcome::Applied(result)
            if result.session() == session
    ));
    Ok(CanonicalUuid::from_uuid(session.into_uuid()))
}

struct RunningRuntime {
    container: ContainerAsync<Postgres>,
    pool: PgPool,
    socket_directory: SocketDirectory,
    shutdown: watch::Sender<bool>,
    runtime_task: JoinHandle<Result<(), ProcessRuntimeError>>,
    work_source: Option<InProcessEligibilityWorkSource<PostgresEligibilitySweep>>,
    provider_text_deltas: ProcessProviderTextDeltaSink,
}

impl RunningRuntime {
    async fn start() -> Result<Self, Box<dyn Error>> {
        let (container, pool) = postgres().await?;
        let socket_directory = SocketDirectory::create()?;
        let listener = LocalProcessListener::bind(socket_directory.socket())?;
        let sweep = PostgresEligibilitySweep::new(pool.clone());
        let (eligibility_nudge, work_source) = InProcessEligibilityWorkSource::new(sweep);
        let model_configuration = HubModelConfiguration::parse(MODEL_CONFIGURATION)?;
        let runtime = ProcessRuntime::new(
            listener,
            pool.clone(),
            eligibility_nudge,
            InProcessToolDispatchGate::default(),
            model_configuration,
        );
        let provider_text_deltas = runtime.provider_text_delta_sink();
        let (shutdown, shutdown_receiver) = watch::channel(false);
        let runtime_task = tokio::spawn(runtime.run(shutdown_receiver));
        Ok(Self {
            container,
            pool,
            socket_directory,
            shutdown,
            runtime_task,
            work_source: Some(work_source),
            provider_text_deltas,
        })
    }

    fn socket(&self) -> &Path {
        self.socket_directory.socket()
    }

    fn take_work_source(&mut self) -> InProcessEligibilityWorkSource<PostgresEligibilitySweep> {
        self.work_source
            .take()
            .expect("the streaming fixture takes the work source once")
    }

    fn provider_text_delta_sink(&self) -> ProcessProviderTextDeltaSink {
        self.provider_text_deltas.clone()
    }

    async fn stop(self) -> Result<(), Box<dyn Error>> {
        self.shutdown.send(true)?;
        timeout(Duration::from_secs(10), self.runtime_task).await???;
        self.pool.close().await;
        self.socket_directory.cleanup()?;
        drop(self.container);
        Ok(())
    }
}

async fn create_alias_session(
    connection: &mut Connection,
) -> Result<CanonicalUuid, Box<dyn Error>> {
    connection
        .request(
            1,
            ClientRequest::CreateSession {
                command_id: command()?,
                initial_model_selection: ModelSelection::Alias {
                    alias_id: CanonicalUuid::from_uuid(Uuid::from_u128(2)),
                },
                system_prompt: SystemPromptMember::absent(),
            },
        )
        .await?;
    match connection.response().await?.message() {
        ServerMessage::SessionCreated { session_id } => Ok(*session_id),
        message => Err(io::Error::other(format!(
            "unexpected create-session fixture response: {message:?}"
        ))
        .into()),
    }
}

async fn submit_first_input(
    connection: &mut Connection,
    session_id: CanonicalUuid,
    content: String,
) -> Result<(CanonicalUuid, CanonicalUuid), Box<dyn Error>> {
    connection
        .request(
            2,
            ClientRequest::SubmitInput {
                command_id: command()?,
                session_id,
                content: InputContent::new(content),
                expected_defaults_version: CanonicalU64::new(1),
            },
        )
        .await?;
    match connection.response().await?.message() {
        ServerMessage::InputSubmitted {
            session_id: submitted_session,
            accepted_input_id,
            acceptance_position,
            turn_id,
        } if *submitted_session == session_id && acceptance_position.value() == 1 => {
            Ok((*accepted_input_id, *turn_id))
        }
        message => Err(io::Error::other(format!(
            "unexpected first-input fixture response: {message:?}"
        ))
        .into()),
    }
}

/// Reads one accepted-input acknowledgement and returns the successor turn it
/// names, requiring the exact session and acceptance ordinal the caller states.
async fn accepted_successor_turn(
    connection: &mut Connection,
    session_id: CanonicalUuid,
    acceptance: u64,
) -> Result<CanonicalUuid, Box<dyn Error>> {
    match response_within(connection).await?.message() {
        ServerMessage::InputSubmitted {
            session_id: accepted_session,
            acceptance_position,
            turn_id,
            ..
        } if *accepted_session == session_id && acceptance_position.value() == acceptance => {
            Ok(*turn_id)
        }
        message => {
            Err(io::Error::other(format!("unexpected accepted-input response: {message:?}")).into())
        }
    }
}

async fn response_within(connection: &mut Connection) -> Result<ServerFrame, Box<dyn Error>> {
    timeout(Duration::from_secs(5), connection.response()).await?
}

async fn attach_empty_follower(
    socket: &Path,
    version: ProtocolVersion,
    request_id: u64,
    session_id: CanonicalUuid,
) -> Result<Connection, Box<dyn Error>> {
    let mut follow = Connection::connect(socket).await?;
    follow
        .request_version(
            version,
            request_id,
            ClientRequest::FollowSession { session_id },
        )
        .await?;
    assert!(matches!(
        response_within(&mut follow).await?.message(),
        ServerMessage::TranscriptSnapshotStart {
            session_id: snapshot_session,
            ..
        } if *snapshot_session == session_id
    ));
    assert!(matches!(
        response_within(&mut follow).await?.message(),
        ServerMessage::TranscriptSnapshotEnd {
            session_id: snapshot_session,
            turn_count,
            entry_count,
            ..
        } if *snapshot_session == session_id
            && turn_count.value() == 0
            && entry_count.value() == 0
    ));
    Ok(follow)
}

struct StreamedFollowOutcome {
    delta_count: usize,
    text: String,
}

async fn follow_streamed_turn_to_completion(
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

async fn receive_resync(mut follow: Connection) -> Result<usize, Box<dyn Error>> {
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

async fn read_completed_assistant(
    socket: &Path,
    request_id: u64,
    session_id: CanonicalUuid,
    turn_id: CanonicalUuid,
) -> Result<String, Box<dyn Error>> {
    let mut connection = Connection::connect(socket).await?;
    connection
        .request_version(
            ProtocolVersion::Twelve,
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

fn streamed_script(delta_count: usize, delta: String) -> (Script, String) {
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

async fn wait_for_terminal(pool: &PgPool, session: SessionId, turn: TurnId) {
    loop {
        let terminal: bool = sqlx::query_scalar(
            "SELECT EXISTS (
                SELECT 1
                  FROM turn_lifecycle
                 WHERE session_id = $1
                   AND turn_id = $2
                   AND state_kind = 'terminal'
            )",
        )
        .bind(session.into_uuid())
        .bind(turn.into_uuid())
        .fetch_one(pool)
        .await
        .unwrap_or(false);
        if terminal {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn execute_streamed_turn(
    runtime: &mut RunningRuntime,
    scripted: ScriptedModel<ModelCallId>,
    session_id: CanonicalUuid,
    turn_id: CanonicalUuid,
) -> Result<ScriptedModel<ModelCallId>, Box<dyn Error>> {
    let model_configuration = HubModelConfiguration::parse(MODEL_CONFIGURATION)?;
    let probe = scripted.clone();
    let provider =
        RuntimeModelCallProvider::new(scripted, model_configuration.runtime_model_catalog())
            .with_text_delta_sink(runtime.provider_text_delta_sink());
    let (execution, fatal_execution) =
        FatalExecutionSupervisor::new(PostgresProviderModelExecution::new(
            PostgresModelCallRepository::new(
                runtime.pool.clone(),
                model_configuration.target_catalog(),
                ModelCallCredentialReference::new("streaming-fixture"),
            ),
            InProcessAttemptDispatchGate::default(),
            provider,
        ));
    let pass = ActivatedTurnPass::new(
        StartEligibleTurnService::new(
            UuidV7StartEligibleTurnIdGenerator,
            StartEligibleTurnRepository::new(runtime.pool.clone()),
        ),
        execution,
    );
    let mut scheduler = SchedulerLoop::new(runtime.take_work_source(), pass);
    let observation_pool = runtime.pool.clone();
    let session = SessionId::from_uuid(session_id.into_uuid());
    let turn = TurnId::from_uuid(turn_id.into_uuid());
    let fatal_shutdown = fatal_execution.clone();
    let shutdown = async move {
        tokio::select! {
            () = wait_for_terminal(&observation_pool, session, turn) => {}
            () = fatal_shutdown.wait() => {}
        }
    };
    assert_eq!(
        timeout(Duration::from_secs(10), scheduler.run_until(shutdown)).await?,
        SchedulerLoopExit::Shutdown
    );
    assert!(!fatal_execution.is_triggered());
    Ok(probe)
}

#[track_caller]
fn submitted_session(message: &ServerMessage) -> CanonicalUuid {
    match message {
        ServerMessage::InputSubmitted { session_id, .. } => *session_id,
        message => panic!("fixture expected input-submitted, got {message:?}"),
    }
}

#[track_caller]
fn replaced_defaults(message: &ServerMessage) -> (CanonicalUuid, u64) {
    match message {
        ServerMessage::SessionDefaultsReplaced {
            session_id,
            defaults_version,
            ..
        } => (*session_id, defaults_version.value()),
        message => panic!("fixture expected defaults-replaced, got {message:?}"),
    }
}

#[track_caller]
fn protocol_error_code(message: &ServerMessage) -> ErrorCode {
    match message {
        ServerMessage::Error { code, .. } => *code,
        message => panic!("fixture expected protocol error, got {message:?}"),
    }
}

async fn activate_turn(pool: &PgPool, session: SessionId) -> Result<(), Box<dyn Error>> {
    let mut service = StartEligibleTurnService::new(
        UuidV7StartEligibleTurnIdGenerator,
        StartEligibleTurnRepository::new(pool.clone()),
    );
    assert!(matches!(
        service.execute(session).await?,
        StartEligibleTurnOutcome::Activated(_)
    ));
    Ok(())
}

async fn complete_active_text_turn(
    pool: &PgPool,
    session: SessionId,
    targets: ModelTargetCatalog,
) -> Result<(), Box<dyn Error>> {
    let repository = PostgresModelCallRepository::new(
        pool.clone(),
        targets,
        ModelCallCredentialReference::new("process-runtime-fixture"),
    );
    let mut service = ModelCallExecutionService::new(
        UuidV7ModelCallExecutionIdGenerator,
        repository.clone(),
        repository.clone(),
        repository.clone(),
        repository,
        ScriptedModelCallProvider::new([ScriptedModelCallStep::Return(
            ModelCallTerminalObservation::Completed {
                assistant_text: vec![
                    AssistantText::try_new(String::from("fixture response"))
                        .expect("fixture assistant content is valid"),
                ],
            },
        )]),
        InProcessAttemptDispatchGate::default(),
    );
    assert!(matches!(
        service.execute(session).await?,
        ModelCallExecutionOutcome::Checkpointed(_)
    ));
    assert!(matches!(
        service.execute(session).await?,
        ModelCallExecutionOutcome::ObservationCommitted(outcome)
            if matches!(*outcome, ModelCallTerminalOutcome::Completed(_))
    ));
    Ok(())
}

/// S28 / INV-038: the owner-visible operation distinguishes first insertion
/// from exact-snapshot reimport while retaining the winner's identity.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn s28_inv038_version_five_reports_inserted_then_already_imported()
-> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let source = ConversationImportSource::new(
        concat!(
            "{\"sessionId\":\"operational-claude\",\"type\":\"user\",",
            "\"message\":{\"role\":\"user\",\"content\":\"question\"}}\n",
            "{\"sessionId\":\"operational-claude\",\"type\":\"assistant\",",
            "\"message\":{\"role\":\"assistant\",\"content\":\"answer\"}}"
        )
        .as_bytes()
        .to_vec(),
    );

    connection
        .request_version(
            ProtocolVersion::Five,
            1,
            ClientRequest::ImportConversation {
                format: ConversationImportFormat::ClaudeCodeSessionJsonlV2,
                source: source.clone(),
            },
        )
        .await?;
    let inserted = response_within(&mut connection).await?;
    let stored_id: Uuid =
        sqlx::query_scalar("SELECT imported_conversation_id FROM imported_conversation")
            .fetch_one(&runtime.pool)
            .await?;
    assert_eq!(
        inserted.message(),
        &ServerMessage::ConversationImportInserted {
            imported_conversation_id: CanonicalUuid::from_uuid(stored_id),
        }
    );

    connection
        .request_version(
            ProtocolVersion::Five,
            2,
            ClientRequest::ImportConversation {
                format: ConversationImportFormat::ClaudeCodeSessionJsonlV2,
                source,
            },
        )
        .await?;
    let already_imported = response_within(&mut connection).await?;
    assert_eq!(
        already_imported.message(),
        &ServerMessage::ConversationImportAlreadyImported {
            imported_conversation_id: CanonicalUuid::from_uuid(stored_id),
        }
    );

    drop(connection);
    runtime.stop().await
}

/// S28: the explicit Codex selection reaches the fixed Codex converter rather
/// than applying format detection or the Claude Code interpretation.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn s28_version_five_selects_the_codex_rollout_converter() -> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let source = ConversationImportSource::new(
        concat!(
            "{\"timestamp\":\"2026-07-25T00:00:00Z\",\"type\":\"response_item\",",
            "\"payload\":{\"type\":\"message\",\"role\":\"user\",",
            "\"content\":[{\"type\":\"input_text\",\"text\":\"question\"}]}}"
        )
        .as_bytes()
        .to_vec(),
    );

    connection
        .request_version(
            ProtocolVersion::Five,
            1,
            ClientRequest::ImportConversation {
                format: ConversationImportFormat::CodexRolloutJsonlV1,
                source,
            },
        )
        .await?;
    let inserted = response_within(&mut connection).await?;
    let stored_id: Uuid =
        sqlx::query_scalar("SELECT imported_conversation_id FROM imported_conversation")
            .fetch_one(&runtime.pool)
            .await?;
    assert_eq!(
        inserted.message(),
        &ServerMessage::ConversationImportInserted {
            imported_conversation_id: CanonicalUuid::from_uuid(stored_id),
        }
    );
    let stored = ImportedConversationRepository::new(runtime.pool.clone())
        .load(ImportedConversationId::from_uuid(stored_id))
        .await?
        .expect("the successful operation inserted its imported conversation");
    assert_eq!(
        stored.format(),
        ImportedConversationFormat::CodexRolloutJsonlV1
    );

    drop(connection);
    runtime.stop().await
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn process_runtime_lists_the_alias_session_projection() -> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let session_id = create_alias_session(&mut connection).await?;
    let alias_id = CanonicalUuid::from_uuid(Uuid::from_u128(2));

    connection
        .request(2, ClientRequest::ListSessions {})
        .await?;

    let start = response_within(&mut connection).await?;
    assert!(matches!(start.message(), ServerMessage::SessionsStart {}));
    let summary = response_within(&mut connection).await?;
    assert!(matches!(
        summary.message(),
        ServerMessage::SessionSummary {
            session_id: listed,
            defaults_version,
            model_selection: ModelSelection::Alias {
                alias_id: listed_alias
            },
        } if *listed == session_id
            && defaults_version.value() == 1
            && *listed_alias == alias_id
    ));
    let end = response_within(&mut connection).await?;
    assert!(matches!(
        end.message(),
        ServerMessage::SessionsEnd { session_count } if session_count.value() == 1
    ));

    drop(connection);
    runtime.stop().await
}

/// S33 / INV-008 / INV-012 / INV-046: version six maps one complete replacement
/// request through the durable command boundary and validates catalog input
/// before claiming a new command identity.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn s33_inv008_inv012_inv046_process_runtime_replaces_session_model_defaults()
-> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let session_id = create_alias_session(&mut connection).await?;
    let replacement_command = command()?;
    let replacement_selection = CanonicalUuid::from_uuid(Uuid::from_u128(4));
    let replacement = ClientRequest::ReplaceSessionDefaults {
        command_id: replacement_command,
        session_id,
        expected_defaults_version: CanonicalU64::new(1),
        model_selection: ModelSelection::Direct {
            selection_id: replacement_selection,
        },
        dangerous_tool_auto_approval: false,
        system_prompt: SystemPromptMember::absent(),
    };

    connection
        .request_version(ProtocolVersion::Six, 2, replacement.clone())
        .await?;
    assert_eq!(
        response_within(&mut connection).await?.message(),
        &ServerMessage::SessionDefaultsReplaced {
            session_id,
            defaults_version: CanonicalU64::new(2),
            model_selection: ModelSelection::Direct {
                selection_id: replacement_selection,
            },
            dangerous_tool_auto_approval: false,
            system_prompt: SystemPromptMember::absent(),
        }
    );

    connection
        .request_version(ProtocolVersion::Six, 3, replacement)
        .await?;
    assert_eq!(
        response_within(&mut connection).await?.message(),
        &ServerMessage::SessionDefaultsReplaced {
            session_id,
            defaults_version: CanonicalU64::new(2),
            model_selection: ModelSelection::Direct {
                selection_id: replacement_selection,
            },
            dangerous_tool_auto_approval: false,
            system_prompt: SystemPromptMember::absent(),
        }
    );

    let unknown_command = command()?;
    connection
        .request_version(
            ProtocolVersion::Six,
            4,
            ClientRequest::ReplaceSessionDefaults {
                command_id: unknown_command,
                session_id,
                expected_defaults_version: CanonicalU64::new(2),
                model_selection: ModelSelection::Direct {
                    selection_id: CanonicalUuid::from_uuid(Uuid::from_u128(999)),
                },
                dangerous_tool_auto_approval: false,
                system_prompt: SystemPromptMember::absent(),
            },
        )
        .await?;
    assert!(matches!(
        response_within(&mut connection).await?.message(),
        ServerMessage::Error {
            code: ErrorCode::InvalidRequest,
            ..
        }
    ));
    let unknown_claim_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM durable_command WHERE command_id = $1")
            .bind(unknown_command.into_uuid())
            .fetch_one(&runtime.pool)
            .await?;
    assert_eq!(unknown_claim_count, 0);

    drop(connection);
    runtime.stop().await
}

/// S33 / INV-012 / INV-033 / INV-046: a durable submit receipt remains
/// replayable by its original protocol after later history raises the selected
/// session's minimum representable version; an unseen command remains gated.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn s33_inv012_inv033_inv046_submit_replay_precedes_mutable_history_gate()
-> Result<(), Box<dyn Error>> {
    let targets = HubModelConfiguration::parse(MODEL_CONFIGURATION)?.target_catalog();
    let runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let session_id = create_alias_session(&mut connection).await?;
    let session = SessionId::from_uuid(session_id.into_uuid());

    connection
        .request_version(
            ProtocolVersion::Five,
            2,
            ClientRequest::SubmitInput {
                command_id: command()?,
                session_id,
                content: InputContent::new(String::from("first model turn")),
                expected_defaults_version: CanonicalU64::new(1),
            },
        )
        .await?;
    assert_eq!(
        submitted_session(response_within(&mut connection).await?.message()),
        session_id
    );
    activate_turn(&runtime.pool, session).await?;
    complete_active_text_turn(&runtime.pool, session, targets).await?;

    connection
        .request_version(
            ProtocolVersion::Six,
            3,
            ClientRequest::ReplaceSessionDefaults {
                command_id: command()?,
                session_id,
                expected_defaults_version: CanonicalU64::new(1),
                model_selection: ModelSelection::Direct {
                    selection_id: CanonicalUuid::from_uuid(Uuid::from_u128(4)),
                },
                dangerous_tool_auto_approval: false,
                system_prompt: SystemPromptMember::absent(),
            },
        )
        .await?;
    assert_eq!(
        replaced_defaults(response_within(&mut connection).await?.message()),
        (session_id, 2)
    );

    let replayed_command = command()?;
    let replayed_request = ClientRequest::SubmitInput {
        command_id: replayed_command,
        session_id,
        content: InputContent::new(String::from("second model turn")),
        expected_defaults_version: CanonicalU64::new(2),
    };
    connection
        .request_version(ProtocolVersion::Five, 4, replayed_request.clone())
        .await?;
    let first_receipt = response_within(&mut connection).await?;
    assert_eq!(submitted_session(first_receipt.message()), session_id);

    activate_turn(&runtime.pool, session).await?;
    let boundary_count: i64 = sqlx::query_scalar(
        "SELECT count(*)
           FROM semantic_transcript_entry
          WHERE source_session_id = $1
            AND payload_kind = 'model_identity_changed'",
    )
    .bind(session_id.into_uuid())
    .fetch_one(&runtime.pool)
    .await?;
    assert_eq!(boundary_count, 1);

    connection
        .request_version(ProtocolVersion::Five, 5, replayed_request)
        .await?;
    assert_eq!(
        response_within(&mut connection).await?.message(),
        first_receipt.message()
    );

    let unseen_command = command()?;
    connection
        .request_version(
            ProtocolVersion::Five,
            6,
            ClientRequest::SubmitInput {
                command_id: unseen_command,
                session_id,
                content: InputContent::new(String::from("must remain gated")),
                expected_defaults_version: CanonicalU64::new(2),
            },
        )
        .await?;
    assert_eq!(
        protocol_error_code(response_within(&mut connection).await?.message()),
        ErrorCode::UnsupportedVersion
    );
    let unseen_claim_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM durable_command WHERE command_id = $1")
            .bind(unseen_command.into_uuid())
            .fetch_one(&runtime.pool)
            .await?;
    assert_eq!(unseen_claim_count, 0);

    drop(connection);
    runtime.stop().await
}

/// INV-033: metadata wire-shape failures are malformed frames, not application
/// request rejections.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn metadata_shape_failure_is_a_malformed_frame() -> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let required_tags = (0..=256)
        .map(|index| format!("tag-{index:03}"))
        .collect::<Vec<_>>();
    let frame = format!(
        "{{\"version\":4,\"request_id\":\"21\",\"request\":{{\"type\":\"list_session_metadata\",\"required_tags\":{},\"title_contains\":null,\"include_archived\":false,\"page_size\":\"50\",\"after_session_id\":null}}}}\n",
        serde_json::to_string(&required_tags)?
    );

    connection.raw_request(&frame).await?;

    let response = response_within(&mut connection).await?;
    assert_eq!(response.version(), ProtocolVersion::Four);
    assert!(matches!(
        response.message(),
        ServerMessage::Error {
            code: ErrorCode::MalformedFrame,
            ..
        }
    ));

    drop(connection);
    runtime.stop().await
}

/// INV-033: version four exposes the canonical initial metadata projection.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn s01_inv033_version_four_reads_initial_metadata_projection() -> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let first_session = create_alias_session(&mut connection).await?;

    connection
        .request_version(
            ProtocolVersion::Four,
            10,
            ClientRequest::ReadSessionMetadata {
                session_id: first_session,
            },
        )
        .await?;
    let initial = response_within(&mut connection).await?;
    assert_eq!(initial.version(), ProtocolVersion::Four);
    assert!(matches!(
        initial.message(),
        ServerMessage::SessionMetadata {
            session_id,
            metadata,
            last_writer: None,
        } if *session_id == first_session && metadata == &SessionMetadata::empty()
    ));

    drop(connection);
    runtime.stop().await
}

/// INV-012: one metadata command identity applies once, replays exactly, and
/// rejects a structurally different reuse.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn inv012_version_four_enforces_metadata_command_identity() -> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let first_session = create_alias_session(&mut connection).await?;

    let replacement_command = command()?;
    let replacement = SessionMetadata::try_new(
        Some(String::from("Archived plan")),
        vec![String::from("work"), String::from("daily")],
        vec![(String::from("run"), String::from("17"))],
        true,
    )?;
    connection
        .request_version(
            ProtocolVersion::Four,
            11,
            ClientRequest::ReplaceSessionMetadata {
                command_id: replacement_command,
                session_id: first_session,
                metadata: replacement.clone(),
            },
        )
        .await?;
    let applied = response_within(&mut connection).await?;
    assert!(matches!(
        applied.message(),
        ServerMessage::SessionMetadataReplaced {
            session_id,
            metadata,
            last_writer,
        } if *session_id == first_session
            && metadata == &replacement
            && matches!(last_writer.actor(), MetadataActor::Owner {})
    ));

    connection
        .request_version(
            ProtocolVersion::Four,
            12,
            ClientRequest::ReplaceSessionMetadata {
                command_id: replacement_command,
                session_id: first_session,
                metadata: replacement.clone(),
            },
        )
        .await?;
    let replay = response_within(&mut connection).await?;
    assert_eq!(replay.message(), applied.message());

    connection
        .request_version(
            ProtocolVersion::Four,
            13,
            ClientRequest::ReplaceSessionMetadata {
                command_id: replacement_command,
                session_id: first_session,
                metadata: SessionMetadata::empty(),
            },
        )
        .await?;
    assert!(matches!(
        response_within(&mut connection).await?.message(),
        ServerMessage::Error {
            code: ErrorCode::ConflictingReuse,
            ..
        }
    ));

    drop(connection);
    runtime.stop().await
}

/// INV-013: the default metadata list applies exact filters while excluding an
/// archived match.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn inv013_metadata_list_applies_default_visibility_filters() -> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let first_session = create_alias_session(&mut connection).await?;
    let second_session = create_alias_session(&mut connection).await?;
    let archived_metadata = SessionMetadata::try_new(
        Some(String::from("Active archived plan")),
        vec![String::from("daily")],
        Vec::new(),
        true,
    )?;
    connection
        .request_version(
            ProtocolVersion::Four,
            10,
            ClientRequest::ReplaceSessionMetadata {
                command_id: command()?,
                session_id: first_session,
                metadata: archived_metadata,
            },
        )
        .await?;
    assert!(matches!(
        response_within(&mut connection).await?.message(),
        ServerMessage::SessionMetadataReplaced {
            session_id,
            metadata,
            ..
        } if *session_id == first_session && metadata.archived()
    ));

    let second_metadata = SessionMetadata::try_new(
        Some(String::from("Active plan")),
        vec![String::from("daily")],
        Vec::new(),
        false,
    )?;
    connection
        .request_version(
            ProtocolVersion::Four,
            14,
            ClientRequest::ReplaceSessionMetadata {
                command_id: command()?,
                session_id: second_session,
                metadata: second_metadata.clone(),
            },
        )
        .await?;
    assert!(matches!(
        response_within(&mut connection).await?.message(),
        ServerMessage::SessionMetadataReplaced {
            session_id,
            metadata,
            ..
        } if *session_id == second_session && metadata == &second_metadata
    ));

    connection
        .request_version(
            ProtocolVersion::Four,
            15,
            ClientRequest::ListSessionMetadata {
                required_tags: vec![String::from("daily")],
                title_contains: Some(String::from("Active")),
                include_archived: false,
                page_size: CanonicalU64::new(10),
                after_session_id: None,
            },
        )
        .await?;
    assert!(matches!(
        response_within(&mut connection).await?.message(),
        ServerMessage::SessionMetadataPageStart {}
    ));
    assert!(matches!(
        response_within(&mut connection).await?.message(),
        ServerMessage::SessionMetadataSummary {
            session_id,
            dangerous_tool_auto_approval: false,
            title: Some(title),
            tags,
            archived: false,
            ..
        } if *session_id == second_session
            && title == "Active plan"
            && tags == &["daily"]
    ));
    assert!(matches!(
        response_within(&mut connection).await?.message(),
        ServerMessage::SessionMetadataPageEnd {
            session_count,
            next_after_session_id: None,
        } if session_count.value() == 1
    ));

    drop(connection);
    runtime.stop().await
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn metadata_list_uses_bounded_keyset_pages() -> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let first_session = create_alias_session(&mut connection).await?;
    let second_session = create_alias_session(&mut connection).await?;

    connection
        .request_version(
            ProtocolVersion::Four,
            16,
            ClientRequest::ListSessionMetadata {
                required_tags: Vec::new(),
                title_contains: None,
                include_archived: true,
                page_size: CanonicalU64::new(1),
                after_session_id: None,
            },
        )
        .await?;
    assert!(matches!(
        response_within(&mut connection).await?.message(),
        ServerMessage::SessionMetadataPageStart {}
    ));
    let first_page_session = match response_within(&mut connection).await?.message() {
        ServerMessage::SessionMetadataSummary { session_id, .. } => *session_id,
        message => {
            return Err(io::Error::other(format!(
                "unexpected first metadata-page summary: {message:?}"
            ))
            .into());
        }
    };
    let next = match response_within(&mut connection).await?.message() {
        ServerMessage::SessionMetadataPageEnd {
            session_count,
            next_after_session_id: Some(next),
        } if session_count.value() == 1 => *next,
        message => {
            return Err(io::Error::other(format!(
                "unexpected first metadata-page end: {message:?}"
            ))
            .into());
        }
    };
    assert_eq!(next, first_page_session);

    connection
        .request_version(
            ProtocolVersion::Four,
            17,
            ClientRequest::ListSessionMetadata {
                required_tags: Vec::new(),
                title_contains: None,
                include_archived: true,
                page_size: CanonicalU64::new(1),
                after_session_id: Some(next),
            },
        )
        .await?;
    assert!(matches!(
        response_within(&mut connection).await?.message(),
        ServerMessage::SessionMetadataPageStart {}
    ));
    let second_page_session = match response_within(&mut connection).await?.message() {
        ServerMessage::SessionMetadataSummary { session_id, .. } => *session_id,
        message => {
            return Err(io::Error::other(format!(
                "unexpected second metadata-page summary: {message:?}"
            ))
            .into());
        }
    };
    assert_ne!(second_page_session, first_page_session);
    assert!(
        [first_page_session, second_page_session].contains(&first_session)
            && [first_page_session, second_page_session].contains(&second_session)
    );
    assert!(matches!(
        response_within(&mut connection).await?.message(),
        ServerMessage::SessionMetadataPageEnd {
            session_count,
            next_after_session_id: None,
        } if session_count.value() == 1
    ));

    drop(connection);
    runtime.stop().await
}

/// S28: version sixteen lists native sessions and imported conversations in
/// one unified page whose imported row carries the derived title, entry
/// count, and stored source format.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn s28_version_sixteen_lists_native_and_imported_conversations()
-> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let native_session = create_alias_session(&mut connection).await?;
    let source = ConversationImportSource::new(
        concat!(
            "{\"timestamp\":\"2026-07-25T00:00:00Z\",\"type\":\"response_item\",",
            "\"payload\":{\"type\":\"message\",\"role\":\"user\",",
            "\"content\":[{\"type\":\"input_text\",\"text\":\"question\"}]}}"
        )
        .as_bytes()
        .to_vec(),
    );
    connection
        .request_version(
            ProtocolVersion::Five,
            30,
            ClientRequest::ImportConversation {
                format: ConversationImportFormat::CodexRolloutJsonlV1,
                source,
            },
        )
        .await?;
    let imported_id = match response_within(&mut connection).await?.message() {
        ServerMessage::ConversationImportInserted {
            imported_conversation_id,
        } => *imported_conversation_id,
        message => {
            return Err(
                io::Error::other(format!("unexpected import receipt: {message:?}")).into(),
            );
        }
    };

    connection
        .request_version(
            ProtocolVersion::Sixteen,
            31,
            ClientRequest::ListConversations {
                title_contains: None,
                origin: ConversationOriginFilter::All,
                include_archived: false,
                page_size: CanonicalU64::new(10),
                after: None,
            },
        )
        .await?;
    assert!(matches!(
        response_within(&mut connection).await?.message(),
        ServerMessage::ConversationPageStart {}
    ));
    let first_summary = match response_within(&mut connection).await?.message() {
        ServerMessage::ConversationSummary { conversation } => conversation.clone(),
        message => {
            return Err(
                io::Error::other(format!("unexpected first unified summary: {message:?}")).into(),
            );
        }
    };
    let second_summary = match response_within(&mut connection).await?.message() {
        ServerMessage::ConversationSummary { conversation } => conversation.clone(),
        message => {
            return Err(io::Error::other(format!(
                "unexpected second unified summary: {message:?}"
            ))
            .into());
        }
    };
    assert!(matches!(
        response_within(&mut connection).await?.message(),
        ServerMessage::ConversationPageEnd {
            conversation_count,
            next_after: None,
        } if conversation_count.value() == 2
    ));
    assert!(
        first_summary.cursor().conversation_id().into_uuid()
            < second_summary.cursor().conversation_id().into_uuid(),
        "unified summaries must arrive in strict identity order"
    );
    let (native, imported) = match (first_summary, second_summary) {
        (
            native @ ConversationSummary::NativeSession { .. },
            imported @ ConversationSummary::ImportedConversation { .. },
        )
        | (
            imported @ ConversationSummary::ImportedConversation { .. },
            native @ ConversationSummary::NativeSession { .. },
        ) => (native, imported),
        pair => {
            return Err(
                io::Error::other(format!("unexpected unified summary pair: {pair:?}")).into(),
            );
        }
    };
    assert!(matches!(
        native,
        ConversationSummary::NativeSession {
            session_id,
            title: None,
            archived: false,
            defaults_version,
        } if session_id == native_session && defaults_version.value() == 1
    ));
    assert!(matches!(
        imported,
        ConversationSummary::ImportedConversation {
            imported_conversation_id,
            title: Some(title),
            entry_count,
            source_format: ImportedConversationSourceFormat::CodexRolloutJsonlV1,
        } if imported_conversation_id == imported_id
            && title == "question"
            && entry_count.value() == 1
    ));

    drop(connection);
    runtime.stop().await
}

/// INV-033: a version-four metadata read returns the complete current wire
/// projection.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn inv033_version_four_reads_current_metadata_projection() -> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let first_session = create_alias_session(&mut connection).await?;
    let replacement = SessionMetadata::try_new(
        Some(String::from("Current plan")),
        vec![String::from("daily")],
        vec![(String::from("run"), String::from("17"))],
        false,
    )?;
    connection
        .request_version(
            ProtocolVersion::Four,
            10,
            ClientRequest::ReplaceSessionMetadata {
                command_id: command()?,
                session_id: first_session,
                metadata: replacement.clone(),
            },
        )
        .await?;
    assert!(matches!(
        response_within(&mut connection).await?.message(),
        ServerMessage::SessionMetadataReplaced { session_id, .. }
            if *session_id == first_session
    ));

    connection
        .request_version(
            ProtocolVersion::Four,
            18,
            ClientRequest::ReadSessionMetadata {
                session_id: first_session,
            },
        )
        .await?;
    assert!(matches!(
        response_within(&mut connection).await?.message(),
        ServerMessage::SessionMetadata {
            session_id,
            metadata,
            last_writer: Some(last_writer),
        } if *session_id == first_session
            && metadata == &replacement
            && matches!(last_writer.actor(), MetadataActor::Owner {})
    ));

    drop(connection);
    runtime.stop().await
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn inv033_metadata_read_maps_a_missing_session() -> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let absent = CanonicalUuid::from_uuid(Uuid::from_u128(0xdead));
    connection
        .request_version(
            ProtocolVersion::Four,
            19,
            ClientRequest::ReadSessionMetadata { session_id: absent },
        )
        .await?;
    assert!(matches!(
        response_within(&mut connection).await?.message(),
        ServerMessage::Error {
            code: ErrorCode::NotFound,
            ..
        }
    ));

    drop(connection);
    runtime.stop().await
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn inv033_metadata_replace_maps_a_missing_session() -> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let absent = CanonicalUuid::from_uuid(Uuid::from_u128(0xdead));
    connection
        .request_version(
            ProtocolVersion::Four,
            20,
            ClientRequest::ReplaceSessionMetadata {
                command_id: command()?,
                session_id: absent,
                metadata: SessionMetadata::empty(),
            },
        )
        .await?;
    assert!(matches!(
        response_within(&mut connection).await?.message(),
        ServerMessage::Error {
            code: ErrorCode::Rejected,
            detail,
            ..
        } if matches!(
            detail.value(),
            Some(RejectionDetail::SessionNotFound { session_id }) if session_id == absent
        )
    ));

    drop(connection);
    runtime.stop().await
}

/// INV-013: replacing an archived snapshot with `archived = false` returns the
/// same session to the default list.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn inv013_metadata_restore_returns_session_to_default_list() -> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let first_session = create_alias_session(&mut connection).await?;
    let archived = SessionMetadata::try_new(
        Some(String::from("Archived plan")),
        vec![String::from("work"), String::from("daily")],
        vec![(String::from("run"), String::from("17"))],
        true,
    )?;
    connection
        .request_version(
            ProtocolVersion::Four,
            10,
            ClientRequest::ReplaceSessionMetadata {
                command_id: command()?,
                session_id: first_session,
                metadata: archived,
            },
        )
        .await?;
    assert!(matches!(
        response_within(&mut connection).await?.message(),
        ServerMessage::SessionMetadataReplaced {
            session_id,
            metadata,
            ..
        } if *session_id == first_session && metadata.archived()
    ));

    let restored = SessionMetadata::try_new(
        Some(String::from("Archived plan")),
        vec![String::from("work"), String::from("daily")],
        vec![(String::from("run"), String::from("17"))],
        false,
    )?;
    connection
        .request_version(
            ProtocolVersion::Four,
            21,
            ClientRequest::ReplaceSessionMetadata {
                command_id: command()?,
                session_id: first_session,
                metadata: restored.clone(),
            },
        )
        .await?;
    assert!(matches!(
        response_within(&mut connection).await?.message(),
        ServerMessage::SessionMetadataReplaced {
            session_id,
            metadata,
            ..
        } if *session_id == first_session && metadata == &restored
    ));

    connection
        .request_version(
            ProtocolVersion::Four,
            22,
            ClientRequest::ListSessionMetadata {
                required_tags: vec![String::from("daily")],
                title_contains: Some(String::from("Archived")),
                include_archived: false,
                page_size: CanonicalU64::new(10),
                after_session_id: None,
            },
        )
        .await?;
    assert!(matches!(
        response_within(&mut connection).await?.message(),
        ServerMessage::SessionMetadataPageStart {}
    ));
    assert!(matches!(
        response_within(&mut connection).await?.message(),
        ServerMessage::SessionMetadataSummary {
            session_id,
            archived: false,
            ..
        } if *session_id == first_session
    ));
    assert!(matches!(
        response_within(&mut connection).await?.message(),
        ServerMessage::SessionMetadataPageEnd {
            session_count,
            next_after_session_id: None,
        } if session_count.value() == 1
    ));

    drop(connection);
    runtime.stop().await
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn s28_version_one_read_rejects_imported_session() -> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let session_id = create_imported_session(&runtime.pool).await?;

    let mut legacy_read = Connection::connect(runtime.socket()).await?;
    legacy_read
        .request(1, ClientRequest::ReadTranscript { session_id })
        .await?;
    let unsupported = response_within(&mut legacy_read).await?;
    assert_eq!(unsupported.version(), ProtocolVersion::One);
    assert!(matches!(
        unsupported.message(),
        ServerMessage::Error {
            code: ErrorCode::UnsupportedVersion,
            message,
            ..
        } if message.contains("version 2")
    ));

    drop(legacy_read);
    runtime.stop().await
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn s28_version_two_read_streams_conservative_imported_seed_snapshot()
-> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let session_id = create_imported_session(&runtime.pool).await?;

    let mut upgraded_read = Connection::connect(runtime.socket()).await?;
    upgraded_read
        .request_version(
            ProtocolVersion::Two,
            2,
            ClientRequest::ReadTranscript { session_id },
        )
        .await?;
    let start = response_within(&mut upgraded_read).await?;
    assert_eq!(start.version(), ProtocolVersion::Two);
    assert!(matches!(
        start.message(),
        ServerMessage::TranscriptSnapshotStart {
            session_id: selected,
            ..
        } if *selected == session_id
    ));
    let imported_text = response_within(&mut upgraded_read).await?;
    assert_eq!(imported_text.version(), ProtocolVersion::Two);
    assert!(matches!(
        imported_text.message(),
        ServerMessage::TranscriptTextEntry {
            entry_index,
            entry: TranscriptTextEntry::Imported {
                source_speaker: ImportedSourceSpeaker::Attested {
                    speaker: ImportedSpeaker::User,
                },
                ..
            },
            ..
        } if entry_index.value() == 0
    ));
    assert!(matches!(
        response_within(&mut upgraded_read).await?.message(),
        ServerMessage::TranscriptContent {
            entry_index,
            fragment_index,
            final_fragment: true,
            content_fragment,
        } if entry_index.value() == 0
            && fragment_index.value() == 0
            && content_fragment.as_str() == "imported user"
    ));
    assert!(matches!(
        response_within(&mut upgraded_read).await?.message(),
        ServerMessage::TranscriptEntry {
            entry_index,
            entry: TranscriptEntry::Imported {
                source_speaker: ImportedSourceSpeaker::Attested {
                    speaker: ImportedSpeaker::Assistant,
                },
                content_kind: ImportedContentKind::ToolCall,
                ..
            },
            ..
        } if entry_index.value() == 1
    ));
    let end = response_within(&mut upgraded_read).await?;
    assert_eq!(end.version(), ProtocolVersion::Two);
    assert!(matches!(
        end.message(),
        ServerMessage::TranscriptSnapshotEnd {
            turn_count,
            entry_count,
            ..
        } if turn_count.value() == 0 && entry_count.value() == 2
    ));

    drop(upgraded_read);
    runtime.stop().await
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn s28_version_one_submit_rejects_imported_session_without_mutation()
-> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let session_id = create_imported_session(&runtime.pool).await?;

    let mut legacy_submit = Connection::connect(runtime.socket()).await?;
    legacy_submit
        .request(
            3,
            ClientRequest::SubmitInput {
                command_id: command()?,
                session_id,
                content: InputContent::new(String::from("must not mutate")),
                expected_defaults_version: CanonicalU64::new(1),
            },
        )
        .await?;
    assert!(matches!(
        response_within(&mut legacy_submit).await?.message(),
        ServerMessage::Error {
            code: ErrorCode::UnsupportedVersion,
            ..
        }
    ));
    let turn_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM turn_lifecycle WHERE session_id = $1")
            .bind(session_id.into_uuid())
            .fetch_one(&runtime.pool)
            .await?;
    assert_eq!(turn_count, 0);

    drop(legacy_submit);
    runtime.stop().await
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn s28_version_two_submit_accepts_imported_session_continuation() -> Result<(), Box<dyn Error>>
{
    let runtime = RunningRuntime::start().await?;
    let session_id = create_imported_session(&runtime.pool).await?;

    let mut upgraded_submit = Connection::connect(runtime.socket()).await?;
    upgraded_submit
        .request_version(
            ProtocolVersion::Two,
            4,
            ClientRequest::SubmitInput {
                command_id: command()?,
                session_id,
                content: InputContent::new(String::from("native continuation")),
                expected_defaults_version: CanonicalU64::new(1),
            },
        )
        .await?;
    let accepted = response_within(&mut upgraded_submit).await?;
    assert_eq!(accepted.version(), ProtocolVersion::Two);
    assert!(matches!(
        accepted.message(),
        ServerMessage::InputSubmitted {
            session_id: submitted,
            ..
        } if *submitted == session_id
    ));

    drop(upgraded_submit);
    runtime.stop().await
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn process_runtime_rejects_oversized_submitted_input() -> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let session_id = create_alias_session(&mut connection).await?;

    connection
        .request(
            2,
            ClientRequest::SubmitInput {
                command_id: command()?,
                session_id,
                content: InputContent::new("x".repeat(OVERSIZED_SUBMITTED_INPUT_BYTES)),
                expected_defaults_version: CanonicalU64::new(1),
            },
        )
        .await?;

    let response = response_within(&mut connection).await?;
    assert!(matches!(
        response.message(),
        ServerMessage::Error {
            code: ErrorCode::InvalidRequest,
            ..
        }
    ));

    drop(connection);
    runtime.stop().await
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn process_runtime_admits_exact_limit_submitted_input() -> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let session_id = create_alias_session(&mut connection).await?;

    let _submitted = submit_first_input(
        &mut connection,
        session_id,
        "x".repeat(MAX_SUBMITTED_INPUT_BYTES),
    )
    .await?;

    drop(connection);
    runtime.stop().await
}

/// Parks the session's active turn on an ambiguous model call exactly as a
/// prior daemon incarnation would: the queued turn activates, its call is
/// authorized for send, and the next startup scan classifies the unobserved
/// issued call. The fixture writes no terminal state itself, so the parked
/// shape is the one a real restart leaves behind.
async fn park_turn_on_ambiguous_model_call(
    pool: &PgPool,
    session_id: CanonicalUuid,
) -> Result<(), Box<dyn Error>> {
    let session = SessionId::from_uuid(session_id.into_uuid());
    let mut activation = StartEligibleTurnService::new(
        UuidV7StartEligibleTurnIdGenerator,
        StartEligibleTurnRepository::new(pool.clone()),
    );
    let StartEligibleTurnOutcome::Activated(_) = activation.execute(session).await? else {
        return Err(io::Error::other("the queued fixture turn must activate").into());
    };

    let model_configuration = HubModelConfiguration::parse(MODEL_CONFIGURATION)?;
    let calls = PostgresModelCallRepository::new(
        pool.clone(),
        model_configuration.target_catalog(),
        ModelCallCredentialReference::new("scripted-reconciliation-test"),
    );
    let call = ModelCallId::from_uuid(Uuid::now_v7());
    let PrepareInitialModelCallOutcome::Checkpointed(_) = calls
        .prepare_initial_call(
            session,
            call,
            FailedModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::now_v7()),
                ContextFrontierId::from_uuid(Uuid::now_v7()),
            ),
            ContextFrontierId::from_uuid(Uuid::now_v7()),
            |_| {
                (
                    SemanticTranscriptEntryId::from_uuid(Uuid::now_v7()),
                    TurnId::from_uuid(Uuid::now_v7()),
                )
            },
        )
        .await?
    else {
        return Err(io::Error::other("the fixture call must checkpoint").into());
    };
    let AuthorizeModelCallOutcome::Authorized(_) = calls.authorize_send(session, call).await?
    else {
        return Err(io::Error::other("the fixture call must authorize send").into());
    };

    let mut scan = StartupScanService::new(
        UuidV7StartupScanIdGenerator,
        PostgresStartupScanRepository::new(pool.clone()),
    );
    let recovery = scan.execute().await?;
    assert_eq!(
        recovery.recovered_turn_count(),
        0,
        "an unobserved issued call parks its turn instead of terminalizing it"
    );
    Ok(())
}

/// S04 / S07 / INV-029: a turn parked on an ambiguous model call refuses
/// ordinary input until the owner reconciliation decision releases the slot.
///
/// The refusal and the release are one contract: proving the release means
/// nothing unless the same session is demonstrably wedged first, against the
/// same durable state in the same execution (testing-style rule 17).
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn s04_inv029_reconcile_turn_releases_a_wedged_ambiguous_session()
-> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let session_id = create_alias_session(&mut connection).await?;
    let (_, parked_turn_id) =
        submit_first_input(&mut connection, session_id, String::from("first request")).await?;
    park_turn_on_ambiguous_model_call(&runtime.pool, session_id).await?;

    connection
        .request_version(
            ProtocolVersion::Seven,
            3,
            ClientRequest::SubmitInput {
                command_id: command()?,
                session_id,
                content: InputContent::new(String::from("work while the ambiguity is unresolved")),
                expected_defaults_version: CanonicalU64::new(1),
            },
        )
        .await?;
    assert!(
        matches!(
            response_within(&mut connection).await?.message(),
            ServerMessage::Error {
                code: ErrorCode::Rejected,
                detail,
                ..
            } if detail.value() == Some(RejectionDetail::ActiveTurnPresent {
                session_id,
                active_turn_id: parked_turn_id,
            })
        ),
        "an ambiguity wait must keep refusing ordinary input while it holds the slot"
    );

    connection
        .request_version(
            ProtocolVersion::Seven,
            4,
            ClientRequest::ReconcileTurn {
                command_id: command()?,
                session_id,
                expected_active_turn_id: parked_turn_id,
                content: InputContent::new(String::from("continue after reconciliation")),
                expected_defaults_version: CanonicalU64::new(1),
            },
        )
        .await?;
    let successor_turn_id = accepted_successor_turn(&mut connection, session_id, 2).await?;
    assert_ne!(successor_turn_id, parked_turn_id);

    connection
        .request_version(
            ProtocolVersion::Seven,
            5,
            ClientRequest::ReadTranscript { session_id },
        )
        .await?;
    let start = response_within(&mut connection).await?;
    assert!(matches!(
        start.message(),
        ServerMessage::TranscriptSnapshotStart {
            session_id: snapshot_session,
            ..
        } if *snapshot_session == session_id
    ));
    let reconciled_turn = response_within(&mut connection).await?;
    assert!(matches!(
        reconciled_turn.message(),
        ServerMessage::TranscriptTurn {
            turn_id,
            acceptance_position,
            state: TurnState::ReconciliationRequired { .. },
        } if *turn_id == parked_turn_id && acceptance_position.value() == 1
    ));
    let successor_turn = response_within(&mut connection).await?;
    assert!(matches!(
        successor_turn.message(),
        ServerMessage::TranscriptTurn {
            turn_id,
            acceptance_position,
            state: TurnState::Queued { .. },
        } if *turn_id == successor_turn_id && acceptance_position.value() == 2
    ));

    drop(connection);
    runtime.stop().await
}

/// S04 / INV-029: the reconciliation request is refused, without recording a
/// command, for every turn that owes no reconciliation decision — so the verb
/// never becomes a general active-turn stop.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn s04_inv029_reconcile_turn_refuses_a_turn_that_owes_no_decision()
-> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let session_id = create_alias_session(&mut connection).await?;
    let (_, parked_turn_id) =
        submit_first_input(&mut connection, session_id, String::from("first request")).await?;
    park_turn_on_ambiguous_model_call(&runtime.pool, session_id).await?;

    let unparked_turn_id = CanonicalUuid::from_uuid(Uuid::from_u128(0xB1));
    connection
        .request_version(
            ProtocolVersion::Seven,
            3,
            ClientRequest::ReconcileTurn {
                command_id: command()?,
                session_id,
                expected_active_turn_id: unparked_turn_id,
                content: InputContent::new(String::from("names no parked turn")),
                expected_defaults_version: CanonicalU64::new(1),
            },
        )
        .await?;
    assert!(matches!(
        response_within(&mut connection).await?.message(),
        ServerMessage::Error {
            code: ErrorCode::Rejected,
            detail,
            ..
        } if detail.value() == Some(RejectionDetail::TurnNotAwaitingReconciliation {
            session_id,
            turn_id: unparked_turn_id,
        })
    ));

    connection
        .request_version(
            ProtocolVersion::Seven,
            4,
            ClientRequest::ReconcileTurn {
                command_id: command()?,
                session_id,
                expected_active_turn_id: parked_turn_id,
                content: InputContent::new(String::from("continue after reconciliation")),
                expected_defaults_version: CanonicalU64::new(1),
            },
        )
        .await?;
    accepted_successor_turn(&mut connection, session_id, 2).await?;

    connection
        .request_version(
            ProtocolVersion::Seven,
            5,
            ClientRequest::ReconcileTurn {
                command_id: command()?,
                session_id,
                expected_active_turn_id: parked_turn_id,
                content: InputContent::new(String::from("the decision is already recorded")),
                expected_defaults_version: CanonicalU64::new(1),
            },
        )
        .await?;
    assert!(matches!(
        response_within(&mut connection).await?.message(),
        ServerMessage::Error {
            code: ErrorCode::Rejected,
            detail,
            ..
        } if detail.value() == Some(RejectionDetail::TurnNotAwaitingReconciliation {
            session_id,
            turn_id: parked_turn_id,
        })
    ));

    drop(connection);
    runtime.stop().await
}

/// INV-012: a reconciliation decision that already committed replays its exact
/// recorded successor, because a claimed command identity reaches the durable
/// replay boundary before the current-state precondition is applied.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn inv012_reconcile_turn_replays_a_committed_decision() -> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let session_id = create_alias_session(&mut connection).await?;
    let (_, parked_turn_id) =
        submit_first_input(&mut connection, session_id, String::from("first request")).await?;
    park_turn_on_ambiguous_model_call(&runtime.pool, session_id).await?;

    let decision = ClientRequest::ReconcileTurn {
        command_id: command()?,
        session_id,
        expected_active_turn_id: parked_turn_id,
        content: InputContent::new(String::from("continue after reconciliation")),
        expected_defaults_version: CanonicalU64::new(1),
    };
    connection
        .request_version(ProtocolVersion::Seven, 3, decision.clone())
        .await?;
    let successor_turn_id = accepted_successor_turn(&mut connection, session_id, 2).await?;

    connection
        .request_version(ProtocolVersion::Seven, 4, decision)
        .await?;
    let replayed_turn_id = accepted_successor_turn(&mut connection, session_id, 2).await?;

    assert_eq!(
        replayed_turn_id, successor_turn_id,
        "an equal reconciliation retry returns its recorded successor, never a refusal"
    );

    drop(connection);
    runtime.stop().await
}

/// INV-012: two overlapping requests carrying one reconciliation command
/// identity both land on the committed decision.
///
/// The claim probe and the precondition read are separate statements, so the
/// loser can observe the wait already released; it must still reach the replay
/// boundary rather than the unrecorded refusal. Both halves are asserted in one
/// execution because the race is the requirement (testing-style rule 17).
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn inv012_overlapping_equal_reconciliations_both_reach_the_committed_decision()
-> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let mut setup = Connection::connect(runtime.socket()).await?;
    let session_id = create_alias_session(&mut setup).await?;
    let (_, parked_turn_id) =
        submit_first_input(&mut setup, session_id, String::from("first request")).await?;
    park_turn_on_ambiguous_model_call(&runtime.pool, session_id).await?;
    drop(setup);

    let decision = ClientRequest::ReconcileTurn {
        command_id: command()?,
        session_id,
        expected_active_turn_id: parked_turn_id,
        content: InputContent::new(String::from("continue after reconciliation")),
        expected_defaults_version: CanonicalU64::new(1),
    };
    let mut first = Connection::connect(runtime.socket()).await?;
    let mut second = Connection::connect(runtime.socket()).await?;
    first
        .request_version(ProtocolVersion::Seven, 1, decision.clone())
        .await?;
    second
        .request_version(ProtocolVersion::Seven, 1, decision)
        .await?;

    let first_turn_id = accepted_successor_turn(&mut first, session_id, 2).await?;
    let second_turn_id = accepted_successor_turn(&mut second, session_id, 2).await?;

    assert_eq!(
        second_turn_id, first_turn_id,
        "an equal identity that loses the admission race replays the committed successor"
    );
    assert_ne!(first_turn_id, parked_turn_id);

    drop(first);
    drop(second);
    runtime.stop().await
}

/// S04: an absent session is left to the authoritative transaction's recorded
/// `session_not_found`, not collapsed into the precondition refusal.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn s04_reconcile_turn_reports_an_absent_session_exactly() -> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let absent_session_id = CanonicalUuid::from_uuid(Uuid::from_u128(0xB2));

    connection
        .request_version(
            ProtocolVersion::Seven,
            1,
            ClientRequest::ReconcileTurn {
                command_id: command()?,
                session_id: absent_session_id,
                expected_active_turn_id: CanonicalUuid::from_uuid(Uuid::from_u128(0xB3)),
                content: InputContent::new(String::from("names no session")),
                expected_defaults_version: CanonicalU64::new(1),
            },
        )
        .await?;

    assert!(matches!(
        response_within(&mut connection).await?.message(),
        ServerMessage::Error {
            code: ErrorCode::Rejected,
            detail,
            ..
        } if detail.value() == Some(RejectionDetail::SessionNotFound {
            session_id: absent_session_id,
        })
    ));

    drop(connection);
    runtime.stop().await
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn process_runtime_reads_one_queued_transcript_snapshot() -> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let session_id = create_alias_session(&mut connection).await?;
    let content = "queued input".to_owned();
    let (accepted_input, turn) =
        submit_first_input(&mut connection, session_id, content.clone()).await?;

    connection
        .request(3, ClientRequest::ReadTranscript { session_id })
        .await?;

    let start = response_within(&mut connection).await?;
    assert!(matches!(
        start.message(),
        ServerMessage::TranscriptSnapshotStart {
            session_id: snapshot_session,
            cursor,
        } if *snapshot_session == session_id && cursor.value() == 2
    ));
    let queued_turn = response_within(&mut connection).await?;
    assert!(matches!(
        queued_turn.message(),
        ServerMessage::TranscriptTurn {
            turn_id,
            acceptance_position,
            state:
                TurnState::Queued {
                    accepted_input_id,
                    content: projected_content,
                },
        } if *turn_id == turn
            && acceptance_position.value() == 1
            && *accepted_input_id == accepted_input
            && projected_content.as_str() == content
    ));
    let end = response_within(&mut connection).await?;
    assert!(matches!(
        end.message(),
        ServerMessage::TranscriptSnapshotEnd {
            session_id: snapshot_session,
            cursor,
            turn_count,
            entry_count,
        } if *snapshot_session == session_id
            && cursor.value() == 2
            && turn_count.value() == 1
            && entry_count.value() == 0
    ));

    drop(connection);
    runtime.stop().await
}

/// S24 / INV-032: a follow subscription formed before its snapshot observes
/// the next committed outbox event strictly above that snapshot's cursor.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn s24_process_runtime_follow_snapshot_handoff_has_no_race() -> Result<(), Box<dyn Error>> {
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
    let follow_cursor = match follow.response().await?.message() {
        ServerMessage::TranscriptSnapshotStart {
            session_id: snapshot_session,
            cursor,
        } if *snapshot_session == session_id => cursor.value(),
        message => {
            return Err(io::Error::other(format!("unexpected follow start: {message:?}")).into());
        }
    };

    // The exact-limit queued content keeps the snapshot writer blocked after
    // its start frame. Commit the next update before draining the snapshot so
    // only a subscription formed before snapshot transmission can retain it.
    commands
        .request(
            6,
            ClientRequest::SubmitInput {
                command_id: command()?,
                session_id,
                content: InputContent::new("second input".to_owned()),
                expected_defaults_version: CanonicalU64::new(1),
            },
        )
        .await?;
    let second_accepted_input = match commands.response().await?.message() {
        ServerMessage::InputSubmitted {
            session_id: submitted_session,
            accepted_input_id,
            acceptance_position,
            ..
        } if *submitted_session == session_id && acceptance_position.value() == 2 => {
            *accepted_input_id
        }
        message => {
            return Err(io::Error::other(format!("unexpected second submit: {message:?}")).into());
        }
    };

    let queued_turn = response_within(&mut follow).await?;
    assert!(matches!(
        queued_turn.message(),
        ServerMessage::TranscriptTurn {
            turn_id,
            acceptance_position,
            state:
                TurnState::Queued {
                    accepted_input_id,
                    content: projected_content,
                },
        } if *turn_id == first_turn
            && acceptance_position.value() == 1
            && *accepted_input_id == first_accepted_input
            && projected_content.as_str() == first_content
    ));
    let snapshot_end = response_within(&mut follow).await?;
    assert!(matches!(
        snapshot_end.message(),
        ServerMessage::TranscriptSnapshotEnd {
            session_id: snapshot_session,
            cursor,
            turn_count,
            entry_count,
        } if *snapshot_session == session_id
            && cursor.value() == follow_cursor
            && turn_count.value() == 1
            && entry_count.value() == 0
    ));

    let followed = response_within(&mut follow).await?;
    assert!(matches!(
        followed.message(),
        ServerMessage::SessionEvent {
            cursor,
            session_id: event_session,
            event:
                SessionEvent::InputAccepted {
                    accepted_input_id,
                    acceptance_position,
                    content,
                    ..
                },
        } if cursor.value() > follow_cursor
            && *event_session == session_id
            && *accepted_input_id == second_accepted_input
            && acceptance_position.value() == 2
            && content.as_str() == "second input"
    ));

    drop(commands);
    drop(follow);
    runtime.stop().await
}

/// S01 / S02 / S24 / INV-032 / INV-035: the provider bridge asks the scripted
/// runtime for streamed delivery, and two already-attached version-twelve
/// followers each observe the exact already-redacted deltas before the durable
/// terminal entries expose the same complete assistant reply.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn s01_s02_s24_inv032_inv035_streamed_reply_reaches_two_followers_then_durable_truth()
-> Result<(), Box<dyn Error>> {
    let mut runtime = RunningRuntime::start().await?;
    let mut commands = Connection::connect(runtime.socket()).await?;
    let session_id = create_alias_session(&mut commands).await?;
    let first_follow =
        attach_empty_follower(runtime.socket(), ProtocolVersion::Twelve, 10, session_id).await?;
    let second_follow =
        attach_empty_follower(runtime.socket(), ProtocolVersion::Twelve, 11, session_id).await?;
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
    let first_durable = read_completed_assistant(runtime.socket(), 12, session_id, turn_id).await?;
    let second_durable =
        read_completed_assistant(runtime.socket(), 13, session_id, turn_id).await?;
    let operations = probe.received_operations();

    assert_eq!(first.delta_count, expected_delta_count);
    assert_eq!(first.text, assistant);
    assert_eq!(second.delta_count, expected_delta_count);
    assert_eq!(second.text, assistant);
    assert_eq!(first_durable, assistant);
    assert_eq!(second_durable, assistant);
    assert_eq!(operations.len(), 1);
    assert_eq!(operations[0].delivery, DeliveryMode::Streamed);

    drop(commands);
    runtime.stop().await
}

/// S24 / INV-032: a follower that cannot keep up with ephemeral provider
/// deltas receives the existing resynchronization error, loses some deltas,
/// and recovers the exact completed assistant reply from durable transcript
/// truth without any delta persistence or replay.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn s24_streaming_lag_resync_loses_deltas_and_reads_complete_transcript()
-> Result<(), Box<dyn Error>> {
    let mut runtime = RunningRuntime::start().await?;
    let mut commands = Connection::connect(runtime.socket()).await?;
    let session_id = create_alias_session(&mut commands).await?;
    let lagging_follow =
        attach_empty_follower(runtime.socket(), ProtocolVersion::Twelve, 20, session_id).await?;
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

/// S24 / INV-032 / INV-033: version-eleven followers remain on the durable-only
/// fan-out, so version-twelve delta volume neither leaks a newer message shape
/// nor causes legacy clients to lag behind their retained durable vocabulary.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn s24_inv032_inv033_streaming_volume_does_not_perturb_version_eleven_followers()
-> Result<(), Box<dyn Error>> {
    let mut runtime = RunningRuntime::start().await?;
    let mut commands = Connection::connect(runtime.socket()).await?;
    let session_id = create_alias_session(&mut commands).await?;
    let legacy_follow =
        attach_empty_follower(runtime.socket(), ProtocolVersion::Eleven, 30, session_id).await?;
    let (script, assistant) =
        streamed_script(STREAMING_DELTA_COUNT, "y".repeat(STREAMING_DELTA_BYTES));
    let (_, turn_id) = submit_first_input(
        &mut commands,
        session_id,
        String::from("retain the legacy durable stream"),
    )
    .await?;

    let probe = execute_streamed_turn(
        &mut runtime,
        ScriptedModel::single(script),
        session_id,
        turn_id,
    )
    .await?;
    let followed = follow_streamed_turn_to_completion(legacy_follow, session_id, turn_id).await?;
    let durable = read_completed_assistant(runtime.socket(), 31, session_id, turn_id).await?;
    let operations = probe.received_operations();

    assert_eq!(followed.delta_count, 0);
    assert!(followed.text.is_empty());
    assert_eq!(durable, assistant);
    assert_eq!(operations.len(), 1);
    assert_eq!(operations[0].delivery, DeliveryMode::Streamed);

    drop(commands);
    runtime.stop().await
}

/// Activates the session's queued turn, checkpoints its initial model call,
/// and authorizes its send, so the call is durably issued with no terminal
/// observation. Returns the repository and the authorized call for a later
/// observation binding.
async fn authorize_issued_model_call(
    pool: &PgPool,
    session_id: CanonicalUuid,
) -> Result<
    (
        PostgresModelCallRepository,
        signalbox_domain::AuthorizedModelCall,
        ModelCallId,
    ),
    Box<dyn Error>,
> {
    let session = SessionId::from_uuid(session_id.into_uuid());
    activate_turn(pool, session).await?;
    let targets = HubModelConfiguration::parse(MODEL_CONFIGURATION)?.target_catalog();
    let calls = PostgresModelCallRepository::new(
        pool.clone(),
        targets,
        ModelCallCredentialReference::new("turn-control-fixture"),
    );
    let call = ModelCallId::from_uuid(Uuid::now_v7());
    let PrepareInitialModelCallOutcome::Checkpointed(_) = calls
        .prepare_initial_call(
            session,
            call,
            FailedModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::now_v7()),
                ContextFrontierId::from_uuid(Uuid::now_v7()),
            ),
            ContextFrontierId::from_uuid(Uuid::now_v7()),
            |_| {
                (
                    SemanticTranscriptEntryId::from_uuid(Uuid::now_v7()),
                    TurnId::from_uuid(Uuid::now_v7()),
                )
            },
        )
        .await?
    else {
        return Err(io::Error::other("the fixture call must checkpoint").into());
    };
    let AuthorizeModelCallOutcome::Authorized(authorized) =
        calls.authorize_send(session, call).await?
    else {
        return Err(io::Error::other("the fixture call must authorize send").into());
    };
    Ok((calls, *authorized, call))
}

/// Commits a confirm-classified tool round over the issued fixture call, so
/// the active turn parks on the approval wait for the first named request.
async fn park_turn_on_tool_approval(
    pool: &PgPool,
    session_id: CanonicalUuid,
    request_ids: &[CanonicalUuid],
) -> Result<(), Box<dyn Error>> {
    let session = SessionId::from_uuid(session_id.into_uuid());
    let (calls, authorized, _) = authorize_issued_model_call(pool, session_id).await?;
    let response = ToolUsingAssistantResponse::try_from_parts(
        request_ids
            .iter()
            .map(|_| {
                AssistantResponsePart::ToolCall(ToolCallProposal::new(
                    ToolName::try_new(String::from("confirmed"))
                        .expect("the fixture tool name is valid"),
                    NormalizedToolArguments::try_from_provider_text(String::from("{}"))
                        .expect("the fixture arguments are bounded"),
                ))
            })
            .collect(),
    )
    .expect("the fixture proposals form a tool-using response");
    let observation = authorized
        .observation_correlation()
        .bind_terminal_observation(ModelCallTerminalObservation::CompletedWithTools { response });
    let identities = request_ids
        .iter()
        .map(|request_id| {
            ToolResponsePartIdentity::tool_call(
                SemanticTranscriptEntryId::from_uuid(Uuid::now_v7()),
                ToolRequestId::from_uuid(request_id.into_uuid()),
                InitialToolApproval::Confirm,
            )
        })
        .collect();
    let outcome = calls
        .apply_terminal_observation(
            session,
            observation,
            ModelCallTerminalIdentities::ToolRound(ToolRoundModelCallIdentities::new(
                identities,
                ContextFrontierId::from_uuid(Uuid::now_v7()),
                None,
            )),
            |_| panic!("the fixture has no pending steering to reclassify"),
        )
        .await?;
    let first_request = request_ids
        .first()
        .map(|request_id| ToolRequestId::from_uuid(request_id.into_uuid()));
    assert!(
        matches!(
            outcome,
            ModelCallTerminalOutcome::ToolRound(ref round)
                if matches!(
                    round.next_phase(),
                    ActiveTurnPhase::AwaitingApproval { request: waiting }
                        if Some(*waiting) == first_request
                )
        ),
        "the confirm-classified round must park on its first request"
    );
    Ok(())
}

/// Reads one complete transcript snapshot and returns every message between
/// its validated start and end frames.
async fn read_transcript_messages(
    connection: &mut Connection,
    request_id: u64,
    session_id: CanonicalUuid,
) -> Result<Vec<ServerMessage>, Box<dyn Error>> {
    connection
        .request_version(
            ProtocolVersion::Eight,
            request_id,
            ClientRequest::ReadTranscript { session_id },
        )
        .await?;
    let start = response_within(connection).await?;
    assert!(matches!(
        start.message(),
        ServerMessage::TranscriptSnapshotStart {
            session_id: snapshot_session,
            ..
        } if *snapshot_session == session_id
    ));
    let mut messages = Vec::new();
    loop {
        let frame = response_within(connection).await?;
        if let ServerMessage::TranscriptSnapshotEnd {
            session_id: end_session,
            ..
        } = frame.message()
        {
            assert_eq!(*end_session, session_id);
            return Ok(messages);
        }
        messages.push(frame.message().clone());
    }
}

#[track_caller]
fn turn_state_of(messages: &[ServerMessage], selected_turn: CanonicalUuid) -> TurnState {
    messages
        .iter()
        .find_map(|message| match message {
            ServerMessage::TranscriptTurn { turn_id, state, .. } if *turn_id == selected_turn => {
                Some(state.clone())
            }
            _ => None,
        })
        .unwrap_or_else(|| panic!("the snapshot must project turn {selected_turn}"))
}

#[track_caller]
fn cancellation_marker_count(messages: &[ServerMessage], cancelled_turn: CanonicalUuid) -> usize {
    messages
        .iter()
        .filter(|message| {
            matches!(
                message,
                ServerMessage::TranscriptEntry {
                    entry: TranscriptEntry::TurnCancelled { turn_id },
                    ..
                } if *turn_id == cancelled_turn
            )
        })
        .count()
}

#[track_caller]
fn tool_use_entry_names(messages: &[ServerMessage], request: CanonicalUuid) -> Vec<String> {
    messages
        .iter()
        .filter_map(|message| match message {
            ServerMessage::TranscriptEntry {
                entry:
                    TranscriptEntry::AssistantToolUse {
                        tool_request_id,
                        tool_name,
                        ..
                    },
                ..
            } if *tool_request_id == request => Some(tool_name.clone()),
            _ => None,
        })
        .collect()
}

#[track_caller]
fn tool_denied_entry_count(messages: &[ServerMessage], request: CanonicalUuid) -> usize {
    messages
        .iter()
        .filter(|message| {
            matches!(
                message,
                ServerMessage::TranscriptEntry {
                    entry: TranscriptEntry::ToolDenied {
                        tool_request_id,
                        ..
                    },
                    ..
                } if *tool_request_id == request
            )
        })
        .count()
}

#[track_caller]
fn rejected_detail(message: &ServerMessage) -> RejectionDetail {
    match message {
        ServerMessage::Error {
            code: ErrorCode::Rejected,
            detail,
            ..
        } => detail
            .value()
            .expect("a rejected error carries its typed detail"),
        message => panic!("fixture expected a rejected error, got {message:?}"),
    }
}

#[track_caller]
fn decided_receipt(message: &ServerMessage) -> (CanonicalUuid, ToolDecision) {
    match message {
        ServerMessage::ToolRequestDecided {
            tool_request_id,
            decision,
        } => (*tool_request_id, decision.clone()),
        message => panic!("fixture expected a decision receipt, got {message:?}"),
    }
}

/// S07 / INV-029: the stop verb applies the accepted interrupt treatment — a
/// running turn with no prepared call cancels directly through the existing
/// lifecycle while the stop's content becomes the queued immediate successor.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn s07_inv029_stop_turn_cancels_the_activated_turn_and_queues_its_successor()
-> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let session_id = create_alias_session(&mut connection).await?;
    let (_, stopped_turn_id) =
        submit_first_input(&mut connection, session_id, String::from("first request")).await?;
    activate_turn(&runtime.pool, SessionId::from_uuid(session_id.into_uuid())).await?;

    connection
        .request_version(
            ProtocolVersion::Eight,
            3,
            ClientRequest::StopTurn {
                command_id: command()?,
                session_id,
                expected_active_turn_id: stopped_turn_id,
                content: InputContent::new(String::from("continue after the stop")),
                expected_defaults_version: CanonicalU64::new(1),
            },
        )
        .await?;
    let successor_turn_id = accepted_successor_turn(&mut connection, session_id, 2).await?;
    assert_ne!(successor_turn_id, stopped_turn_id);

    let messages = read_transcript_messages(&mut connection, 4, session_id).await?;
    assert!(matches!(
        turn_state_of(&messages, stopped_turn_id),
        TurnState::Cancelled {
            terminal_model_call_id: None,
            ..
        }
    ));
    assert!(matches!(
        turn_state_of(&messages, successor_turn_id),
        TurnState::Queued { content, .. } if content.as_str() == "continue after the stop"
    ));
    assert_eq!(cancellation_marker_count(&messages, stopped_turn_id), 1);

    drop(connection);
    runtime.stop().await
}

/// S07 / INV-029: stopping an issued call records the durable cancellation
/// request and retains the slot for lifecycle closure, and a distinct second
/// stop is refused with the exact prior stop authority named.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn s07_inv029_stop_turn_requests_cancellation_of_an_issued_call_exactly_once()
-> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let session_id = create_alias_session(&mut connection).await?;
    let (_, stopped_turn_id) =
        submit_first_input(&mut connection, session_id, String::from("first request")).await?;
    let (_, _, issued_call) = authorize_issued_model_call(&runtime.pool, session_id).await?;
    let first_stop_command = command()?;

    connection
        .request_version(
            ProtocolVersion::Eight,
            3,
            ClientRequest::StopTurn {
                command_id: first_stop_command,
                session_id,
                expected_active_turn_id: stopped_turn_id,
                content: InputContent::new(String::from("continue after the stop")),
                expected_defaults_version: CanonicalU64::new(1),
            },
        )
        .await?;
    let successor_turn_id = accepted_successor_turn(&mut connection, session_id, 2).await?;
    assert_ne!(successor_turn_id, stopped_turn_id);

    let messages = read_transcript_messages(&mut connection, 4, session_id).await?;
    assert!(matches!(
        turn_state_of(&messages, stopped_turn_id),
        TurnState::ActiveRunning {
            current_model_call: Some(call),
            ..
        } if call.model_call_id().into_uuid() == issued_call.into_uuid()
            && call.state() == CurrentModelCallState::CancellationRequested {}
    ));
    assert!(matches!(
        turn_state_of(&messages, successor_turn_id),
        TurnState::Queued { .. }
    ));

    connection
        .request_version(
            ProtocolVersion::Eight,
            5,
            ClientRequest::StopTurn {
                command_id: command()?,
                session_id,
                expected_active_turn_id: stopped_turn_id,
                content: InputContent::new(String::from("a second distinct stop")),
                expected_defaults_version: CanonicalU64::new(1),
            },
        )
        .await?;
    assert_eq!(
        rejected_detail(response_within(&mut connection).await?.message()),
        RejectionDetail::InterruptAlreadyApplied {
            session_id,
            active_turn_id: stopped_turn_id,
            existing_command_id: CanonicalUuid::from_uuid(first_stop_command.into_uuid()),
        }
    );

    drop(connection);
    runtime.stop().await
}

/// S07: every stop refusal is a recorded typed rejection — an empty session
/// records `no_active_turn` and a stale expected turn records
/// `active_turn_mismatch`.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn s07_stop_turn_refusals_are_typed_and_exact() -> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let session_id = create_alias_session(&mut connection).await?;
    let unstarted_turn_id = CanonicalUuid::from_uuid(Uuid::from_u128(0xC1));

    connection
        .request_version(
            ProtocolVersion::Eight,
            2,
            ClientRequest::StopTurn {
                command_id: command()?,
                session_id,
                expected_active_turn_id: unstarted_turn_id,
                content: InputContent::new(String::from("names no active turn")),
                expected_defaults_version: CanonicalU64::new(1),
            },
        )
        .await?;
    assert_eq!(
        rejected_detail(response_within(&mut connection).await?.message()),
        RejectionDetail::NoActiveTurn {
            session_id,
            expected_active_turn_id: unstarted_turn_id,
        }
    );

    let (_, active_turn_id) =
        submit_first_input(&mut connection, session_id, String::from("first request")).await?;
    activate_turn(&runtime.pool, SessionId::from_uuid(session_id.into_uuid())).await?;
    connection
        .request_version(
            ProtocolVersion::Eight,
            4,
            ClientRequest::StopTurn {
                command_id: command()?,
                session_id,
                expected_active_turn_id: unstarted_turn_id,
                content: InputContent::new(String::from("names a stale turn")),
                expected_defaults_version: CanonicalU64::new(1),
            },
        )
        .await?;
    assert_eq!(
        rejected_detail(response_within(&mut connection).await?.message()),
        RejectionDetail::ActiveTurnMismatch {
            session_id,
            expected_active_turn_id: unstarted_turn_id,
            active_turn_id,
        }
    );

    drop(connection);
    runtime.stop().await
}

/// INV-012: an equal stop replay returns its recorded successor, never a
/// second interrupt or a refusal.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn inv012_stop_turn_replays_its_recorded_successor() -> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let session_id = create_alias_session(&mut connection).await?;
    let (_, stopped_turn_id) =
        submit_first_input(&mut connection, session_id, String::from("first request")).await?;
    activate_turn(&runtime.pool, SessionId::from_uuid(session_id.into_uuid())).await?;

    let decision = ClientRequest::StopTurn {
        command_id: command()?,
        session_id,
        expected_active_turn_id: stopped_turn_id,
        content: InputContent::new(String::from("continue after the stop")),
        expected_defaults_version: CanonicalU64::new(1),
    };
    connection
        .request_version(ProtocolVersion::Eight, 3, decision.clone())
        .await?;
    let successor_turn_id = accepted_successor_turn(&mut connection, session_id, 2).await?;

    connection
        .request_version(ProtocolVersion::Eight, 4, decision)
        .await?;
    let replayed_turn_id = accepted_successor_turn(&mut connection, session_id, 2).await?;

    assert_eq!(
        replayed_turn_id, successor_turn_id,
        "an equal stop retry returns its recorded successor"
    );

    drop(connection);
    runtime.stop().await
}

/// S07 / S10 / INV-029: a stop racing an active tool round never wedges the
/// session. Against the parked approval wait the stop is refused fail-closed
/// with the wait intact; after the pending request is denied through its
/// canonical decision command, the stop cancels the turn with the denial
/// recorded, and the session accepts ordinary later input whose transcript
/// replays cleanly.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn s07_s10_inv029_stop_against_a_tool_round_stays_fail_closed_then_deny_and_stop_release()
-> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let session_id = create_alias_session(&mut connection).await?;
    let (_, parked_turn_id) =
        submit_first_input(&mut connection, session_id, String::from("first request")).await?;
    let pending_request_id = CanonicalUuid::from_uuid(Uuid::from_u128(0xD1));
    park_turn_on_tool_approval(&runtime.pool, session_id, &[pending_request_id]).await?;

    connection
        .request_version(
            ProtocolVersion::Eight,
            3,
            ClientRequest::StopTurn {
                command_id: command()?,
                session_id,
                expected_active_turn_id: parked_turn_id,
                content: InputContent::new(String::from("stop during the approval wait")),
                expected_defaults_version: CanonicalU64::new(1),
            },
        )
        .await?;
    assert_eq!(
        rejected_detail(response_within(&mut connection).await?.message()),
        RejectionDetail::InterruptUnavailableWhileAwaitingApproval {
            session_id,
            active_turn_id: parked_turn_id,
        }
    );

    let parked = read_transcript_messages(&mut connection, 4, session_id).await?;
    assert!(matches!(
        turn_state_of(&parked, parked_turn_id),
        TurnState::ActiveAwaitingToolApproval { tool_request_id }
            if tool_request_id == pending_request_id
    ));
    assert_eq!(
        tool_use_entry_names(&parked, pending_request_id),
        vec![String::from("confirmed")],
        "the pending request's identity and tool name are client-visible"
    );

    connection
        .request_version(
            ProtocolVersion::Eight,
            5,
            ClientRequest::DecideToolRequest {
                command_id: command()?,
                session_id,
                tool_request_id: pending_request_id,
                decision: ToolDecision::Deny {
                    reason: String::from("stop the tool round"),
                },
            },
        )
        .await?;
    assert_eq!(
        decided_receipt(response_within(&mut connection).await?.message()),
        (
            pending_request_id,
            ToolDecision::Deny {
                reason: String::from("stop the tool round"),
            }
        )
    );

    connection
        .request_version(
            ProtocolVersion::Eight,
            6,
            ClientRequest::StopTurn {
                command_id: command()?,
                session_id,
                expected_active_turn_id: parked_turn_id,
                content: InputContent::new(String::from("continue after the denied round")),
                expected_defaults_version: CanonicalU64::new(1),
            },
        )
        .await?;
    let successor_turn_id = accepted_successor_turn(&mut connection, session_id, 2).await?;

    connection
        .request_version(
            ProtocolVersion::Eight,
            7,
            ClientRequest::SubmitInput {
                command_id: command()?,
                session_id,
                content: InputContent::new(String::from("ordinary later work")),
                expected_defaults_version: CanonicalU64::new(1),
            },
        )
        .await?;
    let later_turn_id = accepted_successor_turn(&mut connection, session_id, 3).await?;

    let released = read_transcript_messages(&mut connection, 8, session_id).await?;
    assert!(matches!(
        turn_state_of(&released, parked_turn_id),
        TurnState::Cancelled { .. }
    ));
    assert_eq!(tool_denied_entry_count(&released, pending_request_id), 1);
    assert_eq!(cancellation_marker_count(&released, parked_turn_id), 1);
    assert!(matches!(
        turn_state_of(&released, successor_turn_id),
        TurnState::Queued { .. }
    ));
    assert!(matches!(
        turn_state_of(&released, later_turn_id),
        TurnState::Queued { .. }
    ));

    drop(connection);
    runtime.stop().await
}

/// S10: a decision naming a later request while an earlier one is undecided
/// records the exact proposal-order rejection.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn s10_decide_tool_request_refuses_a_later_request_first() -> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let session_id = create_alias_session(&mut connection).await?;
    submit_first_input(&mut connection, session_id, String::from("first request")).await?;
    let first_request_id = CanonicalUuid::from_uuid(Uuid::from_u128(0xE1));
    let second_request_id = CanonicalUuid::from_uuid(Uuid::from_u128(0xE2));
    park_turn_on_tool_approval(
        &runtime.pool,
        session_id,
        &[first_request_id, second_request_id],
    )
    .await?;

    connection
        .request_version(
            ProtocolVersion::Eight,
            3,
            ClientRequest::DecideToolRequest {
                command_id: command()?,
                session_id,
                tool_request_id: second_request_id,
                decision: ToolDecision::Approve {},
            },
        )
        .await?;
    assert_eq!(
        rejected_detail(response_within(&mut connection).await?.message()),
        RejectionDetail::ToolRequestNotEarliestUndecided {
            tool_request_id: second_request_id,
            earliest_tool_request_id: first_request_id,
        }
    );

    drop(connection);
    runtime.stop().await
}

/// S10: an unknown logical request records the exact absent-request rejection.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn s10_decide_tool_request_reports_an_unknown_request() -> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let session_id = create_alias_session(&mut connection).await?;
    submit_first_input(&mut connection, session_id, String::from("first request")).await?;
    let pending_request_id = CanonicalUuid::from_uuid(Uuid::from_u128(0xE1));
    park_turn_on_tool_approval(&runtime.pool, session_id, &[pending_request_id]).await?;

    let unknown_request_id = CanonicalUuid::from_uuid(Uuid::from_u128(0xE3));
    connection
        .request_version(
            ProtocolVersion::Eight,
            3,
            ClientRequest::DecideToolRequest {
                command_id: command()?,
                session_id,
                tool_request_id: unknown_request_id,
                decision: ToolDecision::Approve {},
            },
        )
        .await?;
    assert_eq!(
        rejected_detail(response_within(&mut connection).await?.message()),
        RejectionDetail::ToolRequestNotFound {
            tool_request_id: unknown_request_id,
        }
    );

    drop(connection);
    runtime.stop().await
}

/// S10: the session-correlation precondition refuses a decision whose named
/// session does not own the named request, before any durable command is
/// recorded.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn s10_decide_tool_request_refuses_a_misrouted_session_without_recording()
-> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let session_id = create_alias_session(&mut connection).await?;
    submit_first_input(&mut connection, session_id, String::from("first request")).await?;
    let pending_request_id = CanonicalUuid::from_uuid(Uuid::from_u128(0xE1));
    park_turn_on_tool_approval(&runtime.pool, session_id, &[pending_request_id]).await?;

    let mut foreign = Connection::connect(runtime.socket()).await?;
    let foreign_session_id = create_alias_session(&mut foreign).await?;
    let misrouted_command = command()?;
    foreign
        .request_version(
            ProtocolVersion::Eight,
            2,
            ClientRequest::DecideToolRequest {
                command_id: misrouted_command,
                session_id: foreign_session_id,
                tool_request_id: pending_request_id,
                decision: ToolDecision::Approve {},
            },
        )
        .await?;
    assert_eq!(
        rejected_detail(response_within(&mut foreign).await?.message()),
        RejectionDetail::ToolRequestNotInSession {
            session_id: foreign_session_id,
            tool_request_id: pending_request_id,
        }
    );
    let misrouted_claim_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM durable_command WHERE command_id = $1")
            .bind(misrouted_command.into_uuid())
            .fetch_one(&runtime.pool)
            .await?;
    assert_eq!(
        misrouted_claim_count, 0,
        "the session-correlation refusal must record no durable command"
    );

    drop(foreign);
    drop(connection);
    runtime.stop().await
}

/// S10: a denial reason outside the domain contract is refused as an invalid
/// request before any durable command is recorded.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn s10_decide_tool_request_refuses_an_unsafe_denial_reason_before_recording()
-> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let session_id = create_alias_session(&mut connection).await?;
    submit_first_input(&mut connection, session_id, String::from("first request")).await?;
    let pending_request_id = CanonicalUuid::from_uuid(Uuid::from_u128(0xE1));
    park_turn_on_tool_approval(&runtime.pool, session_id, &[pending_request_id]).await?;

    let unsafe_reason_command = command()?;
    connection
        .request_version(
            ProtocolVersion::Eight,
            3,
            ClientRequest::DecideToolRequest {
                command_id: unsafe_reason_command,
                session_id,
                tool_request_id: pending_request_id,
                decision: ToolDecision::Deny {
                    reason: String::from(" padded "),
                },
            },
        )
        .await?;
    assert!(matches!(
        response_within(&mut connection).await?.message(),
        ServerMessage::Error {
            code: ErrorCode::InvalidRequest,
            ..
        }
    ));
    let unsafe_claim_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM durable_command WHERE command_id = $1")
            .bind(unsafe_reason_command.into_uuid())
            .fetch_one(&runtime.pool)
            .await?;
    assert_eq!(unsafe_claim_count, 0);

    drop(connection);
    runtime.stop().await
}

/// INV-012: one durable decision identity has one recorded meaning — an equal
/// replay returns the exact recorded receipt, a different payload under the
/// same identity is conflicting reuse, and reusing an identity claimed by
/// another command kind is conflicting reuse too. The steps share one recorded
/// command, so they are asserted against the same durable state in one
/// execution.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn inv012_decide_tool_request_replays_equally_and_refuses_conflicting_reuse()
-> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let session_id = create_alias_session(&mut connection).await?;
    submit_first_input(&mut connection, session_id, String::from("first request")).await?;
    let pending_request_id = CanonicalUuid::from_uuid(Uuid::from_u128(0xE1));
    park_turn_on_tool_approval(&runtime.pool, session_id, &[pending_request_id]).await?;

    let denial_command = command()?;
    let denial = ClientRequest::DecideToolRequest {
        command_id: denial_command,
        session_id,
        tool_request_id: pending_request_id,
        decision: ToolDecision::Deny {
            reason: String::from("writes outside the workspace"),
        },
    };
    connection
        .request_version(ProtocolVersion::Eight, 3, denial.clone())
        .await?;
    assert_eq!(
        decided_receipt(response_within(&mut connection).await?.message()),
        (
            pending_request_id,
            ToolDecision::Deny {
                reason: String::from("writes outside the workspace"),
            }
        )
    );

    connection
        .request_version(ProtocolVersion::Eight, 4, denial)
        .await?;
    assert_eq!(
        decided_receipt(response_within(&mut connection).await?.message()),
        (
            pending_request_id,
            ToolDecision::Deny {
                reason: String::from("writes outside the workspace"),
            }
        ),
        "an equal decision replay returns its exact recorded receipt"
    );

    connection
        .request_version(
            ProtocolVersion::Eight,
            5,
            ClientRequest::DecideToolRequest {
                command_id: denial_command,
                session_id,
                tool_request_id: pending_request_id,
                decision: ToolDecision::Approve {},
            },
        )
        .await?;
    assert!(matches!(
        response_within(&mut connection).await?.message(),
        ServerMessage::Error {
            code: ErrorCode::ConflictingReuse,
            ..
        }
    ));

    let submit_command = command()?;
    connection
        .request_version(
            ProtocolVersion::Eight,
            6,
            ClientRequest::SubmitInput {
                command_id: submit_command,
                session_id,
                content: InputContent::new(String::from("claims a submit identity")),
                expected_defaults_version: CanonicalU64::new(1),
            },
        )
        .await?;
    assert!(matches!(
        rejected_detail(response_within(&mut connection).await?.message()),
        RejectionDetail::ActiveTurnPresent { .. }
    ));
    connection
        .request_version(
            ProtocolVersion::Eight,
            7,
            ClientRequest::DecideToolRequest {
                command_id: submit_command,
                session_id,
                tool_request_id: pending_request_id,
                decision: ToolDecision::Approve {},
            },
        )
        .await?;
    assert!(
        matches!(
            response_within(&mut connection).await?.message(),
            ServerMessage::Error {
                code: ErrorCode::ConflictingReuse,
                ..
            }
        ),
        "an identity claimed by another command kind is conflicting reuse"
    );

    drop(connection);
    runtime.stop().await
}

/// S10: the final approval opens the executing phase.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn s10_decide_tool_request_final_approval_opens_the_executing_phase()
-> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let session_id = create_alias_session(&mut connection).await?;
    let (_, decided_turn_id) =
        submit_first_input(&mut connection, session_id, String::from("first request")).await?;
    let pending_request_id = CanonicalUuid::from_uuid(Uuid::from_u128(0xE1));
    park_turn_on_tool_approval(&runtime.pool, session_id, &[pending_request_id]).await?;

    connection
        .request_version(
            ProtocolVersion::Eight,
            3,
            ClientRequest::DecideToolRequest {
                command_id: command()?,
                session_id,
                tool_request_id: pending_request_id,
                decision: ToolDecision::Approve {},
            },
        )
        .await?;
    assert_eq!(
        decided_receipt(response_within(&mut connection).await?.message()),
        (pending_request_id, ToolDecision::Approve {})
    );

    let decided = read_transcript_messages(&mut connection, 4, session_id).await?;
    assert!(
        matches!(
            turn_state_of(&decided, decided_turn_id),
            TurnState::ActiveRunning { .. }
        ),
        "the final approval opens the executing phase"
    );

    drop(connection);
    runtime.stop().await
}

/// S10: a request that already has a terminal resolution records the exact
/// already-resolved rejection for a later distinct decision.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn s10_decide_tool_request_refuses_an_already_resolved_request() -> Result<(), Box<dyn Error>>
{
    let runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let session_id = create_alias_session(&mut connection).await?;
    submit_first_input(&mut connection, session_id, String::from("first request")).await?;
    let pending_request_id = CanonicalUuid::from_uuid(Uuid::from_u128(0xE1));
    park_turn_on_tool_approval(&runtime.pool, session_id, &[pending_request_id]).await?;

    connection
        .request_version(
            ProtocolVersion::Eight,
            3,
            ClientRequest::DecideToolRequest {
                command_id: command()?,
                session_id,
                tool_request_id: pending_request_id,
                decision: ToolDecision::Approve {},
            },
        )
        .await?;
    assert_eq!(
        decided_receipt(response_within(&mut connection).await?.message()),
        (pending_request_id, ToolDecision::Approve {})
    );

    connection
        .request_version(
            ProtocolVersion::Eight,
            4,
            ClientRequest::DecideToolRequest {
                command_id: command()?,
                session_id,
                tool_request_id: pending_request_id,
                decision: ToolDecision::Approve {},
            },
        )
        .await?;
    assert_eq!(
        rejected_detail(response_within(&mut connection).await?.message()),
        RejectionDetail::ToolRequestAlreadyResolved {
            tool_request_id: pending_request_id,
        }
    );

    drop(connection);
    runtime.stop().await
}

/// S34 / INV-012 / INV-033 / INV-046: version nine creates a
/// prompted session, reads exact current and named defaults epochs, replaces
/// the prompt forward-only with the complete installed echo, and gates a
/// pre-nine replacement while the current epoch carries a prompt without
/// claiming its command identity.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn s34_inv012_inv033_inv046_process_runtime_carries_the_session_system_prompt()
-> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let prompt = SystemPromptText::try_new(String::from("exact review instructions"))
        .expect("test prompt is admissible");
    let selection = CanonicalUuid::from_uuid(Uuid::from_u128(1));

    connection
        .request_version(
            ProtocolVersion::Nine,
            1,
            ClientRequest::CreateSession {
                command_id: command()?,
                initial_model_selection: ModelSelection::Direct {
                    selection_id: selection,
                },
                system_prompt: SystemPromptMember::present(Some(prompt.clone())),
            },
        )
        .await?;
    let ServerMessage::SessionCreated { session_id } =
        *response_within(&mut connection).await?.message()
    else {
        panic!("prompted creation must return its session");
    };

    connection
        .request_version(
            ProtocolVersion::Nine,
            2,
            ClientRequest::ReadSessionDefaults {
                session_id,
                defaults_version: None,
            },
        )
        .await?;
    assert_eq!(
        response_within(&mut connection).await?.message(),
        &ServerMessage::SessionDefaults {
            session_id,
            defaults_version: CanonicalU64::new(1),
            model_selection: ModelSelection::Direct {
                selection_id: selection,
            },
            dangerous_tool_auto_approval: false,
            system_prompt: Some(prompt.clone()),
        }
    );

    // A version-six replacement cannot represent the present prompt it would
    // silently clear; the gate refuses it before any command is recorded.
    let gated_command = command()?;
    connection
        .request_version(
            ProtocolVersion::Six,
            3,
            ClientRequest::ReplaceSessionDefaults {
                command_id: gated_command,
                session_id,
                expected_defaults_version: CanonicalU64::new(1),
                model_selection: ModelSelection::Direct {
                    selection_id: selection,
                },
                dangerous_tool_auto_approval: false,
                system_prompt: SystemPromptMember::absent(),
            },
        )
        .await?;
    let gated = response_within(&mut connection).await?;
    let ServerMessage::Error {
        code: gated_code,
        message: gated_message,
        ..
    } = gated.message()
    else {
        panic!("the pre-nine replacement on a prompted session must be refused");
    };
    assert_eq!(*gated_code, ErrorCode::UnsupportedVersion);
    assert_eq!(
        gated_message,
        "the selected session requires protocol version 9"
    );
    let gated_claim_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM durable_command WHERE command_id = $1")
            .bind(gated_command.into_uuid())
            .fetch_one(&runtime.pool)
            .await?;
    assert_eq!(gated_claim_count, 0);

    // A version-nine replacement states the complete successor explicitly,
    // clearing the prompt, and its receipt echoes the complete install.
    connection
        .request_version(
            ProtocolVersion::Nine,
            4,
            ClientRequest::ReplaceSessionDefaults {
                command_id: command()?,
                session_id,
                expected_defaults_version: CanonicalU64::new(1),
                model_selection: ModelSelection::Direct {
                    selection_id: selection,
                },
                dangerous_tool_auto_approval: false,
                system_prompt: SystemPromptMember::present(None),
            },
        )
        .await?;
    assert_eq!(
        response_within(&mut connection).await?.message(),
        &ServerMessage::SessionDefaultsReplaced {
            session_id,
            defaults_version: CanonicalU64::new(2),
            model_selection: ModelSelection::Direct {
                selection_id: selection,
            },
            dangerous_tool_auto_approval: false,
            system_prompt: SystemPromptMember::present(None),
        }
    );

    // With no prompt on the current epoch, the older vocabulary is admitted
    // again and the prompted history stays readable at its exact epoch.
    connection
        .request_version(
            ProtocolVersion::Six,
            5,
            ClientRequest::ReplaceSessionDefaults {
                command_id: command()?,
                session_id,
                expected_defaults_version: CanonicalU64::new(2),
                model_selection: ModelSelection::Direct {
                    selection_id: selection,
                },
                dangerous_tool_auto_approval: false,
                system_prompt: SystemPromptMember::absent(),
            },
        )
        .await?;
    assert_eq!(
        replaced_defaults(response_within(&mut connection).await?.message()),
        (session_id, 3)
    );
    connection
        .request_version(
            ProtocolVersion::Nine,
            6,
            ClientRequest::ReadSessionDefaults {
                session_id,
                defaults_version: Some(CanonicalU64::new(1)),
            },
        )
        .await?;
    assert_eq!(
        response_within(&mut connection).await?.message(),
        &ServerMessage::SessionDefaults {
            session_id,
            defaults_version: CanonicalU64::new(1),
            model_selection: ModelSelection::Direct {
                selection_id: selection,
            },
            dangerous_tool_auto_approval: false,
            system_prompt: Some(prompt),
        }
    );

    connection
        .request_version(
            ProtocolVersion::Nine,
            7,
            ClientRequest::ReadSessionDefaults {
                session_id,
                defaults_version: Some(CanonicalU64::new(99)),
            },
        )
        .await?;
    assert_eq!(
        protocol_error_code(response_within(&mut connection).await?.message()),
        ErrorCode::NotFound
    );
    connection
        .request_version(
            ProtocolVersion::Nine,
            8,
            ClientRequest::ReadSessionDefaults {
                session_id: CanonicalUuid::from_uuid(Uuid::from_u128(0xdead)),
                defaults_version: None,
            },
        )
        .await?;
    assert_eq!(
        protocol_error_code(response_within(&mut connection).await?.message()),
        ErrorCode::NotFound
    );

    drop(connection);
    runtime.stop().await
}
