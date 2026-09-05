#![allow(
    clippy::expect_used,
    clippy::panic,
    reason = "the standalone integration test uses assertion panics and explicit fixture expectations"
)]

mod support;

use std::{
    collections::{HashSet, VecDeque},
    error::Error,
    fs,
    future::{Future, pending},
    io::{self, ErrorKind},
    os::unix::fs::PermissionsExt,
    panic::{AssertUnwindSafe, resume_unwind},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use futures_util::FutureExt;
use signalbox_application::{
    AuthorizeModelCallOutcome, ClassifyOperatorFailure,
    CreateSessionFromImportedFrontierIdGenerator, CreateSessionFromImportedFrontierOutcome,
    CreateSessionFromImportedFrontierRequest, CreateSessionFromImportedFrontierService,
    EligibilityPass, EligibilitySweep, EligibilitySweepBatch, ImportConversationOutcome,
    ImportConversationService, ImportedConversationIdGenerator, InProcessAttemptDispatchGate,
    InProcessEligibilityNudge, InProcessEligibilityWorkSource, InProcessToolDispatchGate,
    ModelCallCredentialReference, ModelCallExecutionOutcome, ModelCallExecutionService,
    ModelCallInputTokenCount, ModelCallInputTokenCounter, NoToolCatalog, OperatorFailureClass,
    PreparedModelOperation, ReplaceSessionMetadataOutcome, ReplaceSessionMetadataRequest,
    ReplaceSessionMetadataService, SchedulerLoop, SchedulerLoopExit, SchedulerPassExpiryHandler,
    SchedulerPassOccupancyBound, ScriptedModelCallProvider, ScriptedModelCallStep,
    StaleActiveTurnBound, StartEligibleTurnOutcome, StartEligibleTurnService, StartupScanService,
    TurnLivenessScanInterval, UuidV7ModelCallExecutionIdGenerator,
    UuidV7StartEligibleTurnIdGenerator, UuidV7StartupScanIdGenerator,
    scheduler_ordinary_pass_limit,
};
use signalbox_blob_store::BlobObjectKey;
use signalbox_conversation_import_claude_code::ClaudeCodeJsonlConverter;
use signalbox_domain::{
    ActiveTurnPhase, Actor, AssistantResponsePart, AssistantText, BlobDigest, ContextCompactionId,
    ContextCompactionTokenUsage, ContextFrontierId, DirectModelSelection, DurableCommandId,
    FailedModelCallTurnIdentities, ImportedConversationFormat, ImportedConversationId,
    ImportedSessionRelationship, ImportedTranscriptEntryId, InitialToolApproval, ModelCallId,
    ModelCallTerminalIdentities, ModelCallTerminalObservation, ModelCallTerminalOutcome,
    ModelSelectionRequest, ModelTargetCatalog, NormalizedToolArguments,
    PhysicalCancellationModelCallTurnIdentities, ProviderModelIdentity,
    ReplaceSessionMetadataResult, ResolvedProviderTarget, SemanticTranscriptEntryId,
    SessionConfigurationDefaults, SessionConfigurationDefaultsVersion, SessionId,
    SessionMetadataContent, ToolCallProposal, ToolName, ToolRequestId, ToolResponsePartIdentity,
    ToolRoundModelCallIdentities, ToolUsingAssistantResponse, TurnId,
};
use signalbox_model_provider_runtime::{
    RuntimeContextCompactionModel, RuntimeInputTokenCountError, RuntimeModelCallProvider,
    RuntimeModelCallProviderError,
};
use signalbox_model_runtime::{
    AssistantPart, BoundaryLossEvidence, CancellationSignal, CompletionEvidence, CompletionFinish,
    DeliveryMode, ExchangeFacts, InputTokenCountOutcome, LossCause, MessagePart,
    ModelInputTokenCounter, ModelOperation, ModelRuntime, NativeErrorFacts, Observation,
    ObservationFact, ObservationSink, PreparationOutcome, ProviderErrorEvidence, ProviderErrorKind,
    ProviderReportedModel, Script, ScriptedModel, ScriptedPrepared, TerminalEvidence,
    TerminalReport, TokenUsage, ToolCallsAtLoss,
};
use signalbox_persistence::{
    blob::BlobCatalogRepository,
    context_compaction::{
        ContextCompactionCorruption, ContextCompactionRepository, ContextCompactionRepositoryError,
        FailedContextCompactionDisposition, PrepareContextCompactionOutcome,
        PrepareContextCompactionRequest,
    },
    conversation_import::ImportedConversationRepository,
    create_session_from_imported_frontier::ImportedSessionRepository,
    disposable_postgres_server_args, disposable_postgres_state_tmpfs_from_example,
    disposable_test_container_labels, local_test_connection_options, migrate,
    model_execution::{PostgresModelCallRepository, PrepareInitialModelCallOutcome},
    scheduler::PostgresEligibilitySweep,
    session_metadata::SessionMetadataRepository,
    start_eligible_turn::StartEligibleTurnRepository,
    startup::PostgresStartupScanRepository,
    test_support::{FleetSoakCensus, FleetSoakCensusRepository},
    turn_liveness::TurnLivenessPersistenceBounds,
};
use signalbox_process_protocol::{
    BlobChunk, CanonicalBlobDigest, CanonicalDigest, CanonicalU64, CanonicalUuid, ClientFrame,
    ClientRequest, CommandId, CommissionedSessionFence, ConversationImportFormat,
    ConversationImportSource, ConversationOriginFilter, ConversationSummary, CurrentModelCallState,
    DescendantTerminationScope, EffectiveModelSettings, ErrorCode, ErrorDetail, FastMode,
    GoalHistoryEvent, GoalLifecycleState, ImportedContentKind, ImportedConversationSourceFormat,
    ImportedSourceSpeaker, ImportedSpeaker, ImportedTextPreview, InputContent, InputDelivery,
    MAX_SESSION_METADATA_INDEXED_UTF8_BYTES, MetadataActor, ModelChangeAdjustment, ModelSelection,
    ModelSettingSource, ModelSettingsOverlay, ModelSettingsPrecedence, ModelSettingsSnapshot,
    OperatorStatusEndMessage, OperatorStatusMessage, ProtocolVersion, ReasoningLevel,
    RejectionDetail, RequestId, ReviewConcernTerminalOutcome, ReviewDiffSide,
    ReviewExternalObjectKind, ReviewFindingEvent, ReviewFindingInput, ReviewFindingStatus,
    ReviewImportTerminalOutcome, ReviewJudgmentDisposition, ReviewJudgmentEffectTerminalOutcome,
    ReviewJudgmentPlanMember, ReviewOrchestrationConcernInput, ReviewOrchestrationConcernStatus,
    ReviewOrchestrationCounts, ReviewOrchestrationSnapshot, ReviewOrchestrationState,
    ReviewPassTerminalOutcome, ReviewPublicationOutcome, ReviewPublicationTerminalOutcome,
    ReviewRepairOutcome, ReviewRepairTerminalOutcome, ReviewSeverity, ReviewTargetSubject,
    ReviewWorkflow, ServerFrame, ServerMessage, SessionEvent, SessionLifecycleEffect,
    SessionMetadata, SessionPlacement, SettingOverlay, SystemPromptMember, SystemPromptText,
    ToolDecision, TranscriptEntry, TranscriptTextEntry, TurnState, UserInputContent,
    decode_server_line, encode_client_line,
};
use signalboxd::{
    ActivatedTurnPass, BlobStorageClass, BlobStoreRegistry, ContextGuardedTurnPass,
    ContextGuardedTurnPassError, ExpiredPassRecoveryPolicy, FatalExecutionSupervisor,
    HubModelConfiguration, LocalProcessListener, PostgresProviderModelExecution,
    ProcessProviderTextDeltaSink, ProcessRuntime, ProcessRuntimeError, ReportedUsageCompaction,
    ReportedUsageCompactionError, SessionTemplateConfiguration, TurnLivenessNumericBounds,
    TurnLivenessRuntime,
};
use sqlx::{PgPool, postgres::PgPoolOptions};
use tempfile::TempDir;
use testcontainers_modules::{
    postgres::Postgres,
    testcontainers::{ContainerAsync, ImageExt, runners::AsyncRunner},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{
        UnixStream,
        unix::{OwnedReadHalf, OwnedWriteHalf},
    },
    sync::watch,
    task::JoinHandle,
    time::timeout,
};
use uuid::Uuid;

// numeric-bound: test deadline - starvation allowance for one server response
// on the local socket, not a latency this suite asserts. Sixteen of these tests
// share a CI node, each driving its own PostgreSQL container, so a reply the
// daemon has already written can wait on the scheduler for seconds before this
// connection is read again; a response that never comes still fails here well
// inside the job's own cap.
const RESPONSE_ALLOWANCE: Duration = Duration::from_secs(30);
// numeric-bound: test deadline - starvation allowance for a scheduler pass or a
// runtime task to finish work the test has already made eligible, not a
// throughput this suite asserts. The same node contention applies, and the pass
// waits on that test's own database container underneath it.
const RUNTIME_SETTLE_ALLOWANCE: Duration = Duration::from_secs(60);

const POSTGRES_IMAGE_TAG: &str = "18.4-alpine3.23";
const DATABASE_NAME: &str = "signalbox_process_runtime";
const DATABASE_USER: &str = "signalbox";
const DATABASE_PASSWORD: &str = "signalbox-test-only";

fn test_session_credential_pin() -> signalbox_persistence::SessionCredentialPin {
    signalbox_persistence::SessionCredentialPin::try_new(vec![
        signalbox_persistence::SessionModelCredential::new(
            "test-model-family",
            "test-model-primary",
        ),
    ])
    .expect("test credential pin is valid")
}

#[track_caller]
fn exactly_one_credential_reference(references: &[String]) -> &str {
    match references {
        [reference] => reference.as_str(),
        _ => panic!("the fixture pins exactly one credential family"),
    }
}

#[track_caller]
fn reported_usage_still_exceeded_turn(outcome: Result<(), ReportedUsageCompactionError>) -> TurnId {
    match outcome {
        Err(ReportedUsageCompactionError::Compaction {
            turn,
            cause_code: "reported_usage_context_still_exceeded",
            ..
        }) => turn,
        other => panic!("expected a still-exceeded compaction failure, got {other:?}"),
    }
}

#[track_caller]
fn failed_automatic_compaction_turn(
    outcome: Result<
        (),
        ContextGuardedTurnPassError<
            RuntimeInputTokenCountError,
            signalboxd::WorkspaceInstructionPreparedExecutionError<
                signalboxd::PostgresProviderModelExecutionError<RuntimeModelCallProviderError>,
            >,
        >,
    >,
) -> TurnId {
    match outcome {
        Err(ContextGuardedTurnPassError::Compaction {
            turn,
            cause_code: "context_compaction_model",
            ..
        }) => turn,
        other => panic!("expected a failed automatic compaction, got {other:?}"),
    }
}

const MAX_SUBMITTED_INPUT_BYTES: usize = 1024 * 1024;
const OVERSIZED_SUBMITTED_INPUT_BYTES: usize = MAX_SUBMITTED_INPUT_BYTES + 1;
const STREAMING_DELTA_COUNT: usize = 192;
const STREAMING_DELTA_BYTES: usize = 8 * 1024;
const MODEL_CONFIGURATION: &str = r#"
version = 1

[[credential_profiles]]
name = "anthropic-primary"
adapter = "anthropic"
billing_kind = "api_metered"
delivery = "file"
file = "/run/secrets/anthropic-primary"

[[credential_pools]]
name = "anthropic-main"
tie_break = "first_listed"
on_pool_exhausted = "park"
members = [{ profile = "anthropic-primary", priority = 1 }]


[[adapter_mappings]]
model_family = "anthropic"
adapter = "anthropic"
credential_pool = "anthropic-main"

[compaction]
prompt = "Summarize the prior conversation faithfully for continuation."

[[models]]
selection_id = "00000000-0000-0000-0000-000000000001"
target_id = "00000000-0000-0000-0000-000000000003"
model_family = "anthropic"
provider_model = "fixture-model"
max_output_tokens = 256
context_window_tokens = 200000
reasoning_levels = ["low"]

[[models]]
selection_id = "00000000-0000-0000-0000-000000000004"
target_id = "00000000-0000-0000-0000-000000000005"
model_family = "anthropic"
provider_model = "fixture-model-next"
max_output_tokens = 256
context_window_tokens = 200000

[[aliases]]
alias_id = "00000000-0000-0000-0000-000000000002"
selection_id = "00000000-0000-0000-0000-000000000001"

[[aliases]]
alias_id = "7fde05bc-b4c3-44f7-8a87-748814c80191"
selection_id = "00000000-0000-0000-0000-000000000001"

[[aliases]]
alias_id = "540ce009-c2ec-4a04-b823-c411ea189778"
selection_id = "00000000-0000-0000-0000-000000000001"
"#;

fn session_template_configuration(
    models: &HubModelConfiguration,
) -> Result<SessionTemplateConfiguration, Box<dyn Error>> {
    let path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config/session-templates.example.toml");
    Ok(SessionTemplateConfiguration::read(&path, || None, models)?)
}

#[derive(Clone, Debug)]
struct RecordingCountedScriptedModel {
    inner: ScriptedModel<ModelCallId>,
    prepared_operations: Arc<Mutex<Vec<ModelOperation<ModelCallId>>>>,
    counted_operations: Arc<Mutex<Vec<ModelOperation<ModelCallId>>>>,
    counts: Arc<Mutex<VecDeque<u64>>>,
}

impl RecordingCountedScriptedModel {
    fn following(
        scripts: impl IntoIterator<Item = Script>,
        counts: impl IntoIterator<Item = u64>,
    ) -> Self {
        Self {
            inner: ScriptedModel::following(scripts),
            prepared_operations: Arc::new(Mutex::new(Vec::new())),
            counted_operations: Arc::new(Mutex::new(Vec::new())),
            counts: Arc::new(Mutex::new(counts.into_iter().collect())),
        }
    }

    fn prepared_operations(&self) -> Vec<ModelOperation<ModelCallId>> {
        self.prepared_operations
            .lock()
            .expect("the recording fixture lock is available")
            .clone()
    }

    fn counted_operations(&self) -> Vec<ModelOperation<ModelCallId>> {
        self.counted_operations
            .lock()
            .expect("the counting fixture lock is available")
            .clone()
    }
}

impl ModelRuntime<ModelCallId> for RecordingCountedScriptedModel {
    type Prepared = ScriptedPrepared<ModelCallId>;

    async fn prepare(
        &self,
        operation: ModelOperation<ModelCallId>,
        cancellation: CancellationSignal,
    ) -> PreparationOutcome<ModelCallId, Self::Prepared> {
        self.prepared_operations
            .lock()
            .expect("the recording fixture lock is available")
            .push(operation.clone());
        self.inner.prepare(operation, cancellation).await
    }

    async fn execute(
        &self,
        prepared: Self::Prepared,
        sink: &mut (dyn ObservationSink<ModelCallId> + Send),
        cancellation: CancellationSignal,
    ) -> TerminalReport<ModelCallId> {
        self.inner.execute(prepared, sink, cancellation).await
    }
}

impl ModelInputTokenCounter<ModelCallId> for RecordingCountedScriptedModel {
    async fn count_input_tokens(
        &self,
        operation: ModelOperation<ModelCallId>,
        _cancellation: CancellationSignal,
    ) -> InputTokenCountOutcome<ModelCallId> {
        let correlation = operation.correlation;
        self.counted_operations
            .lock()
            .expect("the counting fixture lock is available")
            .push(operation);
        let input_tokens = self
            .counts
            .lock()
            .expect("the count-script lock is available")
            .pop_front()
            .expect("the exact-count fixture has a scripted result");
        InputTokenCountOutcome::Counted {
            correlation,
            input_tokens,
        }
    }
}

async fn postgres() -> Result<(ContainerAsync<Postgres>, PgPool), Box<dyn Error>> {
    let container = Postgres::default()
        .with_db_name(DATABASE_NAME)
        .with_user(DATABASE_USER)
        .with_password(DATABASE_PASSWORD)
        .with_cmd(disposable_postgres_server_args())
        .with_mount(disposable_postgres_state_tmpfs_from_example()?)
        .with_tag(POSTGRES_IMAGE_TAG)
        .with_labels(disposable_test_container_labels())
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SessionCreatedFacts {
    session_id: CanonicalUuid,
    model_settings: ModelSettingsSnapshot,
}

#[track_caller]
fn session_created_facts(message: &ServerMessage) -> SessionCreatedFacts {
    match message {
        ServerMessage::SessionCreated {
            session_id,
            model_settings,
        } => SessionCreatedFacts {
            session_id: *session_id,
            model_settings: *model_settings,
        },
        message => panic!("fixture expected session-created receipt, got {message:?}"),
    }
}

fn provider_default_model_settings() -> ModelSettingsSnapshot {
    ModelSettingsSnapshot {
        precedence: ModelSettingsPrecedence {
            per_call: ModelSettingsOverlay::inherit_all(),
            session: ModelSettingsOverlay::inherit_all(),
            profile: ModelSettingsOverlay::inherit_all(),
            global_default: ModelSettingsOverlay::inherit_all(),
        },
        effective: EffectiveModelSettings {
            reasoning_level: None,
            fast_mode: FastMode::Disabled,
            service_tier: None,
        },
        reasoning_source: None,
        fast_mode_source: None,
        service_tier_source: None,
        validated_for_selection_id: None,
    }
}

fn primary_direct_selection_id() -> CanonicalUuid {
    CanonicalUuid::from_uuid(Uuid::from_u128(1))
}

fn next_direct_selection_id() -> CanonicalUuid {
    CanonicalUuid::from_uuid(Uuid::from_u128(4))
}

fn low_reasoning_override() -> ModelSettingsOverlay {
    ModelSettingsOverlay {
        reasoning_level: SettingOverlay::Value(ReasoningLevel::Low),
        fast_mode: signalbox_process_protocol::FastModeOverlay::Inherit,
        service_tier: SettingOverlay::Inherit,
    }
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

const IMPORTED_USER_CONTENT: &str = "imported user";

async fn create_imported_session(pool: &PgPool) -> Result<CanonicalUuid, Box<dyn Error>> {
    let conversation = ImportedConversationId::from_uuid(Uuid::from_u128(0x100));
    let imported_entries = [
        ImportedTranscriptEntryId::from_uuid(Uuid::from_u128(0x200)),
        ImportedTranscriptEntryId::from_uuid(Uuid::from_u128(0x201)),
    ];
    let source = concat!(
        "{\"type\":\"user\",\"message\":{\"content\":\"<user-content>\"}}\n",
        "{\"type\":\"assistant\",\"message\":{\"content\":[",
        "{\"type\":\"tool_use\",\"id\":\"call\",\"name\":\"lookup\",",
        "\"input\":{\"query\":\"synthetic\"}}]}}"
    )
    .replace("<user-content>", IMPORTED_USER_CONTENT);
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
        ImportedSessionRepository::new(pool.clone(), test_session_credential_pin()),
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

#[derive(Clone, Debug)]
struct ReconciliationWitness {
    completed_cycles: Arc<AtomicUsize>,
    cycle: Arc<Mutex<ReconciliationCycle>>,
}

#[derive(Debug, Default)]
struct ReconciliationCycle {
    hinted_sessions: HashSet<SessionId>,
    processed_sessions: HashSet<SessionId>,
    final_batch_seen: bool,
}

impl ReconciliationWitness {
    fn new() -> Self {
        Self {
            completed_cycles: Arc::new(AtomicUsize::new(0)),
            cycle: Arc::new(Mutex::new(ReconciliationCycle::default())),
        }
    }

    fn record_batch(&self, sessions: &[SessionId], continuation: bool) {
        let mut cycle = self
            .cycle
            .lock()
            .expect("the reconciliation witness lock is available");
        cycle.hinted_sessions.extend(sessions.iter().copied());
        cycle.final_batch_seen = !continuation;
        self.complete_drained_cycle(&mut cycle);
    }

    fn record_processed_session(&self, session: SessionId) {
        let mut cycle = self
            .cycle
            .lock()
            .expect("the reconciliation witness lock is available");
        cycle.processed_sessions.insert(session);
        self.complete_drained_cycle(&mut cycle);
    }

    fn complete_drained_cycle(&self, cycle: &mut ReconciliationCycle) {
        if cycle.final_batch_seen && cycle.hinted_sessions.is_subset(&cycle.processed_sessions) {
            self.completed_cycles.fetch_add(1, Ordering::SeqCst);
            *cycle = ReconciliationCycle::default();
        }
    }

    fn completed_cycles(&self) -> usize {
        self.completed_cycles.load(Ordering::SeqCst)
    }
}

#[test]
fn reconciliation_witness_waits_for_final_batch_hints_to_finish() {
    let witness = ReconciliationWitness::new();
    let session = SessionId::from_uuid(Uuid::from_u128(1));

    witness.record_batch(&[session], false);
    assert_eq!(witness.completed_cycles(), 0);

    witness.record_processed_session(session);
    assert_eq!(witness.completed_cycles(), 1);
}

#[test]
fn reconciliation_witness_completes_an_empty_cycle_immediately() {
    let witness = ReconciliationWitness::new();

    witness.record_batch(&[], false);

    assert_eq!(witness.completed_cycles(), 1);
}

struct WitnessedEligibilitySweep<Sweep> {
    inner: Sweep,
    witness: ReconciliationWitness,
}

impl<Sweep> WitnessedEligibilitySweep<Sweep> {
    fn new(inner: Sweep, witness: ReconciliationWitness) -> Self {
        Self { inner, witness }
    }
}

impl<Sweep> EligibilitySweep for WitnessedEligibilitySweep<Sweep>
where
    Sweep: EligibilitySweep + Send,
{
    type Error = Sweep::Error;

    fn find_sessions(
        &mut self,
    ) -> impl Future<Output = Result<EligibilitySweepBatch, Self::Error>> + Send {
        let witness = self.witness.clone();
        async move {
            let batch = self.inner.find_sessions().await?;
            let (sessions, _dispatch_starts, continuation) = batch.clone().into_parts();
            witness.record_batch(&sessions, continuation);
            Ok(batch)
        }
    }
}

struct WitnessedEligibilityPass<Pass> {
    inner: Pass,
    witness: ReconciliationWitness,
}

impl<Pass> WitnessedEligibilityPass<Pass> {
    fn new(inner: Pass, witness: ReconciliationWitness) -> Self {
        Self { inner, witness }
    }
}

impl<Pass> EligibilityPass for WitnessedEligibilityPass<Pass>
where
    Pass: EligibilityPass + Send,
{
    type Error = Pass::Error;

    fn failure_stage(error: &Self::Error) -> &'static str {
        Pass::failure_stage(error)
    }

    fn failure_turn(error: &Self::Error) -> Option<TurnId> {
        Pass::failure_turn(error)
    }

    // The decorator must forward every boundary the inner pass overrides.
    // Inheriting the trait defaults here silently drops the composed pass's
    // occupancy-expiry handoff and its reserved dispatch-start lane, which the
    // fleet-soak scenarios below depend on.
    fn occupancy_expiry_handler(&self) -> Option<Arc<dyn SchedulerPassExpiryHandler>> {
        self.inner.occupancy_expiry_handler()
    }

    fn run(
        &mut self,
        session: SessionId,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
        let execution = self.inner.run(session);
        let witness = self.witness.clone();
        async move {
            let outcome = execution.await;
            witness.record_processed_session(session);
            outcome
        }
    }

    fn run_dispatch_start(
        &mut self,
        session: SessionId,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
        let execution = self.inner.run_dispatch_start(session);
        let witness = self.witness.clone();
        async move {
            let outcome = execution.await;
            witness.record_processed_session(session);
            outcome
        }
    }
}

type RuntimeEligibilitySweep = WitnessedEligibilitySweep<PostgresEligibilitySweep>;

struct RunningRuntime {
    container: ContainerAsync<Postgres>,
    pool: PgPool,
    socket_directory: SocketDirectory,
    shutdown: watch::Sender<bool>,
    runtime_task: Option<JoinHandle<Result<(), ProcessRuntimeError>>>,
    eligibility_nudge: InProcessEligibilityNudge,
    work_source: Option<InProcessEligibilityWorkSource<RuntimeEligibilitySweep>>,
    reconciliation_witness: ReconciliationWitness,
    provider_text_deltas: ProcessProviderTextDeltaSink,
    blob_store_registry: Option<Arc<BlobStoreRegistry>>,
    blob_storage_root: Option<BlobStorageFixture>,
}

#[derive(Clone, Copy)]
enum BlobStorageFixtureMode {
    Disabled,
    Enabled,
}

impl RunningRuntime {
    async fn start() -> Result<Self, Box<dyn Error>> {
        Self::start_with_optional_compaction(None).await
    }

    async fn start_with_compaction(
        model: ScriptedModel<ModelCallId>,
    ) -> Result<Self, Box<dyn Error>> {
        Self::start_with_optional_compaction(Some(model)).await
    }

    async fn start_with_optional_compaction(
        compaction_model: Option<ScriptedModel<ModelCallId>>,
    ) -> Result<Self, Box<dyn Error>> {
        Self::start_with_options(compaction_model, BlobStorageFixtureMode::Disabled).await
    }

    async fn start_with_blob_storage() -> Result<Self, Box<dyn Error>> {
        Self::start_with_options(None, BlobStorageFixtureMode::Enabled).await
    }

    async fn start_with_options(
        compaction_model: Option<ScriptedModel<ModelCallId>>,
        blob_storage: BlobStorageFixtureMode,
    ) -> Result<Self, Box<dyn Error>> {
        let (container, pool) = postgres().await?;
        let socket_directory = SocketDirectory::create()?;
        let listener = LocalProcessListener::bind(socket_directory.socket())?;
        let reconciliation_witness = ReconciliationWitness::new();
        let sweep = WitnessedEligibilitySweep::new(
            PostgresEligibilitySweep::new(pool.clone()),
            reconciliation_witness.clone(),
        );
        let (eligibility_nudge, work_source) = InProcessEligibilityWorkSource::new(sweep);
        let blob_storage_root = match blob_storage {
            BlobStorageFixtureMode::Disabled => None,
            BlobStorageFixtureMode::Enabled => Some(BlobStorageFixture::create()?),
        };
        let configuration = blob_storage_root.as_ref().map_or_else(
            || String::from(MODEL_CONFIGURATION),
            BlobStorageFixture::model_configuration,
        );
        let model_configuration = support::parse_model_configuration(&configuration)?;
        let blob_store_registry = match blob_storage {
            BlobStorageFixtureMode::Disabled => None,
            BlobStorageFixtureMode::Enabled => BlobStoreRegistry::initialize_for_conformance(
                model_configuration.blob_storage(),
                pool.clone(),
            )
            .await?
            .map(Arc::new),
        };
        let runtime_models = model_configuration.runtime_model_catalog();
        let template_configuration = session_template_configuration(&model_configuration)?;
        let mut runtime = ProcessRuntime::new_with_templates(
            listener,
            pool.clone(),
            eligibility_nudge.clone(),
            InProcessToolDispatchGate::default(),
            model_configuration,
            template_configuration,
        );
        if let Some(compaction_model) = compaction_model {
            runtime = runtime.with_context_compaction_model(RuntimeContextCompactionModel::new(
                compaction_model,
                runtime_models,
            ));
        }
        if let Some(registry) = blob_store_registry.as_ref() {
            runtime = runtime.with_blob_store_registry(Arc::clone(registry));
        }
        let provider_text_deltas = runtime.provider_text_delta_sink();
        let (shutdown, shutdown_receiver) = watch::channel(false);
        let runtime_task = tokio::spawn(runtime.run(shutdown_receiver));
        Ok(Self {
            container,
            pool,
            socket_directory,
            shutdown,
            runtime_task: Some(runtime_task),
            eligibility_nudge,
            work_source: Some(work_source),
            reconciliation_witness,
            provider_text_deltas,
            blob_store_registry,
            blob_storage_root,
        })
    }

    fn socket(&self) -> &Path {
        self.socket_directory.socket()
    }

    async fn restart(&mut self) -> Result<usize, Box<dyn Error>> {
        self.restart_with_model_configuration(MODEL_CONFIGURATION)
            .await
    }

    async fn restart_with_model_configuration(
        &mut self,
        configuration: &str,
    ) -> Result<usize, Box<dyn Error>> {
        let model_configuration = support::parse_model_configuration(configuration)?;
        let template_configuration = session_template_configuration(&model_configuration)?;
        self.restart_with_templates(configuration, template_configuration)
            .await
    }

    /// Restarts over the same database with every session template removed
    /// from configuration, as a template rename or deletion would leave it.
    async fn restart_without_templates(&mut self) -> Result<usize, Box<dyn Error>> {
        self.restart_with_templates(MODEL_CONFIGURATION, SessionTemplateConfiguration::default())
            .await
    }

    async fn restart_with_templates(
        &mut self,
        configuration: &str,
        template_configuration: SessionTemplateConfiguration,
    ) -> Result<usize, Box<dyn Error>> {
        self.shutdown.send_replace(true);
        let runtime_task = self
            .runtime_task
            .as_mut()
            .expect("a running runtime has an installed task");
        timeout(RUNTIME_SETTLE_ALLOWANCE, runtime_task).await???;
        self.runtime_task = None;
        self.restart_after_stop(configuration, template_configuration)
            .await
    }

    async fn restart_after_stop(
        &mut self,
        configuration: &str,
        template_configuration: SessionTemplateConfiguration,
    ) -> Result<usize, Box<dyn Error>> {
        let mut scan = StartupScanService::new(
            UuidV7StartupScanIdGenerator,
            PostgresStartupScanRepository::new(self.pool.clone()),
        );
        let recovered_turn_count = scan.execute().await?.recovered_turn_count();

        let listener = LocalProcessListener::bind(self.socket())?;
        let reconciliation_witness = ReconciliationWitness::new();
        let sweep = WitnessedEligibilitySweep::new(
            PostgresEligibilitySweep::new(self.pool.clone()),
            reconciliation_witness.clone(),
        );
        let (eligibility_nudge, work_source) = InProcessEligibilityWorkSource::new(sweep);
        let model_configuration = support::parse_model_configuration(configuration)?;
        let mut runtime = ProcessRuntime::new_with_templates(
            listener,
            self.pool.clone(),
            eligibility_nudge.clone(),
            InProcessToolDispatchGate::default(),
            model_configuration,
            template_configuration,
        );
        if let Some(registry) = self.blob_store_registry.as_ref() {
            runtime = runtime.with_blob_store_registry(Arc::clone(registry));
        }
        let provider_text_deltas = runtime.provider_text_delta_sink();
        let (shutdown, shutdown_receiver) = watch::channel(false);
        self.shutdown = shutdown;
        self.runtime_task = Some(tokio::spawn(runtime.run(shutdown_receiver)));
        self.eligibility_nudge = eligibility_nudge;
        self.work_source = Some(work_source);
        self.reconciliation_witness = reconciliation_witness;
        self.provider_text_deltas = provider_text_deltas;
        Ok(recovered_turn_count)
    }

    /// Simulates the uncatchable process death used by the fleet soak. The
    /// replacement opens the same socket and database only after the killed
    /// task has stopped, so no graceful runtime shutdown can repair its work.
    async fn kill_and_restart(&mut self) -> Result<usize, Box<dyn Error>> {
        let runtime_task = self
            .runtime_task
            .take()
            .expect("a running runtime has an installed task");
        runtime_task.abort();
        let killed = runtime_task.await;
        let killed = killed.expect_err("the killed runtime task must not return normally");
        assert!(
            killed.is_cancelled(),
            "the runtime task must stop by cancellation, got {killed}"
        );

        let model_configuration = support::parse_model_configuration(MODEL_CONFIGURATION)?;
        let template_configuration = session_template_configuration(&model_configuration)?;
        self.restart_after_stop(MODEL_CONFIGURATION, template_configuration)
            .await
    }

    fn take_work_source(&mut self) -> InProcessEligibilityWorkSource<RuntimeEligibilitySweep> {
        self.work_source
            .take()
            .expect("the streaming fixture takes the work source once")
    }

    fn reconciliation_witness(&self) -> ReconciliationWitness {
        self.reconciliation_witness.clone()
    }

    fn provider_text_delta_sink(&self) -> ProcessProviderTextDeltaSink {
        self.provider_text_deltas.clone()
    }

    fn blob_store_registry(&self) -> Arc<BlobStoreRegistry> {
        Arc::clone(
            self.blob_store_registry
                .as_ref()
                .expect("the fixture enables blob storage"),
        )
    }

    async fn stop(mut self) -> Result<(), Box<dyn Error>> {
        if let Some(runtime_task) = self.runtime_task.take() {
            self.shutdown.send(true)?;
            timeout(RUNTIME_SETTLE_ALLOWANCE, runtime_task).await???;
        }
        self.pool.close().await;
        self.socket_directory.cleanup()?;
        drop(self.blob_storage_root);
        drop(self.container);
        Ok(())
    }
}

struct BlobStorageFixture {
    _root: TempDir,
    staging: PathBuf,
    store: PathBuf,
}

impl BlobStorageFixture {
    fn create() -> Result<Self, io::Error> {
        let root = TempDir::new()?;
        let staging = root.path().join("staging");
        let store = root.path().join("primary");
        fs::create_dir(&staging)?;
        fs::set_permissions(&staging, fs::Permissions::from_mode(0o700))?;
        fs::create_dir(&store)?;
        fs::set_permissions(&store, fs::Permissions::from_mode(0o700))?;
        Ok(Self {
            _root: root,
            staging,
            store,
        })
    }

    fn model_configuration(&self) -> String {
        format!(
            r#"{MODEL_CONFIGURATION}
[blob_storage]
version = 1
staging_directory = "{}"
max_blob_bytes = 268435456

[[blob_storage.stores]]
name = "primary"
namespace_id = "5a100001-0000-4000-8000-000000000001"
kind = "filesystem"
root_directory = "{}"

[blob_storage.routes]
user_attachment = "primary"
tool_artifact = "primary"
imported_source = "primary"
generated_artifact = "primary"
"#,
            self.staging.display(),
            self.store.display(),
        )
    }
}

async fn append_blob_upload(
    connection: &mut Connection,
    request_id: u64,
    bytes: &[u8],
) -> Result<(), Box<dyn Error>> {
    connection
        .request(
            request_id,
            ClientRequest::AppendBlobUpload {
                chunk: BlobChunk::new(bytes.to_vec()),
            },
        )
        .await?;
    let response = connection.response().await?;
    assert_eq!(
        response.message(),
        &ServerMessage::BlobUploadAppended {
            assembled_length_bytes: CanonicalU64::new(u64::try_from(bytes.len())?),
        }
    );
    Ok(())
}

async fn commit_blob_upload(
    connection: &mut Connection,
    wire_digest: CanonicalBlobDigest,
    expected_length: CanonicalU64,
    bytes: &[u8],
) -> Result<(), Box<dyn Error>> {
    connection
        .request(
            1,
            ClientRequest::BeginBlobUpload {
                expected_digest: wire_digest,
                expected_length_bytes: expected_length,
            },
        )
        .await?;
    assert_eq!(
        connection.response().await?.message(),
        &ServerMessage::BlobUploadBegun {
            expected_digest: wire_digest,
            expected_length_bytes: expected_length,
        }
    );
    append_blob_upload(connection, 2, bytes).await?;
    connection
        .request(3, ClientRequest::CommitBlobUpload {})
        .await?;
    assert_eq!(
        connection.response().await?.message(),
        &ServerMessage::BlobUploadCommitted {
            digest: wire_digest,
            byte_length: expected_length,
        }
    );
    Ok(())
}

struct CommittedBlobReadFixture {
    runtime: RunningRuntime,
    connection: Connection,
    bytes: &'static [u8],
    digest: BlobDigest,
    wire_digest: CanonicalBlobDigest,
    expected_length: CanonicalU64,
}

impl CommittedBlobReadFixture {
    async fn start(bytes: &'static [u8]) -> Result<Self, Box<dyn Error>> {
        let runtime = RunningRuntime::start_with_blob_storage().await?;
        let digest = BlobDigest::digest(bytes);
        let wire_digest = CanonicalBlobDigest::from_digest(digest);
        let expected_length = CanonicalU64::new(u64::try_from(bytes.len())?);
        let mut connection = Connection::connect(runtime.socket()).await?;
        commit_blob_upload(&mut connection, wire_digest, expected_length, bytes).await?;
        Ok(Self {
            runtime,
            connection,
            bytes,
            digest,
            wire_digest,
            expected_length,
        })
    }

    fn object_path(&self) -> PathBuf {
        self.runtime
            .blob_storage_root
            .as_ref()
            .expect("the fixture owns one blob store")
            .store
            .join(BlobObjectKey::for_digest(self.digest).as_str())
    }

    fn expected_replica_count(&self) -> CanonicalU64 {
        CanonicalU64::new(1)
    }

    fn expected_range(
        &self,
        offset_bytes: CanonicalU64,
        length_bytes: CanonicalU64,
    ) -> &'static [u8] {
        let offset =
            usize::try_from(offset_bytes.value()).expect("the fixture range offset fits in usize");
        let length =
            usize::try_from(length_bytes.value()).expect("the fixture range length fits in usize");
        let end = offset
            .checked_add(length)
            .expect("the fixture range end is representable");
        self.bytes
            .get(offset..end)
            .expect("the fixture contains the expected range")
    }

    async fn stop(self) -> Result<(), Box<dyn Error>> {
        drop(self.connection);
        self.runtime.stop().await
    }
}

/// INV-060: the daemon streams exact bytes through one upload lifecycle and
/// registers one immutable identity.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn inv060_blob_upload_round_trips_exact_bytes() -> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start_with_blob_storage().await?;
    let bytes = b"exact immutable upload bytes";
    let digest = BlobDigest::digest(bytes);
    let wire_digest = CanonicalBlobDigest::from_digest(digest);
    let expected_length = CanonicalU64::new(u64::try_from(bytes.len())?);
    let mut connection = Connection::connect(runtime.socket()).await?;

    commit_blob_upload(&mut connection, wire_digest, expected_length, bytes).await?;

    let catalog = BlobCatalogRepository::new(runtime.pool.clone())
        .find(digest)
        .await?
        .expect("the committed upload is catalogued");
    assert_eq!(catalog.expected().byte_length(), expected_length.value());
    assert_eq!(catalog.replicas().len(), 1);
    let registry = runtime.blob_store_registry();
    let (store_name, store) = registry.routed_store(BlobStorageClass::UserAttachment);
    assert_eq!(catalog.replicas()[0].store(), store_name);
    let opened = store.open(catalog.replicas()[0].object_key()).await?;
    let mut observed = Vec::new();
    opened.into_reader().read_to_end(&mut observed).await?;
    assert_eq!(observed, bytes);

    drop(connection);
    runtime.stop().await
}

/// INV-060: an exact retry against the routed store short-circuits as already
/// present without accepting another upload body.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn inv060_blob_upload_exact_retry_is_already_present() -> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start_with_blob_storage().await?;
    let bytes = b"exact immutable upload retry bytes";
    let wire_digest = CanonicalBlobDigest::from_digest(BlobDigest::digest(bytes));
    let expected_length = CanonicalU64::new(u64::try_from(bytes.len())?);
    let mut connection = Connection::connect(runtime.socket()).await?;
    commit_blob_upload(&mut connection, wire_digest, expected_length, bytes).await?;

    connection
        .request(
            4,
            ClientRequest::BeginBlobUpload {
                expected_digest: wire_digest,
                expected_length_bytes: expected_length,
            },
        )
        .await?;

    assert_eq!(
        connection.response().await?.message(),
        &ServerMessage::BlobUploadAlreadyPresent {
            digest: wire_digest,
            byte_length: expected_length,
        }
    );
    drop(connection);
    runtime.stop().await
}

/// INV-060: failure to register after verified publication leaves an orphan
/// object and never a dangling catalog reference.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn inv060_registration_failure_after_publication_leaves_only_an_orphan()
-> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start_with_blob_storage().await?;
    let bytes = b"published before unavailable catalog";
    let digest = BlobDigest::digest(bytes);
    let wire_digest = CanonicalBlobDigest::from_digest(digest);
    let expected_length = CanonicalU64::new(u64::try_from(bytes.len())?);
    let mut connection = Connection::connect(runtime.socket()).await?;

    connection
        .request(
            1,
            ClientRequest::BeginBlobUpload {
                expected_digest: wire_digest,
                expected_length_bytes: expected_length,
            },
        )
        .await?;
    assert_eq!(
        connection.response().await?.message(),
        &ServerMessage::BlobUploadBegun {
            expected_digest: wire_digest,
            expected_length_bytes: expected_length,
        }
    );
    append_blob_upload(&mut connection, 2, bytes).await?;
    let catalog = BlobCatalogRepository::new(runtime.pool.clone());
    let catalog_fault = catalog.inject_registration_fault().await?;
    connection
        .request(3, ClientRequest::CommitBlobUpload {})
        .await?;
    assert_eq!(
        connection.response().await?.message(),
        &ServerMessage::Error {
            code: ErrorCode::Unavailable,
            message: String::from("the requested operation is unavailable"),
            detail: ErrorDetail::none(),
        }
    );
    catalog_fault.restore().await?;

    assert!(catalog.find(digest).await?.is_none());
    let registry = runtime.blob_store_registry();
    let (_store_name, store) = registry.routed_store(BlobStorageClass::UserAttachment);
    let orphan = store.open(&BlobObjectKey::for_digest(digest)).await?;
    let mut observed = Vec::new();
    orphan.into_reader().read_to_end(&mut observed).await?;
    assert_eq!(observed, bytes);
    drop(connection);
    runtime.stop().await
}

/// INV-060: metadata reports the catalog's exact bounded identity, length, and
/// replica count.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn inv060_blob_metadata_reports_exact_catalog_facts() -> Result<(), Box<dyn Error>> {
    let mut fixture = CommittedBlobReadFixture::start(b"metadata blob fixture").await?;

    fixture
        .connection
        .request(
            4,
            ClientRequest::ReadBlobMetadata {
                digest: fixture.wire_digest,
            },
        )
        .await?;
    assert_eq!(
        fixture.connection.response().await?.message(),
        &ServerMessage::BlobMetadata {
            digest: fixture.wire_digest,
            byte_length: fixture.expected_length,
            replica_count: fixture.expected_replica_count(),
        }
    );

    fixture.stop().await
}

/// INV-060: a direct range returns the exact requested bytes only after the
/// recorded replica verifies.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn inv060_blob_range_returns_exact_verified_bytes() -> Result<(), Box<dyn Error>> {
    let mut fixture = CommittedBlobReadFixture::start(b"verified direct blob range").await?;
    let offset_bytes = CanonicalU64::new(9);
    let length_bytes = CanonicalU64::new(6);
    let expected_bytes = fixture.expected_range(offset_bytes, length_bytes);

    fixture
        .connection
        .request(
            4,
            ClientRequest::ReadBlobChunk {
                digest: fixture.wire_digest,
                offset_bytes,
                length_bytes,
            },
        )
        .await?;
    assert_eq!(
        fixture.connection.response().await?.message(),
        &ServerMessage::BlobChunkRead {
            digest: fixture.wire_digest,
            offset_bytes,
            bytes: BlobChunk::new(expected_bytes.to_vec()),
        }
    );

    fixture.stop().await
}

/// INV-060: an exact range outside the catalog length is rejected before store
/// access with the typed range facts.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn inv060_blob_range_out_of_bounds_is_typed() -> Result<(), Box<dyn Error>> {
    let mut fixture = CommittedBlobReadFixture::start(b"out of bounds blob fixture").await?;
    let offset_bytes = CanonicalU64::new(u64::MAX);
    let length_bytes = CanonicalU64::new(1);

    fixture
        .connection
        .request(
            4,
            ClientRequest::ReadBlobChunk {
                digest: fixture.wire_digest,
                offset_bytes,
                length_bytes,
            },
        )
        .await?;
    assert_eq!(
        fixture.connection.response().await?.message(),
        &ServerMessage::Error {
            code: ErrorCode::InvalidRequest,
            message: String::from("blob read was rejected"),
            detail: ErrorDetail::invalid_request(RejectionDetail::BlobReadRangeOutOfBounds {
                offset_bytes,
                length_bytes,
                blob_length_bytes: fixture.expected_length,
            }),
        }
    );

    fixture.stop().await
}

/// INV-060: an absent recorded object returns the content-silent missing code.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn inv060_blob_read_missing_replica_is_typed() -> Result<(), Box<dyn Error>> {
    let mut fixture = CommittedBlobReadFixture::start(b"missing replica blob fixture").await?;
    fs::remove_file(fixture.object_path())?;

    fixture
        .connection
        .request(
            4,
            ClientRequest::ReadBlobChunk {
                digest: fixture.wire_digest,
                offset_bytes: CanonicalU64::new(0),
                length_bytes: CanonicalU64::new(1),
            },
        )
        .await?;
    assert_eq!(
        fixture.connection.response().await?.message(),
        &ServerMessage::Error {
            code: ErrorCode::BlobMissing,
            message: String::from("all recorded blob replicas are missing"),
            detail: ErrorDetail::none(),
        }
    );

    fixture.stop().await
}

/// INV-060: a recorded object whose bytes no longer match the catalog returns
/// the content-silent corruption code.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn inv060_blob_read_corrupt_replica_is_typed() -> Result<(), Box<dyn Error>> {
    let mut fixture = CommittedBlobReadFixture::start(b"corrupt replica blob fixture").await?;
    fs::write(fixture.object_path(), b"corrupt")?;

    fixture
        .connection
        .request(
            4,
            ClientRequest::ReadBlobChunk {
                digest: fixture.wire_digest,
                offset_bytes: CanonicalU64::new(0),
                length_bytes: CanonicalU64::new(1),
            },
        )
        .await?;
    assert_eq!(
        fixture.connection.response().await?.message(),
        &ServerMessage::Error {
            code: ErrorCode::BlobCorrupt,
            message: String::from("all usable blob replicas are corrupt"),
            detail: ErrorDetail::none(),
        }
    );

    fixture.stop().await
}

/// INV-060: a digest absent from the catalog returns the content-silent
/// not-found code without store access.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn inv060_blob_metadata_absent_catalog_entry_is_not_found() -> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start_with_blob_storage().await?;
    let absent_digest = CanonicalBlobDigest::from_bytes([0xcd; 32]);
    let mut connection = Connection::connect(runtime.socket()).await?;

    connection
        .request(
            1,
            ClientRequest::ReadBlobMetadata {
                digest: absent_digest,
            },
        )
        .await?;
    assert_eq!(
        connection.response().await?.message(),
        &ServerMessage::Error {
            code: ErrorCode::NotFound,
            message: String::from("the requested blob was not found"),
            detail: ErrorDetail::none(),
        }
    );

    drop(connection);
    runtime.stop().await
}

async fn create_alias_session(
    connection: &mut Connection,
) -> Result<CanonicalUuid, Box<dyn Error>> {
    create_alias_session_with(
        connection,
        CanonicalUuid::from_uuid(Uuid::from_u128(2)),
        SessionPlacement::Pathless {},
    )
    .await
}

async fn create_alias_session_with(
    connection: &mut Connection,
    alias_id: CanonicalUuid,
    placement: SessionPlacement,
) -> Result<CanonicalUuid, Box<dyn Error>> {
    connection
        .request(
            1,
            ClientRequest::CreateSession {
                command_id: command()?,
                initial_model_selection: ModelSelection::Alias { alias_id },
                model_settings: ModelSettingsOverlay::inherit_all(),
                system_prompt: SystemPromptMember::present(None),
                placement,
                lifecycle: signalbox_process_protocol::SessionLifecycleMembers::default(),
            },
        )
        .await?;
    match connection.response().await?.message() {
        ServerMessage::SessionCreated { session_id, .. } => Ok(*session_id),
        message => Err(io::Error::other(format!(
            "unexpected create-session fixture response: {message:?}"
        ))
        .into()),
    }
}

async fn create_direct_session_with_settings(
    connection: &mut Connection,
    selection_id: CanonicalUuid,
    model_settings: ModelSettingsOverlay,
) -> Result<(CanonicalUuid, ModelSettingsSnapshot), Box<dyn Error>> {
    connection
        .request(
            1,
            ClientRequest::CreateSession {
                command_id: command()?,
                initial_model_selection: ModelSelection::Direct { selection_id },
                model_settings,
                system_prompt: SystemPromptMember::present(None),
                placement: SessionPlacement::Pathless {},
                lifecycle: signalbox_process_protocol::SessionLifecycleMembers::default(),
            },
        )
        .await?;
    match response_within(connection).await?.message() {
        ServerMessage::SessionCreated {
            session_id,
            model_settings,
        } => Ok((*session_id, *model_settings)),
        message => Err(io::Error::other(format!(
            "unexpected direct create-session response: {message:?}"
        ))
        .into()),
    }
}

async fn read_goal_messages(
    connection: &mut Connection,
    request_id: u64,
    session_id: CanonicalUuid,
) -> Result<Vec<ServerMessage>, Box<dyn Error>> {
    connection
        .request(request_id, ClientRequest::ReadGoal { session_id })
        .await?;
    let mut messages = Vec::new();
    loop {
        let message = response_within(connection).await?.message().clone();
        let ended = matches!(message, ServerMessage::GoalHistoryEnd { .. });
        messages.push(message);
        if ended {
            return Ok(messages);
        }
    }
}

/// One commission request atomically creates a template session under a
/// recorded authority fence with its goal and first input; the same command
/// identity replays to the committed session, and the same identity naming a
/// different fence is a conflicting reuse.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn commission_session_records_its_fence_goal_and_first_input() -> Result<(), Box<dyn Error>> {
    let mut runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let commission_command = command()?;
    let statement = String::from("Address the review findings on pull request 41.");
    let fence = CommissionedSessionFence::PullRequest {
        repository: String::from("sample-user/sample-repository"),
        pull_request: CanonicalU64::new(41),
        head_sha: String::from("1111111111111111111111111111111111111111"),
        head_repository: String::from("sample-user/sample-repository"),
        head_branch: String::from("agent/sample-feature"),
        base_branch: String::from("main"),
    };
    let request = ClientRequest::CommissionSession {
        command_id: commission_command,
        template_name: String::from("merge-forward"),
        fence: fence.clone(),
        statement: statement.clone(),
        content: InputContent::new(String::from("Respond to the open review threads.")),
    };

    connection.request(2, request.clone()).await?;
    let commissioned = response_within(&mut connection).await?.message().clone();
    let ServerMessage::SessionCommissioned {
        session_id,
        dispatch_id,
    } = commissioned
    else {
        panic!("unexpected commission response: {commissioned:?}");
    };

    connection.request(3, request.clone()).await?;
    let replayed = response_within(&mut connection).await?.message().clone();
    assert_eq!(
        replayed,
        ServerMessage::SessionCommissioned {
            session_id,
            dispatch_id,
        }
    );

    connection
        .request(
            4,
            ClientRequest::CommissionSession {
                command_id: command()?,
                template_name: String::from("merge-forward"),
                fence: fence.clone(),
                statement: statement.clone(),
                content: InputContent::new(String::from("Respond to the open review threads.")),
            },
        )
        .await?;
    let busy = response_within(&mut connection).await?.message().clone();
    assert_eq!(protocol_error_code(&busy), ErrorCode::Rejected);
    assert_eq!(
        protocol_error_detail(&busy),
        Some(RejectionDetail::CommissionTargetBusy { session_id })
    );

    connection
        .request(
            5,
            ClientRequest::CommissionSession {
                command_id: commission_command,
                template_name: String::from("merge-forward"),
                fence: CommissionedSessionFence::Branch {
                    repository: String::from("sample-user/sample-repository"),
                    branch: String::from("main"),
                },
                statement: statement.clone(),
                content: InputContent::new(String::from("Respond to the open review threads.")),
            },
        )
        .await?;
    let conflicting = response_within(&mut connection).await?.message().clone();
    let ServerMessage::Error { code, .. } = conflicting else {
        panic!("a conflicting commission reuse must be refused: {conflicting:?}");
    };
    assert_eq!(code, ErrorCode::ConflictingReuse);

    let history = read_goal_messages(&mut connection, 6, session_id).await?;
    assert_eq!(
        history.first(),
        Some(&ServerMessage::GoalHistoryStart {
            session_id,
            current_generation: CanonicalU64::new(1),
            current_statement: statement.clone(),
        })
    );

    // Template-configuration drift: restart over the same database with every
    // template removed. The committed commission stays discoverable through
    // the exact retry, because replay is resolved from the durable record
    // before the live template catalog is consulted.
    runtime.restart_without_templates().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    connection.request(7, request).await?;
    let drift_replayed = response_within(&mut connection).await?.message().clone();
    assert_eq!(
        drift_replayed,
        ServerMessage::SessionCommissioned {
            session_id,
            dispatch_id,
        }
    );

    // A fresh commission naming the removed template is still refused: only
    // replay of committed work survives configuration drift.
    connection
        .request(
            8,
            ClientRequest::CommissionSession {
                command_id: command()?,
                template_name: String::from("merge-forward"),
                fence,
                statement,
                content: InputContent::new(String::from("Respond to the open review threads.")),
            },
        )
        .await?;
    let refused = response_within(&mut connection).await?.message().clone();
    let ServerMessage::Error { code, .. } = refused else {
        panic!("a fresh commission under a removed template must refuse: {refused:?}");
    };
    assert_eq!(code, ErrorCode::InvalidRequest);
    Ok(())
}

/// INV-048: process goal commands preserve immutable supersession lineage and
/// show returns the complete ordered event stream with its current projection.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn s_goal_inv048_process_protocol_supersession_history_round_trips()
-> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let session_id = create_alias_session(&mut connection).await?;
    let attach_command = command()?;
    let supersede_command = command()?;
    let stop_command = command()?;
    let first_statement = String::from("finish the commissioned task");
    let replacement_statement = String::from("finish the clarified task");

    connection
        .request(
            2,
            ClientRequest::AttachGoal {
                command_id: attach_command,
                session_id,
                statement: first_statement.clone(),
            },
        )
        .await?;
    let attached = response_within(&mut connection).await?.message().clone();
    connection
        .request(
            3,
            ClientRequest::SupersedeGoal {
                command_id: supersede_command,
                session_id,
                statement: replacement_statement.clone(),
            },
        )
        .await?;
    let superseded = response_within(&mut connection).await?.message().clone();
    connection
        .request(
            4,
            ClientRequest::StopGoal {
                command_id: stop_command,
                session_id,
                descendant_scope: DescendantTerminationScope::ParentAlone,
            },
        )
        .await?;
    let stopped = response_within(&mut connection).await?.message().clone();
    let history = read_goal_messages(&mut connection, 5, session_id).await?;

    assert_eq!(
        attached,
        ServerMessage::GoalTransitionApplied {
            session_id,
            event_ordinal: CanonicalU64::new(1),
            generation: CanonicalU64::new(1),
        }
    );
    assert_eq!(
        superseded,
        ServerMessage::GoalTransitionApplied {
            session_id,
            event_ordinal: CanonicalU64::new(2),
            generation: CanonicalU64::new(1),
        }
    );
    assert_eq!(
        stopped,
        ServerMessage::GoalTransitionApplied {
            session_id,
            event_ordinal: CanonicalU64::new(3),
            generation: CanonicalU64::new(2),
        }
    );
    assert_eq!(
        history,
        vec![
            ServerMessage::GoalHistoryStart {
                session_id,
                current_generation: CanonicalU64::new(2),
                current_statement: replacement_statement.clone(),
            },
            ServerMessage::GoalHistoryState {
                current_state: GoalLifecycleState::UserStopped {},
            },
            ServerMessage::GoalHistoryItem {
                event_ordinal: CanonicalU64::new(1),
                generation: CanonicalU64::new(1),
                event: GoalHistoryEvent::Commissioned {
                    statement: first_statement,
                    command_id: attach_command,
                },
            },
            ServerMessage::GoalHistoryItem {
                event_ordinal: CanonicalU64::new(2),
                generation: CanonicalU64::new(1),
                event: GoalHistoryEvent::Superseded {
                    replacement_statement,
                    command_id: supersede_command,
                },
            },
            ServerMessage::GoalHistoryItem {
                event_ordinal: CanonicalU64::new(3),
                generation: CanonicalU64::new(2),
                event: GoalHistoryEvent::UserStopped {
                    command_id: stop_command,
                },
            },
            ServerMessage::GoalHistoryEnd {
                event_count: CanonicalU64::new(3),
            },
        ]
    );

    drop(connection);
    runtime.stop().await
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn create_session_rejects_a_model_absent_from_the_static_mapping()
-> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let unknown_selection = CanonicalUuid::from_uuid(Uuid::from_u128(0xffff));
    connection
        .request(
            1,
            ClientRequest::CreateSession {
                command_id: command()?,
                initial_model_selection: ModelSelection::Direct {
                    selection_id: unknown_selection,
                },
                model_settings: ModelSettingsOverlay::inherit_all(),
                system_prompt: SystemPromptMember::present(None),
                placement: SessionPlacement::Pathless {},
                lifecycle: signalbox_process_protocol::SessionLifecycleMembers::default(),
            },
        )
        .await?;

    let response = response_within(&mut connection).await?;
    let ServerMessage::Error { code, .. } = response.message() else {
        panic!("unmapped model must return a protocol error");
    };
    assert_eq!(*code, ErrorCode::InvalidRequest);

    drop(connection);
    runtime.stop().await
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
                content: UserInputContent::text(content),
                expected_defaults_version: Some(CanonicalU64::new(1)),
                model_settings: ModelSettingsOverlay::inherit_all(),
                delivery: None,
            },
        )
        .await?;
    match connection.response().await?.message() {
        ServerMessage::InputSubmitted {
            session_id: submitted_session,
            accepted_input_id,
            acceptance_position,
            turn_id,
            ..
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

async fn accepted_successor_model_settings(
    connection: &mut Connection,
    session_id: CanonicalUuid,
    acceptance: u64,
) -> Result<ModelSettingsSnapshot, Box<dyn Error>> {
    match response_within(connection).await?.message() {
        ServerMessage::InputSubmitted {
            session_id: accepted_session,
            acceptance_position,
            model_settings,
            ..
        } if *accepted_session == session_id && acceptance_position.value() == acceptance => {
            Ok(*model_settings)
        }
        message => Err(io::Error::other(format!(
            "unexpected accepted-input settings response: {message:?}"
        ))
        .into()),
    }
}

async fn response_within(connection: &mut Connection) -> Result<ServerFrame, Box<dyn Error>> {
    timeout(RESPONSE_ALLOWANCE, connection.response()).await?
}

async fn attach_follower_after_snapshot(
    socket: &Path,
    version: ProtocolVersion,
    request_id: u64,
    session_id: CanonicalUuid,
) -> Result<(Connection, u64), Box<dyn Error>> {
    let mut follow = Connection::connect(socket).await?;
    follow
        .request_version(
            version,
            request_id,
            ClientRequest::FollowSession { session_id },
        )
        .await?;
    let start = response_within(&mut follow).await?;
    let cursor = transcript_snapshot_start_cursor(start.message(), session_id);
    loop {
        let frame = response_within(&mut follow).await?;
        if matches!(
            frame.message(),
            ServerMessage::TranscriptSnapshotEnd {
                session_id: selected,
                ..
            } if *selected == session_id
        ) {
            return Ok((follow, cursor));
        }
    }
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
        ServerMessage::TranscriptModelCallsEnd { model_call_count }
            if model_call_count.value() == 0
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
            ProtocolVersion::One,
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

/// The durable turn shape one scheduler pass is expected to leave behind.
#[derive(Clone, Copy)]
enum TurnSettle {
    /// The turn reached its terminal lifecycle state.
    Terminal,
    /// The turn parked on an unstopped ambiguous model call and still holds
    /// its slot.
    ParkedOnAmbiguity,
}

impl TurnSettle {
    const fn predicate_sql(self) -> &'static str {
        match self {
            Self::Terminal => {
                "SELECT EXISTS (
                    SELECT 1
                      FROM turn_lifecycle
                     WHERE session_id = $1
                       AND turn_id = $2
                       AND state_kind = 'terminal'
                )"
            }
            Self::ParkedOnAmbiguity => {
                "SELECT EXISTS (
                    SELECT 1
                      FROM turn_lifecycle
                     WHERE session_id = $1
                       AND turn_id = $2
                       AND state_kind = 'active'
                       AND active_phase_kind = 'awaiting_model_call_recovery'
                )"
            }
        }
    }
}

async fn wait_for_turn_settle(pool: &PgPool, session: SessionId, turn: TurnId, settle: TurnSettle) {
    loop {
        let settled: bool = sqlx::query_scalar(settle.predicate_sql())
            .bind(session.into_uuid())
            .bind(turn.into_uuid())
            .fetch_one(pool)
            .await
            .unwrap_or(false);
        if settled {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn seed_completed_compaction_session(
    runtime: &mut RunningRuntime,
) -> Result<(Connection, CanonicalUuid), Box<dyn Error>> {
    let mut connection = Connection::connect(runtime.socket()).await?;
    let session_id = create_alias_session(&mut connection).await?;
    let (_, turn) = submit_first_input(
        &mut connection,
        session_id,
        String::from("compaction transaction fixture input"),
    )
    .await?;
    let model = ScriptedModel::single(completed_script(
        "fixture-model",
        "compaction transaction fixture response",
        TokenUsage::unreported(),
    ));
    let probe = execute_streamed_turn(runtime, model, session_id, turn).await?;
    assert_eq!(probe.received_operations().len(), 1);
    Ok((connection, session_id))
}

fn direct_compaction_request(
    session_id: CanonicalUuid,
    command_id: DurableCommandId,
    requested_through_position: Option<u64>,
    identity_base: u128,
) -> PrepareContextCompactionRequest {
    PrepareContextCompactionRequest {
        command: command_id,
        session: SessionId::from_uuid(session_id.into_uuid()),
        requested_through_position,
        automatic_for_turn: None,
        defaults_version: SessionConfigurationDefaultsVersion::first(),
        selection: DirectModelSelection::from_uuid(Uuid::from_u128(1)),
        target: ResolvedProviderTarget::naming(ProviderModelIdentity::from_uuid(Uuid::from_u128(
            3,
        ))),
        input_includes_cache_tokens: false,
        credential_reference: String::from("synthetic-compaction-transaction-credential"),
        call: ModelCallId::from_uuid(Uuid::from_u128(identity_base)),
        compaction: ContextCompactionId::from_uuid(Uuid::from_u128(identity_base + 1)),
        summary_entry: SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(identity_base + 2)),
        result_frontier: ContextFrontierId::from_uuid(Uuid::from_u128(identity_base + 3)),
    }
}

async fn execute_streamed_turn(
    runtime: &mut RunningRuntime,
    scripted: ScriptedModel<ModelCallId>,
    session_id: CanonicalUuid,
    turn_id: CanonicalUuid,
) -> Result<ScriptedModel<ModelCallId>, Box<dyn Error>> {
    execute_streamed_turn_until(runtime, scripted, session_id, turn_id, TurnSettle::Terminal).await
}

async fn execute_streamed_turn_until(
    runtime: &mut RunningRuntime,
    scripted: ScriptedModel<ModelCallId>,
    session_id: CanonicalUuid,
    turn_id: CanonicalUuid,
    settle: TurnSettle,
) -> Result<ScriptedModel<ModelCallId>, Box<dyn Error>> {
    let model_configuration = support::parse_model_configuration(MODEL_CONFIGURATION)?;
    let probe = scripted.clone();
    let provider =
        RuntimeModelCallProvider::new(scripted, model_configuration.runtime_model_catalog(), None)
            .with_text_delta_sink(runtime.provider_text_delta_sink());
    let (execution, fatal_execution) =
        FatalExecutionSupervisor::new(signalboxd::WorkspaceInstructionPreparedExecution::new(
            PostgresProviderModelExecution::new(
                PostgresModelCallRepository::new(
                    runtime.pool.clone(),
                    model_configuration.target_catalog(),
                    ModelCallCredentialReference::new("streaming-fixture"),
                ),
                InProcessAttemptDispatchGate::default(),
                provider,
                None,
            ),
            signalboxd::WorkspaceInstructionRuntime::new(runtime.pool.clone(), None, Vec::new()),
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
            () = wait_for_turn_settle(&observation_pool, session, turn, settle) => {}
            () = fatal_shutdown.wait() => {}
        }
    };
    assert_eq!(
        timeout(RUNTIME_SETTLE_ALLOWANCE, scheduler.run_until(shutdown)).await?,
        SchedulerLoopExit::Shutdown
    );
    assert!(!fatal_execution.is_triggered());
    Ok(probe)
}

async fn execute_recorded_turn(
    runtime: &mut RunningRuntime,
    scripted: RecordingCountedScriptedModel,
    model_configuration: HubModelConfiguration,
    session_id: CanonicalUuid,
    turn_id: CanonicalUuid,
) -> Result<RecordingCountedScriptedModel, Box<dyn Error>> {
    let probe = scripted.clone();
    let provider =
        RuntimeModelCallProvider::new(scripted, model_configuration.runtime_model_catalog(), None)
            .with_text_delta_sink(runtime.provider_text_delta_sink());
    let (execution, fatal_execution) =
        FatalExecutionSupervisor::new(signalboxd::WorkspaceInstructionPreparedExecution::new(
            PostgresProviderModelExecution::new(
                PostgresModelCallRepository::new(
                    runtime.pool.clone(),
                    model_configuration.target_catalog(),
                    ModelCallCredentialReference::new("recording-fixture"),
                ),
                InProcessAttemptDispatchGate::default(),
                provider,
                None,
            ),
            signalboxd::WorkspaceInstructionRuntime::new(runtime.pool.clone(), None, Vec::new()),
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
            () = wait_for_turn_settle(&observation_pool, session, turn, TurnSettle::Terminal) => {}
            () = fatal_shutdown.wait() => {}
        }
    };
    let scheduler_outcome = timeout(RUNTIME_SETTLE_ALLOWANCE, scheduler.run_until(shutdown)).await;
    let Ok(scheduler_exit) = scheduler_outcome else {
        let lifecycle = sqlx::query_as::<_, (String, Option<String>)>(
            "SELECT state_kind, active_phase_kind
               FROM turn_lifecycle
              WHERE session_id = $1 AND turn_id = $2",
        )
        .bind(session.into_uuid())
        .bind(turn.into_uuid())
        .fetch_one(&runtime.pool)
        .await?;
        let calls: Vec<String> = sqlx::query_scalar(
            "SELECT state_kind FROM model_call
              WHERE session_id = $1 AND turn_id = $2",
        )
        .bind(session.into_uuid())
        .bind(turn.into_uuid())
        .fetch_all(&runtime.pool)
        .await?;
        let mut diagnostic_activation = StartEligibleTurnService::new(
            UuidV7StartEligibleTurnIdGenerator,
            StartEligibleTurnRepository::new(runtime.pool.clone()),
        );
        let activation = diagnostic_activation.execute(session).await;
        panic!(
            "recorded turn timed out: lifecycle={lifecycle:?}; calls={calls:?}; activation={activation:?}"
        );
    };
    assert_eq!(scheduler_exit, SchedulerLoopExit::Shutdown);
    assert!(!fatal_execution.is_triggered());
    Ok(probe)
}

async fn execute_guarded_turn(
    runtime: &mut RunningRuntime,
    scripted: RecordingCountedScriptedModel,
    summary_runtime: ScriptedModel<ModelCallId>,
    model_configuration: HubModelConfiguration,
    session_id: CanonicalUuid,
    turn_id: CanonicalUuid,
) -> Result<RecordingCountedScriptedModel, Box<dyn Error>> {
    let probe = scripted.clone();
    let runtime_models = model_configuration.runtime_model_catalog();
    let provider = RuntimeModelCallProvider::new(scripted, runtime_models.clone(), None)
        .with_text_delta_sink(runtime.provider_text_delta_sink());
    let counter = provider.clone();
    let repository = PostgresModelCallRepository::new(
        runtime.pool.clone(),
        model_configuration.target_catalog(),
        ModelCallCredentialReference::new("guarded-recording-fixture"),
    )
    .with_session_credentials(model_configuration.credential_family_catalog());
    let guarded_repository = repository.clone();
    let (execution, fatal_execution) =
        FatalExecutionSupervisor::new(signalboxd::WorkspaceInstructionPreparedExecution::new(
            PostgresProviderModelExecution::new(
                repository,
                InProcessAttemptDispatchGate::default(),
                provider,
                None,
            ),
            signalboxd::WorkspaceInstructionRuntime::new(runtime.pool.clone(), None, Vec::new()),
        ));
    let compaction_model: Arc<dyn signalbox_model_provider_runtime::ContextCompactionModel> =
        Arc::new(RuntimeContextCompactionModel::new(
            summary_runtime,
            runtime_models.clone(),
        ));
    let pass = ContextGuardedTurnPass::new(
        StartEligibleTurnRepository::new(runtime.pool.clone()),
        guarded_repository,
        counter,
        NoToolCatalog,
        runtime_models,
        model_configuration,
        compaction_model,
        execution,
    )
    .with_workspace_instructions(signalboxd::WorkspaceInstructionRuntime::new(
        runtime.pool.clone(),
        None,
        Vec::new(),
    ));
    let mut scheduler = SchedulerLoop::new(runtime.take_work_source(), pass);
    let observation_pool = runtime.pool.clone();
    let session = SessionId::from_uuid(session_id.into_uuid());
    let turn = TurnId::from_uuid(turn_id.into_uuid());
    let fatal_shutdown = fatal_execution.clone();
    let shutdown = async move {
        tokio::select! {
            () = wait_for_turn_settle(&observation_pool, session, turn, TurnSettle::Terminal) => {}
            () = fatal_shutdown.wait() => {}
        }
    };
    assert_eq!(
        timeout(RUNTIME_SETTLE_ALLOWANCE, scheduler.run_until(shutdown)).await?,
        SchedulerLoopExit::Shutdown
    );
    assert!(!fatal_execution.is_triggered());
    Ok(probe)
}

fn completed_script(provider_model: &str, text: &str, usage: TokenUsage) -> Script {
    Script::delivering(TerminalEvidence::Completed(CompletionEvidence {
        exchange: ExchangeFacts::default(),
        message_id: None,
        reported_model: Some(ProviderReportedModel::new(provider_model)),
        finish: CompletionFinish::EndTurn,
        content: vec![AssistantPart::Text(text.to_owned())],
        usage,
    }))
}

// Fleet-soak coverage for issue #1027. Slow/failing tools, boundary loss, an
// unprovisioned workspace, and scheduled goal resumption are named follow-on
// slices: they need the same fleet census but not more boot infrastructure.

// numeric-bound: test fixture - mirrors the `scheduler_pass_admission_cap`
// numeric bound that `config/signalboxd.example.toml` supplies, which
// `support::parse_model_configuration` splices into this fixture's
// configuration. The cap is deployment configuration rather than a compiled
// constant, so the derived ordinary limit below is stated against it.
const FLEET_PASS_ADMISSION_CAP: usize = 16;
// numeric-bound: derived ceiling from the configured pass admission cap.
// One place inside the shared admission cap stays reserved for a
// repository-watch dispatch start, so a fleet that saturates ordinary
// scheduler capacity is one session smaller than the cap itself.
const FLEET_SESSION_COUNT: usize = scheduler_ordinary_pass_limit(FLEET_PASS_ADMISSION_CAP);
// numeric-bound: test setup - preserves the ordinary production occupancy fixture
const FLEET_BASELINE_OCCUPANCY_BOUND: Duration = Duration::from_secs(900);
// numeric-bound: test deadline - exercises the production recovery path promptly
const FLEET_OCCUPANCY_BOUND: Duration = Duration::from_secs(1);
// numeric-bound: test deadline - keeps each fault observation short in CI
const FLEET_ASSERTION_BOUND: Duration = Duration::from_secs(2);
// numeric-bound: test setup - admits a full contended fleet inside two CI minutes
const FLEET_SETUP_BOUND: Duration = Duration::from_secs(120);

struct FleetPrepared {
    correlation: ModelCallId,
    inner: ScriptedPrepared<ModelCallId>,
}

/// How many scripted executions complete and how many hang.
///
/// The completions are served first: a scenario that stands a healthy baseline
/// fleet up before injecting one fault gets the fault on the last execution.
#[derive(Clone, Copy)]
struct FleetModelCardinality {
    hanging: usize,
    completing: usize,
}

#[derive(Clone)]
struct FleetScriptedModel {
    inner: ScriptedModel<ModelCallId>,
    completions_before_hangs: Arc<AtomicUsize>,
    hangs_remaining: Arc<AtomicUsize>,
    in_flight_hangs: Arc<AtomicUsize>,
    completed_calls: Arc<Mutex<Vec<ModelCallId>>>,
}

impl FleetScriptedModel {
    fn new(cardinality: FleetModelCardinality) -> Self {
        Self {
            inner: ScriptedModel::following(std::iter::repeat_n(
                completed_script(
                    "fixture-model",
                    "fleet session completed",
                    TokenUsage::unreported(),
                ),
                cardinality.hanging + cardinality.completing,
            )),
            completions_before_hangs: Arc::new(AtomicUsize::new(cardinality.completing)),
            hangs_remaining: Arc::new(AtomicUsize::new(cardinality.hanging)),
            in_flight_hangs: Arc::new(AtomicUsize::new(0)),
            completed_calls: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn in_flight_hangs(&self) -> usize {
        self.in_flight_hangs.load(Ordering::SeqCst)
    }

    fn completed_call_ids(&self) -> Vec<ModelCallId> {
        self.completed_calls
            .lock()
            .expect("the fleet completion lock is available")
            .clone()
    }

    fn record_completed_call(&self, correlation: ModelCallId) {
        self.completed_calls
            .lock()
            .expect("the fleet completion lock is available")
            .push(correlation);
    }
}

struct FleetHangGuard(Arc<AtomicUsize>);

impl Drop for FleetHangGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

impl ModelRuntime<ModelCallId> for FleetScriptedModel {
    type Prepared = FleetPrepared;

    async fn prepare(
        &self,
        operation: ModelOperation<ModelCallId>,
        cancellation: CancellationSignal,
    ) -> PreparationOutcome<ModelCallId, Self::Prepared> {
        let correlation = operation.correlation;
        match self.inner.prepare(operation, cancellation).await {
            PreparationOutcome::Prepared(inner) => {
                PreparationOutcome::Prepared(FleetPrepared { correlation, inner })
            }
            PreparationOutcome::Defect {
                correlation,
                defect,
            } => PreparationOutcome::Defect {
                correlation,
                defect,
            },
            PreparationOutcome::Cancelled { correlation } => {
                PreparationOutcome::Cancelled { correlation }
            }
            PreparationOutcome::Failed {
                correlation,
                failure,
            } => PreparationOutcome::Failed {
                correlation,
                failure,
            },
        }
    }

    async fn execute(
        &self,
        prepared: Self::Prepared,
        sink: &mut (dyn ObservationSink<ModelCallId> + Send),
        cancellation: CancellationSignal,
    ) -> TerminalReport<ModelCallId> {
        let correlation = prepared.correlation;
        let completes = self
            .completions_before_hangs
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
                remaining.checked_sub(1)
            })
            .is_ok();
        if completes {
            let report = self.inner.execute(prepared.inner, sink, cancellation).await;
            self.record_completed_call(correlation);
            return report;
        }
        let hangs = self
            .hangs_remaining
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
                remaining.checked_sub(1)
            })
            .is_ok();
        if hangs {
            sink.observe(Observation {
                correlation,
                fact: ObservationFact::SendCommenced,
            });
            self.in_flight_hangs.fetch_add(1, Ordering::SeqCst);
            let _guard = FleetHangGuard(Arc::clone(&self.in_flight_hangs));
            pending::<TerminalReport<ModelCallId>>().await
        } else {
            let report = self.inner.execute(prepared.inner, sink, cancellation).await;
            self.record_completed_call(correlation);
            report
        }
    }
}

#[derive(Debug)]
struct CommissionedFleet {
    sessions: Vec<CanonicalUuid>,
}

async fn commission_fleet(
    runtime: &RunningRuntime,
    first_index: usize,
    session_count: usize,
) -> Result<CommissionedFleet, Box<dyn Error>> {
    let mut connection = Connection::connect(runtime.socket()).await?;
    let mut sessions = Vec::with_capacity(session_count);
    for offset in 0..session_count {
        let index = first_index + offset;
        connection
            .request(
                u64::try_from(index + 2)?,
                ClientRequest::CommissionSession {
                    command_id: command()?,
                    template_name: String::from("merge-forward"),
                    fence: CommissionedSessionFence::Branch {
                        repository: String::from("sample-user/sample-repository"),
                        branch: format!("agent/fleet-soak-{index}"),
                    },
                    statement: format!("complete fleet soak session {index}"),
                    content: InputContent::new(String::from("return the scripted reply")),
                },
            )
            .await?;
        let response = response_within(&mut connection).await?.message().clone();
        let ServerMessage::SessionCommissioned { session_id, .. } = response else {
            panic!("fleet commission returned {response:?}");
        };
        sessions.push(session_id);
    }
    Ok(CommissionedFleet { sessions })
}

struct FleetRuntimeTasks {
    shutdown: watch::Sender<bool>,
    scheduler: JoinHandle<SchedulerLoopExit>,
    turn_liveness: JoinHandle<()>,
}

impl FleetRuntimeTasks {
    async fn stop(self) -> Result<(), Box<dyn Error>> {
        self.shutdown.send_replace(true);
        let scheduler_exit = timeout(RUNTIME_SETTLE_ALLOWANCE, self.scheduler).await??;
        timeout(RUNTIME_SETTLE_ALLOWANCE, self.turn_liveness).await??;
        if scheduler_exit != SchedulerLoopExit::Shutdown {
            return Err(io::Error::other("fleet scheduler returned a non-shutdown exit").into());
        }
        Ok(())
    }

    async fn kill(self) -> Result<(), Box<dyn Error>> {
        self.scheduler.abort();
        self.turn_liveness.abort();
        let scheduler = self.scheduler.await;
        let turn_liveness = self.turn_liveness.await;
        let scheduler = scheduler.expect_err("the killed scheduler task must not return normally");
        let turn_liveness =
            turn_liveness.expect_err("the killed turn-liveness task must not return normally");
        assert!(
            scheduler.is_cancelled(),
            "the scheduler task must stop by cancellation, got {scheduler}"
        );
        assert!(
            turn_liveness.is_cancelled(),
            "the turn-liveness task must stop by cancellation, got {turn_liveness}"
        );
        Ok(())
    }
}

async fn wait_for_fleet_shutdown(mut shutdown: watch::Receiver<bool>) {
    while !*shutdown.borrow_and_update() {
        if shutdown.changed().await.is_err() {
            return;
        }
    }
}

/// Commissions one extra session after a restart, as a readiness control that
/// proves the replacement scheduler is admitting fresh work.
async fn commission_fleet_control(
    runtime: &RunningRuntime,
) -> Result<CanonicalUuid, Box<dyn Error>> {
    let mut connection = Connection::connect(runtime.socket()).await?;
    connection
        .request(
            1,
            ClientRequest::CommissionSession {
                command_id: command()?,
                template_name: String::from("merge-forward"),
                fence: CommissionedSessionFence::Branch {
                    repository: String::from("sample-user/sample-repository"),
                    branch: String::from("agent/fleet-soak-control"),
                },
                statement: String::from("complete the fleet scheduler readiness control"),
                content: InputContent::new(String::from("return the scripted reply")),
            },
        )
        .await?;
    let response = response_within(&mut connection).await?.message().clone();
    let ServerMessage::SessionCommissioned { session_id, .. } = response else {
        return Err(
            io::Error::other(format!("fleet control commission returned {response:?}")).into(),
        );
    };
    Ok(session_id)
}

fn start_fleet_scheduler(
    runtime: &mut RunningRuntime,
    model: FleetScriptedModel,
    occupancy_bound: SchedulerPassOccupancyBound,
) -> Result<FleetRuntimeTasks, Box<dyn Error>> {
    let configuration = support::parse_model_configuration(MODEL_CONFIGURATION)?;
    let bounds = configuration.numeric_bounds();
    let expired_pass_recovery_policy = ExpiredPassRecoveryPolicy::new(
        bounds
            .integer("expired_pass_recovery_attempts")
            .flatten()
            .and_then(|value| u32::try_from(value).ok()),
        bounds
            .duration("expired_pass_recovery_attempt_bound")
            .flatten(),
        bounds
            .duration("expired_pass_recovery_lock_retry_delay")
            .flatten(),
        bounds
            .duration("expired_pass_recovery_conservative_retry_delay")
            .flatten(),
    );
    let turn_liveness_persistence_bounds = TurnLivenessPersistenceBounds::new(
        bounds.duration("terminalization_lock_wait").flatten(),
        bounds.duration("terminalization_acquire_wait").flatten(),
        bounds.duration("terminalization_write_lock_wait").flatten(),
    );
    let turn_liveness_numeric_bounds = TurnLivenessNumericBounds::new(
        bounds
            .integer("terminalizations_per_liveness_scan")
            .flatten()
            .and_then(|value| usize::try_from(value).ok()),
        bounds
            .duration("turn_liveness_recovery_attempt_bound")
            .flatten(),
        bounds
            .integer("automatic_reconciliations_per_liveness_scan")
            .flatten()
            .and_then(|value| usize::try_from(value).ok()),
        bounds
            .duration("automatic_reconciliation_attempt_bound")
            .flatten(),
        turn_liveness_persistence_bounds,
    );
    let stale_active_turn_bound = bounds
        .duration("stale_active_turn_bound")
        .flatten()
        .map(StaleActiveTurnBound::try_new)
        .transpose()?;
    let turn_liveness_scan_interval = bounds
        .duration("turn_liveness_scan_interval")
        .flatten()
        .map(TurnLivenessScanInterval::try_new)
        .transpose()?;
    let automatic_reconciliation_attempt_budget = bounds
        .integer("automatic_reconciliation_attempt_budget")
        .flatten()
        .and_then(|value| u32::try_from(value).ok());
    let automatic_reconciliation_base_backoff = bounds
        .duration("automatic_reconciliation_base_backoff")
        .flatten();
    let automatic_reconciliation_backoff_cap = bounds
        .duration("automatic_reconciliation_backoff_cap")
        .flatten();
    let provider =
        RuntimeModelCallProvider::new(model, configuration.runtime_model_catalog(), None)
            .with_text_delta_sink(runtime.provider_text_delta_sink());
    let (execution, fatal_execution) =
        FatalExecutionSupervisor::new(signalboxd::WorkspaceInstructionPreparedExecution::new(
            PostgresProviderModelExecution::new(
                PostgresModelCallRepository::new(
                    runtime.pool.clone(),
                    configuration.target_catalog(),
                    ModelCallCredentialReference::new("fleet-soak-fixture"),
                ),
                InProcessAttemptDispatchGate::default(),
                provider,
                None,
            ),
            signalboxd::WorkspaceInstructionRuntime::new(runtime.pool.clone(), None, Vec::new()),
        ));
    let pass = ActivatedTurnPass::new(
        StartEligibleTurnService::new(
            UuidV7StartEligibleTurnIdGenerator,
            StartEligibleTurnRepository::new(runtime.pool.clone()),
        ),
        execution,
    )
    .with_occupancy_recovery(
        runtime.pool.clone(),
        runtime.eligibility_nudge.clone(),
        expired_pass_recovery_policy,
        turn_liveness_persistence_bounds,
    );
    let pass = WitnessedEligibilityPass::new(pass, runtime.reconciliation_witness());
    let mut scheduler =
        SchedulerLoop::new(runtime.take_work_source(), pass).with_occupancy_bound(occupancy_bound);
    let turn_liveness = TurnLivenessRuntime::new(
        runtime.pool.clone(),
        stale_active_turn_bound,
        turn_liveness_scan_interval,
        automatic_reconciliation_attempt_budget,
        automatic_reconciliation_base_backoff,
        automatic_reconciliation_backoff_cap,
        turn_liveness_numeric_bounds,
    );
    let (shutdown, shutdown_receiver) = watch::channel(false);
    let scheduler_shutdown = shutdown_receiver.clone();
    let scheduler = tokio::spawn(async move {
        scheduler
            .run_until(async move {
                tokio::select! {
                    () = fatal_execution.wait() => {}
                    () = wait_for_fleet_shutdown(scheduler_shutdown) => {}
                }
            })
            .await
    });
    let turn_liveness = tokio::spawn(turn_liveness.run(shutdown_receiver));
    Ok(FleetRuntimeTasks {
        shutdown,
        scheduler,
        turn_liveness,
    })
}

async fn wait_for_hangs(model: &FleetScriptedModel, expected: usize) -> Result<(), Box<dyn Error>> {
    let observed = timeout(FLEET_SETUP_BOUND, async {
        while model.in_flight_hangs() != expected {
            tokio::task::yield_now().await;
        }
    })
    .await;
    if observed.is_err() {
        return Err(io::Error::other(format!(
            "fleet hang setup expected {expected} in flight, observed {}",
            model.in_flight_hangs()
        ))
        .into());
    }
    Ok(())
}

/// Waits for one drained eligibility cycle, so a replacement scheduler's
/// reconciliation pass is observed as completed rather than slept for.
async fn wait_for_reconciliation(witness: &ReconciliationWitness) -> Result<(), Box<dyn Error>> {
    timeout(FLEET_SETUP_BOUND, async {
        while witness.completed_cycles() == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await?;
    Ok(())
}

/// Tears the scheduler and turn-liveness tasks down from whatever state they
/// are in, so a panicking scenario still releases the fixture.
async fn abort_fleet_scheduler(tasks: FleetRuntimeTasks) -> Result<(), Box<dyn Error>> {
    let FleetRuntimeTasks {
        shutdown,
        scheduler,
        turn_liveness,
    } = tasks;
    shutdown.send_replace(true);
    scheduler.abort();
    turn_liveness.abort();
    let stopped = scheduler.await;
    let liveness = turn_liveness.await;
    if !matches!(&stopped, Ok(SchedulerLoopExit::Shutdown))
        && !matches!(&stopped, Err(error) if error.is_cancelled())
    {
        return Err(io::Error::other(format!(
            "the fleet scheduler must stop by cancellation or fatal-driven shutdown: {stopped:?}"
        ))
        .into());
    }
    if !matches!(&liveness, Ok(())) && !matches!(&liveness, Err(error) if error.is_cancelled()) {
        return Err(io::Error::other(format!(
            "the fleet turn-liveness runtime must stop by cancellation or shutdown: {liveness:?}"
        ))
        .into());
    }
    Ok(())
}

async fn wait_for_completed_calls(
    model: &FleetScriptedModel,
    expected: usize,
) -> Result<Vec<ModelCallId>, Box<dyn Error>> {
    Ok(timeout(FLEET_SETUP_BOUND, async {
        loop {
            let completed = model.completed_call_ids();
            if completed.len() == expected {
                return completed;
            }
            tokio::task::yield_now().await;
        }
    })
    .await?)
}

async fn wait_for_model_call_for_session(
    repository: &FleetSoakCensusRepository,
    session: CanonicalUuid,
) -> Result<ModelCallId, Box<dyn Error>> {
    timeout(FLEET_SETUP_BOUND, async {
        loop {
            if let Some(model_call) = repository
                .model_call_id_for_session(SessionId::from_uuid(session.into_uuid()))
                .await?
            {
                return Ok::<ModelCallId, Box<dyn Error>>(model_call);
            }
            tokio::task::yield_now().await;
        }
    })
    .await?
}

async fn wait_for_completed_call(
    model: &FleetScriptedModel,
    expected: ModelCallId,
) -> Result<(), Box<dyn Error>> {
    timeout(FLEET_SETUP_BOUND, async {
        while !model.completed_call_ids().contains(&expected) {
            tokio::task::yield_now().await;
        }
    })
    .await?;
    Ok(())
}

async fn wait_for_terminal_calls(
    repository: &FleetSoakCensusRepository,
    model_calls: &[ModelCallId],
) -> Result<(), Box<dyn Error>> {
    timeout(FLEET_SETUP_BOUND, async {
        loop {
            if repository
                .census_for(model_calls)
                .await?
                .terminal_model_calls()
                == i64::try_from(model_calls.len())?
            {
                return Ok::<(), Box<dyn Error>>(());
            }
            tokio::task::yield_now().await;
        }
    })
    .await??;
    Ok(())
}

async fn wait_for_terminal_turns(
    repository: &FleetSoakCensusRepository,
    model_calls: &[ModelCallId],
) -> Result<(), Box<dyn Error>> {
    timeout(FLEET_SETUP_BOUND, async {
        loop {
            if repository.census_for(model_calls).await?.terminal_turns()
                == i64::try_from(model_calls.len())?
            {
                return Ok::<(), Box<dyn Error>>(());
            }
            tokio::task::yield_now().await;
        }
    })
    .await??;
    Ok(())
}

/// Waits for exactly `hung_model_call` to reach its typed ambiguity park with
/// its execution released, rather than counting parks across the database.
async fn wait_for_ambiguity_park(
    repository: &FleetSoakCensusRepository,
    model: &FleetScriptedModel,
    hung_model_call: ModelCallId,
    bound: Duration,
) -> Result<(), Box<dyn Error>> {
    timeout(bound, async {
        loop {
            if repository
                .has_ambiguous_recovery_park(hung_model_call)
                .await?
                && model.in_flight_hangs() == 0
            {
                return Ok::<(), Box<dyn Error>>(());
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .map_err(|_| io::Error::other("fleet model call did not reach its ambiguity park"))??;
    Ok(())
}

fn assert_hung_fleet_outcome(
    model: &FleetScriptedModel,
    census: FleetSoakCensus,
    hung_call_has_ambiguity_park: bool,
) -> Result<(), Box<dyn Error>> {
    let active = census.active_turns();
    let terminal = census.terminal_turns();
    let typed_terminal_calls = census.terminal_model_calls();
    if model.in_flight_hangs() != 0
        || active != 1
        || terminal != i64::try_from(FLEET_SESSION_COUNT - 1)?
        || typed_terminal_calls != i64::try_from(FLEET_SESSION_COUNT)?
        || census.awaiting_model_call_recovery_turns() != 1
        || census.ambiguous_model_calls() != 1
        || !hung_call_has_ambiguity_park
    {
        return Err(io::Error::other(format!(
            "fleet liveness failed: hangs={}, active={active}, terminal={terminal}, typed_terminal_calls={typed_terminal_calls}, recovery_parks={}, ambiguous_calls={}, hung_call_has_ambiguity_park={hung_call_has_ambiguity_park}",
            model.in_flight_hangs(),
            census.awaiting_model_call_recovery_turns(),
            census.ambiguous_model_calls()
        ))
        .into());
    }
    Ok(())
}

fn assert_restarted_fleet_outcome(
    census: FleetSoakCensus,
    original_model: &FleetScriptedModel,
    replacement_model: &FleetScriptedModel,
) -> Result<(), Box<dyn Error>> {
    if census.active_turns() != 0
        || census.terminal_turns() != i64::try_from(FLEET_SESSION_COUNT)?
        || census.awaiting_model_call_recovery_turns() != 0
        || census.terminal_model_calls() != i64::try_from(FLEET_SESSION_COUNT)?
        || original_model.in_flight_hangs() != 0
        || replacement_model.in_flight_hangs() != 0
    {
        return Err(io::Error::other(format!(
            "restart must release every original execution and reconcile every ambiguous operation into a terminal turn without a user decision: census={census:?}, original_hangs={}, replacement_hangs={}",
            original_model.in_flight_hangs(),
            replacement_model.in_flight_hangs()
        ))
        .into());
    }
    Ok(())
}

/// Issue #1027: a post-acceptance model hang releases its authoritative pass
/// and reaches a durable typed ambiguity park inside the occupancy bound.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn fleet_soak_hung_model_call_has_bounded_pass_occupancy_and_typed_disposition()
-> Result<(), Box<dyn Error>> {
    let mut runtime = RunningRuntime::start().await?;
    let mut tasks: Option<FleetRuntimeTasks> = None;
    let scenario = AssertUnwindSafe(async {
        let census_repository = FleetSoakCensusRepository::new(runtime.pool.clone());
        let baseline_fleet = commission_fleet(&runtime, 0, FLEET_SESSION_COUNT - 1).await?;
        let model = FleetScriptedModel::new(FleetModelCardinality {
            hanging: 1,
            completing: FLEET_SESSION_COUNT - 1,
        });
        tasks = Some(start_fleet_scheduler(
            &mut runtime,
            model.clone(),
            SchedulerPassOccupancyBound::try_new(FLEET_BASELINE_OCCUPANCY_BOUND)?,
        )?);
        let completed_calls = wait_for_completed_calls(&model, FLEET_SESSION_COUNT - 1).await?;
        wait_for_terminal_calls(&census_repository, &completed_calls).await?;
        wait_for_terminal_turns(&census_repository, &completed_calls).await?;
        tasks
            .take()
            .expect("the baseline fleet scheduler was installed")
            .stop()
            .await?;
        runtime.restart().await?;
        let fault_fleet = commission_fleet(&runtime, FLEET_SESSION_COUNT - 1, 1).await?;
        tasks = Some(start_fleet_scheduler(
            &mut runtime,
            model.clone(),
            SchedulerPassOccupancyBound::try_new(FLEET_OCCUPANCY_BOUND)?,
        )?);
        wait_for_hangs(&model, 1).await?;
        let model_calls = census_repository.model_call_ids().await?;
        assert_eq!(
            baseline_fleet.sessions.len(),
            FLEET_SESSION_COUNT - 1,
            "baseline fleet session cardinality mismatch"
        );
        assert_eq!(
            fault_fleet.sessions.len(),
            1,
            "fault fleet session cardinality mismatch"
        );
        assert_eq!(
            model_calls.len(),
            FLEET_SESSION_COUNT,
            "fleet model-call cardinality mismatch"
        );
        let hung_model_calls = model_calls
            .iter()
            .copied()
            .filter(|model_call| !completed_calls.contains(model_call))
            .collect::<Vec<_>>();
        let [hung_model_call] = hung_model_calls.as_slice() else {
            return Err(io::Error::other(format!(
                "expected one hung model call, observed {hung_model_calls:?}"
            ))
            .into());
        };
        wait_for_ambiguity_park(
            &census_repository,
            &model,
            *hung_model_call,
            FLEET_ASSERTION_BOUND,
        )
        .await?;
        let census = census_repository.census_for(&model_calls).await?;
        let hung_call_has_ambiguity_park = census_repository
            .has_ambiguous_recovery_park(*hung_model_call)
            .await?;
        assert_hung_fleet_outcome(&model, census, hung_call_has_ambiguity_park)
    })
    .catch_unwind()
    .await;

    let scheduler_cleanup = match tasks {
        Some(tasks) => abort_fleet_scheduler(tasks).await,
        None => Ok(()),
    };
    let runtime_cleanup = runtime.stop().await;
    match scenario {
        Ok(outcome) => {
            scheduler_cleanup?;
            runtime_cleanup?;
            outcome
        }
        Err(panic) => {
            if let Err(error) = scheduler_cleanup {
                eprintln!("fleet scheduler cleanup after panic failed: {error}");
            }
            if let Err(error) = runtime_cleanup {
                eprintln!("fleet runtime cleanup after panic failed: {error}");
            }
            resume_unwind(panic)
        }
    }
}

/// Issue #1027 / INV-034: killing the daemon with a full fleet in model
/// execution leaves every model call ambiguous. Ambiguous-operation
/// reconciliation must release local scheduler ownership and then resume or
/// terminalize every such turn once a replacement daemon takes over, without
/// waiting on a user decision.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn fleet_soak_kill_restart_resumes_or_terminalizes_every_active_turn()
-> Result<(), Box<dyn Error>> {
    let mut runtime = RunningRuntime::start().await?;
    let mut tasks: Option<FleetRuntimeTasks> = None;
    let scenario = AssertUnwindSafe(async {
        let census_repository = FleetSoakCensusRepository::new(runtime.pool.clone());
        let fleet = commission_fleet(&runtime, 0, FLEET_SESSION_COUNT).await?;
        let hanging_model = FleetScriptedModel::new(FleetModelCardinality {
            hanging: FLEET_SESSION_COUNT,
            completing: 0,
        });
        tasks = Some(start_fleet_scheduler(
            &mut runtime,
            hanging_model.clone(),
            SchedulerPassOccupancyBound::try_new(FLEET_BASELINE_OCCUPANCY_BOUND)?,
        )?);
        wait_for_hangs(&hanging_model, FLEET_SESSION_COUNT).await?;
        let pre_kill_model_call_ids = census_repository.model_call_ids().await?;
        assert_eq!(
            pre_kill_model_call_ids.len(),
            FLEET_SESSION_COUNT,
            "pre-kill model-call cardinality mismatch"
        );
        tasks
            .take()
            .expect("the first fleet scheduler was installed")
            .kill()
            .await?;
        wait_for_hangs(&hanging_model, 0).await?;
        let _recovered = runtime.kill_and_restart().await?;
        // One script per recoverable turn plus the readiness control, so the
        // fixture does not decide whether reconciliation reissues a call.
        let replacement_model = FleetScriptedModel::new(FleetModelCardinality {
            hanging: 0,
            completing: FLEET_SESSION_COUNT + 1,
        });
        let replacement_reconciliation = runtime.reconciliation_witness();
        tasks = Some(start_fleet_scheduler(
            &mut runtime,
            replacement_model.clone(),
            SchedulerPassOccupancyBound::try_new(FLEET_BASELINE_OCCUPANCY_BOUND)?,
        )?);
        wait_for_reconciliation(&replacement_reconciliation).await?;
        let control_session = commission_fleet_control(&runtime).await?;
        let control_model_call =
            wait_for_model_call_for_session(&census_repository, control_session).await?;
        wait_for_completed_call(&replacement_model, control_model_call).await?;
        wait_for_terminal_turns(&census_repository, &pre_kill_model_call_ids).await?;
        let census = census_repository
            .census_for(&pre_kill_model_call_ids)
            .await?;
        assert_eq!(
            fleet.sessions.len(),
            FLEET_SESSION_COUNT,
            "fleet session cardinality mismatch"
        );
        assert_restarted_fleet_outcome(census, &hanging_model, &replacement_model)
    })
    .catch_unwind()
    .await;

    let scheduler_cleanup = match tasks {
        Some(tasks) => abort_fleet_scheduler(tasks).await,
        None => Ok(()),
    };
    let runtime_cleanup = runtime.stop().await;
    match scenario {
        Ok(outcome) => {
            scheduler_cleanup?;
            runtime_cleanup?;
            outcome
        }
        Err(panic) => {
            if let Err(error) = scheduler_cleanup {
                eprintln!("fleet scheduler cleanup after panic failed: {error}");
            }
            if let Err(error) = runtime_cleanup {
                eprintln!("fleet runtime cleanup after panic failed: {error}");
            }
            resume_unwind(panic)
        }
    }
}

fn rendered_text_messages(
    operation: &ModelOperation<ModelCallId>,
) -> Vec<(signalbox_model_runtime::ConversationRole, String)> {
    operation
        .messages
        .iter()
        .map(|message| {
            let [MessagePart::Text(text)] = message.parts.as_slice() else {
                panic!("the compaction fixture expects text-only runtime messages")
            };
            (message.role, text.clone())
        })
        .collect()
}

#[track_caller]
fn submitted_session(message: &ServerMessage) -> CanonicalUuid {
    match message {
        ServerMessage::InputSubmitted { session_id, .. } => *session_id,
        message => panic!("fixture expected input-submitted, got {message:?}"),
    }
}

#[track_caller]
fn transcript_snapshot_start_cursor(
    message: &ServerMessage,
    expected_session: CanonicalUuid,
) -> u64 {
    match message {
        ServerMessage::TranscriptSnapshotStart {
            session_id, cursor, ..
        } if *session_id == expected_session => cursor.value(),
        message => panic!("fixture expected transcript-snapshot start, got {message:?}"),
    }
}

#[track_caller]
fn transcript_turn_projection(message: &ServerMessage) -> (CanonicalUuid, u64, TurnState) {
    match message {
        ServerMessage::TranscriptTurn {
            turn_id,
            acceptance_position,
            state,
            ..
        } => (*turn_id, acceptance_position.value(), state.clone()),
        message => panic!("fixture expected transcript-turn projection, got {message:?}"),
    }
}

#[track_caller]
fn submitted_input_identity(
    message: &ServerMessage,
    expected_session: CanonicalUuid,
    expected_position: u64,
) -> CanonicalUuid {
    match message {
        ServerMessage::InputSubmitted {
            session_id,
            accepted_input_id,
            acceptance_position,
            ..
        } if *session_id == expected_session
            && acceptance_position.value() == expected_position =>
        {
            *accepted_input_id
        }
        message => panic!("fixture expected input-submitted receipt, got {message:?}"),
    }
}

#[track_caller]
fn transcript_model_call_count(message: &ServerMessage) -> u64 {
    match message {
        ServerMessage::TranscriptModelCallsEnd { model_call_count } => model_call_count.value(),
        message => panic!("fixture expected transcript-model-calls end, got {message:?}"),
    }
}

#[derive(Debug, Eq, PartialEq)]
struct TranscriptSnapshotEndFacts {
    session_id: CanonicalUuid,
    cursor: u64,
    turn_count: u64,
    entry_count: u64,
}

#[track_caller]
fn transcript_snapshot_end_facts(message: &ServerMessage) -> TranscriptSnapshotEndFacts {
    match message {
        ServerMessage::TranscriptSnapshotEnd {
            session_id,
            cursor,
            turn_count,
            entry_count,
        } => TranscriptSnapshotEndFacts {
            session_id: *session_id,
            cursor: cursor.value(),
            turn_count: turn_count.value(),
            entry_count: entry_count.value(),
        },
        message => panic!("fixture expected transcript-snapshot end, got {message:?}"),
    }
}

#[derive(Debug, Eq, PartialEq)]
struct InputAcceptedEventFacts {
    cursor: u64,
    session_id: CanonicalUuid,
    accepted_input_id: CanonicalUuid,
    acceptance_position: u64,
    content: UserInputContent,
}

#[derive(Debug, Eq, PartialEq)]
struct TurnModelSettingsResolvedEventFacts {
    cursor: u64,
    session_id: CanonicalUuid,
    accepted_input_id: CanonicalUuid,
}

#[derive(Debug, Eq, PartialEq)]
struct SessionModelSettingsChangedEventFacts {
    session_id: CanonicalUuid,
    prior_defaults_version: u64,
    installed_defaults_version: u64,
    installed_settings: ModelSettingsSnapshot,
    caller_override: ModelSettingsOverlay,
    adjustments: Vec<ModelChangeAdjustment>,
}

#[derive(Debug, Eq, PartialEq)]
struct SessionDefaultsReplacedFacts {
    defaults_version: CanonicalU64,
    installed_settings: ModelSettingsSnapshot,
}

#[track_caller]
fn session_defaults_replaced_facts(message: &ServerMessage) -> SessionDefaultsReplacedFacts {
    match message {
        ServerMessage::SessionDefaultsReplaced {
            defaults_version,
            model_settings,
            ..
        } => SessionDefaultsReplacedFacts {
            defaults_version: *defaults_version,
            installed_settings: *model_settings,
        },
        message => panic!("fixture expected defaults-replaced receipt, got {message:?}"),
    }
}

#[track_caller]
fn session_model_settings_changed_event_facts(
    message: &ServerMessage,
) -> SessionModelSettingsChangedEventFacts {
    match message {
        ServerMessage::SessionEvent {
            session_id,
            event:
                SessionEvent::SessionModelSettingsChanged {
                    prior_defaults_version,
                    installed_defaults_version,
                    installed_settings,
                    caller_override,
                    adjustments,
                    ..
                },
            ..
        } => SessionModelSettingsChangedEventFacts {
            session_id: *session_id,
            prior_defaults_version: prior_defaults_version.value(),
            installed_defaults_version: installed_defaults_version.value(),
            installed_settings: *installed_settings,
            caller_override: *caller_override,
            adjustments: adjustments.clone(),
        },
        message => panic!("fixture expected session-settings change event, got {message:?}"),
    }
}

#[track_caller]
fn turn_model_settings_resolved_event_facts(
    message: &ServerMessage,
) -> TurnModelSettingsResolvedEventFacts {
    match message {
        ServerMessage::SessionEvent {
            cursor,
            session_id,
            event:
                SessionEvent::TurnModelSettingsResolved {
                    accepted_input_id, ..
                },
        } => TurnModelSettingsResolvedEventFacts {
            cursor: cursor.value(),
            session_id: *session_id,
            accepted_input_id: *accepted_input_id,
        },
        message => panic!("fixture expected turn-settings resolution event, got {message:?}"),
    }
}

#[track_caller]
fn input_accepted_event_facts(message: &ServerMessage) -> InputAcceptedEventFacts {
    match message {
        ServerMessage::SessionEvent {
            cursor,
            session_id,
            event:
                SessionEvent::InputAccepted {
                    accepted_input_id,
                    acceptance_position,
                    content,
                    ..
                },
        } => InputAcceptedEventFacts {
            cursor: cursor.value(),
            session_id: *session_id,
            accepted_input_id: *accepted_input_id,
            acceptance_position: acceptance_position.value(),
            content: content.clone(),
        },
        message => panic!("fixture expected input-accepted event, got {message:?}"),
    }
}

#[track_caller]
fn protocol_error_code(message: &ServerMessage) -> ErrorCode {
    match message {
        ServerMessage::Error { code, .. } => *code,
        message => panic!("fixture expected protocol error, got {message:?}"),
    }
}

#[track_caller]
fn protocol_error_detail(message: &ServerMessage) -> Option<RejectionDetail> {
    match message {
        ServerMessage::Error { detail, .. } => detail.value(),
        message => panic!("fixture expected protocol error detail, got {message:?}"),
    }
}

async fn activate_turn(pool: &PgPool, session: SessionId) -> Result<(), Box<dyn Error>> {
    let mut service = StartEligibleTurnService::new(
        UuidV7StartEligibleTurnIdGenerator,
        StartEligibleTurnRepository::new(pool.clone()),
    );
    let StartEligibleTurnOutcome::Activated(activated) = service.execute(session).await? else {
        return Err(io::Error::other("the fixture turn must activate").into());
    };
    let recorded = signalboxd::WorkspaceInstructionRuntime::new(pool.clone(), None, Vec::new())
        .prepare(session, activated.turn())
        .await?;
    if !recorded {
        return Err(io::Error::other("the fixture instruction manifest must record").into());
    }
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
        None,
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

/// S28 / INV-038: the user-visible operation distinguishes first insertion
/// from exact-snapshot reimport while retaining the winner's identity.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn s28_inv038_single_shot_and_chunked_import_resolve_the_same_snapshot()
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
            ProtocolVersion::One,
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

    let declared_size_bytes = CanonicalU64::new(u64::try_from(source.as_bytes().len())?);
    connection
        .request_version(
            ProtocolVersion::One,
            2,
            ClientRequest::BeginConversationImport {
                format: ConversationImportFormat::ClaudeCodeSessionJsonlV2,
                declared_size_bytes,
            },
        )
        .await?;
    let begun = response_within(&mut connection).await?;
    assert_eq!(
        begun.message(),
        &ServerMessage::ConversationImportBegun {
            declared_size_bytes,
        }
    );
    connection
        .request_version(
            ProtocolVersion::One,
            3,
            ClientRequest::AppendConversationImport { chunk: source },
        )
        .await?;
    let appended = response_within(&mut connection).await?;
    assert_eq!(
        appended.message(),
        &ServerMessage::ConversationImportAppended {
            assembled_size_bytes: declared_size_bytes,
        }
    );
    connection
        .request_version(
            ProtocolVersion::One,
            4,
            ClientRequest::CommitConversationImport {},
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

/// S28: disconnect discards per-connection partial import state.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn s28_disconnect_discards_a_partial_chunked_import() -> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let chunk = vec![b'x'];
    let declared_size_bytes = CanonicalU64::new(u64::try_from(chunk.len())?);
    let mut abandoned = Connection::connect(runtime.socket()).await?;
    abandoned
        .request_version(
            ProtocolVersion::One,
            1,
            ClientRequest::BeginConversationImport {
                format: ConversationImportFormat::CodexRolloutJsonlV1,
                declared_size_bytes,
            },
        )
        .await?;
    let begun = response_within(&mut abandoned).await?;
    assert_eq!(
        begun.message(),
        &ServerMessage::ConversationImportBegun {
            declared_size_bytes,
        }
    );
    abandoned
        .request_version(
            ProtocolVersion::One,
            2,
            ClientRequest::AppendConversationImport {
                chunk: ConversationImportSource::new(chunk.clone()),
            },
        )
        .await?;
    let appended = response_within(&mut abandoned).await?;
    assert_eq!(
        appended.message(),
        &ServerMessage::ConversationImportAppended {
            assembled_size_bytes: declared_size_bytes,
        }
    );
    drop(abandoned);

    let mut replacement = Connection::connect(runtime.socket()).await?;
    replacement
        .request_version(
            ProtocolVersion::One,
            3,
            ClientRequest::BeginConversationImport {
                format: ConversationImportFormat::CodexRolloutJsonlV1,
                declared_size_bytes,
            },
        )
        .await?;
    let replacement_begun = response_within(&mut replacement).await?;
    assert_eq!(
        replacement_begun.message(),
        &ServerMessage::ConversationImportBegun {
            declared_size_bytes,
        }
    );
    replacement
        .request_version(
            ProtocolVersion::One,
            4,
            ClientRequest::AbortConversationImport {},
        )
        .await?;
    let aborted = response_within(&mut replacement).await?;
    assert_eq!(
        aborted.message(),
        &ServerMessage::ConversationImportAborted {}
    );

    drop(replacement);
    runtime.stop().await
}

/// One durable synthetic imported conversation and the identities its
/// selectable positions carry.
struct ImportedInspectionFixture {
    conversation: CanonicalUuid,
    user_entry: CanonicalUuid,
    tool_entry: CanonicalUuid,
    user_text: &'static str,
    /// The greatest selectable position, which is also the entry count: the
    /// two-record source below emits exactly one entry per record.
    last_position: CanonicalU64,
}

impl ImportedInspectionFixture {
    /// The exact attested user text at position one. Position two is a tool
    /// call, which the conservative projection carries as a kind alone.
    const USER_TEXT: &'static str = "imported question";

    async fn insert(pool: &PgPool) -> Result<Self, Box<dyn Error>> {
        let conversation = ImportedConversationId::from_uuid(Uuid::from_u128(0x900));
        let user_entry = ImportedTranscriptEntryId::from_uuid(Uuid::from_u128(0x901));
        let tool_entry = ImportedTranscriptEntryId::from_uuid(Uuid::from_u128(0x902));
        let source = concat!(
            "{\"type\":\"user\",\"message\":{\"role\":\"user\",",
            "\"content\":\"imported question\"}}\n",
            "{\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"content\":[",
            "{\"type\":\"tool_use\",\"id\":\"call\",\"name\":\"lookup\",",
            "\"input\":{\"query\":\"synthetic\"}}]}}"
        );
        let mut import_service = ImportConversationService::new(
            FixedImportIds {
                conversations: [conversation].into(),
                entries: [user_entry, tool_entry].into(),
            },
            ClaudeCodeJsonlConverter,
            ImportedConversationRepository::new(pool.clone()),
        );
        assert_eq!(
            import_service.execute(source.as_bytes()).await?,
            ImportConversationOutcome::Inserted { conversation }
        );
        Ok(Self {
            conversation: CanonicalUuid::from_uuid(conversation.into_uuid()),
            user_entry: CanonicalUuid::from_uuid(user_entry.into_uuid()),
            tool_entry: CanonicalUuid::from_uuid(tool_entry.into_uuid()),
            user_text: Self::USER_TEXT,
            last_position: CanonicalU64::new(2),
        })
    }
}

/// S28: the inspection read names every selectable imported position with its
/// attestation, content kind, and bounded preview, so the ordinal
/// `create_session_from_imported_frontier` consumes is observable before it is
/// consumed.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn s28_reads_every_selectable_imported_position() -> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let fixture = ImportedInspectionFixture::insert(&runtime.pool).await?;
    let mut connection = Connection::connect(runtime.socket()).await?;

    connection
        .request_version(
            ProtocolVersion::One,
            1,
            ClientRequest::ReadImportedConversation {
                imported_conversation_id: fixture.conversation,
            },
        )
        .await?;

    let start = response_within(&mut connection).await?;
    assert_eq!(
        start.message(),
        &ServerMessage::ImportedConversationStart {
            imported_conversation_id: fixture.conversation,
        }
    );
    let first = response_within(&mut connection).await?;
    assert_eq!(
        first.message(),
        &ServerMessage::ImportedConversationEntry {
            position: CanonicalU64::new(1),
            imported_entry_id: fixture.user_entry,
            source_speaker: ImportedSourceSpeaker::Attested {
                speaker: ImportedSpeaker::User,
            },
            content_kind: ImportedContentKind::Text,
            text_preview: Some(ImportedTextPreview::of_exact_text(fixture.user_text)),
        }
    );
    let second = response_within(&mut connection).await?;
    assert_eq!(
        second.message(),
        &ServerMessage::ImportedConversationEntry {
            position: fixture.last_position,
            imported_entry_id: fixture.tool_entry,
            source_speaker: ImportedSourceSpeaker::Attested {
                speaker: ImportedSpeaker::Assistant,
            },
            content_kind: ImportedContentKind::ToolCall,
            text_preview: None,
        }
    );
    let end = response_within(&mut connection).await?;
    assert_eq!(
        end.message(),
        &ServerMessage::ImportedConversationEnd {
            imported_conversation_id: fixture.conversation,
            entry_count: fixture.last_position,
        }
    );

    drop(connection);
    runtime.stop().await
}

/// S28: an absent imported conversation is a read miss naming an imported
/// conversation, never the absent-session diagnostic.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn s28_read_names_an_absent_imported_conversation() -> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;

    connection
        .request_version(
            ProtocolVersion::One,
            1,
            ClientRequest::ReadImportedConversation {
                imported_conversation_id: CanonicalUuid::from_uuid(Uuid::from_u128(0x9ff)),
            },
        )
        .await?;

    let response = response_within(&mut connection).await?;
    let ServerMessage::Error { code, message, .. } = response.message() else {
        panic!("an absent imported conversation returns an error");
    };
    assert_eq!(*code, ErrorCode::NotFound);
    assert_eq!(message, "the requested imported conversation was not found");

    drop(connection);
    runtime.stop().await
}

/// S28: a valid imported conversation carrying an out-of-range position is a
/// rejection naming the selectable range, not a `not_found` claiming the
/// identity was absent.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn s28_continuation_names_the_selectable_position_range() -> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let fixture = ImportedInspectionFixture::insert(&runtime.pool).await?;
    let mut connection = Connection::connect(runtime.socket()).await?;

    connection
        .request_version(
            ProtocolVersion::One,
            1,
            ClientRequest::CreateSessionFromImportedFrontier {
                command_id: command()?,
                imported_conversation_id: fixture.conversation,
                through_position: CanonicalU64::new(999_999),
                relationship: signalbox_process_protocol::ImportedSessionRelationship::Resume,
                initial_model_selection: ModelSelection::Direct {
                    selection_id: CanonicalUuid::from_uuid(Uuid::from_u128(1)),
                },
                model_settings: ModelSettingsOverlay::inherit_all(),
            },
        )
        .await?;

    let response = response_within(&mut connection).await?;
    let ServerMessage::Error { code, detail, .. } = response.message() else {
        panic!("an out-of-range imported position returns an error");
    };
    assert_eq!(*code, ErrorCode::Rejected);
    assert_eq!(
        detail.value(),
        Some(RejectionDetail::ImportedFrontierPositionOutOfRange {
            imported_conversation_id: fixture.conversation,
            requested_position: CanonicalU64::new(999_999),
            last_position: fixture.last_position,
        })
    );

    drop(connection);
    runtime.stop().await
}

/// S28: an absent imported conversation on the continuation command names an
/// imported conversation as the missing target.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn s28_continuation_names_an_absent_imported_conversation() -> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let absent = CanonicalUuid::from_uuid(Uuid::from_u128(0x9ff));
    let mut connection = Connection::connect(runtime.socket()).await?;

    connection
        .request_version(
            ProtocolVersion::One,
            1,
            ClientRequest::CreateSessionFromImportedFrontier {
                command_id: command()?,
                imported_conversation_id: absent,
                through_position: CanonicalU64::new(1),
                relationship: signalbox_process_protocol::ImportedSessionRelationship::Resume,
                initial_model_selection: ModelSelection::Direct {
                    selection_id: CanonicalUuid::from_uuid(Uuid::from_u128(1)),
                },
                model_settings: ModelSettingsOverlay::inherit_all(),
            },
        )
        .await?;

    let response = response_within(&mut connection).await?;
    let ServerMessage::Error { code, detail, .. } = response.message() else {
        panic!("an absent imported conversation returns an error");
    };
    assert_eq!(*code, ErrorCode::Rejected);
    assert_eq!(
        detail.value(),
        Some(RejectionDetail::ImportedConversationNotFound {
            imported_conversation_id: absent,
        })
    );

    drop(connection);
    runtime.stop().await
}

/// S28 / INV-012: the imported wire address resolves against the immutable
/// aggregate before settings admission, so an absent conversation and an
/// out-of-range position each win over an explicit setting the selected model
/// cannot support.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn s28_inv012_imported_address_precedes_settings_validation() -> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let fixture = ImportedInspectionFixture::insert(&runtime.pool).await?;
    let absent = CanonicalUuid::from_uuid(Uuid::from_u128(0x28f0));
    let mut connection = Connection::connect(runtime.socket()).await?;

    connection
        .request(
            1,
            ClientRequest::CreateSessionFromImportedFrontier {
                command_id: command()?,
                imported_conversation_id: absent,
                through_position: CanonicalU64::new(1),
                relationship: signalbox_process_protocol::ImportedSessionRelationship::Resume,
                initial_model_selection: ModelSelection::Direct {
                    selection_id: next_direct_selection_id(),
                },
                model_settings: low_reasoning_override(),
            },
        )
        .await?;
    let missing_conversation = response_within(&mut connection).await?.message().clone();

    connection
        .request(
            2,
            ClientRequest::CreateSessionFromImportedFrontier {
                command_id: command()?,
                imported_conversation_id: fixture.conversation,
                through_position: CanonicalU64::new(999_999),
                relationship: signalbox_process_protocol::ImportedSessionRelationship::Resume,
                initial_model_selection: ModelSelection::Direct {
                    selection_id: next_direct_selection_id(),
                },
                model_settings: low_reasoning_override(),
            },
        )
        .await?;
    let out_of_range = response_within(&mut connection).await?.message().clone();

    assert_eq!(
        protocol_error_code(&missing_conversation),
        ErrorCode::Rejected
    );
    assert_eq!(
        protocol_error_detail(&missing_conversation),
        Some(RejectionDetail::ImportedConversationNotFound {
            imported_conversation_id: absent,
        })
    );
    assert_eq!(protocol_error_code(&out_of_range), ErrorCode::Rejected);
    assert_eq!(
        protocol_error_detail(&out_of_range),
        Some(RejectionDetail::ImportedFrontierPositionOutOfRange {
            imported_conversation_id: fixture.conversation,
            requested_position: CanonicalU64::new(999_999),
            last_position: fixture.last_position,
        })
    );

    drop(connection);
    runtime.stop().await
}

/// S28: the explicit Codex selection reaches the fixed Codex converter rather
/// than applying format detection or the Claude Code interpretation.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn s28_selects_the_codex_rollout_converter() -> Result<(), Box<dyn Error>> {
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
            ProtocolVersion::One,
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
    let alias_id = CanonicalUuid::from_uuid(Uuid::from_u128(2));
    let expected_placement = SessionPlacement::Pathless {};
    let session_id =
        create_alias_session_with(&mut connection, alias_id, expected_placement.clone()).await?;

    connection
        .request(2, ClientRequest::ListSessions {})
        .await?;

    let start = response_within(&mut connection).await?;
    assert_eq!(start.message(), &ServerMessage::SessionsStart {});
    let summary = response_within(&mut connection).await?;
    assert_eq!(
        summary.message(),
        &ServerMessage::SessionSummary {
            session_id,
            defaults_version: CanonicalU64::new(
                SessionConfigurationDefaultsVersion::first().as_u64(),
            ),
            model_selection: ModelSelection::Alias { alias_id },
            placement_version: CanonicalU64::new(
                signalbox_domain::SessionPlacementVersion::INITIAL.as_u64(),
            ),
            placement: expected_placement,
            runner: None,
        }
    );
    let end = response_within(&mut connection).await?;
    assert_eq!(
        end.message(),
        &ServerMessage::SessionsEnd {
            session_count: CanonicalU64::new(1),
        }
    );

    drop(connection);
    runtime.stop().await
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn process_runtime_reads_an_empty_operator_status_snapshot() -> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;

    connection
        .request(1, ClientRequest::ReadOperatorStatus {})
        .await?;

    let start = response_within(&mut connection).await?;
    assert_eq!(
        start.message(),
        &ServerMessage::OperatorStatus(Box::new(OperatorStatusMessage::Start {}))
    );
    let end = response_within(&mut connection).await?;
    assert_eq!(
        end.message(),
        &ServerMessage::OperatorStatus(Box::new(OperatorStatusMessage::End(Box::new(
            OperatorStatusEndMessage {
                held_slot_count: CanonicalU64::new(0),
                queued_obligation_count: CanonicalU64::new(0),
                lifecycle_week_count: CanonicalU64::new(0),
                lifecycle_deadline_violation_count: CanonicalU64::new(0),
            },
        ))))
    );

    drop(connection);
    runtime.stop().await
}

/// S33 / INV-008 / INV-012 / INV-046: one complete replacement
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
        model_settings: ModelSettingsOverlay::inherit_all(),
        system_prompt: SystemPromptMember::present(None),
    };

    connection
        .request_version(ProtocolVersion::One, 2, replacement.clone())
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
            model_settings: provider_default_model_settings(),
            system_prompt: SystemPromptMember::present(None),
        }
    );

    connection
        .request_version(ProtocolVersion::One, 3, replacement)
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
            model_settings: provider_default_model_settings(),
            system_prompt: SystemPromptMember::present(None),
        }
    );

    let unknown_command = command()?;
    connection
        .request_version(
            ProtocolVersion::One,
            4,
            ClientRequest::ReplaceSessionDefaults {
                command_id: unknown_command,
                session_id,
                expected_defaults_version: CanonicalU64::new(2),
                model_selection: ModelSelection::Direct {
                    selection_id: CanonicalUuid::from_uuid(Uuid::from_u128(999)),
                },
                dangerous_tool_auto_approval: false,
                model_settings: ModelSettingsOverlay::inherit_all(),
                system_prompt: SystemPromptMember::present(None),
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

/// INV-033: metadata wire-shape failures are malformed frames, not application
/// request rejections.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn metadata_shape_failure_is_a_malformed_frame() -> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let required_tags = vec!["x".repeat(MAX_SESSION_METADATA_INDEXED_UTF8_BYTES + 1)];
    let frame = format!(
        "{{\"version\":1,\"request_id\":\"21\",\"request\":{{\"type\":\"list_session_metadata\",\"required_tags\":{},\"title_contains\":null,\"include_archived\":false,\"page_size\":\"50\",\"after_session_id\":null}}}}\n",
        serde_json::to_string(&required_tags)?
    );

    connection.raw_request(&frame).await?;

    let response = response_within(&mut connection).await?;
    assert_eq!(response.version(), ProtocolVersion::One);
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
async fn s01_inv033_reads_initial_metadata_projection() -> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let first_session = create_alias_session(&mut connection).await?;

    connection
        .request_version(
            ProtocolVersion::One,
            10,
            ClientRequest::ReadSessionMetadata {
                session_id: first_session,
            },
        )
        .await?;
    let initial = response_within(&mut connection).await?;
    assert_eq!(initial.version(), ProtocolVersion::One);
    let ServerMessage::SessionMetadata {
        session_id,
        metadata,
        last_writer: None,
    } = initial.message()
    else {
        panic!(
            "fixture expected initial metadata, got {:?}",
            initial.message()
        );
    };
    assert_eq!(*session_id, first_session);
    assert_eq!(metadata, &SessionMetadata::empty());

    drop(connection);
    runtime.stop().await
}

/// INV-033: a durable snapshot whose last writer is tool execution projects onto
/// both metadata read surfaces. The tool-facing replacement constructor is
/// production-registered, so this row shape exists in ordinary operation; a
/// missing wire projection would fail the read as an encode invariant, which is
/// fatal to the daemon and repeats on every later read of the same row.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn inv033_reads_back_tool_written_metadata() -> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let session = create_alias_session(&mut connection).await?;
    let tool_request = ToolRequestId::from_uuid(Uuid::now_v7());

    let replacement = SessionMetadataContent::try_new(
        Some(String::from("Status from the tool")),
        vec![String::from("automated")],
        Vec::new(),
        false,
    )
    .map_err(|error| io::Error::other(format!("metadata fixture is invalid: {error:?}")))?;
    let write = ReplaceSessionMetadataRequest::try_new_for_tool(
        DurableCommandId::from_uuid(Uuid::now_v7()),
        SessionId::from_uuid(session.into_uuid()),
        tool_request,
        replacement,
    )?;
    let mut writer =
        ReplaceSessionMetadataService::new(SessionMetadataRepository::new(runtime.pool.clone()));
    let ReplaceSessionMetadataOutcome::Recorded(ReplaceSessionMetadataResult::Applied(applied)) =
        writer.execute(write).await?
    else {
        panic!("fixture expected the tool replacement to apply");
    };
    assert_eq!(
        applied.snapshot().last_writer().map(|last| last.actor()),
        Some(Actor::Tool {
            request: tool_request,
        })
    );

    connection
        .request(
            11,
            ClientRequest::ReadSessionMetadata {
                session_id: session,
            },
        )
        .await?;
    let read = response_within(&mut connection).await?;
    let ServerMessage::SessionMetadata {
        last_writer: Some(last_writer),
        ..
    } = read.message()
    else {
        panic!(
            "fixture expected tool-written metadata, got {:?}",
            read.message()
        );
    };
    assert_eq!(
        last_writer.actor(),
        MetadataActor::Tool {
            tool_request_id: CanonicalUuid::from_uuid(tool_request.into_uuid()),
        }
    );

    connection
        .request(
            12,
            ClientRequest::ListSessionMetadata {
                required_tags: Vec::new(),
                title_contains: None,
                include_archived: false,
                page_size: CanonicalU64::new(50),
                after_session_id: None,
            },
        )
        .await?;
    let page_start = response_within(&mut connection).await?;
    assert!(matches!(
        page_start.message(),
        ServerMessage::SessionMetadataPageStart {}
    ));
    let summary = response_within(&mut connection).await?;
    let ServerMessage::SessionMetadataSummary {
        last_writer: Some(listed_writer),
        ..
    } = summary.message()
    else {
        panic!(
            "fixture expected the tool-written summary, got {:?}",
            summary.message()
        );
    };
    assert_eq!(listed_writer.actor(), last_writer.actor());
    let page_end = response_within(&mut connection).await?;
    assert!(matches!(
        page_end.message(),
        ServerMessage::SessionMetadataPageEnd { .. }
    ));

    // The daemon survives both reads: a later request on a fresh connection is
    // still served, which a fatal encode invariant would have prevented.
    let mut later = Connection::connect(runtime.socket()).await?;
    later
        .request(
            13,
            ClientRequest::ReadSessionMetadata {
                session_id: session,
            },
        )
        .await?;
    assert!(matches!(
        response_within(&mut later).await?.message(),
        ServerMessage::SessionMetadata { .. }
    ));

    drop(later);
    drop(connection);
    runtime.stop().await
}

/// INV-012: one metadata command identity applies once, replays exactly, and
/// rejects a structurally different reuse.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn inv012_enforces_metadata_command_identity() -> Result<(), Box<dyn Error>> {
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
            ProtocolVersion::One,
            11,
            ClientRequest::ReplaceSessionMetadata {
                command_id: replacement_command,
                session_id: first_session,
                metadata: replacement.clone(),
            },
        )
        .await?;
    let applied = response_within(&mut connection).await?;
    let ServerMessage::SessionMetadataReplaced {
        session_id,
        metadata,
        last_writer,
    } = applied.message()
    else {
        panic!(
            "fixture expected replaced metadata, got {:?}",
            applied.message()
        );
    };
    assert_eq!(*session_id, first_session);
    assert_eq!(metadata, &replacement);
    assert!(matches!(last_writer.actor(), MetadataActor::User {}));

    connection
        .request_version(
            ProtocolVersion::One,
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
            ProtocolVersion::One,
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
            ProtocolVersion::One,
            10,
            ClientRequest::ReplaceSessionMetadata {
                command_id: command()?,
                session_id: first_session,
                metadata: archived_metadata,
            },
        )
        .await?;
    let archived_receipt = response_within(&mut connection).await?;
    let ServerMessage::SessionMetadataReplaced {
        session_id,
        metadata,
        ..
    } = archived_receipt.message()
    else {
        panic!(
            "fixture expected archived metadata receipt, got {:?}",
            archived_receipt.message()
        );
    };
    assert_eq!(*session_id, first_session);
    assert!(metadata.archived());

    let second_metadata = SessionMetadata::try_new(
        Some(String::from("Active plan")),
        vec![String::from("daily")],
        Vec::new(),
        false,
    )?;
    connection
        .request_version(
            ProtocolVersion::One,
            14,
            ClientRequest::ReplaceSessionMetadata {
                command_id: command()?,
                session_id: second_session,
                metadata: second_metadata.clone(),
            },
        )
        .await?;
    let second_receipt = response_within(&mut connection).await?;
    let ServerMessage::SessionMetadataReplaced {
        session_id,
        metadata,
        ..
    } = second_receipt.message()
    else {
        panic!(
            "fixture expected second metadata receipt, got {:?}",
            second_receipt.message()
        );
    };
    assert_eq!(*session_id, second_session);
    assert_eq!(metadata, &second_metadata);

    connection
        .request_version(
            ProtocolVersion::One,
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
    let summary = response_within(&mut connection).await?;
    let ServerMessage::SessionMetadataSummary {
        session_id,
        dangerous_tool_auto_approval: false,
        title: Some(title),
        tags,
        archived: false,
        ..
    } = summary.message()
    else {
        panic!(
            "fixture expected active metadata summary, got {:?}",
            summary.message()
        );
    };
    assert_eq!(*session_id, second_session);
    assert_eq!(
        title.as_str(),
        second_metadata
            .title()
            .expect("the fixture metadata states its title")
    );
    assert!(tags.iter().map(String::as_str).eq(second_metadata.tags()));
    let page_end = response_within(&mut connection).await?;
    let ServerMessage::SessionMetadataPageEnd {
        session_count,
        next_after_session_id: None,
    } = page_end.message()
    else {
        panic!(
            "fixture expected terminal metadata page, got {:?}",
            page_end.message()
        );
    };
    assert_eq!(session_count.value(), 1);

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
            ProtocolVersion::One,
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
    let first_summary = response_within(&mut connection).await?;
    let ServerMessage::SessionMetadataSummary {
        session_id: first_page_session,
        ..
    } = first_summary.message()
    else {
        panic!(
            "unexpected first metadata-page summary: {:?}",
            first_summary.message()
        );
    };
    let first_page_session = *first_page_session;
    let first_end = response_within(&mut connection).await?;
    let ServerMessage::SessionMetadataPageEnd {
        session_count,
        next_after_session_id: Some(next),
    } = first_end.message()
    else {
        panic!(
            "unexpected first metadata-page end: {:?}",
            first_end.message()
        );
    };
    assert_eq!(session_count.value(), 1);
    let next = *next;
    assert_eq!(next, first_page_session);

    connection
        .request_version(
            ProtocolVersion::One,
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
    let second_summary = response_within(&mut connection).await?;
    let ServerMessage::SessionMetadataSummary {
        session_id: second_page_session,
        ..
    } = second_summary.message()
    else {
        panic!(
            "unexpected second metadata-page summary: {:?}",
            second_summary.message()
        );
    };
    let second_page_session = *second_page_session;
    assert_ne!(second_page_session, first_page_session);
    assert!(
        [first_page_session, second_page_session].contains(&first_session)
            && [first_page_session, second_page_session].contains(&second_session)
    );
    let page_end = response_within(&mut connection).await?;
    let ServerMessage::SessionMetadataPageEnd {
        session_count,
        next_after_session_id: None,
    } = page_end.message()
    else {
        panic!(
            "fixture expected terminal metadata page, got {:?}",
            page_end.message()
        );
    };
    assert_eq!(session_count.value(), 1);

    drop(connection);
    runtime.stop().await
}

/// Requires the next response to be the inserted-import receipt and returns
/// the inserted imported-conversation identity.
async fn require_inserted_import_receipt(
    connection: &mut Connection,
) -> Result<CanonicalUuid, Box<dyn Error>> {
    match response_within(connection).await?.message() {
        ServerMessage::ConversationImportInserted {
            imported_conversation_id,
        } => Ok(*imported_conversation_id),
        message => Err(io::Error::other(format!("unexpected import receipt: {message:?}")).into()),
    }
}

/// Requires the next response to be one unified conversation summary.
async fn require_conversation_summary(
    connection: &mut Connection,
) -> Result<ConversationSummary, Box<dyn Error>> {
    match response_within(connection).await?.message() {
        ServerMessage::ConversationSummary { conversation } => Ok(conversation.clone()),
        message => Err(io::Error::other(format!("unexpected unified summary: {message:?}")).into()),
    }
}

/// Splits one native and one imported summary out of a pair listed in either
/// order.
fn partition_native_and_imported(
    first: ConversationSummary,
    second: ConversationSummary,
) -> Result<(ConversationSummary, ConversationSummary), Box<dyn Error>> {
    match (first, second) {
        (
            native @ ConversationSummary::NativeSession { .. },
            imported @ ConversationSummary::ImportedConversation { .. },
        )
        | (
            imported @ ConversationSummary::ImportedConversation { .. },
            native @ ConversationSummary::NativeSession { .. },
        ) => Ok((native, imported)),
        pair => Err(io::Error::other(format!("unexpected unified summary pair: {pair:?}")).into()),
    }
}

/// S28: the unified request lists native sessions and imported conversations in
/// one unified page whose imported row carries the derived title, entry
/// count, and stored source format.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn s28_lists_native_and_imported_conversations() -> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let native_session = create_alias_session(&mut connection).await?;
    let source_title = "question";
    let source = ConversationImportSource::new(
        concat!(
            "{\"timestamp\":\"2026-07-25T00:00:00Z\",\"type\":\"response_item\",",
            "\"payload\":{\"type\":\"message\",\"role\":\"user\",",
            "\"content\":[{\"type\":\"input_text\",\"text\":\"<title>\"}]}}"
        )
        .replace("<title>", source_title)
        .into_bytes(),
    );
    connection
        .request_version(
            ProtocolVersion::One,
            30,
            ClientRequest::ImportConversation {
                format: ConversationImportFormat::CodexRolloutJsonlV1,
                source,
            },
        )
        .await?;
    let imported_id = require_inserted_import_receipt(&mut connection).await?;

    connection
        .request_version(
            ProtocolVersion::One,
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
    let first_summary = require_conversation_summary(&mut connection).await?;
    let second_summary = require_conversation_summary(&mut connection).await?;
    let page_end = response_within(&mut connection).await?;
    let ServerMessage::ConversationPageEnd {
        conversation_count,
        next_after: None,
    } = page_end.message()
    else {
        panic!(
            "fixture expected conversation page end, got {:?}",
            page_end.message()
        );
    };
    assert_eq!(conversation_count.value(), 2);
    assert!(
        first_summary.cursor().conversation_id().into_uuid()
            < second_summary.cursor().conversation_id().into_uuid(),
        "unified summaries must arrive in strict identity order"
    );
    let (native, imported) = partition_native_and_imported(first_summary, second_summary)?;
    let ConversationSummary::NativeSession {
        session_id,
        title: None,
        archived: false,
        defaults_version,
    } = native
    else {
        panic!("fixture expected native conversation summary");
    };
    assert_eq!(session_id, native_session);
    assert_eq!(defaults_version.value(), 1);
    let ConversationSummary::ImportedConversation {
        imported_conversation_id,
        title: Some(title),
        entry_count,
        source_format: ImportedConversationSourceFormat::CodexRolloutJsonlV1,
    } = imported
    else {
        panic!("fixture expected imported conversation summary");
    };
    assert_eq!(imported_conversation_id, imported_id);
    assert_eq!(title, source_title);
    assert_eq!(entry_count.value(), 1);

    drop(connection);
    runtime.stop().await
}

/// INV-033: a metadata read returns the complete current wire
/// projection.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn inv033_reads_current_metadata_projection() -> Result<(), Box<dyn Error>> {
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
            ProtocolVersion::One,
            10,
            ClientRequest::ReplaceSessionMetadata {
                command_id: command()?,
                session_id: first_session,
                metadata: replacement.clone(),
            },
        )
        .await?;
    let replacement_receipt = response_within(&mut connection).await?;
    let ServerMessage::SessionMetadataReplaced { session_id, .. } = replacement_receipt.message()
    else {
        panic!(
            "fixture expected metadata replacement, got {:?}",
            replacement_receipt.message()
        );
    };
    assert_eq!(*session_id, first_session);

    connection
        .request_version(
            ProtocolVersion::One,
            18,
            ClientRequest::ReadSessionMetadata {
                session_id: first_session,
            },
        )
        .await?;
    let read = response_within(&mut connection).await?;
    let ServerMessage::SessionMetadata {
        session_id,
        metadata,
        last_writer: Some(last_writer),
    } = read.message()
    else {
        panic!(
            "fixture expected current metadata, got {:?}",
            read.message()
        );
    };
    assert_eq!(*session_id, first_session);
    assert_eq!(metadata, &replacement);
    assert!(matches!(last_writer.actor(), MetadataActor::User {}));

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
            ProtocolVersion::One,
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
            ProtocolVersion::One,
            20,
            ClientRequest::ReplaceSessionMetadata {
                command_id: command()?,
                session_id: absent,
                metadata: SessionMetadata::empty(),
            },
        )
        .await?;
    assert_eq!(
        rejected_detail(response_within(&mut connection).await?.message()),
        RejectionDetail::SessionNotFound { session_id: absent }
    );

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
            ProtocolVersion::One,
            10,
            ClientRequest::ReplaceSessionMetadata {
                command_id: command()?,
                session_id: first_session,
                metadata: archived,
            },
        )
        .await?;
    let archived_receipt = response_within(&mut connection).await?;
    let ServerMessage::SessionMetadataReplaced {
        session_id,
        metadata,
        ..
    } = archived_receipt.message()
    else {
        panic!(
            "fixture expected archived metadata receipt, got {:?}",
            archived_receipt.message()
        );
    };
    assert_eq!(*session_id, first_session);
    assert!(metadata.archived());

    let restored = SessionMetadata::try_new(
        Some(String::from("Archived plan")),
        vec![String::from("work"), String::from("daily")],
        vec![(String::from("run"), String::from("17"))],
        false,
    )?;
    connection
        .request_version(
            ProtocolVersion::One,
            21,
            ClientRequest::ReplaceSessionMetadata {
                command_id: command()?,
                session_id: first_session,
                metadata: restored.clone(),
            },
        )
        .await?;
    let restored_receipt = response_within(&mut connection).await?;
    let ServerMessage::SessionMetadataReplaced {
        session_id,
        metadata,
        ..
    } = restored_receipt.message()
    else {
        panic!(
            "fixture expected restored metadata, got {:?}",
            restored_receipt.message()
        );
    };
    assert_eq!(*session_id, first_session);
    assert_eq!(metadata, &restored);

    connection
        .request_version(
            ProtocolVersion::One,
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
    let summary = response_within(&mut connection).await?;
    let ServerMessage::SessionMetadataSummary {
        session_id,
        archived: false,
        ..
    } = summary.message()
    else {
        panic!(
            "fixture expected restored metadata summary, got {:?}",
            summary.message()
        );
    };
    assert_eq!(*session_id, first_session);
    let page_end = response_within(&mut connection).await?;
    let ServerMessage::SessionMetadataPageEnd {
        session_count,
        next_after_session_id: None,
    } = page_end.message()
    else {
        panic!(
            "fixture expected terminal metadata page, got {:?}",
            page_end.message()
        );
    };
    assert_eq!(session_count.value(), 1);

    drop(connection);
    runtime.stop().await
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn s28_read_streams_conservative_imported_seed_snapshot() -> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let session_id = create_imported_session(&runtime.pool).await?;

    let mut read_connection = Connection::connect(runtime.socket()).await?;
    read_connection
        .request_version(
            ProtocolVersion::One,
            2,
            ClientRequest::ReadTranscript { session_id },
        )
        .await?;
    let start = response_within(&mut read_connection).await?;
    assert_eq!(start.version(), ProtocolVersion::One);
    let ServerMessage::TranscriptSnapshotStart {
        session_id: selected,
        ..
    } = start.message()
    else {
        panic!(
            "fixture expected transcript start, got {:?}",
            start.message()
        );
    };
    assert_eq!(*selected, session_id);
    let model_calls_end = response_within(&mut read_connection).await?;
    assert_eq!(transcript_model_call_count(model_calls_end.message()), 0);
    let imported_text = response_within(&mut read_connection).await?;
    assert_eq!(imported_text.version(), ProtocolVersion::One);
    let ServerMessage::TranscriptTextEntry {
        entry_index,
        entry:
            TranscriptTextEntry::Imported {
                source_speaker:
                    ImportedSourceSpeaker::Attested {
                        speaker: ImportedSpeaker::User,
                    },
                ..
            },
        ..
    } = imported_text.message()
    else {
        panic!(
            "fixture expected imported text entry, got {:?}",
            imported_text.message()
        );
    };
    assert_eq!(entry_index.value(), 0);
    let content = response_within(&mut read_connection).await?;
    let ServerMessage::TranscriptContent {
        entry_index,
        fragment_index,
        final_fragment: true,
        content_fragment,
    } = content.message()
    else {
        panic!(
            "fixture expected imported text content, got {:?}",
            content.message()
        );
    };
    assert_eq!(entry_index.value(), 0);
    assert_eq!(fragment_index.value(), 0);
    assert_eq!(content_fragment.as_str(), IMPORTED_USER_CONTENT);
    let conservative = response_within(&mut read_connection).await?;
    let ServerMessage::TranscriptEntry {
        entry_index,
        entry:
            TranscriptEntry::Imported {
                source_speaker:
                    ImportedSourceSpeaker::Attested {
                        speaker: ImportedSpeaker::Assistant,
                    },
                content_kind: ImportedContentKind::ToolCall,
                ..
            },
        ..
    } = conservative.message()
    else {
        panic!(
            "fixture expected conservative imported entry, got {:?}",
            conservative.message()
        );
    };
    assert_eq!(entry_index.value(), 1);
    let end = response_within(&mut read_connection).await?;
    assert_eq!(end.version(), ProtocolVersion::One);
    let ServerMessage::TranscriptSnapshotEnd {
        turn_count,
        entry_count,
        ..
    } = end.message()
    else {
        panic!("fixture expected transcript end, got {:?}", end.message());
    };
    assert_eq!(turn_count.value(), 0);
    assert_eq!(entry_count.value(), 2);

    drop(read_connection);
    runtime.stop().await
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn s28_submit_accepts_imported_session_continuation() -> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let session_id = create_imported_session(&runtime.pool).await?;

    let mut submit_connection = Connection::connect(runtime.socket()).await?;
    submit_connection
        .request_version(
            ProtocolVersion::One,
            4,
            ClientRequest::SubmitInput {
                command_id: command()?,
                session_id,
                content: UserInputContent::text(String::from("native continuation")),
                expected_defaults_version: Some(CanonicalU64::new(1)),
                model_settings: ModelSettingsOverlay::inherit_all(),
                delivery: None,
            },
        )
        .await?;
    let accepted = response_within(&mut submit_connection).await?;
    assert_eq!(accepted.version(), ProtocolVersion::One);
    assert_eq!(submitted_session(accepted.message()), session_id);

    drop(submit_connection);
    runtime.stop().await
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn process_runtime_rejects_oversized_submitted_input() -> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let session_id = create_alias_session(&mut connection).await?;

    let frame = serde_json::json!({
        "version": 1,
        "request_id": "2",
        "request": {
            "type": "submit_input",
            "command_id": command()?,
            "session_id": session_id,
            "content": [{
                "type": "text",
                "text": "x".repeat(OVERSIZED_SUBMITTED_INPUT_BYTES),
            }],
            "expected_defaults_version": "1",
            "model_settings": {
                "reasoning_level": { "kind": "inherit" },
                "fast_mode": { "kind": "inherit" },
                "service_tier": { "kind": "inherit" },
            },
        },
    });
    connection.raw_request(&format!("{frame}\n")).await?;

    let response = response_within(&mut connection).await?;
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
    activate_turn(pool, session).await?;

    let model_configuration = support::parse_model_configuration(MODEL_CONFIGURATION)?;
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
/// ordinary input until the user reconciliation decision releases the slot.
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
            ProtocolVersion::One,
            3,
            ClientRequest::SubmitInput {
                command_id: command()?,
                session_id,
                content: UserInputContent::text(String::from(
                    "work while the ambiguity is unresolved",
                )),
                expected_defaults_version: Some(CanonicalU64::new(1)),
                model_settings: ModelSettingsOverlay::inherit_all(),
                delivery: None,
            },
        )
        .await?;
    assert_eq!(
        rejected_detail(response_within(&mut connection).await?.message()),
        RejectionDetail::ActiveTurnPresent {
            session_id,
            active_turn_id: parked_turn_id,
        },
        "an ambiguity wait must keep refusing ordinary input while it holds the slot"
    );

    connection
        .request_version(
            ProtocolVersion::One,
            4,
            ClientRequest::ReconcileTurn {
                command_id: command()?,
                session_id,
                expected_active_turn_id: parked_turn_id,
                content: UserInputContent::text(String::from("continue after reconciliation")),
                expected_defaults_version: CanonicalU64::new(1),
                model_settings: ModelSettingsOverlay::inherit_all(),
            },
        )
        .await?;
    let successor_turn_id = accepted_successor_turn(&mut connection, session_id, 2).await?;
    assert_ne!(successor_turn_id, parked_turn_id);

    connection
        .request_version(
            ProtocolVersion::One,
            5,
            ClientRequest::ReadTranscript { session_id },
        )
        .await?;
    let start = response_within(&mut connection).await?;
    transcript_snapshot_start_cursor(start.message(), session_id);
    let reconciled_turn = response_within(&mut connection).await?;
    let (projected_reconciled_turn, reconciled_position, reconciled_state) =
        transcript_turn_projection(reconciled_turn.message());
    assert_eq!(projected_reconciled_turn, parked_turn_id);
    assert_eq!(reconciled_position, 1);
    assert!(matches!(
        reconciled_state,
        TurnState::ReconciliationRequired { .. }
    ));
    let successor_turn = response_within(&mut connection).await?;
    let (projected_successor_turn, successor_position, successor_state) =
        transcript_turn_projection(successor_turn.message());
    assert_eq!(projected_successor_turn, successor_turn_id);
    assert_eq!(successor_position, 2);
    assert!(matches!(successor_state, TurnState::Queued { .. }));

    drop(connection);
    runtime.stop().await
}

/// The terminal disposition the session's single model call recorded, when
/// one exists.
async fn sole_terminal_call_disposition(
    pool: &PgPool,
    session_id: CanonicalUuid,
    turn_id: CanonicalUuid,
) -> Result<Option<String>, Box<dyn Error>> {
    Ok(sqlx::query_scalar(
        "SELECT terminal_disposition_kind
           FROM model_call
          WHERE session_id = $1
            AND turn_id = $2",
    )
    .bind(session_id.into_uuid())
    .bind(turn_id.into_uuid())
    .fetch_one(pool)
    .await?)
}

/// S04 / INV-029: a live streamed provider exchange that fails its stream
/// integrity check parks the turn on an unstopped ambiguous model call —
/// exactly the wedge a mid-stream protocol violation produces — and the
/// reconciliation verb releases the session with a queued
/// successor.
///
/// This is the process-level recovery contract for the streamed-delivery
/// path: the scripted model declares the same boundary-loss evidence the
/// Anthropic decoder emits for a protocol violation, so the park is produced
/// by the real bridge, scheduler, and persistence chain rather than by a
/// startup-scan fixture.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn s04_inv029_streamed_protocol_violation_parks_then_reconciles() -> Result<(), Box<dyn Error>>
{
    let mut runtime = RunningRuntime::start().await?;
    let mut commands = Connection::connect(runtime.socket()).await?;
    let session_id = create_alias_session(&mut commands).await?;
    let (_, parked_turn_id) = submit_first_input(
        &mut commands,
        session_id,
        String::from("provoke a mid-stream integrity failure"),
    )
    .await?;
    let script = Script::delivering(TerminalEvidence::BoundaryLoss(BoundaryLossEvidence {
        cause: LossCause::StreamProtocolViolation {
            detail: String::from("thinking block carries more than one signature"),
        },
        exchange: ExchangeFacts::default(),
        reported_model: Some(ProviderReportedModel::new("fixture-model")),
        finish_reported: None,
        tool_calls: ToolCallsAtLoss::NoneOpened,
        usage: TokenUsage::unreported(),
    }))
    .observing(ObservationFact::SendCommenced);

    let probe = execute_streamed_turn_until(
        &mut runtime,
        ScriptedModel::single(script),
        session_id,
        parked_turn_id,
        TurnSettle::ParkedOnAmbiguity,
    )
    .await?;

    let operations = probe.received_operations();
    assert_eq!(operations.len(), 1);
    assert_eq!(operations[0].delivery, DeliveryMode::Streamed);
    assert_eq!(
        sole_terminal_call_disposition(&runtime.pool, session_id, parked_turn_id).await?,
        Some(String::from("ambiguous")),
        "a mid-stream protocol violation must close the issued call as ambiguous"
    );

    connection_reconciles_the_parked_turn(&mut commands, session_id, parked_turn_id).await?;

    drop(commands);
    runtime.stop().await
}

/// Issues the reconciliation decision for one parked turn and
/// proves a distinct successor turn was queued from its content.
async fn connection_reconciles_the_parked_turn(
    connection: &mut Connection,
    session_id: CanonicalUuid,
    parked_turn_id: CanonicalUuid,
) -> Result<(), Box<dyn Error>> {
    connection
        .request_version(
            ProtocolVersion::One,
            3,
            ClientRequest::ReconcileTurn {
                command_id: command()?,
                session_id,
                expected_active_turn_id: parked_turn_id,
                content: UserInputContent::text(String::from("continue after the wedge")),
                expected_defaults_version: CanonicalU64::new(1),
                model_settings: ModelSettingsOverlay::inherit_all(),
            },
        )
        .await?;
    let successor_turn_id = accepted_successor_turn(connection, session_id, 2).await?;
    assert_ne!(successor_turn_id, parked_turn_id);
    Ok(())
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
            ProtocolVersion::One,
            3,
            ClientRequest::ReconcileTurn {
                command_id: command()?,
                session_id,
                expected_active_turn_id: unparked_turn_id,
                content: UserInputContent::text(String::from("names no parked turn")),
                expected_defaults_version: CanonicalU64::new(1),
                model_settings: ModelSettingsOverlay::inherit_all(),
            },
        )
        .await?;
    assert_eq!(
        rejected_detail(response_within(&mut connection).await?.message()),
        RejectionDetail::TurnNotAwaitingReconciliation {
            session_id,
            turn_id: unparked_turn_id,
        }
    );

    connection
        .request_version(
            ProtocolVersion::One,
            4,
            ClientRequest::ReconcileTurn {
                command_id: command()?,
                session_id,
                expected_active_turn_id: parked_turn_id,
                content: UserInputContent::text(String::from("continue after reconciliation")),
                expected_defaults_version: CanonicalU64::new(1),
                model_settings: ModelSettingsOverlay::inherit_all(),
            },
        )
        .await?;
    accepted_successor_turn(&mut connection, session_id, 2).await?;

    connection
        .request_version(
            ProtocolVersion::One,
            5,
            ClientRequest::ReconcileTurn {
                command_id: command()?,
                session_id,
                expected_active_turn_id: parked_turn_id,
                content: UserInputContent::text(String::from("the decision is already recorded")),
                expected_defaults_version: CanonicalU64::new(1),
                model_settings: ModelSettingsOverlay::inherit_all(),
            },
        )
        .await?;
    assert_eq!(
        rejected_detail(response_within(&mut connection).await?.message()),
        RejectionDetail::TurnNotAwaitingReconciliation {
            session_id,
            turn_id: parked_turn_id,
        }
    );

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
        content: UserInputContent::text(String::from("continue after reconciliation")),
        expected_defaults_version: CanonicalU64::new(1),
        model_settings: ModelSettingsOverlay::inherit_all(),
    };
    connection
        .request_version(ProtocolVersion::One, 3, decision.clone())
        .await?;
    let successor_turn_id = accepted_successor_turn(&mut connection, session_id, 2).await?;

    connection
        .request_version(ProtocolVersion::One, 4, decision)
        .await?;
    let replayed_turn_id = accepted_successor_turn(&mut connection, session_id, 2).await?;

    assert_eq!(
        replayed_turn_id, successor_turn_id,
        "an equal reconciliation retry returns its recorded successor, never a refusal"
    );

    drop(connection);
    runtime.stop().await
}

/// S37 / INV-053: reconciliation records the explicit per-call contribution
/// with the successor origin instead of dropping it at the daemon boundary.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn s37_inv053_reconcile_turn_records_its_per_call_model_settings()
-> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let session_id = create_alias_session(&mut connection).await?;
    let (_, parked_turn_id) =
        submit_first_input(&mut connection, session_id, String::from("first request")).await?;
    park_turn_on_ambiguous_model_call(&runtime.pool, session_id).await?;
    let requested = low_reasoning_override();

    connection
        .request_version(
            ProtocolVersion::One,
            3,
            ClientRequest::ReconcileTurn {
                command_id: command()?,
                session_id,
                expected_active_turn_id: parked_turn_id,
                content: UserInputContent::text(String::from("continue with deliberate reasoning")),
                expected_defaults_version: CanonicalU64::new(1),
                model_settings: requested,
            },
        )
        .await?;
    let settings = accepted_successor_model_settings(&mut connection, session_id, 2).await?;

    assert_eq!(settings.precedence.per_call, requested);
    assert_eq!(
        settings.effective.reasoning_level,
        Some(ReasoningLevel::Low)
    );
    assert_eq!(settings.reasoning_source, Some(ModelSettingSource::PerCall));
    assert_eq!(
        settings.validated_for_selection_id,
        Some(primary_direct_selection_id())
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
        content: UserInputContent::text(String::from("continue after reconciliation")),
        expected_defaults_version: CanonicalU64::new(1),
        model_settings: ModelSettingsOverlay::inherit_all(),
    };
    let mut first = Connection::connect(runtime.socket()).await?;
    let mut second = Connection::connect(runtime.socket()).await?;
    first
        .request_version(ProtocolVersion::One, 1, decision.clone())
        .await?;
    second
        .request_version(ProtocolVersion::One, 1, decision)
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
            ProtocolVersion::One,
            1,
            ClientRequest::ReconcileTurn {
                command_id: command()?,
                session_id: absent_session_id,
                expected_active_turn_id: CanonicalUuid::from_uuid(Uuid::from_u128(0xB3)),
                content: UserInputContent::text(String::from("names no session")),
                expected_defaults_version: CanonicalU64::new(1),
                model_settings: ModelSettingsOverlay::inherit_all(),
            },
        )
        .await?;

    assert_eq!(
        rejected_detail(response_within(&mut connection).await?.message()),
        RejectionDetail::SessionNotFound {
            session_id: absent_session_id,
        }
    );

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
    let expected_snapshot_cursor = 4;
    let (accepted_input, turn) =
        submit_first_input(&mut connection, session_id, content.clone()).await?;

    connection
        .request(3, ClientRequest::ReadTranscript { session_id })
        .await?;

    let start = response_within(&mut connection).await?;
    assert_eq!(
        transcript_snapshot_start_cursor(start.message(), session_id),
        expected_snapshot_cursor
    );
    let queued_turn = response_within(&mut connection).await?;
    let (projected_turn, projected_position, projected_state) =
        transcript_turn_projection(queued_turn.message());
    assert_eq!(projected_turn, turn);
    assert_eq!(projected_position, 1);
    assert_eq!(
        projected_state,
        TurnState::Queued {
            accepted_input_id: accepted_input,
            content: UserInputContent::text(content),
        }
    );
    let model_calls_end = response_within(&mut connection).await?;
    assert_eq!(transcript_model_call_count(model_calls_end.message()), 0);
    let end = response_within(&mut connection).await?;
    assert_eq!(
        transcript_snapshot_end_facts(end.message()),
        TranscriptSnapshotEndFacts {
            session_id,
            cursor: expected_snapshot_cursor,
            turn_count: 1,
            entry_count: 0,
        }
    );

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
    let follow_start = follow.response().await?;
    let follow_cursor = transcript_snapshot_start_cursor(follow_start.message(), session_id);

    // The exact-limit queued content keeps the snapshot writer blocked after
    // its start frame. Commit the next update before draining the snapshot so
    // only a subscription formed before snapshot transmission can retain it.
    let second_position = 2;
    let second_content = UserInputContent::text(String::from("second input"));
    commands
        .request(
            6,
            ClientRequest::SubmitInput {
                command_id: command()?,
                session_id,
                content: second_content.clone(),
                expected_defaults_version: Some(CanonicalU64::new(1)),
                model_settings: ModelSettingsOverlay::inherit_all(),
                delivery: None,
            },
        )
        .await?;
    let second_submit = commands.response().await?;
    let second_accepted_input =
        submitted_input_identity(second_submit.message(), session_id, second_position);

    let queued_turn = response_within(&mut follow).await?;
    let (projected_turn, projected_position, projected_state) =
        transcript_turn_projection(queued_turn.message());
    assert_eq!(projected_turn, first_turn);
    assert_eq!(projected_position, 1);
    assert_eq!(
        projected_state,
        TurnState::Queued {
            accepted_input_id: first_accepted_input,
            content: UserInputContent::text(first_content),
        }
    );
    let model_calls_end = response_within(&mut follow).await?;
    assert_eq!(transcript_model_call_count(model_calls_end.message()), 0);
    let snapshot_end = response_within(&mut follow).await?;
    assert_eq!(
        transcript_snapshot_end_facts(snapshot_end.message()),
        TranscriptSnapshotEndFacts {
            session_id,
            cursor: follow_cursor,
            turn_count: 1,
            entry_count: 0,
        }
    );

    let settings_followed = response_within(&mut follow).await?;
    let settings_event = turn_model_settings_resolved_event_facts(settings_followed.message());
    assert!(settings_event.cursor > follow_cursor);
    assert_eq!(settings_event.session_id, session_id);
    assert_eq!(settings_event.accepted_input_id, second_accepted_input);

    let followed = response_within(&mut follow).await?;
    let event = input_accepted_event_facts(followed.message());
    assert!(event.cursor > settings_event.cursor);
    assert_eq!(event.session_id, session_id);
    assert_eq!(event.accepted_input_id, second_accepted_input);
    assert_eq!(event.acceptance_position, second_position);
    assert_eq!(event.content, second_content);

    drop(commands);
    drop(follow);
    runtime.stop().await
}

/// S24 / INV-032 / INV-033: followers receive the ephemeral provider-text stream.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn s24_inv032_inv033_inherits_provider_text_streaming() -> Result<(), Box<dyn Error>> {
    let mut runtime = RunningRuntime::start().await?;
    let mut commands = Connection::connect(runtime.socket()).await?;
    let session_id = create_alias_session(&mut commands).await?;
    let follow =
        attach_empty_follower(runtime.socket(), ProtocolVersion::One, 9, session_id).await?;
    let expected_delta_count = 1;
    let (script, assistant) =
        streamed_script(expected_delta_count, String::from("already [redacted]"));
    let (_, turn_id) = submit_first_input(
        &mut commands,
        session_id,
        String::from("retain streamed provider text"),
    )
    .await?;

    let probe = execute_streamed_turn(
        &mut runtime,
        ScriptedModel::single(script),
        session_id,
        turn_id,
    )
    .await?;
    let followed = follow_streamed_turn_to_completion(follow, session_id, turn_id).await?;
    let operations = probe.received_operations();

    assert_eq!(followed.delta_count, expected_delta_count);
    assert_eq!(followed.text, assistant);
    assert_eq!(operations.len(), 1);
    assert_eq!(operations[0].delivery, DeliveryMode::Streamed);

    drop(commands);
    runtime.stop().await
}

/// S01 / S02 / S24 / INV-032 / INV-035: the provider bridge asks the scripted
/// runtime for streamed delivery, and three already-attached followers each
/// observe the exact already-redacted deltas before durable terminal entries
/// expose the same complete assistant reply.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn s01_s02_s24_inv032_inv035_streamed_reply_reaches_three_followers_then_durable_truth()
-> Result<(), Box<dyn Error>> {
    let mut runtime = RunningRuntime::start().await?;
    let mut commands = Connection::connect(runtime.socket()).await?;
    let session_id = create_alias_session(&mut commands).await?;
    let first_follow =
        attach_empty_follower(runtime.socket(), ProtocolVersion::One, 10, session_id).await?;
    let second_follow =
        attach_empty_follower(runtime.socket(), ProtocolVersion::One, 11, session_id).await?;
    let third_follow =
        attach_empty_follower(runtime.socket(), ProtocolVersion::One, 12, session_id).await?;
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
    let third = follow_streamed_turn_to_completion(third_follow, session_id, turn_id).await?;
    let first_durable = read_completed_assistant(runtime.socket(), 13, session_id, turn_id).await?;
    let second_durable =
        read_completed_assistant(runtime.socket(), 14, session_id, turn_id).await?;
    let third_durable = read_completed_assistant(runtime.socket(), 15, session_id, turn_id).await?;
    let operations = probe.received_operations();

    assert_eq!(first.delta_count, expected_delta_count);
    assert_eq!(first.text, assistant);
    assert_eq!(second.delta_count, expected_delta_count);
    assert_eq!(second.text, assistant);
    assert_eq!(third.delta_count, expected_delta_count);
    assert_eq!(third.text, assistant);
    assert_eq!(first_durable, assistant);
    assert_eq!(second_durable, assistant);
    assert_eq!(third_durable, assistant);
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
        attach_empty_follower(runtime.socket(), ProtocolVersion::One, 20, session_id).await?;
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

/// S24 / INV-033: followers receive the ephemeral delta stream.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn s24_inv033_followers_inherit_the_streamed_deltas() -> Result<(), Box<dyn Error>> {
    let mut runtime = RunningRuntime::start().await?;
    let mut commands = Connection::connect(runtime.socket()).await?;
    let session_id = create_alias_session(&mut commands).await?;
    let follow =
        attach_empty_follower(runtime.socket(), ProtocolVersion::One, 40, session_id).await?;
    let expected_delta_count = 3;
    let (script, assistant) =
        streamed_script(expected_delta_count, String::from("already [redacted] "));
    let (_, turn_id) = submit_first_input(
        &mut commands,
        session_id,
        String::from("stream to the follower"),
    )
    .await?;

    let probe = execute_streamed_turn(
        &mut runtime,
        ScriptedModel::single(script),
        session_id,
        turn_id,
    )
    .await?;
    let followed = follow_streamed_turn_to_completion(follow, session_id, turn_id).await?;
    let durable = read_completed_assistant(runtime.socket(), 41, session_id, turn_id).await?;
    let operations = probe.received_operations();

    assert_eq!(followed.delta_count, expected_delta_count);
    assert_eq!(followed.text, assistant);
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
        Box<signalbox_domain::AuthorizedModelCall>,
        ModelCallId,
    ),
    Box<dyn Error>,
> {
    let session = SessionId::from_uuid(session_id.into_uuid());
    activate_turn(pool, session).await?;
    let targets = support::parse_model_configuration(MODEL_CONFIGURATION)?.target_catalog();
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
    Ok((calls, authorized, call))
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
            ProtocolVersion::One,
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

    let successor_content = String::from("continue after the stop");
    connection
        .request_version(
            ProtocolVersion::One,
            3,
            ClientRequest::StopTurn {
                command_id: command()?,
                session_id,
                expected_active_turn_id: stopped_turn_id,
                content: UserInputContent::text(successor_content.clone()),
                expected_defaults_version: CanonicalU64::new(1),
                descendant_scope: DescendantTerminationScope::ParentAlone,
                model_settings: ModelSettingsOverlay::inherit_all(),
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
    let TurnState::Queued { content, .. } = turn_state_of(&messages, successor_turn_id) else {
        panic!("fixture expected queued successor turn");
    };
    assert_eq!(content.single_text(), Some(successor_content.as_str()));
    assert_eq!(cancellation_marker_count(&messages, stopped_turn_id), 1);

    drop(connection);
    runtime.stop().await
}

/// S07 / INV-029: stopping an issued call records the durable cancellation
/// request and retains the slot for lifecycle closure, and a distinct second
/// stop is refused with the exact prior stop authority named.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn s07_inv029_stop_turn_requests_cancellation_of_an_issued_call_exactly_once()
-> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let session_id = create_alias_session(&mut connection).await?;
    let (_, stopped_turn_id) =
        submit_first_input(&mut connection, session_id, String::from("first request")).await?;
    let (_, _, issued_call) =
        Box::pin(authorize_issued_model_call(&runtime.pool, session_id)).await?;
    let first_stop_command = command()?;

    connection
        .request_version(
            ProtocolVersion::One,
            3,
            ClientRequest::StopTurn {
                command_id: first_stop_command,
                session_id,
                expected_active_turn_id: stopped_turn_id,
                content: UserInputContent::text(String::from("continue after the stop")),
                expected_defaults_version: CanonicalU64::new(1),
                descendant_scope: DescendantTerminationScope::ParentAlone,
                model_settings: ModelSettingsOverlay::inherit_all(),
            },
        )
        .await?;
    let successor_turn_id = accepted_successor_turn(&mut connection, session_id, 2).await?;
    assert_ne!(successor_turn_id, stopped_turn_id);

    let messages = read_transcript_messages(&mut connection, 4, session_id).await?;
    let TurnState::ActiveRunning {
        current_model_call: Some(call),
        ..
    } = turn_state_of(&messages, stopped_turn_id)
    else {
        panic!("fixture expected stopped turn with an issued model call");
    };
    assert_eq!(call.model_call_id().into_uuid(), issued_call.into_uuid());
    assert_eq!(
        call.state(),
        CurrentModelCallState::CancellationRequested {}
    );
    assert!(matches!(
        turn_state_of(&messages, successor_turn_id),
        TurnState::Queued { .. }
    ));

    connection
        .request_version(
            ProtocolVersion::One,
            5,
            ClientRequest::StopTurn {
                command_id: command()?,
                session_id,
                expected_active_turn_id: stopped_turn_id,
                content: UserInputContent::text(String::from("a second distinct stop")),
                expected_defaults_version: CanonicalU64::new(1),
                descendant_scope: DescendantTerminationScope::ParentAlone,
                model_settings: ModelSettingsOverlay::inherit_all(),
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

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn lifecycle_closure_retransmission_after_settlement_issues_no_second_interrupt()
-> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let session_id = create_alias_session(&mut connection).await?;
    let (_, live_turn_id) =
        submit_first_input(&mut connection, session_id, String::from("first request")).await?;
    let (calls, issued, _) =
        Box::pin(authorize_issued_model_call(&runtime.pool, session_id)).await?;
    let lifecycle_command = command()?;

    connection
        .request_version(
            ProtocolVersion::One,
            3,
            ClientRequest::StopSession {
                command_id: lifecycle_command,
                session_id,
                sticky: true,
                descendant_scope: DescendantTerminationScope::ParentAlone,
            },
        )
        .await?;
    let first = response_within(&mut connection).await?;
    let cancellation = issued
        .observation_correlation()
        .bind_terminal_observation(ModelCallTerminalObservation::Cancelled);
    let terminal = calls
        .apply_terminal_observation(
            SessionId::from_uuid(session_id.into_uuid()),
            cancellation,
            ModelCallTerminalIdentities::PhysicalCancellation(
                PhysicalCancellationModelCallTurnIdentities::new(
                    SemanticTranscriptEntryId::from_uuid(Uuid::now_v7()),
                    ContextFrontierId::from_uuid(Uuid::now_v7()),
                ),
            ),
            |_| panic!("the fixture has no pending steering to reclassify"),
        )
        .await?;
    assert!(matches!(terminal, ModelCallTerminalOutcome::Cancelled(_)));
    connection
        .request_version(
            ProtocolVersion::One,
            4,
            ClientRequest::StopSession {
                command_id: lifecycle_command,
                session_id,
                sticky: true,
                descendant_scope: DescendantTerminationScope::ParentAlone,
            },
        )
        .await?;
    let replay = response_within(&mut connection).await?;
    let expected = ServerMessage::SessionLifecycleCommandApplied {
        session_id,
        effect: SessionLifecycleEffect::ClosurePending { live_turn_id },
    };
    let applied_core_interrupts: (i64, i64) = sqlx::query_as(
        "SELECT count(*), count(DISTINCT command.command_id)
           FROM submit_input_command AS command
           JOIN durable_command AS envelope USING (command_id)
          WHERE command.session_id = $1
            AND command.delivery_kind = 'interrupt'
            AND command.actor_kind = 'core'
            AND envelope.issuer_kind = 'core'",
    )
    .bind(session_id.into_uuid())
    .fetch_one(&runtime.pool)
    .await?;

    assert_eq!(first.message(), &expected);
    assert_eq!(applied_core_interrupts, (1, 1));
    assert_eq!(replay.message(), &expected);

    drop(connection);
    runtime.stop().await
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn lifecycle_closure_interrupt_does_not_resolve_a_retired_session_model()
-> Result<(), Box<dyn Error>> {
    let mut runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let session_id = create_alias_session(&mut connection).await?;
    let (_, live_turn_id) =
        submit_first_input(&mut connection, session_id, String::from("first request")).await?;
    let _issued = Box::pin(authorize_issued_model_call(&runtime.pool, session_id)).await?;
    drop(connection);

    let retired_model_definition = r#"[[models]]
selection_id = "00000000-0000-0000-0000-000000000001"
target_id = "00000000-0000-0000-0000-000000000003"
model_family = "anthropic"
provider_model = "fixture-model"
max_output_tokens = 256
context_window_tokens = 200000
reasoning_levels = ["low"]

"#;
    let retired_alias_definitions = r#"[[aliases]]
alias_id = "00000000-0000-0000-0000-000000000002"
selection_id = "00000000-0000-0000-0000-000000000001"

[[aliases]]
alias_id = "7fde05bc-b4c3-44f7-8a87-748814c80191"
selection_id = "00000000-0000-0000-0000-000000000001"

[[aliases]]
alias_id = "540ce009-c2ec-4a04-b823-c411ea189778"
selection_id = "00000000-0000-0000-0000-000000000001"
"#;
    let configuration_without_model = MODEL_CONFIGURATION
        .replacen(retired_model_definition, "", 1)
        .replacen(retired_alias_definitions, "", 1);
    let _recovered_turn_count = runtime
        .restart_with_templates(
            &configuration_without_model,
            SessionTemplateConfiguration::default(),
        )
        .await?;
    let mut connection = Connection::connect(runtime.socket()).await?;

    connection
        .request_version(
            ProtocolVersion::One,
            3,
            ClientRequest::StopSession {
                command_id: command()?,
                session_id,
                sticky: true,
                descendant_scope: DescendantTerminationScope::ParentAlone,
            },
        )
        .await?;
    assert_eq!(
        response_within(&mut connection).await?.message(),
        &ServerMessage::SessionLifecycleCommandApplied {
            session_id,
            effect: SessionLifecycleEffect::ClosurePending { live_turn_id },
        }
    );
    let closure_state: (String, Option<String>, i64) = sqlx::query_as(
        "SELECT lifecycle.state_kind, lifecycle.pending_terminal_outcome_kind,
                count(command.command_id)
           FROM session_lifecycle AS lifecycle
           LEFT JOIN submit_input_command AS command
             ON command.session_id = lifecycle.session_id
            AND command.delivery_kind = 'interrupt'
            AND command.actor_kind = 'core'
            AND command.model_override_kind = 'replace_with'
            AND command.replacement_model_kind = 'direct'
          WHERE lifecycle.session_id = $1
          GROUP BY lifecycle.state_kind, lifecycle.pending_terminal_outcome_kind",
    )
    .bind(session_id.into_uuid())
    .fetch_one(&runtime.pool)
    .await?;
    assert_eq!(closure_state, (String::from("terminal"), None, 1));

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
            ProtocolVersion::One,
            2,
            ClientRequest::StopTurn {
                command_id: command()?,
                session_id,
                expected_active_turn_id: unstarted_turn_id,
                content: UserInputContent::text(String::from("names no active turn")),
                expected_defaults_version: CanonicalU64::new(1),
                descendant_scope: DescendantTerminationScope::ParentAlone,
                model_settings: ModelSettingsOverlay::inherit_all(),
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
            ProtocolVersion::One,
            4,
            ClientRequest::StopTurn {
                command_id: command()?,
                session_id,
                expected_active_turn_id: unstarted_turn_id,
                content: UserInputContent::text(String::from("names a stale turn")),
                expected_defaults_version: CanonicalU64::new(1),
                descendant_scope: DescendantTerminationScope::ParentAlone,
                model_settings: ModelSettingsOverlay::inherit_all(),
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
        content: UserInputContent::text(String::from("continue after the stop")),
        expected_defaults_version: CanonicalU64::new(1),
        descendant_scope: DescendantTerminationScope::ParentAlone,
        model_settings: ModelSettingsOverlay::inherit_all(),
    };
    connection
        .request_version(ProtocolVersion::One, 3, decision.clone())
        .await?;
    let successor_turn_id = accepted_successor_turn(&mut connection, session_id, 2).await?;

    connection
        .request_version(ProtocolVersion::One, 4, decision)
        .await?;
    let replayed_turn_id = accepted_successor_turn(&mut connection, session_id, 2).await?;

    assert_eq!(
        replayed_turn_id, successor_turn_id,
        "an equal stop retry returns its recorded successor"
    );

    drop(connection);
    runtime.stop().await
}

/// S37 / INV-053: stopping a turn records the explicit per-call contribution
/// with the successor origin instead of dropping it at the daemon boundary.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn s37_inv053_stop_turn_records_its_per_call_model_settings() -> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let session_id = create_alias_session(&mut connection).await?;
    let (_, stopped_turn_id) =
        submit_first_input(&mut connection, session_id, String::from("first request")).await?;
    activate_turn(&runtime.pool, SessionId::from_uuid(session_id.into_uuid())).await?;
    let requested = low_reasoning_override();

    connection
        .request_version(
            ProtocolVersion::One,
            3,
            ClientRequest::StopTurn {
                command_id: command()?,
                session_id,
                expected_active_turn_id: stopped_turn_id,
                content: UserInputContent::text(String::from("continue with deliberate reasoning")),
                expected_defaults_version: CanonicalU64::new(1),
                descendant_scope: DescendantTerminationScope::ParentAlone,
                model_settings: requested,
            },
        )
        .await?;
    let settings = accepted_successor_model_settings(&mut connection, session_id, 2).await?;

    assert_eq!(settings.precedence.per_call, requested);
    assert_eq!(
        settings.effective.reasoning_level,
        Some(ReasoningLevel::Low)
    );
    assert_eq!(settings.reasoning_source, Some(ModelSettingSource::PerCall));
    assert_eq!(
        settings.validated_for_selection_id,
        Some(primary_direct_selection_id())
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
            ProtocolVersion::One,
            3,
            ClientRequest::StopTurn {
                command_id: command()?,
                session_id,
                expected_active_turn_id: parked_turn_id,
                content: UserInputContent::text(String::from("stop during the approval wait")),
                expected_defaults_version: CanonicalU64::new(1),
                descendant_scope: DescendantTerminationScope::ParentAlone,
                model_settings: ModelSettingsOverlay::inherit_all(),
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
    assert_eq!(
        turn_state_of(&parked, parked_turn_id),
        TurnState::ActiveAwaitingToolApproval {
            tool_request_id: pending_request_id,
        }
    );
    assert_eq!(
        tool_use_entry_names(&parked, pending_request_id),
        vec![String::from("confirmed")],
        "the pending request's identity and tool name are client-visible"
    );

    connection
        .request_version(
            ProtocolVersion::One,
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
            ProtocolVersion::One,
            6,
            ClientRequest::StopTurn {
                command_id: command()?,
                session_id,
                expected_active_turn_id: parked_turn_id,
                content: UserInputContent::text(String::from("continue after the denied round")),
                expected_defaults_version: CanonicalU64::new(1),
                descendant_scope: DescendantTerminationScope::ParentAlone,
                model_settings: ModelSettingsOverlay::inherit_all(),
            },
        )
        .await?;
    let successor_turn_id = accepted_successor_turn(&mut connection, session_id, 2).await?;

    connection
        .request_version(
            ProtocolVersion::One,
            7,
            ClientRequest::SubmitInput {
                command_id: command()?,
                session_id,
                content: UserInputContent::text(String::from("ordinary later work")),
                expected_defaults_version: Some(CanonicalU64::new(1)),
                model_settings: ModelSettingsOverlay::inherit_all(),
                delivery: None,
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
            ProtocolVersion::One,
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
            ProtocolVersion::One,
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
            ProtocolVersion::One,
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
            ProtocolVersion::One,
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
        .request_version(ProtocolVersion::One, 3, denial.clone())
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
        .request_version(ProtocolVersion::One, 4, denial)
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
            ProtocolVersion::One,
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
            ProtocolVersion::One,
            6,
            ClientRequest::SubmitInput {
                command_id: submit_command,
                session_id,
                content: UserInputContent::text(String::from("claims a submit identity")),
                expected_defaults_version: Some(CanonicalU64::new(1)),
                model_settings: ModelSettingsOverlay::inherit_all(),
                delivery: None,
            },
        )
        .await?;
    assert!(matches!(
        rejected_detail(response_within(&mut connection).await?.message()),
        RejectionDetail::ActiveTurnPresent { .. }
    ));
    connection
        .request_version(
            ProtocolVersion::One,
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
            ProtocolVersion::One,
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

/// A decision in flight when the daemon drains is either acknowledged and
/// durable or unacknowledged and unclaimed; after the restart the same command
/// replays to one applied decision with one `delivered` receipt.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn decide_tool_request_survives_a_drain_and_restart() -> Result<(), Box<dyn Error>> {
    let mut runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let session_id = create_alias_session(&mut connection).await?;
    submit_first_input(&mut connection, session_id, String::from("first request")).await?;
    let pending_request_id = CanonicalUuid::from_uuid(Uuid::from_u128(0xE1));
    park_turn_on_tool_approval(&runtime.pool, session_id, &[pending_request_id]).await?;

    let decision_command = command()?;
    let decision = ClientRequest::DecideToolRequest {
        command_id: decision_command,
        session_id,
        tool_request_id: pending_request_id,
        decision: ToolDecision::Approve {},
    };
    connection
        .request_version(ProtocolVersion::One, 3, decision.clone())
        .await?;
    runtime.shutdown.send_replace(true);
    // Whether the drain let this reply through or closed the socket first is
    // the race under test; either way the replay below settles the decision.
    let _acknowledgement = timeout(Duration::from_secs(5), connection.response()).await;
    drop(connection);
    runtime.restart().await?;

    let mut connection = Connection::connect(runtime.socket()).await?;
    connection
        .request_version(ProtocolVersion::One, 4, decision)
        .await?;
    assert_eq!(
        decided_receipt(response_within(&mut connection).await?.message()),
        (pending_request_id, ToolDecision::Approve {}),
        "the drained decision replays as one applied decision"
    );
    let settled: (i64, String) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM tool_approval_decision WHERE request_id = $1),
                receipt.outcome_kind
           FROM injection_settled_outbox_event AS receipt
          WHERE receipt.command_id = $2",
    )
    .bind(pending_request_id.into_uuid())
    .bind(decision_command.into_uuid())
    .fetch_one(&runtime.pool)
    .await?;
    assert_eq!(settled, (1, String::from("delivered")));

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
            ProtocolVersion::One,
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
            ProtocolVersion::One,
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

async fn submit_queued_input(
    connection: &mut Connection,
    request_id: u64,
    session_id: CanonicalUuid,
    expected_active_turn_id: CanonicalUuid,
    acceptance_position: u64,
    content: &str,
) -> Result<CanonicalUuid, Box<dyn Error>> {
    connection
        .request_version(
            ProtocolVersion::One,
            request_id,
            ClientRequest::SubmitInput {
                command_id: command()?,
                session_id,
                content: UserInputContent::text(content.to_owned()),
                expected_defaults_version: Some(CanonicalU64::new(1)),
                model_settings: ModelSettingsOverlay::inherit_all(),
                delivery: Some(InputDelivery::Queue {
                    expected_active_turn_id,
                }),
            },
        )
        .await?;
    accepted_successor_turn(connection, session_id, acceptance_position).await
}

async fn activate_expected_turn(
    pool: &PgPool,
    session: SessionId,
    expected_turn: CanonicalUuid,
) -> Result<(), Box<dyn Error>> {
    let mut service = StartEligibleTurnService::new(
        UuidV7StartEligibleTurnIdGenerator,
        StartEligibleTurnRepository::new(pool.clone()),
    );
    match service.execute(session).await? {
        StartEligibleTurnOutcome::Activated(activated)
            if activated.turn().into_uuid() == expected_turn.into_uuid() =>
        {
            let recorded =
                signalboxd::WorkspaceInstructionRuntime::new(pool.clone(), None, Vec::new())
                    .prepare(session, activated.turn())
                    .await?;
            if !recorded {
                return Err(
                    io::Error::other("the fixture instruction manifest must record").into(),
                );
            }
            Ok(())
        }
        StartEligibleTurnOutcome::Activated(activated) => Err(io::Error::other(format!(
            "activated turn {} instead of expected {expected_turn}",
            activated.turn().into_uuid()
        ))
        .into()),
        StartEligibleTurnOutcome::NoEligibleTurn => {
            Err(io::Error::other("the expected queued turn was not eligible").into())
        }
    }
}

/// S08: steering against an idle session is a durable-submit
/// refusal with the exact expected turn, never an internal daemon error.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn s08_steering_without_an_active_turn_is_a_typed_rejection() -> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let session_id = create_alias_session(&mut connection).await?;
    let expected_active_turn_id = CanonicalUuid::from_uuid(Uuid::from_u128(0x1301));

    connection
        .request_version(
            ProtocolVersion::One,
            2,
            ClientRequest::SubmitInput {
                command_id: command()?,
                session_id,
                content: UserInputContent::text(String::from("steer no turn")),
                expected_defaults_version: None,
                model_settings: ModelSettingsOverlay::inherit_all(),
                delivery: Some(InputDelivery::Steer {
                    expected_active_turn_id,
                }),
            },
        )
        .await?;
    assert_eq!(
        rejected_detail(response_within(&mut connection).await?.message()),
        RejectionDetail::NoActiveTurn {
            session_id,
            expected_active_turn_id,
        }
    );

    drop(connection);
    runtime.stop().await
}

/// S09: two after-current-turn inputs stay queued until the occupied slot terminalizes, then activate in
/// immutable acceptance order.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn s09_queued_inputs_deliver_in_acceptance_order_after_the_active_turn()
-> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let session_id = create_alias_session(&mut connection).await?;
    let (_, active_turn_id) =
        submit_first_input(&mut connection, session_id, String::from("active request")).await?;
    let session = SessionId::from_uuid(session_id.into_uuid());
    activate_turn(&runtime.pool, session).await?;

    let first_queued_turn = submit_queued_input(
        &mut connection,
        3,
        session_id,
        active_turn_id,
        2,
        "first queued request",
    )
    .await?;
    let second_queued_turn = submit_queued_input(
        &mut connection,
        4,
        session_id,
        active_turn_id,
        3,
        "second queued request",
    )
    .await?;
    assert_ne!(first_queued_turn, second_queued_turn);

    let targets = support::parse_model_configuration(MODEL_CONFIGURATION)?.target_catalog();
    complete_active_text_turn(&runtime.pool, session, targets.clone()).await?;
    activate_expected_turn(&runtime.pool, session, first_queued_turn).await?;
    complete_active_text_turn(&runtime.pool, session, targets).await?;
    activate_expected_turn(&runtime.pool, session, second_queued_turn).await?;

    drop(connection);
    runtime.stop().await
}

/// S03: an acknowledged after-current-turn input remains durable across an
/// actual process stop, startup scan, and listener restart, then activates as
/// the exact queued turn after the abandoned active turn is recovered.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn s03_queued_input_survives_process_restart_and_startup_scan() -> Result<(), Box<dyn Error>>
{
    let mut runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let session_id = create_alias_session(&mut connection).await?;
    let (_, active_turn_id) =
        submit_first_input(&mut connection, session_id, String::from("active request")).await?;
    let session = SessionId::from_uuid(session_id.into_uuid());
    activate_turn(&runtime.pool, session).await?;
    let queued_turn_id = submit_queued_input(
        &mut connection,
        3,
        session_id,
        active_turn_id,
        2,
        "durable queued request",
    )
    .await?;
    drop(connection);

    assert_eq!(runtime.restart().await?, 1);
    activate_expected_turn(&runtime.pool, session, queued_turn_id).await?;

    runtime.stop().await
}

/// S34 / INV-012 / INV-033 / INV-046: a prompted session exposes exact current
/// and named defaults epochs and replaces the prompt forward-only with the
/// complete installed echo.
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
            ProtocolVersion::One,
            1,
            ClientRequest::CreateSession {
                command_id: command()?,
                initial_model_selection: ModelSelection::Direct {
                    selection_id: selection,
                },
                model_settings: ModelSettingsOverlay::inherit_all(),
                system_prompt: SystemPromptMember::present(Some(prompt.clone())),
                placement: SessionPlacement::Pathless {},
                lifecycle: signalbox_process_protocol::SessionLifecycleMembers::default(),
            },
        )
        .await?;
    let created = session_created_facts(response_within(&mut connection).await?.message());
    let session_id = created.session_id;
    assert_eq!(created.model_settings, provider_default_model_settings());

    connection
        .request_version(
            ProtocolVersion::One,
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
            model_settings: provider_default_model_settings(),
            system_prompt: Some(prompt.clone()),
        }
    );

    // The replacement states the complete successor explicitly,
    // clearing the prompt, and its receipt echoes the complete install.
    connection
        .request_version(
            ProtocolVersion::One,
            4,
            ClientRequest::ReplaceSessionDefaults {
                command_id: command()?,
                session_id,
                expected_defaults_version: CanonicalU64::new(1),
                model_selection: ModelSelection::Direct {
                    selection_id: selection,
                },
                dangerous_tool_auto_approval: false,
                model_settings: ModelSettingsOverlay::inherit_all(),
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
            model_settings: provider_default_model_settings(),
            system_prompt: SystemPromptMember::present(None),
        }
    );

    connection
        .request_version(
            ProtocolVersion::One,
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
            model_settings: provider_default_model_settings(),
            system_prompt: Some(prompt),
        }
    );

    connection
        .request_version(
            ProtocolVersion::One,
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
            ProtocolVersion::One,
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

/// S37 / INV-051: an explicit unsupported replacement value is a typed caller
/// error even when changing models would have adjusted an inherited value.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn s37_inv051_model_change_rejects_an_explicit_unsupported_setting()
-> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let requested_reasoning = ReasoningLevel::Low;
    let requested = ModelSettingsOverlay {
        reasoning_level: SettingOverlay::Value(requested_reasoning),
        fast_mode: signalbox_process_protocol::FastModeOverlay::Inherit,
        service_tier: SettingOverlay::Inherit,
    };
    let (session_id, _) = create_direct_session_with_settings(
        &mut connection,
        primary_direct_selection_id(),
        requested,
    )
    .await?;

    connection
        .request(
            2,
            ClientRequest::ReplaceSessionDefaults {
                command_id: command()?,
                session_id,
                expected_defaults_version: CanonicalU64::new(1),
                model_selection: ModelSelection::Direct {
                    selection_id: next_direct_selection_id(),
                },
                model_settings: requested,
                dangerous_tool_auto_approval: false,
                system_prompt: SystemPromptMember::present(None),
            },
        )
        .await?;
    let error = response_within(&mut connection).await?.message().clone();

    assert_eq!(protocol_error_code(&error), ErrorCode::Rejected);
    assert_eq!(
        protocol_error_detail(&error),
        Some(RejectionDetail::UnsupportedReasoningLevel {
            selection_id: next_direct_selection_id(),
            requested: requested_reasoning,
        })
    );

    drop(connection);
    runtime.stop().await
}

/// S37 / INV-052 / INV-053: defaults replacement carries the prior session
/// layer across a model change, clears an inherited incompatible value, and
/// emits the exact automatic adjustment as durable follower evidence.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn s37_inv052_inv053_model_change_clamps_inherited_session_settings()
-> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let requested_reasoning = ReasoningLevel::Low;
    let caller_session_settings = ModelSettingsOverlay {
        reasoning_level: SettingOverlay::Value(requested_reasoning),
        fast_mode: signalbox_process_protocol::FastModeOverlay::Inherit,
        service_tier: SettingOverlay::Inherit,
    };
    let (session_id, created_settings) = create_direct_session_with_settings(
        &mut connection,
        primary_direct_selection_id(),
        caller_session_settings,
    )
    .await?;
    let mut follow =
        attach_empty_follower(runtime.socket(), ProtocolVersion::One, 10, session_id).await?;

    connection
        .request(
            2,
            ClientRequest::ReplaceSessionDefaults {
                command_id: command()?,
                session_id,
                expected_defaults_version: CanonicalU64::new(1),
                model_selection: ModelSelection::Direct {
                    selection_id: next_direct_selection_id(),
                },
                model_settings: ModelSettingsOverlay::inherit_all(),
                dangerous_tool_auto_approval: false,
                system_prompt: SystemPromptMember::present(None),
            },
        )
        .await?;
    let replacement =
        session_defaults_replaced_facts(response_within(&mut connection).await?.message());
    let defaults_version = replacement.defaults_version;
    let installed_settings = replacement.installed_settings;
    let event =
        session_model_settings_changed_event_facts(response_within(&mut follow).await?.message());

    assert_eq!(
        created_settings.effective.reasoning_level,
        Some(requested_reasoning)
    );
    assert_eq!(defaults_version.value(), 2);
    assert_eq!(installed_settings.effective.reasoning_level, None);
    assert_eq!(
        installed_settings.precedence.session.reasoning_level,
        SettingOverlay::ProviderDefault
    );
    assert_eq!(
        installed_settings.validated_for_selection_id,
        Some(next_direct_selection_id())
    );
    assert_eq!(event.session_id, session_id);
    assert_eq!(event.prior_defaults_version, 1);
    assert_eq!(event.installed_defaults_version, defaults_version.value());
    assert_eq!(event.installed_settings, installed_settings);
    assert_eq!(event.caller_override, ModelSettingsOverlay::inherit_all());
    assert_eq!(
        event.adjustments,
        [ModelChangeAdjustment::ReasoningLevelCleared {
            from: requested_reasoning,
        }]
    );

    drop(follow);
    drop(connection);
    runtime.stop().await
}

/// S01 / INV-012: an equal explicit-creation replay is decided from its
/// durable command before the current deployment revalidates model settings.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn s01_inv012_create_session_replays_after_capability_removal() -> Result<(), Box<dyn Error>>
{
    let mut runtime = RunningRuntime::start().await?;
    let command_id = command()?;
    let requested_settings = ModelSettingsOverlay {
        reasoning_level: SettingOverlay::Value(ReasoningLevel::Low),
        fast_mode: signalbox_process_protocol::FastModeOverlay::Inherit,
        service_tier: SettingOverlay::Inherit,
    };
    let mut connection = Connection::connect(runtime.socket()).await?;
    connection
        .request(
            1,
            ClientRequest::CreateSession {
                command_id,
                initial_model_selection: ModelSelection::Direct {
                    selection_id: primary_direct_selection_id(),
                },
                model_settings: requested_settings,
                system_prompt: SystemPromptMember::present(None),
                placement: SessionPlacement::Pathless {},
                lifecycle: signalbox_process_protocol::SessionLifecycleMembers::default(),
            },
        )
        .await?;
    let applied = session_created_facts(response_within(&mut connection).await?.message());
    drop(connection);

    let configuration_without_reasoning =
        MODEL_CONFIGURATION.replace("reasoning_levels = [\"low\"]\n", "");
    let _recovered_turn_count = runtime
        .restart_with_model_configuration(&configuration_without_reasoning)
        .await?;
    let mut replay_connection = Connection::connect(runtime.socket()).await?;
    replay_connection
        .request(
            2,
            ClientRequest::CreateSession {
                command_id,
                initial_model_selection: ModelSelection::Direct {
                    selection_id: primary_direct_selection_id(),
                },
                model_settings: requested_settings,
                system_prompt: SystemPromptMember::present(None),
                placement: SessionPlacement::Pathless {},
                lifecycle: signalbox_process_protocol::SessionLifecycleMembers::default(),
            },
        )
        .await?;
    let replayed = session_created_facts(response_within(&mut replay_connection).await?.message());

    assert_eq!(replayed, applied);

    drop(replay_connection);
    runtime.stop().await
}

/// S28 / INV-012: an equal imported-continuation replay is decided from its
/// durable command before the current deployment revalidates model settings.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn s28_inv012_imported_session_replays_after_capability_removal() -> Result<(), Box<dyn Error>>
{
    let mut runtime = RunningRuntime::start().await?;
    let fixture = ImportedInspectionFixture::insert(&runtime.pool).await?;
    let command_id = command()?;
    let requested_settings = ModelSettingsOverlay {
        reasoning_level: SettingOverlay::Value(ReasoningLevel::Low),
        fast_mode: signalbox_process_protocol::FastModeOverlay::Inherit,
        service_tier: SettingOverlay::Inherit,
    };
    let mut connection = Connection::connect(runtime.socket()).await?;
    connection
        .request(
            1,
            ClientRequest::CreateSessionFromImportedFrontier {
                command_id,
                imported_conversation_id: fixture.conversation,
                through_position: fixture.last_position,
                relationship: signalbox_process_protocol::ImportedSessionRelationship::Resume,
                initial_model_selection: ModelSelection::Direct {
                    selection_id: primary_direct_selection_id(),
                },
                model_settings: requested_settings,
            },
        )
        .await?;
    let applied = session_created_facts(response_within(&mut connection).await?.message());
    drop(connection);

    let configuration_without_reasoning =
        MODEL_CONFIGURATION.replace("reasoning_levels = [\"low\"]\n", "");
    let _recovered_turn_count = runtime
        .restart_with_model_configuration(&configuration_without_reasoning)
        .await?;
    let mut replay_connection = Connection::connect(runtime.socket()).await?;
    replay_connection
        .request(
            2,
            ClientRequest::CreateSessionFromImportedFrontier {
                command_id,
                imported_conversation_id: fixture.conversation,
                through_position: fixture.last_position,
                relationship: signalbox_process_protocol::ImportedSessionRelationship::Resume,
                initial_model_selection: ModelSelection::Direct {
                    selection_id: primary_direct_selection_id(),
                },
                model_settings: requested_settings,
            },
        )
        .await?;
    let replayed = session_created_facts(response_within(&mut replay_connection).await?.message());

    assert_eq!(replayed, applied);

    drop(replay_connection);
    runtime.stop().await
}

/// S37 / INV-012: an equal defaults-replacement replay returns its durable
/// result before the current deployment revalidates model settings.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn s37_inv012_defaults_replacement_replays_after_capability_removal()
-> Result<(), Box<dyn Error>> {
    let mut runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let (session_id, _) = create_direct_session_with_settings(
        &mut connection,
        primary_direct_selection_id(),
        ModelSettingsOverlay::inherit_all(),
    )
    .await?;
    let command_id = command()?;
    let requested_settings = ModelSettingsOverlay {
        reasoning_level: SettingOverlay::Value(ReasoningLevel::Low),
        fast_mode: signalbox_process_protocol::FastModeOverlay::Inherit,
        service_tier: SettingOverlay::Inherit,
    };
    connection
        .request(
            2,
            ClientRequest::ReplaceSessionDefaults {
                command_id,
                session_id,
                expected_defaults_version: CanonicalU64::new(1),
                model_selection: ModelSelection::Direct {
                    selection_id: primary_direct_selection_id(),
                },
                model_settings: requested_settings,
                dangerous_tool_auto_approval: false,
                system_prompt: SystemPromptMember::present(None),
            },
        )
        .await?;
    let applied =
        session_defaults_replaced_facts(response_within(&mut connection).await?.message());
    drop(connection);

    let configuration_without_reasoning =
        MODEL_CONFIGURATION.replace("reasoning_levels = [\"low\"]\n", "");
    let _recovered_turn_count = runtime
        .restart_with_model_configuration(&configuration_without_reasoning)
        .await?;
    let mut replay_connection = Connection::connect(runtime.socket()).await?;
    replay_connection
        .request(
            3,
            ClientRequest::ReplaceSessionDefaults {
                command_id,
                session_id,
                expected_defaults_version: CanonicalU64::new(1),
                model_selection: ModelSelection::Direct {
                    selection_id: primary_direct_selection_id(),
                },
                model_settings: requested_settings,
                dangerous_tool_auto_approval: false,
                system_prompt: SystemPromptMember::present(None),
            },
        )
        .await?;
    let replayed =
        session_defaults_replaced_facts(response_within(&mut replay_connection).await?.message());

    assert_eq!(replayed, applied);

    drop(replay_connection);
    runtime.stop().await
}

/// S37 / INV-012: a stale replacement records and replays its authoritative
/// version mismatch before current capability validation can reject settings.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn s37_inv012_stale_defaults_replacement_precedes_settings_validation()
-> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let expected_version = CanonicalU64::new(1);
    let current_version = CanonicalU64::new(2);
    let (session_id, _) = create_direct_session_with_settings(
        &mut connection,
        primary_direct_selection_id(),
        ModelSettingsOverlay::inherit_all(),
    )
    .await?;
    connection
        .request(
            2,
            ClientRequest::ReplaceSessionDefaults {
                command_id: command()?,
                session_id,
                expected_defaults_version: expected_version,
                model_selection: ModelSelection::Direct {
                    selection_id: primary_direct_selection_id(),
                },
                model_settings: ModelSettingsOverlay::inherit_all(),
                dangerous_tool_auto_approval: false,
                system_prompt: SystemPromptMember::present(None),
            },
        )
        .await?;
    let applied =
        session_defaults_replaced_facts(response_within(&mut connection).await?.message());
    let stale = ClientRequest::ReplaceSessionDefaults {
        command_id: command()?,
        session_id,
        expected_defaults_version: expected_version,
        model_selection: ModelSelection::Direct {
            selection_id: next_direct_selection_id(),
        },
        model_settings: ModelSettingsOverlay {
            reasoning_level: SettingOverlay::Value(ReasoningLevel::Low),
            fast_mode: signalbox_process_protocol::FastModeOverlay::Inherit,
            service_tier: SettingOverlay::Inherit,
        },
        dangerous_tool_auto_approval: false,
        system_prompt: SystemPromptMember::present(None),
    };
    let expected_rejection = RejectionDetail::DefaultsVersionMismatch {
        session_id,
        expected: expected_version,
        current: current_version,
    };

    connection.request(3, stale.clone()).await?;
    let first = rejected_detail(response_within(&mut connection).await?.message());
    connection.request(4, stale).await?;
    let replayed = rejected_detail(response_within(&mut connection).await?.message());

    assert_eq!(applied.defaults_version, current_version);
    assert_eq!(first, expected_rejection);
    assert_eq!(replayed, expected_rejection);

    drop(connection);
    runtime.stop().await
}

/// S37 / INV-012: an unknown replacement selection is the read-only catalog
/// error even when the same frame names an epoch the session has not reached,
/// and it leaves the command identity available for the corrected request.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn s37_inv012_unknown_replacement_model_precedes_defaults_version_mismatch()
-> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let (session_id, _) = create_direct_session_with_settings(
        &mut connection,
        primary_direct_selection_id(),
        ModelSettingsOverlay::inherit_all(),
    )
    .await?;
    let command_id = command()?;
    let unknown_selection = CanonicalUuid::from_uuid(Uuid::from_u128(0xffff));

    connection
        .request(
            2,
            ClientRequest::ReplaceSessionDefaults {
                command_id,
                session_id,
                expected_defaults_version: CanonicalU64::new(2),
                model_selection: ModelSelection::Direct {
                    selection_id: unknown_selection,
                },
                model_settings: ModelSettingsOverlay::inherit_all(),
                dangerous_tool_auto_approval: false,
                system_prompt: SystemPromptMember::present(None),
            },
        )
        .await?;
    let unknown = response_within(&mut connection).await?.message().clone();

    assert_eq!(protocol_error_code(&unknown), ErrorCode::InvalidRequest);
    assert_eq!(protocol_error_detail(&unknown), None);

    connection
        .request(
            3,
            ClientRequest::ReplaceSessionDefaults {
                command_id,
                session_id,
                expected_defaults_version: CanonicalU64::new(1),
                model_selection: ModelSelection::Direct {
                    selection_id: primary_direct_selection_id(),
                },
                model_settings: ModelSettingsOverlay::inherit_all(),
                dangerous_tool_auto_approval: false,
                system_prompt: SystemPromptMember::present(None),
            },
        )
        .await?;
    let corrected =
        session_defaults_replaced_facts(response_within(&mut connection).await?.message());

    assert_eq!(corrected.defaults_version, CanonicalU64::new(2));

    drop(connection);
    runtime.stop().await
}

/// S37 / INV-012: an absent session reaches the durable replacement boundary
/// before compatibility validation and replays its recorded terminal result.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn s37_inv012_absent_defaults_replacement_precedes_settings_validation()
-> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let absent_session_id = CanonicalUuid::from_uuid(Uuid::from_u128(0x3701));
    let replacement = ClientRequest::ReplaceSessionDefaults {
        command_id: command()?,
        session_id: absent_session_id,
        expected_defaults_version: CanonicalU64::new(1),
        model_selection: ModelSelection::Direct {
            selection_id: next_direct_selection_id(),
        },
        model_settings: low_reasoning_override(),
        dangerous_tool_auto_approval: false,
        system_prompt: SystemPromptMember::present(None),
    };
    let expected = RejectionDetail::SessionNotFound {
        session_id: absent_session_id,
    };

    connection.request(1, replacement.clone()).await?;
    let first = rejected_detail(response_within(&mut connection).await?.message());
    connection.request(2, replacement).await?;
    let replayed = rejected_detail(response_within(&mut connection).await?.message());

    assert_eq!(first, expected);
    assert_eq!(replayed, expected);

    drop(connection);
    runtime.stop().await
}

/// S01 / S03 / INV-005 / INV-014 / INV-015: explicit compaction uses a
/// dedicated scripted call, retains the complete transcript and exact usage /
/// range provenance, survives startup scan, and projects summary plus suffix
/// into the next ordinary scripted call.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn s01_s03_inv005_inv014_inv015_explicit_compaction_survives_restart_and_projects()
-> Result<(), Box<dyn Error>> {
    let usage = TokenUsage {
        input_tokens: Some(41),
        output_tokens: Some(7),
        cache_creation_input_tokens: Some(5),
        cache_read_input_tokens: Some(29),
    };
    let summary_text = String::from("durable scripted summary");
    let summary_runtime =
        ScriptedModel::single(completed_script("fixture-model", &summary_text, usage));
    let mut runtime = RunningRuntime::start_with_compaction(summary_runtime).await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let session_id = create_alias_session(&mut connection).await?;
    let first_user = String::from("first durable request");
    let (_, first_turn) =
        submit_first_input(&mut connection, session_id, first_user.clone()).await?;
    let first_assistant = String::from("first durable reply");
    let first_model = ScriptedModel::single(completed_script(
        "fixture-model",
        &first_assistant,
        TokenUsage::unreported(),
    ));
    let first_probe =
        execute_streamed_turn(&mut runtime, first_model, session_id, first_turn).await?;
    assert_eq!(first_probe.received_operations().len(), 1);
    let (mut follow, follow_cursor) =
        attach_follower_after_snapshot(runtime.socket(), ProtocolVersion::One, 30, session_id)
            .await?;
    let before_members = sqlx::query_as::<_, (Uuid, Uuid)>(
        "SELECT member.source_session_id, member.semantic_entry_id
           FROM turn_lifecycle AS lifecycle
           JOIN context_frontier_member AS member
             ON member.owning_session_id = lifecycle.session_id
            AND member.context_frontier_id = lifecycle.terminal_frontier_id
          WHERE lifecycle.session_id = $1
            AND lifecycle.turn_id = $2
          ORDER BY member.member_position",
    )
    .bind(session_id.into_uuid())
    .bind(first_turn.into_uuid())
    .fetch_all(&runtime.pool)
    .await?;
    let compaction_command = command()?;
    connection
        .request_version(
            ProtocolVersion::One,
            3,
            ClientRequest::CompactSession {
                command_id: compaction_command,
                session_id,
                through_position: None,
            },
        )
        .await?;
    let receipt = response_within(&mut connection).await?;
    let ServerMessage::SessionCompacted {
        session_id: compacted_session,
        context_compaction_id,
        model_call_id,
        through_position,
        summary_entry_id,
        result_frontier_id,
    } = receipt.message()
    else {
        panic!(
            "the explicit compaction fixture expected a receipt, got {:?}",
            receipt.message()
        )
    };
    assert_eq!(*compacted_session, session_id);
    assert_eq!(through_position.value(), before_members.len() as u64);
    let followed = response_within(&mut follow).await?;
    let ServerMessage::SessionEvent {
        cursor: followed_cursor,
        session_id: followed_session,
        event:
            SessionEvent::ContextCompacted {
                context_compaction_id: followed_compaction,
                model_call_id: followed_call,
                through_position: followed_through,
                summary_entry_id: followed_summary,
                result_frontier_id: followed_frontier,
            },
    } = followed.message()
    else {
        panic!(
            "the established follower expected a compaction event, got {:?}",
            followed.message()
        );
    };
    assert!(followed_cursor.value() > follow_cursor);
    assert_eq!(*followed_session, session_id);
    assert_eq!(*followed_compaction, *context_compaction_id);
    assert_eq!(*followed_call, *model_call_id);
    assert_eq!(*followed_through, *through_position);
    assert_eq!(*followed_summary, *summary_entry_id);
    assert_eq!(*followed_frontier, *result_frontier_id);
    let after_members = sqlx::query_as::<_, (Uuid, Uuid)>(
        "SELECT source_session_id, semantic_entry_id
           FROM context_frontier_member
          WHERE owning_session_id = $1
            AND context_frontier_id = $2
          ORDER BY member_position",
    )
    .bind(session_id.into_uuid())
    .bind(result_frontier_id.into_uuid())
    .fetch_all(&runtime.pool)
    .await?;
    assert_eq!(
        &after_members[..before_members.len()],
        before_members.as_slice()
    );
    assert_eq!(after_members.len(), before_members.len() + 1);
    assert_eq!(
        after_members.last().map(|member| member.1),
        Some(summary_entry_id.into_uuid())
    );
    let stored_provenance = sqlx::query_as::<_, (Uuid, Uuid, Uuid, Uuid, Uuid, Uuid)>(
        "SELECT compaction.producing_call_id,
                compaction.first_source_session_id, compaction.first_entry_id,
                compaction.through_source_session_id, compaction.through_entry_id,
                summary.context_summary_producing_call_id
           FROM context_compaction AS compaction
           JOIN semantic_transcript_entry AS summary
             ON summary.source_session_id = compaction.session_id
            AND summary.semantic_entry_id = compaction.summary_entry_id
          WHERE compaction.context_compaction_id = $1",
    )
    .bind(context_compaction_id.into_uuid())
    .fetch_one(&runtime.pool)
    .await?;
    assert_eq!(stored_provenance.0, model_call_id.into_uuid());
    assert_eq!(
        (stored_provenance.1, stored_provenance.2),
        before_members[0]
    );
    assert_eq!(
        (stored_provenance.3, stored_provenance.4),
        *before_members
            .last()
            .expect("the terminal frontier is nonempty")
    );
    assert_eq!(stored_provenance.5, model_call_id.into_uuid());
    let stored_usage = sqlx::query_as::<_, (Option<i64>, Option<i64>, Option<i64>, Option<i64>)>(
        "SELECT input_tokens::bigint, output_tokens::bigint,
                cache_creation_input_tokens::bigint, cache_read_input_tokens::bigint
           FROM context_compaction_model_call
          WHERE model_call_id = $1",
    )
    .bind(model_call_id.into_uuid())
    .fetch_one(&runtime.pool)
    .await?;
    assert_eq!(stored_usage.0, usage.input_tokens.map(|value| value as i64));
    assert_eq!(
        stored_usage.1,
        usage.output_tokens.map(|value| value as i64)
    );
    assert_eq!(
        stored_usage.2,
        usage.cache_creation_input_tokens.map(|value| value as i64)
    );
    assert_eq!(
        stored_usage.3,
        usage.cache_read_input_tokens.map(|value| value as i64)
    );

    drop(connection);
    assert_eq!(runtime.restart().await?, 0);
    let mut successor = Connection::connect(runtime.socket()).await?;
    successor
        .request_version(
            ProtocolVersion::One,
            4,
            ClientRequest::CompactSession {
                command_id: compaction_command,
                session_id,
                through_position: None,
            },
        )
        .await?;
    assert_eq!(
        response_within(&mut successor).await?.message(),
        &ServerMessage::SessionCompacted {
            session_id,
            context_compaction_id: *context_compaction_id,
            model_call_id: *model_call_id,
            through_position: *through_position,
            summary_entry_id: *summary_entry_id,
            result_frontier_id: *result_frontier_id,
        }
    );
    let second_user = String::from("post-restart suffix request");
    successor
        .request_version(
            ProtocolVersion::One,
            5,
            ClientRequest::SubmitInput {
                command_id: command()?,
                session_id,
                content: UserInputContent::text(second_user.clone()),
                expected_defaults_version: Some(CanonicalU64::new(1)),
                model_settings: ModelSettingsOverlay::inherit_all(),
                delivery: None,
            },
        )
        .await?;
    let second_turn = accepted_successor_turn(&mut successor, session_id, 2).await?;
    let second_model = RecordingCountedScriptedModel::following(
        [completed_script(
            "fixture-model",
            "post-restart reply",
            TokenUsage::unreported(),
        )],
        [],
    );
    let second_probe = execute_recorded_turn(
        &mut runtime,
        second_model,
        support::parse_model_configuration(MODEL_CONFIGURATION)?,
        session_id,
        second_turn,
    )
    .await?;
    let prepared = second_probe.prepared_operations();
    assert_eq!(prepared.len(), 1);
    assert_eq!(
        rendered_text_messages(&prepared[0]),
        vec![
            (
                signalbox_model_runtime::ConversationRole::User,
                format!("Signalbox prior-conversation summary:\n{summary_text}"),
            ),
            (signalbox_model_runtime::ConversationRole::User, second_user,),
        ]
    );
    let persisted_summary: String = sqlx::query_scalar(
        "SELECT context_summary_value
           FROM semantic_transcript_entry
          WHERE source_session_id = $1
            AND semantic_entry_id = $2",
    )
    .bind(session_id.into_uuid())
    .bind(summary_entry_id.into_uuid())
    .fetch_one(&runtime.pool)
    .await?;
    assert_eq!(persisted_summary, summary_text);

    let recovery_repository = ContextCompactionRepository::new(runtime.pool.clone());
    let prepared_call = ModelCallId::from_uuid(Uuid::from_u128(0xcc20));
    let prepared_outcome = recovery_repository
        .prepare(PrepareContextCompactionRequest {
            command: DurableCommandId::from_uuid(Uuid::from_u128(0xcc21)),
            session: SessionId::from_uuid(session_id.into_uuid()),
            requested_through_position: None,
            automatic_for_turn: None,
            defaults_version: SessionConfigurationDefaultsVersion::first(),
            selection: DirectModelSelection::from_uuid(Uuid::from_u128(1)),
            target: ResolvedProviderTarget::naming(ProviderModelIdentity::from_uuid(
                Uuid::from_u128(3),
            )),
            input_includes_cache_tokens: false,
            credential_reference: String::from("synthetic-compaction-credential"),
            call: prepared_call,
            compaction: ContextCompactionId::from_uuid(Uuid::from_u128(0xcc22)),
            summary_entry: SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xcc23)),
            result_frontier: ContextFrontierId::from_uuid(Uuid::from_u128(0xcc24)),
        })
        .await?;
    let PrepareContextCompactionOutcome::Prepared(prepared) = prepared_outcome else {
        panic!("the recovery fixture must leave a Prepared compaction call");
    };
    assert_eq!(prepared.call(), prepared_call);

    drop(successor);
    assert_eq!(runtime.restart().await?, 0);
    let prepared_recovery = sqlx::query_as::<_, (String, String, String)>(
        "SELECT call.state_kind, call.terminal_disposition_kind, command.result_kind
           FROM context_compaction_model_call AS call
           JOIN compact_session_command AS command
             ON command.session_id = call.session_id
            AND command.model_call_id = call.model_call_id
          WHERE call.model_call_id = $1",
    )
    .bind(prepared_call.into_uuid())
    .fetch_one(&runtime.pool)
    .await?;
    assert_eq!(
        prepared_recovery,
        (
            String::from("terminal"),
            String::from("known_failed"),
            String::from("failed"),
        )
    );

    let in_flight_call = ModelCallId::from_uuid(Uuid::from_u128(0xcc25));
    let in_flight_outcome = recovery_repository
        .prepare(PrepareContextCompactionRequest {
            command: DurableCommandId::from_uuid(Uuid::from_u128(0xcc26)),
            session: SessionId::from_uuid(session_id.into_uuid()),
            requested_through_position: None,
            automatic_for_turn: None,
            defaults_version: SessionConfigurationDefaultsVersion::first(),
            selection: DirectModelSelection::from_uuid(Uuid::from_u128(1)),
            target: ResolvedProviderTarget::naming(ProviderModelIdentity::from_uuid(
                Uuid::from_u128(3),
            )),
            input_includes_cache_tokens: false,
            credential_reference: String::from("synthetic-compaction-credential"),
            call: in_flight_call,
            compaction: ContextCompactionId::from_uuid(Uuid::from_u128(0xcc27)),
            summary_entry: SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xcc28)),
            result_frontier: ContextFrontierId::from_uuid(Uuid::from_u128(0xcc29)),
        })
        .await?;
    let PrepareContextCompactionOutcome::Prepared(in_flight) = in_flight_outcome else {
        panic!("the recovery fixture must authorize an InFlight compaction call");
    };
    recovery_repository.authorize(&in_flight).await?;
    assert_eq!(runtime.restart().await?, 0);
    let in_flight_recovery = sqlx::query_as::<_, (String, String, String)>(
        "SELECT call.state_kind, call.terminal_disposition_kind, command.result_kind
           FROM context_compaction_model_call AS call
           JOIN compact_session_command AS command
             ON command.session_id = call.session_id
            AND command.model_call_id = call.model_call_id
          WHERE call.model_call_id = $1",
    )
    .bind(in_flight_call.into_uuid())
    .fetch_one(&runtime.pool)
    .await?;
    assert_eq!(
        in_flight_recovery,
        (
            String::from("terminal"),
            String::from("ambiguous"),
            String::from("failed"),
        )
    );
    let physical_summary_count: i64 = sqlx::query_scalar(
        "SELECT count(*)
           FROM semantic_transcript_entry
          WHERE source_session_id = $1
            AND payload_kind = 'context_summary'",
    )
    .bind(session_id.into_uuid())
    .fetch_one(&runtime.pool)
    .await?;
    assert_eq!(physical_summary_count, 1);

    runtime.stop().await
}

/// Configuration-owned limits reject an oversized dedicated summary while
/// retaining the adapter-reported terminal usage as durable evidence.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn explicit_compaction_over_limit_retains_usage_without_summary() -> Result<(), Box<dyn Error>>
{
    let usage = TokenUsage {
        input_tokens: Some(17),
        output_tokens: Some(257),
        cache_creation_input_tokens: Some(3),
        cache_read_input_tokens: Some(11),
    };
    let summary_runtime = ScriptedModel::single(completed_script(
        "fixture-model",
        "oversized summary must not persist",
        usage,
    ));
    let mut runtime = RunningRuntime::start_with_compaction(summary_runtime).await?;
    let (mut connection, session_id) = seed_completed_compaction_session(&mut runtime).await?;
    connection
        .request_version(
            ProtocolVersion::One,
            3,
            ClientRequest::CompactSession {
                command_id: command()?,
                session_id,
                through_position: None,
            },
        )
        .await?;

    assert_eq!(
        protocol_error_code(response_within(&mut connection).await?.message()),
        ErrorCode::Unavailable
    );
    let stored = sqlx::query_as::<
        _,
        (
            String,
            String,
            Option<i64>,
            Option<i64>,
            Option<i64>,
            Option<i64>,
        ),
    >(
        "SELECT state_kind, terminal_disposition_kind,
                input_tokens::bigint, output_tokens::bigint,
                cache_creation_input_tokens::bigint, cache_read_input_tokens::bigint
           FROM context_compaction_model_call
          WHERE session_id = $1",
    )
    .bind(session_id.into_uuid())
    .fetch_one(&runtime.pool)
    .await?;
    assert_eq!(
        stored,
        (
            String::from("terminal"),
            String::from("known_failed"),
            usage.input_tokens.map(|value| value as i64),
            usage.output_tokens.map(|value| value as i64),
            usage.cache_creation_input_tokens.map(|value| value as i64),
            usage.cache_read_input_tokens.map(|value| value as i64),
        )
    );
    let summary_count: i64 = sqlx::query_scalar(
        "SELECT count(*)
           FROM semantic_transcript_entry
          WHERE source_session_id = $1 AND payload_kind = 'context_summary'",
    )
    .bind(session_id.into_uuid())
    .fetch_one(&runtime.pool)
    .await?;
    assert_eq!(summary_count, 0);

    drop(connection);
    runtime.stop().await
}

/// INV-012 / INV-014 / INV-015: exact authorization, completion, and failure
/// retries replay their durable outcomes without duplicate summary evidence.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn inv012_inv014_inv015_compaction_lifecycle_retries_are_exact() -> Result<(), Box<dyn Error>>
{
    let mut runtime = RunningRuntime::start().await?;
    let (connection, session_id) = seed_completed_compaction_session(&mut runtime).await?;
    let repository = ContextCompactionRepository::new(runtime.pool.clone());
    let completed_outcome = repository
        .prepare(direct_compaction_request(
            session_id,
            DurableCommandId::from_uuid(Uuid::from_u128(0xdd01)),
            None,
            0xdd02,
        ))
        .await?;
    let PrepareContextCompactionOutcome::Prepared(completed) = completed_outcome else {
        panic!("the completion replay fixture must prepare its call");
    };
    repository.authorize(&completed).await?;
    repository.authorize(&completed).await?;
    let usage = ContextCompactionTokenUsage::unreported()
        .with_input_tokens(Some(13))
        .with_output_tokens(Some(5));
    let first = repository
        .complete(&completed, "exact retained summary", usage)
        .await?;
    let replay = repository
        .complete(&completed, "exact retained summary", usage)
        .await?;
    assert_eq!(replay, first);
    let summary_count: i64 = sqlx::query_scalar(
        "SELECT count(*)
           FROM semantic_transcript_entry
          WHERE source_session_id = $1
            AND semantic_entry_id = $2
            AND payload_kind = 'context_summary'",
    )
    .bind(session_id.into_uuid())
    .bind(first.summary_entry.into_uuid())
    .fetch_one(&runtime.pool)
    .await?;
    assert_eq!(summary_count, 1);

    let failed_outcome = repository
        .prepare(direct_compaction_request(
            session_id,
            DurableCommandId::from_uuid(Uuid::from_u128(0xdd10)),
            None,
            0xdd11,
        ))
        .await?;
    let PrepareContextCompactionOutcome::Prepared(failed) = failed_outcome else {
        panic!("the failure replay fixture must prepare its call");
    };
    repository.authorize(&failed).await?;
    let failed_usage = ContextCompactionTokenUsage::unreported()
        .with_input_tokens(Some(17))
        .with_output_tokens(Some(257));
    repository
        .fail_with_usage(
            &failed,
            FailedContextCompactionDisposition::KnownFailed,
            failed_usage,
        )
        .await?;
    repository
        .fail_with_usage(
            &failed,
            FailedContextCompactionDisposition::KnownFailed,
            failed_usage,
        )
        .await?;
    let failed_state = sqlx::query_as::<_, (String, String, String, Option<i64>, Option<i64>)>(
        "SELECT call.state_kind, call.terminal_disposition_kind, command.result_kind,
                call.input_tokens::bigint, call.output_tokens::bigint
           FROM context_compaction_model_call AS call
           JOIN compact_session_command AS command
             ON command.model_call_id = call.model_call_id
            AND command.session_id = call.session_id
          WHERE call.model_call_id = $1",
    )
    .bind(failed.call().into_uuid())
    .fetch_one(&runtime.pool)
    .await?;
    assert_eq!(
        failed_state,
        (
            String::from("terminal"),
            String::from("known_failed"),
            String::from("failed"),
            failed_usage.input_tokens().map(|value| value as i64),
            failed_usage.output_tokens().map(|value| value as i64),
        )
    );

    drop(connection);
    runtime.stop().await
}

/// INV-012: concurrent reuse of one user-global command identity elects one
/// claimant and makes the loser inspect the committed winner exactly.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn inv012_concurrent_compaction_command_claim_has_one_winner() -> Result<(), Box<dyn Error>> {
    let mut runtime = RunningRuntime::start().await?;
    let (connection, session_id) = seed_completed_compaction_session(&mut runtime).await?;
    let command_id = DurableCommandId::from_uuid(Uuid::from_u128(0xde01));
    let left_repository = ContextCompactionRepository::new(runtime.pool.clone());
    let right_repository = left_repository.clone();
    let left_request = direct_compaction_request(session_id, command_id, None, 0xde10);
    let right_request = direct_compaction_request(session_id, command_id, Some(1), 0xde20);
    let (left, right) = tokio::join!(
        left_repository.prepare(left_request),
        right_repository.prepare(right_request),
    );
    let left = left?;
    let right = right?;
    assert!(
        matches!(left, PrepareContextCompactionOutcome::Prepared(_))
            && matches!(right, PrepareContextCompactionOutcome::ConflictingReuse)
            || matches!(left, PrepareContextCompactionOutcome::ConflictingReuse)
                && matches!(right, PrepareContextCompactionOutcome::Prepared(_))
    );
    let claim_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM durable_command WHERE command_id = $1")
            .bind(command_id.into_uuid())
            .fetch_one(&runtime.pool)
            .await?;
    assert_eq!(claim_count, 1);

    drop(connection);
    runtime.stop().await
}

/// INV-009 / INV-014: compaction preparation and turn activation share the
/// scheduler lock, so exactly one can claim the session boundary and the loser
/// reconstitutes the winner before committing any conflicting lifecycle.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn inv009_inv014_compaction_preparation_serializes_turn_activation()
-> Result<(), Box<dyn Error>> {
    let mut runtime = RunningRuntime::start().await?;
    let (mut connection, session_id) = seed_completed_compaction_session(&mut runtime).await?;
    connection
        .request_version(
            ProtocolVersion::One,
            91,
            ClientRequest::SubmitInput {
                command_id: command()?,
                session_id,
                content: UserInputContent::text(String::from(
                    "scheduler race successor remains singular",
                )),
                expected_defaults_version: Some(CanonicalU64::new(1)),
                model_settings: ModelSettingsOverlay::inherit_all(),
                delivery: None,
            },
        )
        .await?;
    let queued_turn = accepted_successor_turn(&mut connection, session_id, 2).await?;
    let session = SessionId::from_uuid(session_id.into_uuid());
    let mut activation = StartEligibleTurnService::new(
        UuidV7StartEligibleTurnIdGenerator,
        StartEligibleTurnRepository::new(runtime.pool.clone()),
    );
    let repository = ContextCompactionRepository::new(runtime.pool.clone());
    let compaction_request = direct_compaction_request(
        session_id,
        DurableCommandId::from_uuid(Uuid::from_u128(0xde31)),
        None,
        0xde40,
    );

    let (activation_outcome, compaction_outcome) = tokio::join!(
        activation.execute(session),
        repository.prepare(compaction_request),
    );
    let activation_outcome = activation_outcome?;
    let compaction_outcome = compaction_outcome?;
    assert!(
        matches!(activation_outcome, StartEligibleTurnOutcome::Activated(_))
            && matches!(compaction_outcome, PrepareContextCompactionOutcome::Busy)
            || matches!(activation_outcome, StartEligibleTurnOutcome::NoEligibleTurn)
                && matches!(
                    compaction_outcome,
                    PrepareContextCompactionOutcome::Prepared(_)
                ),
        "scheduler serialization must admit exactly one owner: activation={activation_outcome:?}; compaction={compaction_outcome:?}"
    );
    let durable_state: (String, i64) = sqlx::query_as(
        "SELECT turn.state_kind,
                (SELECT count(*)
                   FROM context_compaction_model_call AS call
                  WHERE call.session_id = turn.session_id
                    AND call.state_kind <> 'terminal')
           FROM turn_lifecycle AS turn
          WHERE turn.session_id = $1 AND turn.turn_id = $2",
    )
    .bind(session_id.into_uuid())
    .bind(queued_turn.into_uuid())
    .fetch_one(&runtime.pool)
    .await?;
    assert!(
        durable_state == (String::from("active"), 0)
            || durable_state == (String::from("queued"), 1),
        "the durable boundary must have one owner: {durable_state:?}"
    );

    drop(connection);
    runtime.stop().await
}

/// S01 / S03 / INV-014 / INV-015: an exact provider-native count above the
/// input plus its reserved maximum output above the operator-declared context
/// window compacts before activation, recounts the
/// projected summary-plus-suffix input, and sends only that fitting operation.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn s01_s03_inv014_inv015_automatic_guard_compacts_before_ordinary_send()
-> Result<(), Box<dyn Error>> {
    let mut runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let session_id = create_alias_session(&mut connection).await?;
    let first_user = String::from("automatic guard historical request");
    let (_, first_turn) =
        submit_first_input(&mut connection, session_id, first_user.clone()).await?;
    let first_assistant = String::from("automatic guard historical reply");
    let first_runtime = ScriptedModel::single(completed_script(
        "fixture-model",
        &first_assistant,
        TokenUsage::unreported(),
    ));
    let first_probe =
        execute_streamed_turn(&mut runtime, first_runtime, session_id, first_turn).await?;
    assert_eq!(first_probe.received_operations().len(), 1);

    drop(connection);
    assert_eq!(runtime.restart().await?, 0);
    let mut successor = Connection::connect(runtime.socket()).await?;
    let second_user = String::from("automatic guard current suffix");
    successor
        .request_version(
            ProtocolVersion::One,
            3,
            ClientRequest::SubmitInput {
                command_id: command()?,
                session_id,
                content: UserInputContent::text(second_user.clone()),
                expected_defaults_version: Some(CanonicalU64::new(1)),
                model_settings: ModelSettingsOverlay::inherit_all(),
                delivery: None,
            },
        )
        .await?;
    let second_turn = accepted_successor_turn(&mut successor, session_id, 2).await?;
    let guarded_configuration = support::parse_model_configuration(
        &MODEL_CONFIGURATION
            .replace("max_output_tokens = 256", "max_output_tokens = 1")
            .replace(
                "context_window_tokens = 200000",
                "context_window_tokens = 4096",
            ),
    )?;
    let ordinary_runtime = RecordingCountedScriptedModel::following(
        [completed_script(
            "fixture-model",
            "automatic guard current reply",
            TokenUsage::unreported(),
        )],
        [8192, 4],
    );
    let summary_text = String::from("automatic guard summary");
    let summary_runtime = ScriptedModel::single(completed_script(
        "fixture-model",
        &summary_text,
        TokenUsage::unreported(),
    ));
    let probe = execute_guarded_turn(
        &mut runtime,
        ordinary_runtime,
        summary_runtime,
        guarded_configuration,
        session_id,
        second_turn,
    )
    .await?;
    let counted = probe.counted_operations();
    assert_eq!(counted.len(), 2);
    let first_counted_text = rendered_text_messages(&counted[0]);
    assert!(
        first_counted_text
            .iter()
            .any(|message| message.1 == first_user)
    );
    assert!(
        first_counted_text
            .iter()
            .any(|message| message.1 == first_assistant)
    );
    assert_eq!(
        rendered_text_messages(&counted[1]),
        vec![
            (
                signalbox_model_runtime::ConversationRole::User,
                format!("Signalbox prior-conversation summary:\n{summary_text}"),
            ),
            (
                signalbox_model_runtime::ConversationRole::User,
                second_user.clone(),
            ),
        ]
    );
    let prepared = probe.prepared_operations();
    assert_eq!(prepared.len(), 1);
    assert_eq!(
        rendered_text_messages(&prepared[0]),
        rendered_text_messages(&counted[1])
    );
    let compaction_count: i64 = sqlx::query_scalar(
        "SELECT count(*)
           FROM context_compaction
          WHERE session_id = $1",
    )
    .bind(session_id.into_uuid())
    .fetch_one(&runtime.pool)
    .await?;
    assert_eq!(compaction_count, 1);
    let summary_count: i64 = sqlx::query_scalar(
        "SELECT count(*)
           FROM semantic_transcript_entry
          WHERE source_session_id = $1
            AND payload_kind = 'context_summary'
            AND context_summary_value = $2",
    )
    .bind(session_id.into_uuid())
    .bind(&summary_text)
    .fetch_one(&runtime.pool)
    .await?;
    assert_eq!(summary_count, 1);

    drop(successor);
    runtime.stop().await
}

/// S01 / S03 / INV-014 / INV-015: provider-reported preflight rechecks the
/// completed summary and closes the queued candidate call-free when reserved
/// headroom is still unavailable. The compaction retains its summary output,
/// not the source input that summary replaced, so a summary larger than the
/// window is what leaves the queued turn unservable.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn s01_s03_inv014_inv015_reported_usage_rechecks_compaction_headroom()
-> Result<(), Box<dyn Error>> {
    let mut runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let session_id = create_alias_session(&mut connection).await?;
    let (_, first_turn) = submit_first_input(
        &mut connection,
        session_id,
        String::from("reported usage historical request"),
    )
    .await?;
    let saturated_usage = TokenUsage {
        input_tokens: Some(5000),
        output_tokens: Some(0),
        cache_creation_input_tokens: None,
        cache_read_input_tokens: None,
    };
    let first_runtime = ScriptedModel::single(completed_script(
        "fixture-model",
        "reported usage historical reply",
        saturated_usage,
    ));
    let first_probe =
        execute_streamed_turn(&mut runtime, first_runtime, session_id, first_turn).await?;
    assert_eq!(first_probe.received_operations().len(), 1);

    connection
        .request_version(
            ProtocolVersion::One,
            40,
            ClientRequest::SubmitInput {
                command_id: command()?,
                session_id,
                content: UserInputContent::text(String::from("reported usage queued suffix")),
                expected_defaults_version: Some(CanonicalU64::new(1)),
                model_settings: ModelSettingsOverlay::inherit_all(),
                delivery: None,
            },
        )
        .await?;
    let queued_turn = accepted_successor_turn(&mut connection, session_id, 2).await?;
    let configuration = support::parse_model_configuration(
        &MODEL_CONFIGURATION
            .replace("max_output_tokens = 256", "max_output_tokens = 1")
            .replace(
                "context_window_tokens = 200000",
                "context_window_tokens = 4096",
            ),
    )?;
    let runtime_models = configuration.runtime_model_catalog();
    let saturated_summary_usage = TokenUsage {
        input_tokens: Some(5000),
        output_tokens: Some(5000),
        cache_creation_input_tokens: None,
        cache_read_input_tokens: None,
    };
    let summary_runtime = ScriptedModel::single(completed_script(
        "fixture-model",
        "reported usage summary remains saturated",
        saturated_summary_usage,
    ));
    let summary_probe = summary_runtime.clone();
    let compaction_model: Arc<dyn signalbox_model_provider_runtime::ContextCompactionModel> =
        Arc::new(RuntimeContextCompactionModel::new(
            summary_runtime,
            runtime_models.clone(),
        ));
    let repository = PostgresModelCallRepository::new(
        runtime.pool.clone(),
        configuration.target_catalog(),
        ModelCallCredentialReference::new("reported-usage-recheck-fixture"),
    )
    .with_session_credentials(configuration.credential_family_catalog());
    let compaction = ReportedUsageCompaction::new(
        StartEligibleTurnRepository::new(runtime.pool.clone()),
        repository,
        NoToolCatalog,
        runtime_models,
        configuration,
        compaction_model,
    );

    let failed_turn = reported_usage_still_exceeded_turn(
        compaction
            .compact_if_needed(SessionId::from_uuid(session_id.into_uuid()), None)
            .await,
    );

    assert_eq!(*failed_turn.as_uuid(), queued_turn.into_uuid());
    assert_eq!(summary_probe.received_operations().len(), 1);
    let ordinary_call_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM model_call WHERE turn_id = $1")
            .bind(queued_turn.into_uuid())
            .fetch_one(&runtime.pool)
            .await?;
    assert_eq!(ordinary_call_count, 0);
    let lifecycle: (String, Option<String>, Option<Uuid>) = sqlx::query_as(
        "SELECT state_kind, terminal_disposition_kind, terminal_model_call_id
           FROM turn_lifecycle
          WHERE session_id = $1 AND turn_id = $2",
    )
    .bind(session_id.into_uuid())
    .bind(queued_turn.into_uuid())
    .fetch_one(&runtime.pool)
    .await?;
    assert_eq!(
        lifecycle,
        (String::from("terminal"), Some(String::from("failed")), None)
    );

    drop(connection);
    runtime.stop().await
}

/// S01 / S03 / INV-014 / INV-015: the provider-reported preflight scores the
/// queued turn's own input. Reported usage that fits on its own exhausts the
/// reserved headroom once the waiting input is counted, and the daemon compacts
/// that queued turn before activating it.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn s01_s03_inv014_inv015_reported_usage_preflight_counts_the_queued_input()
-> Result<(), Box<dyn Error>> {
    let mut runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let session_id = create_alias_session(&mut connection).await?;
    let (_, first_turn) = submit_first_input(
        &mut connection,
        session_id,
        String::from("queued input preflight historical request"),
    )
    .await?;
    // The declared window below is 4096 with a one-token output reservation, so
    // this reported input leaves 95 tokens of headroom on its own.
    let fitting_usage = TokenUsage {
        input_tokens: Some(4000),
        output_tokens: Some(0),
        cache_creation_input_tokens: None,
        cache_read_input_tokens: None,
    };
    let first_runtime = ScriptedModel::single(completed_script(
        "fixture-model",
        "queued input preflight historical reply",
        fitting_usage,
    ));
    let first_probe =
        execute_streamed_turn(&mut runtime, first_runtime, session_id, first_turn).await?;
    assert_eq!(first_probe.received_operations().len(), 1);

    // 103 ASCII characters: under the byte-per-token allowance the queued input
    // alone exceeds the remaining headroom.
    let queued_input = String::from(
        "queued input preflight suffix long enough on its own to exhaust the remaining declared context headroom",
    );
    connection
        .request_version(
            ProtocolVersion::One,
            40,
            ClientRequest::SubmitInput {
                command_id: command()?,
                session_id,
                content: UserInputContent::text(queued_input),
                expected_defaults_version: Some(CanonicalU64::new(1)),
                model_settings: ModelSettingsOverlay::inherit_all(),
                delivery: None,
            },
        )
        .await?;
    let queued_turn = accepted_successor_turn(&mut connection, session_id, 2).await?;
    let configuration = support::parse_model_configuration(
        &MODEL_CONFIGURATION
            .replace("max_output_tokens = 256", "max_output_tokens = 1")
            .replace(
                "context_window_tokens = 200000",
                "context_window_tokens = 4096",
            ),
    )?;
    let runtime_models = configuration.runtime_model_catalog();
    let summary_text = String::from("queued input preflight summary");
    let summary_runtime = ScriptedModel::single(completed_script(
        "fixture-model",
        &summary_text,
        TokenUsage {
            input_tokens: Some(4000),
            output_tokens: Some(20),
            cache_creation_input_tokens: None,
            cache_read_input_tokens: None,
        },
    ));
    let summary_probe = summary_runtime.clone();
    let compaction_model: Arc<dyn signalbox_model_provider_runtime::ContextCompactionModel> =
        Arc::new(RuntimeContextCompactionModel::new(
            summary_runtime,
            runtime_models.clone(),
        ));
    let repository = PostgresModelCallRepository::new(
        runtime.pool.clone(),
        configuration.target_catalog(),
        ModelCallCredentialReference::new("queued-input-preflight-fixture"),
    )
    .with_session_credentials(configuration.credential_family_catalog());
    let compaction = ReportedUsageCompaction::new(
        StartEligibleTurnRepository::new(runtime.pool.clone()),
        repository,
        NoToolCatalog,
        runtime_models,
        configuration,
        compaction_model,
    );

    // No occupancy-recovery window surrounds this fixture's guard call, so
    // preparation has no window to name its compaction to. That is the case the
    // window's own contract states: a window that prepared nothing owes no
    // recovery. What this fixture exercises is the queued-turn headroom
    // arithmetic, not window-named recovery.
    compaction
        .compact_if_needed(SessionId::from_uuid(session_id.into_uuid()), None)
        .await?;

    assert_eq!(summary_probe.received_operations().len(), 1);
    let compaction_count: i64 = sqlx::query_scalar(
        "SELECT count(*)
           FROM context_compaction
          WHERE session_id = $1",
    )
    .bind(session_id.into_uuid())
    .fetch_one(&runtime.pool)
    .await?;
    assert_eq!(compaction_count, 1);
    let summary_count: i64 = sqlx::query_scalar(
        "SELECT count(*)
           FROM semantic_transcript_entry
          WHERE source_session_id = $1
            AND payload_kind = 'context_summary'
            AND context_summary_value = $2",
    )
    .bind(session_id.into_uuid())
    .bind(&summary_text)
    .fetch_one(&runtime.pool)
    .await?;
    assert_eq!(summary_count, 1);
    let lifecycle: (String, Option<String>) = sqlx::query_as(
        "SELECT state_kind, terminal_disposition_kind
           FROM turn_lifecycle
          WHERE session_id = $1 AND turn_id = $2",
    )
    .bind(session_id.into_uuid())
    .bind(queued_turn.into_uuid())
    .fetch_one(&runtime.pool)
    .await?;
    assert_eq!(lifecycle, (String::from("queued"), None));

    drop(connection);
    runtime.stop().await
}

/// S01 / S03 / INV-014 / INV-015: a failed automatic compaction closes the
/// queued candidate call-free, so a later eligibility pass cannot dispatch
/// the known-oversized ordinary request.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn s01_s03_inv014_inv015_failed_automatic_compaction_closes_turn_call_free()
-> Result<(), Box<dyn Error>> {
    let mut runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let session_id = create_alias_session(&mut connection).await?;
    let (_, first_turn) = submit_first_input(
        &mut connection,
        session_id,
        String::from("retry guard historical request"),
    )
    .await?;
    let first_runtime = ScriptedModel::single(completed_script(
        "fixture-model",
        "retry guard historical reply",
        TokenUsage::unreported(),
    ));
    let first_probe =
        execute_streamed_turn(&mut runtime, first_runtime, session_id, first_turn).await?;
    assert_eq!(first_probe.received_operations().len(), 1);

    let oversized_suffix = String::from("oversized suffix remains above the declared window");
    connection
        .request_version(
            ProtocolVersion::One,
            40,
            ClientRequest::SubmitInput {
                command_id: command()?,
                session_id,
                content: UserInputContent::text(oversized_suffix),
                expected_defaults_version: Some(CanonicalU64::new(1)),
                model_settings: ModelSettingsOverlay::inherit_all(),
                delivery: None,
            },
        )
        .await?;
    let queued_turn = accepted_successor_turn(&mut connection, session_id, 2).await?;
    let guarded_configuration = support::parse_model_configuration(
        &MODEL_CONFIGURATION
            .replace("max_output_tokens = 256", "max_output_tokens = 1")
            .replace(
                "context_window_tokens = 200000",
                "context_window_tokens = 4096",
            ),
    )?;
    let ordinary_runtime =
        RecordingCountedScriptedModel::following(std::iter::empty::<Script>(), [8192, 8192, 8192]);
    let ordinary_probe = ordinary_runtime.clone();
    let summary_runtime = ScriptedModel::single(Script::delivering(
        TerminalEvidence::ProviderError(ProviderErrorEvidence {
            exchange: ExchangeFacts::default(),
            reported_model: None,
            kind: ProviderErrorKind::Unrecognized,
            non_acceptance_proven: true,
            native: NativeErrorFacts::default(),
            usage: TokenUsage::unreported(),
        }),
    ));
    let summary_probe = summary_runtime.clone();
    let runtime_models = guarded_configuration.runtime_model_catalog();
    let provider = RuntimeModelCallProvider::new(ordinary_runtime, runtime_models.clone(), None)
        .with_text_delta_sink(runtime.provider_text_delta_sink());
    let counter = provider.clone();
    let repository = PostgresModelCallRepository::new(
        runtime.pool.clone(),
        guarded_configuration.target_catalog(),
        ModelCallCredentialReference::new("retry-guard-recording-fixture"),
    )
    .with_session_credentials(guarded_configuration.credential_family_catalog());
    let guarded_repository = repository.clone();
    let (execution, fatal_execution) =
        FatalExecutionSupervisor::new(signalboxd::WorkspaceInstructionPreparedExecution::new(
            PostgresProviderModelExecution::new(
                repository,
                InProcessAttemptDispatchGate::default(),
                provider,
                None,
            ),
            signalboxd::WorkspaceInstructionRuntime::new(runtime.pool.clone(), None, Vec::new()),
        ));
    let compaction_model: Arc<dyn signalbox_model_provider_runtime::ContextCompactionModel> =
        Arc::new(RuntimeContextCompactionModel::new(
            summary_runtime,
            runtime_models.clone(),
        ));
    // The parsed fixture, not an independent literal, is the authority on the
    // reference automatic compaction must reuse; it pins exactly one family.
    let pinned_references: Vec<String> = guarded_configuration
        .session_credential_pin()
        .credentials()
        .map(|credential| credential.credential_reference().to_owned())
        .collect();
    let expected_compaction_credential = exactly_one_credential_reference(&pinned_references);
    let mut pass = ContextGuardedTurnPass::new(
        StartEligibleTurnRepository::new(runtime.pool.clone()),
        guarded_repository,
        counter,
        NoToolCatalog,
        runtime_models,
        guarded_configuration,
        compaction_model,
        execution,
    )
    .with_workspace_instructions(signalboxd::WorkspaceInstructionRuntime::new(
        runtime.pool.clone(),
        None,
        Vec::new(),
    ));
    let session = SessionId::from_uuid(session_id.into_uuid());
    let turn = failed_automatic_compaction_turn(pass.run(session).await);
    assert_eq!(*turn.as_uuid(), queued_turn.into_uuid());
    let second_attempt = pass.run(session).await;
    assert!(second_attempt.is_ok());
    assert!(!fatal_execution.is_triggered());
    assert_eq!(ordinary_probe.counted_operations().len(), 1);
    assert_eq!(ordinary_probe.prepared_operations().len(), 0);
    assert_eq!(summary_probe.received_operations().len(), 1);
    assert_eq!(
        summary_probe.received_operations()[0]
            .credential_reference
            .as_str(),
        expected_compaction_credential
    );
    let compaction_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM context_compaction WHERE session_id = $1")
            .bind(session_id.into_uuid())
            .fetch_one(&runtime.pool)
            .await?;
    assert_eq!(compaction_count, 0);
    let automatic_command_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM compact_session_command
          WHERE session_id = $1 AND automatic_for_turn_id = $2",
    )
    .bind(session_id.into_uuid())
    .bind(queued_turn.into_uuid())
    .fetch_one(&runtime.pool)
    .await?;
    assert_eq!(automatic_command_count, 1);
    let compaction_call: (String, Option<String>) = sqlx::query_as(
        "SELECT state_kind, terminal_disposition_kind
           FROM context_compaction_model_call
          WHERE session_id = $1",
    )
    .bind(session_id.into_uuid())
    .fetch_one(&runtime.pool)
    .await?;
    assert_eq!(
        compaction_call,
        (String::from("terminal"), Some(String::from("known_failed")))
    );
    let ordinary_call_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM model_call WHERE turn_id = $1")
            .bind(queued_turn.into_uuid())
            .fetch_one(&runtime.pool)
            .await?;
    assert_eq!(ordinary_call_count, 0);
    let lifecycle: (String, Option<String>, Option<Uuid>) = sqlx::query_as(
        "SELECT state_kind, terminal_disposition_kind, terminal_model_call_id
           FROM turn_lifecycle
          WHERE session_id = $1 AND turn_id = $2",
    )
    .bind(session_id.into_uuid())
    .bind(queued_turn.into_uuid())
    .fetch_one(&runtime.pool)
    .await?;
    assert_eq!(
        lifecycle,
        (String::from("terminal"), Some(String::from("failed")), None)
    );

    drop(connection);
    runtime.stop().await
}

/// A classified guarded-pass failure whose durable commit outcome is unknown.
///
/// Every durable stage of `ContextGuardedTurnPass` can report
/// `OperatorFailureClass::Infrastructure { commit_ambiguous: true }`; the
/// counting seam is the one a fixture can drive without a provable database
/// commit failure, and the pass owes the same reported outcome to all of them.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CommitAmbiguousCountFailure;

impl std::fmt::Display for CommitAmbiguousCountFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("guarded count acknowledgement was lost")
    }
}

impl Error for CommitAmbiguousCountFailure {}

impl ClassifyOperatorFailure for CommitAmbiguousCountFailure {
    fn operator_failure_class(&self) -> OperatorFailureClass {
        OperatorFailureClass::Infrastructure {
            commit_ambiguous: true,
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct CommitAmbiguousCounter;

impl ModelCallInputTokenCounter for CommitAmbiguousCounter {
    type Error = CommitAmbiguousCountFailure;

    fn count_input_tokens<Cancellation>(
        &self,
        _operation: PreparedModelOperation,
        _cancellation: Cancellation,
    ) -> impl std::future::Future<Output = Result<ModelCallInputTokenCount, Self::Error>> + Send
    where
        Cancellation: std::future::Future<Output = ()> + Send + 'static,
    {
        std::future::ready(Err(CommitAmbiguousCountFailure))
    }
}

/// S03 / INV-034: the production guarded pass reports post-activation failure
/// for the declared ambiguous-commit class, so the daemon stops scheduling and
/// startup recovery regains authority over durable state whose outcome ordinary
/// scheduler retry cannot decide.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn s03_inv034_ambiguous_guarded_stage_raises_the_fatal_recovery_signal()
-> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let session_id = create_alias_session(&mut connection).await?;
    submit_first_input(
        &mut connection,
        session_id,
        String::from("ambiguous guarded stage request"),
    )
    .await?;
    let model_configuration = support::parse_model_configuration(MODEL_CONFIGURATION)?;
    let runtime_models = model_configuration.runtime_model_catalog();
    let provider = RuntimeModelCallProvider::new(
        ScriptedModel::<ModelCallId>::following(std::iter::empty::<Script>()),
        runtime_models.clone(),
        None,
    )
    .with_text_delta_sink(runtime.provider_text_delta_sink());
    let repository = PostgresModelCallRepository::new(
        runtime.pool.clone(),
        model_configuration.target_catalog(),
        ModelCallCredentialReference::new("ambiguous-guard-fixture"),
    );
    let guarded_repository = repository.clone();
    let (execution, fatal_execution) =
        FatalExecutionSupervisor::new(signalboxd::WorkspaceInstructionPreparedExecution::new(
            PostgresProviderModelExecution::new(
                repository,
                InProcessAttemptDispatchGate::default(),
                provider,
                None,
            ),
            signalboxd::WorkspaceInstructionRuntime::new(runtime.pool.clone(), None, Vec::new()),
        ));
    let compaction_model: Arc<dyn signalbox_model_provider_runtime::ContextCompactionModel> =
        Arc::new(RuntimeContextCompactionModel::new(
            ScriptedModel::<ModelCallId>::following(std::iter::empty::<Script>()),
            runtime_models.clone(),
        ));
    let mut pass = ContextGuardedTurnPass::new(
        StartEligibleTurnRepository::new(runtime.pool.clone()),
        guarded_repository,
        CommitAmbiguousCounter,
        NoToolCatalog,
        runtime_models,
        model_configuration,
        compaction_model,
        execution,
    )
    .with_workspace_instructions(signalboxd::WorkspaceInstructionRuntime::new(
        runtime.pool.clone(),
        None,
        Vec::new(),
    ));
    let session = SessionId::from_uuid(session_id.into_uuid());

    let outcome = pass.run(session).await;

    assert!(matches!(
        outcome,
        Err(ContextGuardedTurnPassError::Count {
            source: CommitAmbiguousCountFailure,
            ..
        })
    ));
    assert!(fatal_execution.is_triggered());

    drop(connection);
    runtime.stop().await
}
/// S03 / INV-012 / INV-015: a daemon-minted compaction result identity that
/// already names a durable record is reminted before the provider is called,
/// exactly as a colliding call identity already is. Discovering it in
/// `complete` instead would cost a paid summary and admit no remint, because
/// the in-flight lifecycle pins the identities by then. The rejected claim
/// rolls back so the reminting caller can reuse its user-global command.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn s03_inv012_inv015_taken_compaction_result_identities_remint_before_sending()
-> Result<(), Box<dyn Error>> {
    let mut runtime = RunningRuntime::start().await?;
    let (connection, session_id) = seed_completed_compaction_session(&mut runtime).await?;
    let repository = ContextCompactionRepository::new(runtime.pool.clone());
    let seeded = repository
        .prepare(direct_compaction_request(
            session_id,
            DurableCommandId::from_uuid(Uuid::from_u128(0xfa01)),
            None,
            0xfa10,
        ))
        .await?;
    let PrepareContextCompactionOutcome::Prepared(seeded) = seeded else {
        panic!("the result-identity fixture must prepare its first call");
    };
    repository.authorize(&seeded).await?;
    let applied = repository
        .complete(
            &seeded,
            "result identity fixture summary",
            ContextCompactionTokenUsage::unreported(),
        )
        .await?;

    let summary_command = DurableCommandId::from_uuid(Uuid::from_u128(0xfa02));
    let mut summary_collision =
        direct_compaction_request(session_id, summary_command, None, 0xfa20);
    summary_collision.summary_entry = applied.summary_entry;
    let summary_outcome = repository.prepare(summary_collision).await;
    let mut frontier_collision = direct_compaction_request(
        session_id,
        DurableCommandId::from_uuid(Uuid::from_u128(0xfa03)),
        None,
        0xfa30,
    );
    frontier_collision.result_frontier = applied.result_frontier;
    let frontier_outcome = repository.prepare(frontier_collision).await;
    let mut compaction_collision = direct_compaction_request(
        session_id,
        DurableCommandId::from_uuid(Uuid::from_u128(0xfa04)),
        None,
        0xfa40,
    );
    compaction_collision.compaction = applied.compaction;
    let compaction_outcome = repository.prepare(compaction_collision).await;

    assert!(matches!(
        summary_outcome,
        Err(ContextCompactionRepositoryError::IdentityCollision)
    ));
    assert!(matches!(
        frontier_outcome,
        Err(ContextCompactionRepositoryError::IdentityCollision)
    ));
    assert!(matches!(
        compaction_outcome,
        Err(ContextCompactionRepositoryError::IdentityCollision)
    ));
    let claimed: i64 =
        sqlx::query_scalar("SELECT count(*) FROM durable_command WHERE command_id = $1")
            .bind(summary_command.into_uuid())
            .fetch_one(&runtime.pool)
            .await?;
    assert_eq!(claimed, 0);
    let calls: i64 = sqlx::query_scalar(
        "SELECT count(*)
           FROM context_compaction_model_call
          WHERE session_id = $1 AND state_kind <> 'terminal'",
    )
    .bind(session_id.into_uuid())
    .fetch_one(&runtime.pool)
    .await?;
    assert_eq!(calls, 0);

    drop(connection);
    runtime.stop().await
}

/// S03 / INV-012 / INV-015: a result identity taken after preparation fails the
/// completion closed rather than surfacing as a retryable database failure.
///
/// `complete_context_compaction_until_resolved` retries exactly the database
/// and ambiguous-commit classes, so classifying this decided uniqueness
/// violation as either would resubmit the identical rejected statement forever
/// and block the session with no error surfaced. The call stays in flight for
/// startup recovery, which is the audited path for a durable record whose
/// executor stopped.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn s03_inv012_inv015_late_result_identity_collision_fails_completion_closed()
-> Result<(), Box<dyn Error>> {
    let mut runtime = RunningRuntime::start().await?;
    let (connection, session_id) = seed_completed_compaction_session(&mut runtime).await?;
    let repository = ContextCompactionRepository::new(runtime.pool.clone());
    let outcome = repository
        .prepare(direct_compaction_request(
            session_id,
            DurableCommandId::from_uuid(Uuid::from_u128(0xfb01)),
            None,
            0xfb10,
        ))
        .await?;
    let PrepareContextCompactionOutcome::Prepared(prepared) = outcome else {
        panic!("the late-collision fixture must prepare its call");
    };
    repository.authorize(&prepared).await?;
    sqlx::query(
        "INSERT INTO context_frontier
            (owning_session_id, context_frontier_id, member_count)
         VALUES ($1, $2, 0)",
    )
    .bind(session_id.into_uuid())
    .bind(prepared.result_frontier().into_uuid())
    .execute(&runtime.pool)
    .await?;

    let outcome = repository
        .complete(
            &prepared,
            "late collision fixture summary",
            ContextCompactionTokenUsage::unreported(),
        )
        .await;

    assert!(matches!(
        outcome,
        Err(ContextCompactionRepositoryError::Corruption(
            ContextCompactionCorruption::Inconsistent("compaction result identity")
        ))
    ));
    assert!(!matches!(
        outcome,
        Err(ContextCompactionRepositoryError::Database(_)
            | ContextCompactionRepositoryError::CommitAmbiguous(_))
    ));
    let call_state: String = sqlx::query_scalar(
        "SELECT state_kind FROM context_compaction_model_call WHERE model_call_id = $1",
    )
    .bind(prepared.call().into_uuid())
    .fetch_one(&runtime.pool)
    .await?;
    assert_eq!(call_state, "in_flight");

    drop(connection);
    runtime.stop().await
}

const REVIEW_IMPORT_TEMPLATE: &str = "review-import";
const REVIEW_JUDGMENT_TEMPLATE: &str = "review-judgment";
const REVIEW_REPAIR_TEMPLATE: &str = "review-repair";
const REVIEW_PUBLICATION_TEMPLATE: &str = "review-publication";
const REVIEW_CONCERN_SET_VERSION: &str = "initial-five-v1";

fn review_identity(value: u128) -> CanonicalUuid {
    CanonicalUuid::from_uuid(Uuid::from_u128(value))
}

fn review_concern_inputs() -> Vec<ReviewOrchestrationConcernInput> {
    vec![
        ReviewOrchestrationConcernInput {
            key: String::from("correctness"),
            template_name: String::from("review-concern-correctness"),
        },
        ReviewOrchestrationConcernInput {
            key: String::from("interface-and-type-design"),
            template_name: String::from("review-concern-interface-and-type-design"),
        },
        ReviewOrchestrationConcernInput {
            key: String::from("test-quality"),
            template_name: String::from("review-concern-test-quality"),
        },
        ReviewOrchestrationConcernInput {
            key: String::from("security"),
            template_name: String::from("review-concern-security"),
        },
        ReviewOrchestrationConcernInput {
            key: String::from("documentation-code-drift"),
            template_name: String::from("review-concern-documentation-code-drift"),
        },
    ]
}

#[derive(Clone, Copy)]
struct ReviewPassFixture {
    run: CanonicalUuid,
    pass: CanonicalUuid,
    turn: CanonicalUuid,
    frontier: CanonicalUuid,
}

#[derive(Clone, Copy)]
struct ReviewFindingFixtures {
    accepted_and_fixed: CanonicalUuid,
    duplicate: CanonicalUuid,
    accepted_and_published: CanonicalUuid,
}

struct ReviewConcernEvidence {
    key: String,
    pass: CanonicalUuid,
}

struct ReviewRuntimeDriver {
    connection: Connection,
    pool: PgPool,
    target: CanonicalUuid,
    next_request: u64,
}

impl ReviewRuntimeDriver {
    async fn connect(
        runtime: &RunningRuntime,
        target: CanonicalUuid,
    ) -> Result<Self, Box<dyn Error>> {
        sqlx::query(
            "CREATE TABLE test_rejected_review_orchestration_receipt (command_id uuid PRIMARY KEY)",
        )
        .execute(&runtime.pool)
        .await?;
        sqlx::query(
            "CREATE FUNCTION test_review_orchestration_receipt_allowed(candidate uuid)
             RETURNS boolean LANGUAGE sql
             RETURN NOT EXISTS (
                 SELECT 1 FROM test_rejected_review_orchestration_receipt
                  WHERE command_id = candidate
             )",
        )
        .execute(&runtime.pool)
        .await?;
        sqlx::query(
            "ALTER TABLE review_orchestration_command
             ADD CONSTRAINT test_reject_orchestration_receipt
             CHECK (test_review_orchestration_receipt_allowed(command_id))",
        )
        .execute(&runtime.pool)
        .await?;
        sqlx::query(
            "CREATE TABLE test_rejected_review_orchestration_recovery (command_id uuid PRIMARY KEY)",
        )
        .execute(&runtime.pool)
        .await?;
        sqlx::query(
            "CREATE FUNCTION test_review_orchestration_recovery_allowed(candidate uuid)
             RETURNS boolean LANGUAGE sql
             RETURN NOT EXISTS (
                 SELECT 1 FROM test_rejected_review_orchestration_recovery
                  WHERE command_id = candidate
             )",
        )
        .execute(&runtime.pool)
        .await?;
        sqlx::query(
            "ALTER TABLE review_orchestration_command_recovery
             ADD CONSTRAINT test_reject_orchestration_recovery
             CHECK (test_review_orchestration_recovery_allowed(command_id))",
        )
        .execute(&runtime.pool)
        .await?;
        Ok(Self {
            connection: Connection::connect(runtime.socket()).await?,
            pool: runtime.pool.clone(),
            target,
            next_request: 1,
        })
    }

    fn request_id(&mut self) -> u64 {
        let request = self.next_request;
        self.next_request += 1;
        request
    }

    async fn request_expect(
        &mut self,
        request: ClientRequest,
        expected: ServerMessage,
    ) -> Result<(), Box<dyn Error>> {
        let request_id = self.request_id();
        self.connection.request(request_id, request).await?;
        assert_eq!(
            response_within(&mut self.connection).await?.message(),
            &expected
        );
        Ok(())
    }

    async fn request_expect_after_lost_orchestration_receipt(
        &mut self,
        command_id: CommandId,
        request: ClientRequest,
        expected: ServerMessage,
    ) -> Result<(), Box<dyn Error>> {
        self.request_with_lost_orchestration_receipt(command_id, request.clone())
            .await?;
        self.request_expect(request, expected).await
    }

    async fn request_with_lost_orchestration_receipt(
        &mut self,
        command_id: CommandId,
        request: ClientRequest,
    ) -> Result<(), Box<dyn Error>> {
        sqlx::query(
            "INSERT INTO test_rejected_review_orchestration_receipt (command_id) VALUES ($1)",
        )
        .bind(command_id.into_uuid())
        .execute(&self.pool)
        .await?;
        let request_id = self.request_id();
        self.connection.request(request_id, request).await?;
        assert_eq!(
            protocol_error_code(response_within(&mut self.connection).await?.message()),
            ErrorCode::CommitAmbiguous,
        );
        let removed = sqlx::query(
            "DELETE FROM test_rejected_review_orchestration_receipt WHERE command_id = $1",
        )
        .bind(command_id.into_uuid())
        .execute(&self.pool)
        .await?;
        assert_eq!(removed.rows_affected(), 1);
        Ok(())
    }

    async fn request_with_lost_orchestration_recovery(
        &mut self,
        command_id: CommandId,
        request: ClientRequest,
    ) -> Result<(), Box<dyn Error>> {
        sqlx::query(
            "INSERT INTO test_rejected_review_orchestration_recovery (command_id) VALUES ($1)",
        )
        .bind(command_id.into_uuid())
        .execute(&self.pool)
        .await?;
        let request_id = self.request_id();
        self.connection.request(request_id, request).await?;
        assert_eq!(
            protocol_error_code(response_within(&mut self.connection).await?.message()),
            ErrorCode::CommitAmbiguous,
        );
        let removed = sqlx::query(
            "DELETE FROM test_rejected_review_orchestration_recovery WHERE command_id = $1",
        )
        .bind(command_id.into_uuid())
        .execute(&self.pool)
        .await?;
        assert_eq!(removed.rows_affected(), 1);
        Ok(())
    }

    async fn request_invalid(&mut self, request: ClientRequest) -> Result<(), Box<dyn Error>> {
        let request_id = self.request_id();
        self.connection.request(request_id, request).await?;
        assert_eq!(
            protocol_error_code(response_within(&mut self.connection).await?.message()),
            ErrorCode::InvalidRequest,
        );
        Ok(())
    }

    async fn create_target(&mut self) -> Result<(), Box<dyn Error>> {
        self.request_expect(
            ClientRequest::CreateReviewTarget {
                command_id: command()?,
                target_id: self.target,
                provider: String::from("github"),
                repository: String::from("keenwill/signalbox"),
                subject: ReviewTargetSubject::ChangeRequest {
                    number: CanonicalU64::new(343),
                },
                head_revision: String::from("reviewed-head-revision"),
                base_revision: Some(String::from("reviewed-base-revision")),
                stack_parent_target_id: None,
            },
            ServerMessage::ReviewTargetCreated {
                target_id: self.target,
            },
        )
        .await
    }

    async fn create_session_from_template(
        &mut self,
        template_name: &str,
    ) -> Result<CanonicalUuid, Box<dyn Error>> {
        let request_id = self.request_id();
        self.connection
            .request(
                request_id,
                ClientRequest::CreateSessionFromTemplate {
                    command_id: command()?,
                    template_name: String::from(template_name),
                    placement: SessionPlacement::Pathless {},
                    lifecycle: signalbox_process_protocol::SessionLifecycleMembers::default(),
                },
            )
            .await?;
        match response_within(&mut self.connection).await?.message() {
            ServerMessage::SessionCreated { session_id, .. } => Ok(*session_id),
            message => Err(io::Error::other(format!(
                "unexpected review-template session response: {message:?}"
            ))
            .into()),
        }
    }

    async fn submit_review_input(
        &mut self,
        session: CanonicalUuid,
        seed: u128,
    ) -> Result<(CanonicalUuid, CanonicalUuid), Box<dyn Error>> {
        let request_id = self.request_id();
        self.connection
            .request(
                request_id,
                ClientRequest::SubmitInput {
                    command_id: command()?,
                    session_id: session,
                    content: UserInputContent::text(format!("review pass fixture {seed}")),
                    expected_defaults_version: Some(CanonicalU64::new(1)),
                    model_settings: ModelSettingsOverlay::inherit_all(),
                    delivery: None,
                },
            )
            .await?;
        match response_within(&mut self.connection).await?.message() {
            ServerMessage::InputSubmitted {
                session_id,
                accepted_input_id,
                turn_id,
                ..
            } if *session_id == session => Ok((*accepted_input_id, *turn_id)),
            message => Err(io::Error::other(format!(
                "unexpected review-pass input response: {message:?}"
            ))
            .into()),
        }
    }

    async fn create_completed_turn_pass(
        &mut self,
        template_name: &str,
        workflow: ReviewWorkflow,
        seed: u128,
    ) -> Result<ReviewPassFixture, Box<dyn Error>> {
        let session = self.create_session_from_template(template_name).await?;
        let (accepted_input, turn) = self.submit_review_input(session, seed).await?;
        let run = review_identity(seed);
        let pass = review_identity(seed + 1);
        self.request_expect(
            ClientRequest::StartReviewRun {
                command_id: command()?,
                target_id: self.target,
                run_id: run,
                pass_id: pass,
                workflow,
                session_id: session,
                accepted_input_id: accepted_input,
            },
            ServerMessage::ReviewRunStarted {
                run_id: run,
                pass_id: pass,
            },
        )
        .await?;
        activate_turn(&self.pool, SessionId::from_uuid(session.into_uuid())).await?;
        self.request_expect(
            ClientRequest::ActivateReviewPass {
                command_id: command()?,
                run_id: run,
                pass_id: pass,
                turn_id: turn,
            },
            ServerMessage::ReviewPassActivated {
                run_id: run,
                pass_id: pass,
            },
        )
        .await?;
        let targets = support::parse_model_configuration(MODEL_CONFIGURATION)?.target_catalog();
        complete_active_text_turn(
            &self.pool,
            SessionId::from_uuid(session.into_uuid()),
            targets,
        )
        .await?;
        let frontier: Uuid = sqlx::query_scalar(
            "SELECT terminal_frontier_id FROM turn_lifecycle WHERE turn_id = $1",
        )
        .bind(turn.into_uuid())
        .fetch_one(&self.pool)
        .await?;
        Ok(ReviewPassFixture {
            run,
            pass,
            turn,
            frontier: CanonicalUuid::from_uuid(frontier),
        })
    }

    async fn reject_mismatched_pass_completion(
        &mut self,
        fixture: ReviewPassFixture,
    ) -> Result<(), Box<dyn Error>> {
        self.request_invalid(ClientRequest::CompleteReviewPass {
            command_id: command()?,
            run_id: fixture.run,
            pass_id: fixture.pass,
            turn_id: Some(fixture.turn),
            output_frontier_id: Some(review_identity(0xdead)),
            outcome: ReviewPassTerminalOutcome::Succeeded,
        })
        .await
    }

    async fn complete_result_free_pass(
        &mut self,
        fixture: ReviewPassFixture,
    ) -> Result<(), Box<dyn Error>> {
        self.request_expect(
            ClientRequest::CompleteReviewPass {
                command_id: command()?,
                run_id: fixture.run,
                pass_id: fixture.pass,
                turn_id: Some(fixture.turn),
                output_frontier_id: Some(fixture.frontier),
                outcome: ReviewPassTerminalOutcome::Succeeded,
            },
            ServerMessage::ReviewPassCompleted {
                run_id: fixture.run,
                pass_id: fixture.pass,
                state: signalbox_process_protocol::ReviewPassLifecycle::Succeeded,
            },
        )
        .await
    }

    async fn complete_failed_pass(
        &mut self,
        fixture: ReviewPassFixture,
    ) -> Result<(), Box<dyn Error>> {
        self.request_expect(
            ClientRequest::CompleteReviewPass {
                command_id: command()?,
                run_id: fixture.run,
                pass_id: fixture.pass,
                turn_id: Some(fixture.turn),
                output_frontier_id: None,
                outcome: ReviewPassTerminalOutcome::Failed,
            },
            ServerMessage::ReviewPassCompleted {
                run_id: fixture.run,
                pass_id: fixture.pass,
                state: signalbox_process_protocol::ReviewPassLifecycle::Failed,
            },
        )
        .await
    }

    async fn record_findings(
        &mut self,
        fixture: ReviewPassFixture,
        findings: Vec<ReviewFindingInput>,
    ) -> Result<(), Box<dyn Error>> {
        let finding_count = CanonicalU64::new(u64::try_from(findings.len())?);
        self.request_expect(
            ClientRequest::RecordReviewFindings {
                command_id: command()?,
                run_id: fixture.run,
                pass_id: fixture.pass,
                turn_id: fixture.turn,
                output_frontier_id: fixture.frontier,
                findings,
            },
            ServerMessage::ReviewFindingsRecorded {
                run_id: fixture.run,
                pass_id: fixture.pass,
                finding_count,
            },
        )
        .await
    }

    async fn record_finding_event(
        &mut self,
        fixture: ReviewPassFixture,
        finding: CanonicalUuid,
        ordinal: u64,
        event: ReviewFindingEvent,
        expected_status: ReviewFindingStatus,
    ) -> Result<(), Box<dyn Error>> {
        self.request_expect(
            ClientRequest::RecordReviewFindingEvent {
                command_id: command()?,
                run_id: fixture.run,
                pass_id: fixture.pass,
                turn_id: fixture.turn,
                output_frontier_id: Some(fixture.frontier),
                finding_id: finding,
                event_ordinal: CanonicalU64::new(ordinal),
                event,
            },
            ServerMessage::ReviewFindingEventRecorded {
                finding_id: finding,
                status: expected_status,
            },
        )
        .await
    }

    fn start_attempt_request(
        &self,
        command_id: CommandId,
        attempt: CanonicalUuid,
    ) -> ClientRequest {
        ClientRequest::StartReviewOrchestration {
            command_id,
            attempt_id: attempt,
            target_id: self.target,
            concern_set_version: String::from(REVIEW_CONCERN_SET_VERSION),
            import_template_name: String::from(REVIEW_IMPORT_TEMPLATE),
            judgment_template_name: String::from(REVIEW_JUDGMENT_TEMPLATE),
            repair_template_name: String::from(REVIEW_REPAIR_TEMPLATE),
            publication_template_name: String::from(REVIEW_PUBLICATION_TEMPLATE),
            concerns: review_concern_inputs(),
        }
    }

    async fn start_attempt(&mut self, attempt: CanonicalUuid) -> Result<(), Box<dyn Error>> {
        let command_id = command()?;
        let request = self.start_attempt_request(command_id, attempt);
        self.request_expect_after_lost_orchestration_receipt(
            command_id,
            request,
            ServerMessage::ReviewOrchestrationStarted {
                attempt_id: attempt,
            },
        )
        .await
    }

    async fn reject_result_free_read_only_success(
        &mut self,
        fixture: ReviewPassFixture,
    ) -> Result<(), Box<dyn Error>> {
        self.request_invalid(ClientRequest::CompleteReviewPass {
            command_id: command()?,
            run_id: fixture.run,
            pass_id: fixture.pass,
            turn_id: Some(fixture.turn),
            output_frontier_id: Some(fixture.frontier),
            outcome: ReviewPassTerminalOutcome::Succeeded,
        })
        .await
    }

    async fn reject_restart_after_import(
        &mut self,
        attempt: CanonicalUuid,
    ) -> Result<(), Box<dyn Error>> {
        let request = self.start_attempt_request(command()?, attempt);
        self.request_invalid(request).await
    }

    async fn record_import(
        &mut self,
        attempt: CanonicalUuid,
        pass: CanonicalUuid,
    ) -> Result<(), Box<dyn Error>> {
        let command_id = command()?;
        self.request_expect_after_lost_orchestration_receipt(
            command_id,
            ClientRequest::RecordReviewImportOutcome {
                command_id,
                attempt_id: attempt,
                pass_id: Some(pass),
                external_link_id: None,
                context_digest: Some(CanonicalDigest::try_new("11".repeat(32))?),
                outcome: ReviewImportTerminalOutcome::Succeeded,
            },
            ServerMessage::ReviewOrchestrationAdvanced {
                attempt_id: attempt,
                state: ReviewOrchestrationState::AwaitingConcerns,
            },
        )
        .await
    }

    async fn record_import_with_lost_recovery(
        &mut self,
        attempt: CanonicalUuid,
        pass: CanonicalUuid,
    ) -> Result<ClientRequest, Box<dyn Error>> {
        let command_id = command()?;
        let request = ClientRequest::RecordReviewImportOutcome {
            command_id,
            attempt_id: attempt,
            pass_id: Some(pass),
            external_link_id: None,
            context_digest: Some(CanonicalDigest::try_new("11".repeat(32))?),
            outcome: ReviewImportTerminalOutcome::Succeeded,
        };
        self.request_with_lost_orchestration_recovery(command_id, request.clone())
            .await?;
        Ok(request)
    }

    async fn reject_fresh_import_after_progress(
        &mut self,
        attempt: CanonicalUuid,
        pass: CanonicalUuid,
    ) -> Result<(), Box<dyn Error>> {
        let request = ClientRequest::RecordReviewImportOutcome {
            command_id: command()?,
            attempt_id: attempt,
            pass_id: Some(pass),
            external_link_id: None,
            context_digest: Some(CanonicalDigest::try_new("11".repeat(32))?),
            outcome: ReviewImportTerminalOutcome::Succeeded,
        };
        self.request_invalid(request.clone()).await?;
        self.request_invalid(request).await
    }

    async fn record_concern(
        &mut self,
        attempt: CanonicalUuid,
        concern: &ReviewConcernEvidence,
        expected_state: ReviewOrchestrationState,
    ) -> Result<(), Box<dyn Error>> {
        let command_id = command()?;
        self.request_expect_after_lost_orchestration_receipt(
            command_id,
            ClientRequest::RecordReviewConcernOutcome {
                command_id,
                attempt_id: attempt,
                concern: concern.key.clone(),
                pass_id: Some(concern.pass),
                outcome: ReviewConcernTerminalOutcome::Succeeded,
            },
            ServerMessage::ReviewOrchestrationAdvanced {
                attempt_id: attempt,
                state: expected_state,
            },
        )
        .await
    }

    async fn record_failed_concern_with_lost_recovery(
        &mut self,
        attempt: CanonicalUuid,
        concern: &ReviewConcernEvidence,
        failed_pass: CanonicalUuid,
    ) -> Result<ClientRequest, Box<dyn Error>> {
        let command_id = command()?;
        let request = ClientRequest::RecordReviewConcernOutcome {
            command_id,
            attempt_id: attempt,
            concern: concern.key.clone(),
            pass_id: Some(failed_pass),
            outcome: ReviewConcernTerminalOutcome::Failed,
        };
        self.request_with_lost_orchestration_recovery(command_id, request.clone())
            .await?;
        Ok(request)
    }

    async fn record_concerns_after_first(
        &mut self,
        attempt: CanonicalUuid,
        concerns: &[ReviewConcernEvidence],
    ) -> Result<(), Box<dyn Error>> {
        self.record_concern(
            attempt,
            &concerns[1],
            ReviewOrchestrationState::AwaitingConcerns,
        )
        .await?;
        self.record_concern(
            attempt,
            &concerns[2],
            ReviewOrchestrationState::AwaitingConcerns,
        )
        .await?;
        self.record_concern(
            attempt,
            &concerns[3],
            ReviewOrchestrationState::AwaitingConcerns,
        )
        .await?;
        self.record_concern(
            attempt,
            &concerns[4],
            ReviewOrchestrationState::AwaitingJudgment,
        )
        .await
    }

    async fn record_complete_concerns(
        &mut self,
        attempt: CanonicalUuid,
        concerns: &[ReviewConcernEvidence],
    ) -> Result<(), Box<dyn Error>> {
        self.record_concern(
            attempt,
            &concerns[0],
            ReviewOrchestrationState::AwaitingConcerns,
        )
        .await?;
        self.record_concern(
            attempt,
            &concerns[1],
            ReviewOrchestrationState::AwaitingConcerns,
        )
        .await?;
        self.record_concern(
            attempt,
            &concerns[2],
            ReviewOrchestrationState::AwaitingConcerns,
        )
        .await?;
        self.record_concern(
            attempt,
            &concerns[3],
            ReviewOrchestrationState::AwaitingConcerns,
        )
        .await?;
        self.record_concern(
            attempt,
            &concerns[4],
            ReviewOrchestrationState::AwaitingJudgment,
        )
        .await
    }

    async fn record_judgment_plan(
        &mut self,
        attempt: CanonicalUuid,
        analysis_pass: CanonicalUuid,
        members: Vec<ReviewJudgmentPlanMember>,
    ) -> Result<(), Box<dyn Error>> {
        let command_id = command()?;
        self.request_expect_after_lost_orchestration_receipt(
            command_id,
            ClientRequest::RecordReviewJudgmentPlan {
                command_id,
                attempt_id: attempt,
                analysis_pass_id: analysis_pass,
                members,
            },
            ServerMessage::ReviewOrchestrationAdvanced {
                attempt_id: attempt,
                state: ReviewOrchestrationState::AwaitingJudgmentEffects,
            },
        )
        .await
    }

    async fn record_effect(
        &mut self,
        attempt: CanonicalUuid,
        finding: CanonicalUuid,
        pass: CanonicalUuid,
        expected_state: ReviewOrchestrationState,
    ) -> Result<(), Box<dyn Error>> {
        let command_id = command()?;
        self.request_expect_after_lost_orchestration_receipt(
            command_id,
            ClientRequest::RecordReviewJudgmentEffect {
                command_id,
                attempt_id: attempt,
                finding_id: finding,
                event_pass_id: Some(pass),
                outcome: ReviewJudgmentEffectTerminalOutcome::Applied,
            },
            ServerMessage::ReviewOrchestrationAdvanced {
                attempt_id: attempt,
                state: expected_state,
            },
        )
        .await
    }
}

fn review_finding(finding_id: CanonicalUuid, title: &str, category: &str) -> ReviewFindingInput {
    ReviewFindingInput {
        finding_id,
        file_path: String::from("apps/signalboxd/src/process_runtime.rs"),
        line_start: Some(CanonicalU64::new(1)),
        line_end: Some(CanonicalU64::new(1)),
        diff_side: Some(ReviewDiffSide::Right),
        title: String::from(title),
        body: String::from("The fixture supplies concrete repository evidence."),
        severity: ReviewSeverity::Medium,
        is_real_confidence: CanonicalU64::new(9_200),
        severity_label_confidence: CanonicalU64::new(8_000),
        category: String::from(category),
        recommended_fix: Some(String::from("Apply the bounded fixture repair.")),
    }
}

fn complete_judgment_members(findings: ReviewFindingFixtures) -> Vec<ReviewJudgmentPlanMember> {
    vec![
        ReviewJudgmentPlanMember {
            finding_id: findings.accepted_and_fixed,
            disposition: ReviewJudgmentDisposition::Accepted {},
        },
        ReviewJudgmentPlanMember {
            finding_id: findings.duplicate,
            disposition: ReviewJudgmentDisposition::Duplicate {
                canonical_finding_id: findings.accepted_and_fixed,
            },
        },
        ReviewJudgmentPlanMember {
            finding_id: findings.accepted_and_published,
            disposition: ReviewJudgmentDisposition::Accepted {},
        },
    ]
}

fn direct_cycle_members(findings: ReviewFindingFixtures) -> Vec<ReviewJudgmentPlanMember> {
    vec![
        ReviewJudgmentPlanMember {
            finding_id: findings.accepted_and_fixed,
            disposition: ReviewJudgmentDisposition::Duplicate {
                canonical_finding_id: findings.duplicate,
            },
        },
        ReviewJudgmentPlanMember {
            finding_id: findings.duplicate,
            disposition: ReviewJudgmentDisposition::Duplicate {
                canonical_finding_id: findings.accepted_and_fixed,
            },
        },
        ReviewJudgmentPlanMember {
            finding_id: findings.accepted_and_published,
            disposition: ReviewJudgmentDisposition::Accepted {},
        },
    ]
}

fn transitive_cycle_members(findings: ReviewFindingFixtures) -> Vec<ReviewJudgmentPlanMember> {
    vec![
        ReviewJudgmentPlanMember {
            finding_id: findings.accepted_and_fixed,
            disposition: ReviewJudgmentDisposition::Duplicate {
                canonical_finding_id: findings.duplicate,
            },
        },
        ReviewJudgmentPlanMember {
            finding_id: findings.duplicate,
            disposition: ReviewJudgmentDisposition::Duplicate {
                canonical_finding_id: findings.accepted_and_published,
            },
        },
        ReviewJudgmentPlanMember {
            finding_id: findings.accepted_and_published,
            disposition: ReviewJudgmentDisposition::Duplicate {
                canonical_finding_id: findings.accepted_and_fixed,
            },
        },
    ]
}

#[derive(Clone, Copy)]
enum PlanRejectionFanout {
    Complete,
    FirstConcernOnly,
}

async fn prove_orchestration_plan_rejection(
    driver: &mut ReviewRuntimeDriver,
    attempt: CanonicalUuid,
    import_pass: CanonicalUuid,
    concerns: &[ReviewConcernEvidence],
    analysis_pass: CanonicalUuid,
    members: Vec<ReviewJudgmentPlanMember>,
    fanout: PlanRejectionFanout,
) -> Result<(), Box<dyn Error>> {
    driver.start_attempt(attempt).await?;
    driver.record_import(attempt, import_pass).await?;
    match fanout {
        PlanRejectionFanout::Complete => {
            driver.record_complete_concerns(attempt, concerns).await?;
        }
        PlanRejectionFanout::FirstConcernOnly => {
            driver
                .record_concern(
                    attempt,
                    &concerns[0],
                    ReviewOrchestrationState::AwaitingConcerns,
                )
                .await?;
        }
    }
    driver
        .request_invalid(ClientRequest::RecordReviewJudgmentPlan {
            command_id: command()?,
            attempt_id: attempt,
            analysis_pass_id: analysis_pass,
            members,
        })
        .await
}

fn assert_complete_review_snapshot(
    snapshot: &ReviewOrchestrationSnapshot,
    attempt: CanonicalUuid,
    expected_counts: ReviewOrchestrationCounts,
    target: CanonicalUuid,
    concerns: &[ReviewConcernEvidence],
) {
    assert_eq!(snapshot.attempt_id, attempt);
    assert_eq!(snapshot.target_id, target);
    assert_eq!(snapshot.state, ReviewOrchestrationState::Complete);
    assert_eq!(snapshot.concern_set_version, REVIEW_CONCERN_SET_VERSION);
    assert_eq!(snapshot.concerns.len(), concerns.len());
    assert_eq!(snapshot.concerns[0].key, concerns[0].key);
    assert_eq!(
        snapshot.concerns[0].status,
        ReviewOrchestrationConcernStatus::Succeeded
    );
    assert_eq!(snapshot.concerns[0].pass_id, Some(concerns[0].pass));
    assert_eq!(snapshot.concerns[1].key, concerns[1].key);
    assert_eq!(
        snapshot.concerns[1].status,
        ReviewOrchestrationConcernStatus::Succeeded
    );
    assert_eq!(snapshot.concerns[1].pass_id, Some(concerns[1].pass));
    assert_eq!(snapshot.concerns[2].key, concerns[2].key);
    assert_eq!(
        snapshot.concerns[2].status,
        ReviewOrchestrationConcernStatus::Succeeded
    );
    assert_eq!(snapshot.concerns[2].pass_id, Some(concerns[2].pass));
    assert_eq!(snapshot.concerns[3].key, concerns[3].key);
    assert_eq!(
        snapshot.concerns[3].status,
        ReviewOrchestrationConcernStatus::Succeeded
    );
    assert_eq!(snapshot.concerns[3].pass_id, Some(concerns[3].pass));
    assert_eq!(snapshot.concerns[4].key, concerns[4].key);
    assert_eq!(
        snapshot.concerns[4].status,
        ReviewOrchestrationConcernStatus::Succeeded
    );
    assert_eq!(snapshot.concerns[4].pass_id, Some(concerns[4].pass));
    assert_eq!(snapshot.counts, expected_counts);
}

async fn read_complete_review_snapshot(
    driver: &mut ReviewRuntimeDriver,
    attempt: CanonicalUuid,
    expected_counts: ReviewOrchestrationCounts,
    concerns: &[ReviewConcernEvidence],
) -> Result<(), Box<dyn Error>> {
    let request_id = driver.request_id();
    driver
        .connection
        .request(
            request_id,
            ClientRequest::ReadReviewOrchestration {
                attempt_id: attempt,
            },
        )
        .await?;
    match response_within(&mut driver.connection).await?.message() {
        ServerMessage::ReviewOrchestration { snapshot } => {
            assert_complete_review_snapshot(
                snapshot,
                attempt,
                expected_counts,
                driver.target,
                concerns,
            );
            Ok(())
        }
        message => Err(io::Error::other(format!(
            "unexpected complete review-orchestration snapshot: {message:?}"
        ))
        .into()),
    }
}

async fn drive_review_orchestration_process_loop() -> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let target = review_identity(0xa000);
    let attempt = review_identity(0xa100);
    let findings = ReviewFindingFixtures {
        accepted_and_fixed: review_identity(0xb001),
        duplicate: review_identity(0xb002),
        accepted_and_published: review_identity(0xb003),
    };
    let expected_counts = ReviewOrchestrationCounts {
        finding_count: CanonicalU64::new(3),
        judgment_member_count: CanonicalU64::new(3),
        judgment_effect_applied_count: CanonicalU64::new(3),
        repair_fixed_count: CanonicalU64::new(1),
        publication_published_count: CanonicalU64::new(1),
    };
    let mut driver = ReviewRuntimeDriver::connect(&runtime, target).await?;
    driver.create_target().await?;
    driver.start_attempt(attempt).await?;

    let import = driver
        .create_completed_turn_pass(
            REVIEW_IMPORT_TEMPLATE,
            ReviewWorkflow::ImportExternalContext,
            0xc000,
        )
        .await?;
    driver.reject_mismatched_pass_completion(import).await?;
    driver.complete_result_free_pass(import).await?;
    let import_retry = driver
        .record_import_with_lost_recovery(attempt, import.pass)
        .await?;
    driver.reject_restart_after_import(attempt).await?;

    let failed_correctness = driver
        .create_completed_turn_pass(
            "review-concern-correctness",
            ReviewWorkflow::ReadOnlyReview,
            0xc050,
        )
        .await?;
    driver.complete_failed_pass(failed_correctness).await?;

    let correctness = driver
        .create_completed_turn_pass(
            "review-concern-correctness",
            ReviewWorkflow::ReadOnlyReview,
            0xc100,
        )
        .await?;
    driver
        .reject_result_free_read_only_success(correctness)
        .await?;
    driver
        .record_findings(
            correctness,
            vec![review_finding(
                findings.accepted_and_fixed,
                "Accepted repair",
                "correctness",
            )],
        )
        .await?;
    let interface = driver
        .create_completed_turn_pass(
            "review-concern-interface-and-type-design",
            ReviewWorkflow::ReadOnlyReview,
            0xc200,
        )
        .await?;
    driver
        .record_findings(
            interface,
            vec![review_finding(
                findings.duplicate,
                "Cross-concern duplicate",
                "interface-and-type-design",
            )],
        )
        .await?;
    let tests = driver
        .create_completed_turn_pass(
            "review-concern-test-quality",
            ReviewWorkflow::ReadOnlyReview,
            0xc300,
        )
        .await?;
    driver
        .record_findings(
            tests,
            vec![review_finding(
                findings.accepted_and_published,
                "Accepted publication",
                "test-quality",
            )],
        )
        .await?;
    let security = driver
        .create_completed_turn_pass(
            "review-concern-security",
            ReviewWorkflow::ReadOnlyReview,
            0xc400,
        )
        .await?;
    driver.record_findings(security, Vec::new()).await?;
    let documentation = driver
        .create_completed_turn_pass(
            "review-concern-documentation-code-drift",
            ReviewWorkflow::ReadOnlyReview,
            0xc500,
        )
        .await?;
    driver.record_findings(documentation, Vec::new()).await?;
    let concerns = vec![
        ReviewConcernEvidence {
            key: String::from("correctness"),
            pass: correctness.pass,
        },
        ReviewConcernEvidence {
            key: String::from("interface-and-type-design"),
            pass: interface.pass,
        },
        ReviewConcernEvidence {
            key: String::from("test-quality"),
            pass: tests.pass,
        },
        ReviewConcernEvidence {
            key: String::from("security"),
            pass: security.pass,
        },
        ReviewConcernEvidence {
            key: String::from("documentation-code-drift"),
            pass: documentation.pass,
        },
    ];
    let first_concern_retry = driver
        .record_failed_concern_with_lost_recovery(attempt, &concerns[0], failed_correctness.pass)
        .await?;
    driver
        .record_concern(
            attempt,
            &concerns[0],
            ReviewOrchestrationState::AwaitingConcerns,
        )
        .await?;
    driver
        .record_concerns_after_first(attempt, &concerns)
        .await?;
    driver
        .request_expect(
            import_retry,
            ServerMessage::ReviewOrchestrationAdvanced {
                attempt_id: attempt,
                state: ReviewOrchestrationState::AwaitingConcerns,
            },
        )
        .await?;
    driver
        .request_expect(
            first_concern_retry,
            ServerMessage::ReviewOrchestrationAdvanced {
                attempt_id: attempt,
                state: ReviewOrchestrationState::AwaitingConcerns,
            },
        )
        .await?;
    driver
        .reject_fresh_import_after_progress(attempt, import.pass)
        .await?;

    let analysis = driver
        .create_completed_turn_pass(
            REVIEW_JUDGMENT_TEMPLATE,
            ReviewWorkflow::JudgeFindings,
            0xc600,
        )
        .await?;
    driver.complete_result_free_pass(analysis).await?;
    driver
        .record_judgment_plan(attempt, analysis.pass, complete_judgment_members(findings))
        .await?;

    prove_orchestration_plan_rejection(
        &mut driver,
        review_identity(0xa200),
        import.pass,
        &concerns,
        analysis.pass,
        complete_judgment_members(findings),
        PlanRejectionFanout::FirstConcernOnly,
    )
    .await?;
    prove_orchestration_plan_rejection(
        &mut driver,
        review_identity(0xa300),
        import.pass,
        &concerns,
        analysis.pass,
        direct_cycle_members(findings),
        PlanRejectionFanout::Complete,
    )
    .await?;
    prove_orchestration_plan_rejection(
        &mut driver,
        review_identity(0xa400),
        import.pass,
        &concerns,
        analysis.pass,
        transitive_cycle_members(findings),
        PlanRejectionFanout::Complete,
    )
    .await?;

    let accepted_fixed = driver
        .create_completed_turn_pass(
            REVIEW_JUDGMENT_TEMPLATE,
            ReviewWorkflow::JudgeFindings,
            0xc700,
        )
        .await?;
    driver
        .record_finding_event(
            accepted_fixed,
            findings.accepted_and_fixed,
            1,
            ReviewFindingEvent::Accepted {},
            ReviewFindingStatus::Accepted,
        )
        .await?;
    driver
        .record_effect(
            attempt,
            findings.accepted_and_fixed,
            accepted_fixed.pass,
            ReviewOrchestrationState::AwaitingJudgmentEffects,
        )
        .await?;
    let duplicate = driver
        .create_completed_turn_pass(
            REVIEW_JUDGMENT_TEMPLATE,
            ReviewWorkflow::DedupeFindings,
            0xc800,
        )
        .await?;
    driver
        .record_finding_event(
            duplicate,
            findings.duplicate,
            1,
            ReviewFindingEvent::Duplicate {
                canonical_finding_id: findings.accepted_and_fixed,
            },
            ReviewFindingStatus::Duplicate,
        )
        .await?;
    driver
        .record_effect(
            attempt,
            findings.duplicate,
            duplicate.pass,
            ReviewOrchestrationState::AwaitingJudgmentEffects,
        )
        .await?;
    let accepted_published = driver
        .create_completed_turn_pass(
            REVIEW_JUDGMENT_TEMPLATE,
            ReviewWorkflow::JudgeFindings,
            0xc900,
        )
        .await?;
    driver
        .record_finding_event(
            accepted_published,
            findings.accepted_and_published,
            1,
            ReviewFindingEvent::Accepted {},
            ReviewFindingStatus::Accepted,
        )
        .await?;
    driver
        .record_effect(
            attempt,
            findings.accepted_and_published,
            accepted_published.pass,
            ReviewOrchestrationState::AwaitingRepair,
        )
        .await?;

    let repair = driver
        .create_completed_turn_pass(REVIEW_REPAIR_TEMPLATE, ReviewWorkflow::FixFindings, 0xca00)
        .await?;
    driver
        .record_finding_event(
            repair,
            findings.accepted_and_fixed,
            2,
            ReviewFindingEvent::Fixed {},
            ReviewFindingStatus::Fixed,
        )
        .await?;
    let repair_command = command()?;
    let repair_request = ClientRequest::RecordReviewRepairOutcomes {
        command_id: repair_command,
        attempt_id: attempt,
        outcomes: vec![
            ReviewRepairOutcome {
                finding_id: findings.accepted_and_fixed,
                event_pass_id: Some(repair.pass),
                outcome: ReviewRepairTerminalOutcome::Fixed,
            },
            ReviewRepairOutcome {
                finding_id: findings.accepted_and_published,
                event_pass_id: None,
                outcome: ReviewRepairTerminalOutcome::Failed,
            },
        ],
    };
    driver
        .request_with_lost_orchestration_receipt(repair_command, repair_request.clone())
        .await?;

    let external_link = review_identity(0xd000);
    driver
        .request_expect(
            ClientRequest::ReserveReviewExternalLink {
                command_id: command()?,
                external_link_id: external_link,
                finding_id: findings.accepted_and_published,
                provider: String::from("github"),
                object_kind: ReviewExternalObjectKind::ReviewComment,
            },
            ServerMessage::ReviewExternalLinkReserved {
                external_link_id: external_link,
            },
        )
        .await?;
    let publication = driver
        .create_completed_turn_pass(
            REVIEW_PUBLICATION_TEMPLATE,
            ReviewWorkflow::PublishReview,
            0xcb00,
        )
        .await?;
    driver
        .request_expect(
            ClientRequest::AttachReviewExternalLink {
                command_id: command()?,
                external_link_id: external_link,
                run_id: publication.run,
                pass_id: publication.pass,
                turn_id: publication.turn,
                output_frontier_id: publication.frontier,
                external_object: String::from("provider-comment-1"),
                event_ordinal: CanonicalU64::new(2),
            },
            ServerMessage::ReviewExternalLinkAttached {
                external_link_id: external_link,
                external_object: String::from("provider-comment-1"),
            },
        )
        .await?;
    let publication_command = command()?;
    driver
        .request_expect_after_lost_orchestration_receipt(
            publication_command,
            ClientRequest::RecordReviewPublicationOutcomes {
                command_id: publication_command,
                attempt_id: attempt,
                outcomes: vec![ReviewPublicationOutcome {
                    finding_id: findings.accepted_and_published,
                    external_link_id: Some(external_link),
                    outcome: ReviewPublicationTerminalOutcome::Published,
                }],
            },
            ServerMessage::ReviewOrchestrationAdvanced {
                attempt_id: attempt,
                state: ReviewOrchestrationState::Complete,
            },
        )
        .await?;
    driver
        .request_expect(
            repair_request,
            ServerMessage::ReviewOrchestrationAdvanced {
                attempt_id: attempt,
                state: ReviewOrchestrationState::AwaitingPublication,
            },
        )
        .await?;
    read_complete_review_snapshot(&mut driver, attempt, expected_counts, &concerns).await?;

    drop(driver);
    runtime.stop().await
}

/// One process client can drive the frozen five-concern review library through
/// its structural fan-out barrier, cross-concern deduplication, repair, and
/// reservation-backed publication against the real PostgreSQL adapters.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn review_orchestration_reaches_complete_through_the_process_protocol()
-> Result<(), Box<dyn Error>> {
    drive_review_orchestration_process_loop().await
}
