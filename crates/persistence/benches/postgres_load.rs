//! On-demand PostgreSQL saturation harness.

use std::{
    env,
    error::Error,
    fmt,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use signalbox_application::{
    AuthorizeModelCallOutcome, ModelCallCredentialReference, StartEligibleTurnOutcome,
    discover_workspace_instructions,
};
use signalbox_domain::{
    AcceptedInputId, AcceptedInputTurnActivationIdentities, AssistantResponsePart,
    CancelledModelCallTurnIdentities, ContextFrontierId, CreateSession, CurrentToolAttempt,
    CurrentToolAttemptState, DecideToolRequest, DirectModelSelection, DurableCommandId,
    FailedModelCallTurnIdentities, InitialToolApproval, InstructionBundleId,
    InstructionDiscoveryId, ModelCallId, ModelCallTerminalIdentities, ModelCallTerminalObservation,
    ModelCallTerminalOutcome, ModelSelectionOverride, ModelSelectionRequest, ModelTargetCatalog,
    ModelTargetDefinition, NormalizedToolArguments, PerInputConfigurationChoices,
    ProviderModelIdentity, ReconstitutedToolAttempt, ResolvedProviderTarget,
    SemanticTranscriptEntryId, SessionConfigurationDefaults, SessionConfigurationDefaultsVersion,
    SessionCreationCause, SessionCreationProvenance, SessionId, SubmitInput,
    SubmitInputAppliedResult, SubmitInputResult, ToolApprovalDecision, ToolAttemptEnd,
    ToolAttemptId, ToolAttemptObservation, ToolCallProposal, ToolEffectClass, ToolName,
    ToolRequestId, ToolResponsePartIdentity, ToolResultContent, ToolResultText,
    ToolRoundModelCallIdentities, ToolUsingAssistantResponse, TranscriptAncestry, TurnAttemptId,
    TurnId, TurnInstructionManifest, TurnInstructionManifestId, UserContent,
};
use signalbox_persistence::{
    DISPOSABLE_TEST_CONTAINER_LIFETIME_HOURS,
    create_session::{CreateSessionHandlingOutcome, CreateSessionRepository},
    disposable_test_container_labels, local_test_connection_options, migrate,
    model_execution::{PostgresModelCallRepository, PrepareInitialModelCallOutcome},
    outlives_the_disposable_container_sweep,
    start_eligible_turn::StartEligibleTurnRepository,
    submit_input::{SubmitInputHandlingOutcome, SubmitInputRepository},
    tool_loop::PostgresToolLoopRepository,
    workspace_instructions::{
        RecordTurnInstructionSnapshotOutcome, WorkspaceInstructionRepository,
    },
};
use sqlx::{PgPool, postgres::PgPoolOptions, types::Uuid};
use testcontainers_modules::{
    postgres::Postgres,
    testcontainers::{ContainerAsync, ImageExt, runners::AsyncRunner},
};
use tokio::{sync::Barrier, task::JoinSet, time::timeout};

const POSTGRES_IMAGE_TAG: &str = "18.4-alpine3.23";
const DATABASE_USER: &str = "signalbox";
const DATABASE_PASSWORD: &str = "signalbox-benchmark-only";

fn test_session_credential_pin() -> HarnessResult<signalbox_persistence::SessionCredentialPin> {
    signalbox_persistence::SessionCredentialPin::try_new(vec![
        signalbox_persistence::SessionModelCredential::new(
            "test-model-family",
            "test-model-primary",
        ),
    ])
    .map_err(|error| {
        std::io::Error::other(format!("invalid benchmark credential pin: {error:?}")).into()
    })
}
const ADMIN_DATABASE: &str = "signalbox_benchmark";
const DEFAULT_DURATION_SECONDS: u64 = 60;
const DEFAULT_POOL_SIZE: u32 = 80;
const DEFAULT_CONCURRENCIES: [usize; 7] = [1, 2, 4, 8, 16, 32, 64];
const SERVER_CONNECTION_HEADROOM: u32 = 4;
const MAX_POOL_SIZE: u32 = u32::MAX - SERVER_CONNECTION_HEADROOM;
const MAX_CONCURRENCY: u32 = MAX_POOL_SIZE - 1;
const OPERATION_TIMEOUT: Duration = Duration::from_secs(60);
const WARMUP_DURATION: Duration = Duration::from_secs(5);
const IDENTITY_PREFIX: u128 = 0x5b00_0000_u128 << 96;
// These synthetic fixtures provide stable, valid payloads; their wording has no
// domain meaning.
const SESSION_INPUT_FIXTURE: &str = "Measure the current state.";
const MODEL_CREDENTIAL_FIXTURE: &str = "benchmark-provider-primary";
const TOOL_NAME_FIXTURE: &str = "current_time";
const TOOL_ARGUMENTS_FIXTURE: &str = "{}";
const TOOL_RESULT_FIXTURE: &str = "2026-07-30T12:00:00Z";

type HarnessResult<T> = Result<T, Box<dyn Error + Send + Sync + 'static>>;

#[derive(Debug)]
struct HarnessError(String);

impl fmt::Display for HarnessError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Error for HarnessError {}

fn error(message: impl Into<String>) -> Box<dyn Error + Send + Sync + 'static> {
    Box::new(HarnessError(message.into()))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FsyncMode {
    On,
    Off,
}

impl FsyncMode {
    const fn label(self) -> &'static str {
        match self {
            Self::On => "on",
            Self::Off => "off",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Scenario {
    SessionCreation,
    FullPath,
    SchedulerLock,
}

impl Scenario {
    const fn label(self) -> &'static str {
        match self {
            Self::SessionCreation => "session_creation",
            Self::FullPath => "full_path",
            Self::SchedulerLock => "scheduler_lock",
        }
    }
}

#[derive(Debug)]
struct Config {
    duration: Duration,
    pool_size: u32,
    concurrencies: Vec<usize>,
    fsync_modes: Vec<FsyncMode>,
    scenarios: Vec<Scenario>,
    server_max_connections: u32,
}

enum ParsedArgs {
    Run(Config),
    Help,
    Skip,
}

fn parse_args() -> HarnessResult<ParsedArgs> {
    let mut duration_seconds = DEFAULT_DURATION_SECONDS;
    let mut pool_size = DEFAULT_POOL_SIZE;
    let mut concurrencies = DEFAULT_CONCURRENCIES.to_vec();
    let mut fsync_modes = vec![FsyncMode::On, FsyncMode::Off];
    let mut scenarios = vec![
        Scenario::SessionCreation,
        Scenario::FullPath,
        Scenario::SchedulerLock,
    ];
    let arguments = env::args().skip(1).collect::<Vec<_>>();
    if !arguments.iter().any(|argument| argument == "--bench") {
        return Ok(ParsedArgs::Skip);
    }
    let mut arguments = arguments.into_iter();

    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--duration-seconds" => {
                let value = arguments
                    .next()
                    .ok_or_else(|| error("--duration-seconds requires a value"))?;
                duration_seconds = value.parse::<u64>().map_err(|parse_error| {
                    error(format!(
                        "invalid --duration-seconds value {value:?}: {parse_error}"
                    ))
                })?;
            }
            "--pool-size" => {
                let value = arguments
                    .next()
                    .ok_or_else(|| error("--pool-size requires a value"))?;
                pool_size = value.parse::<u32>().map_err(|parse_error| {
                    error(format!(
                        "invalid --pool-size value {value:?}: {parse_error}"
                    ))
                })?;
            }
            "--concurrency" => {
                let value = arguments
                    .next()
                    .ok_or_else(|| error("--concurrency requires a value"))?;
                concurrencies = parse_concurrencies(&value)?;
            }
            "--fsync" => {
                let value = arguments
                    .next()
                    .ok_or_else(|| error("--fsync requires a value"))?;
                fsync_modes = match value.as_str() {
                    "both" => vec![FsyncMode::On, FsyncMode::Off],
                    "on" => vec![FsyncMode::On],
                    "off" => vec![FsyncMode::Off],
                    _ => {
                        return Err(error(format!(
                            "invalid --fsync value {value:?}; use both, on, or off"
                        )));
                    }
                };
            }
            "--scenario" => {
                let value = arguments
                    .next()
                    .ok_or_else(|| error("--scenario requires a value"))?;
                scenarios = match value.as_str() {
                    "all" => vec![
                        Scenario::SessionCreation,
                        Scenario::FullPath,
                        Scenario::SchedulerLock,
                    ],
                    "session-creation" => vec![Scenario::SessionCreation],
                    "full-path" => vec![Scenario::FullPath],
                    "scheduler-lock" => vec![Scenario::SchedulerLock],
                    _ => {
                        return Err(error(format!(
                            "invalid --scenario value {value:?}; use all, session-creation, \
                             full-path, or scheduler-lock"
                        )));
                    }
                };
            }
            "--bench" => {}
            "--help" | "-h" => return Ok(ParsedArgs::Help),
            _ => return Err(error(format!("unknown argument {argument:?}"))),
        }
    }

    if duration_seconds == 0 {
        return Err(error("--duration-seconds must be positive"));
    }
    // One point holds one disposable container from before the warmup until
    // after the offered load, and the orphan sweep removes marked containers
    // past this age. Refusing here fails the run before it starts a container
    // rather than partway through; it cannot be the whole guard, because how
    // long the container takes to become ready is not knowable yet, so the
    // point re-checks against its measured setup once it has one.
    let point_lifetime = Duration::from_secs(duration_seconds)
        .saturating_add(WARMUP_DURATION)
        .saturating_add(OPERATION_TIMEOUT);
    if outlives_the_disposable_container_sweep(point_lifetime) {
        return Err(error(format!(
            "--duration-seconds must leave the whole point under {}h, counting the {}s \
             warmup and the {}s an operation in flight may still take: one disposable \
             container serves the point, and tooling/sweep-test-containers.sh removes \
             marked containers past that age",
            DISPOSABLE_TEST_CONTAINER_LIFETIME_HOURS,
            WARMUP_DURATION.as_secs(),
            OPERATION_TIMEOUT.as_secs()
        )));
    }
    let highest_concurrency = concurrencies
        .iter()
        .copied()
        .max()
        .ok_or_else(|| error("at least one concurrency is required"))?;
    let highest_concurrency = u32::try_from(highest_concurrency)
        .map_err(|_| error("the highest concurrency does not fit the SQLx pool limit"))?;
    if pool_size <= highest_concurrency {
        return Err(error(format!(
            "--pool-size must be above the highest concurrency ({highest_concurrency})"
        )));
    }
    let server_max_connections = pool_size
        .checked_add(SERVER_CONNECTION_HEADROOM)
        .ok_or_else(|| error("--pool-size is too large to configure PostgreSQL"))?;

    let duration = Duration::from_secs(duration_seconds);
    Instant::now()
        .checked_add(duration)
        .ok_or_else(|| error("--duration-seconds exceeds the platform timer range"))?;

    Ok(ParsedArgs::Run(Config {
        duration,
        pool_size,
        concurrencies,
        fsync_modes,
        scenarios,
        server_max_connections,
    }))
}

fn parse_concurrencies(value: &str) -> HarnessResult<Vec<usize>> {
    let parsed = value
        .split(',')
        .map(|part| {
            part.parse::<usize>().map_err(|parse_error| {
                error(format!(
                    "invalid concurrency {part:?} in {value:?}: {parse_error}"
                ))
            })
        })
        .collect::<HarnessResult<Vec<_>>>()?;
    if parsed.is_empty() || parsed.contains(&0) {
        return Err(error(
            "--concurrency must contain one or more positive comma-separated integers",
        ));
    }
    Ok(parsed)
}

fn print_help() {
    println!(
        "Usage: cargo bench -p signalbox-persistence --features postgres-integration \
         --bench postgres_load -- [OPTIONS]\n\n\
         Options:\n\
           --duration-seconds N   Positive offered-load seconds per point, within the \
         platform timer range (default: 60)\n\
           --pool-size N          Pre-opened pool size, above the highest concurrency and \
         at most {MAX_POOL_SIZE} (default: 80)\n\
           --concurrency LIST     Comma-separated positive sweep; each value at most \
         {MAX_CONCURRENCY} and below pool size (default: 1,2,4,8,16,32,64)\n\
           --fsync MODE           both, on, or off (default: both)\n\
           --scenario NAME        all, session-creation, full-path, or scheduler-lock \
         (default: all)\n\
           -h, --help             Show this help"
    );
}

struct PostgresEnvironment {
    _container: ContainerAsync<Postgres>,
    admin_pool: PgPool,
    host: String,
    port: u16,
    fsync: FsyncMode,
    server_max_connections: u32,
}

impl PostgresEnvironment {
    async fn start(fsync: FsyncMode, server_max_connections: u32) -> HarnessResult<Self> {
        let image = Postgres::default()
            .with_db_name(ADMIN_DATABASE)
            .with_user(DATABASE_USER)
            .with_password(DATABASE_PASSWORD);
        let (image, mut command) = match fsync {
            FsyncMode::On => (image.with_fsync_enabled(), Vec::new()),
            FsyncMode::Off => (image, vec![String::from("-c"), String::from("fsync=off")]),
        };
        command.extend([
            String::from("-c"),
            format!("max_connections={server_max_connections}"),
        ]);
        let container = image
            .with_tag(POSTGRES_IMAGE_TAG)
            .with_cmd(command)
            .with_labels(disposable_test_container_labels())
            .start()
            .await?;
        let host = container.get_host().await?.to_string();
        let port = container.get_host_port_ipv4(5432).await?;
        let admin_url = database_url(&host, port, ADMIN_DATABASE);
        let admin_pool = PgPoolOptions::new()
            .max_connections(1)
            .connect_with(local_test_connection_options(&admin_url)?)
            .await?;
        Ok(Self {
            _container: container,
            admin_pool,
            host,
            port,
            fsync,
            server_max_connections,
        })
    }

    async fn migrated_pool(&self, database_sequence: u64, pool_size: u32) -> HarnessResult<PgPool> {
        let database = format!("signalbox_bench_{database_sequence}");
        let create_database = format!("CREATE DATABASE {database}");
        // `database` is formed locally from a fixed ASCII prefix and a u64, so
        // no caller-controlled text reaches this audited identifier.
        sqlx::query(sqlx::AssertSqlSafe(create_database.as_str()))
            .execute(&self.admin_pool)
            .await?;
        let url = database_url(&self.host, self.port, &database);
        let pool = PgPoolOptions::new()
            .min_connections(pool_size)
            .max_connections(pool_size)
            .connect_with(local_test_connection_options(&url)?)
            .await?;
        migrate(&pool).await?;
        let observed: String = sqlx::query_scalar("SHOW fsync").fetch_one(&pool).await?;
        if observed != self.fsync.label() {
            pool.close().await;
            return Err(error(format!(
                "container fsync mismatch: requested {}, observed {observed}",
                self.fsync.label()
            )));
        }
        let observed_max: String = sqlx::query_scalar("SHOW max_connections")
            .fetch_one(&pool)
            .await?;
        let expected_max = self.server_max_connections.to_string();
        if observed_max != expected_max {
            pool.close().await;
            return Err(error(format!(
                "container max_connections mismatch: requested {expected_max}, observed \
                 {observed_max}"
            )));
        }
        Ok(pool)
    }
}

fn database_url(host: &str, port: u16, database: &str) -> String {
    format!("postgres://{DATABASE_USER}:{DATABASE_PASSWORD}@{host}:{port}/{database}")
}

#[derive(Clone, Copy)]
struct OperationIds {
    base: u128,
}

#[derive(Clone, Copy)]
#[repr(u128)]
enum IdentityRole {
    CreationCommand = 1,
    Session = 2,
    ModelSelection = 3,
    SubmitCommand = 4,
    AcceptedInput = 5,
    Turn = 6,
    CancelledModelCallEntry = 7,
    CancelledModelCallFrontier = 8,
    CancelledModelCallSuccessorTurn = 9,
    CancelledEntriesFrontier = 128,
    ActivationUserEntry = 129,
    ActivationAssistantEntry = 130,
    ActivationFrontier = 131,
    TurnAttempt = 132,
    ProviderModel = 133,
    ModelCall = 134,
    FailedModelCallEntry = 135,
    FailedModelCallFrontier = 136,
    PreparedModelCallFrontier = 137,
    FailedModelCallSuccessorEntry = 138,
    FailedModelCallSuccessorTurn = 139,
    ResumeCandidateModelCall = 140,
    ResumeFailedModelCallEntry = 141,
    ResumeFailedModelCallFrontier = 142,
    ResumePreparedModelCallFrontier = 143,
    ResumeFailedModelCallSuccessorEntry = 144,
    ResumeFailedModelCallSuccessorTurn = 145,
    ToolRequest = 146,
    ToolResponseEntry = 147,
    ToolRoundFrontier = 148,
    ToolRoundSuccessorTurn = 149,
    ToolDecisionCommand = 150,
    ToolDecisionTurnAttempt = 151,
    ToolAttempt = 152,
    InstructionDiscovery = 153,
    InstructionManifest = 154,
    InstructionBundle = 155,
}

const CANCELLED_TOOL_ENTRY_OFFSET: u128 = 64;

impl OperationIds {
    const fn uuid(self, role: IdentityRole) -> Uuid {
        Uuid::from_u128(self.base + role as u128)
    }

    const fn cancelled_tool_entry(self, index: u128) -> Uuid {
        Uuid::from_u128(self.base + CANCELLED_TOOL_ENTRY_OFFSET + index)
    }
}

struct LoadContext {
    next_operation: AtomicU64,
}

impl LoadContext {
    const fn new() -> Self {
        Self {
            next_operation: AtomicU64::new(1),
        }
    }

    fn next_ids(&self) -> OperationIds {
        let operation = self.next_operation.fetch_add(1, Ordering::Relaxed);
        OperationIds {
            base: IDENTITY_PREFIX | (u128::from(operation) << 16),
        }
    }
}

#[derive(Default)]
struct WorkerMeasurements {
    latencies: Vec<Duration>,
    completed_during_offered_load: usize,
}

#[derive(Clone, Copy)]
struct MeasurementWindow {
    started: Instant,
    deadline: Instant,
}

struct PointResult {
    scenario: Scenario,
    fsync: FsyncMode,
    concurrency: usize,
    warmup_duration: Duration,
    offered_duration: Duration,
    elapsed: Duration,
    completed_during_offered_load: usize,
    pool_size: u32,
    server_max_connections: u32,
    available_parallelism: usize,
    latencies: Vec<Duration>,
}

#[derive(Clone, Copy)]
struct PointConfig {
    scenario: Scenario,
    fsync: FsyncMode,
    pool_size: u32,
    server_max_connections: u32,
    concurrency: usize,
    duration: Duration,
    available_parallelism: usize,
}

impl PointResult {
    fn measured_latencies(&self) -> usize {
        self.latencies.len()
    }

    fn throughput(&self) -> f64 {
        self.completed_during_offered_load as f64 / self.offered_duration.as_secs_f64()
    }

    fn percentile_milliseconds(&self, percentile: usize) -> Option<f64> {
        if self.latencies.is_empty() {
            return None;
        }
        let sample_count = u128::try_from(self.latencies.len()).ok()?;
        let percentile = u128::try_from(percentile).ok()?;
        let rank = sample_count.checked_mul(percentile)?.div_ceil(100);
        let index = usize::try_from(rank.checked_sub(1)?).ok()?;
        self.latencies
            .get(index)
            .map(|duration| duration.as_secs_f64() * 1_000.0)
    }
}

async fn run_point(
    config: PointConfig,
    pool: PgPool,
    identities: Arc<LoadContext>,
) -> HarnessResult<PointResult> {
    let window = Arc::new(OnceLock::<MeasurementWindow>::new());
    let barrier_participants = config
        .concurrency
        .checked_add(1)
        .ok_or_else(|| error("worker barrier participant count overflowed"))?;
    let start_barrier = Arc::new(Barrier::new(barrier_participants));
    let mut workers = JoinSet::new();

    for _ in 0..config.concurrency {
        let worker_pool = pool.clone();
        let worker_window = Arc::clone(&window);
        let worker_start_barrier = Arc::clone(&start_barrier);
        let worker_identities = Arc::clone(&identities);
        workers.spawn(async move {
            let mut measurements = WorkerMeasurements::default();
            worker_start_barrier.wait().await;
            loop {
                let started = Instant::now();
                if let Some(measurement) = worker_window.get().copied()
                    && started >= measurement.deadline
                {
                    break;
                }
                let operation_ids = worker_identities.next_ids();
                perform_operation_with_timeout(config.scenario, &worker_pool, operation_ids)
                    .await?;
                let completed = Instant::now();

                if let Some(measurement) = worker_window.get().copied() {
                    if completed >= measurement.started && completed <= measurement.deadline {
                        measurements.completed_during_offered_load += 1;
                    }
                    if started >= measurement.started && started < measurement.deadline {
                        measurements
                            .latencies
                            .push(completed.duration_since(started));
                    }
                }
            }
            Ok::<_, Box<dyn Error + Send + Sync + 'static>>(measurements)
        });
    }

    start_barrier.wait().await;
    tokio::select! {
        () = tokio::time::sleep(WARMUP_DURATION) => {}
        worker = workers.join_next() => {
            let worker_error = match worker {
                Some(Ok(Err(worker_error))) => worker_error,
                Some(Err(join_error)) => {
                    error(format!("load worker failed to join during warmup: {join_error}"))
                }
                Some(Ok(Ok(_))) => error("a load worker stopped unexpectedly during warmup"),
                None => error("all load workers stopped unexpectedly during warmup"),
            };
            workers.abort_all();
            while workers.join_next().await.is_some() {}
            return Err(worker_error);
        }
    }

    let measurement_started = Instant::now();
    let measurement_deadline = measurement_started
        .checked_add(config.duration)
        .ok_or_else(|| error("measurement duration exceeds the platform timer range"))?;
    window
        .set(MeasurementWindow {
            started: measurement_started,
            deadline: measurement_deadline,
        })
        .map_err(|_| error("measurement window was initialized twice"))?;
    let mut latencies = Vec::new();
    let mut completed_during_offered_load = 0;
    while let Some(worker) = workers.join_next().await {
        let measurements = worker
            .map_err(|join_error| error(format!("load worker failed to join: {join_error}")))??;
        latencies.extend(measurements.latencies);
        completed_during_offered_load += measurements.completed_during_offered_load;
    }
    latencies.sort_unstable();
    let elapsed = measurement_started.elapsed();

    Ok(PointResult {
        scenario: config.scenario,
        fsync: config.fsync,
        concurrency: config.concurrency,
        warmup_duration: WARMUP_DURATION,
        offered_duration: config.duration,
        elapsed,
        completed_during_offered_load,
        pool_size: config.pool_size,
        server_max_connections: config.server_max_connections,
        available_parallelism: config.available_parallelism,
        latencies,
    })
}

async fn perform_operation_with_timeout(
    scenario: Scenario,
    pool: &PgPool,
    ids: OperationIds,
) -> HarnessResult<()> {
    timeout(OPERATION_TIMEOUT, perform_operation(scenario, pool, ids))
        .await
        .map_err(|_| {
            error(format!(
                "{} operation exceeded the {}-second liveness timeout",
                scenario.label(),
                OPERATION_TIMEOUT.as_secs()
            ))
        })?
}

async fn perform_operation(
    scenario: Scenario,
    pool: &PgPool,
    ids: OperationIds,
) -> HarnessResult<()> {
    match scenario {
        Scenario::SessionCreation => {
            create_session(pool, ids).await?;
        }
        Scenario::FullPath => {
            full_path(pool, ids).await?;
        }
        Scenario::SchedulerLock => {
            let flow = create_and_submit_turn(pool, ids).await?;
            activate_turn(pool, ids, flow).await?;
        }
    }
    Ok(())
}

#[derive(Clone, Copy)]
struct TurnFlow {
    session: SessionId,
    turn: TurnId,
    selection: DirectModelSelection,
}

async fn create_session(pool: &PgPool, ids: OperationIds) -> HarnessResult<TurnFlow> {
    let command = DurableCommandId::from_uuid(ids.uuid(IdentityRole::CreationCommand));
    let session = SessionId::from_uuid(ids.uuid(IdentityRole::Session));
    let selection = DirectModelSelection::from_uuid(ids.uuid(IdentityRole::ModelSelection));
    let prepared = CreateSession::new(
        command,
        SessionCreationProvenance::new(SessionCreationCause::Interactive, TranscriptAncestry::None),
        SessionConfigurationDefaults::new(ModelSelectionRequest::Direct(selection)),
    )
    .prepare(session)
    .map_err(|preparation_error| {
        error(format!(
            "session creation fixture was rejected: {preparation_error:?}"
        ))
    })?;
    let outcome = CreateSessionRepository::new(pool.clone(), test_session_credential_pin()?)
        .handle(prepared)
        .await?;
    match outcome {
        CreateSessionHandlingOutcome::Applied(result) if result.session() == session => {}
        CreateSessionHandlingOutcome::Applied(_) => {
            return Err(error("session creation returned a different session"));
        }
        CreateSessionHandlingOutcome::ConflictingReuse { .. } => {
            return Err(error("fresh session creation identity conflicted"));
        }
    }
    Ok(TurnFlow {
        session,
        turn: TurnId::from_uuid(ids.uuid(IdentityRole::Turn)),
        selection,
    })
}

async fn create_and_submit_turn(pool: &PgPool, ids: OperationIds) -> HarnessResult<TurnFlow> {
    let flow = create_session(pool, ids).await?;
    let accepted_input = AcceptedInputId::from_uuid(ids.uuid(IdentityRole::AcceptedInput));
    let command = SubmitInput::new(
        DurableCommandId::from_uuid(ids.uuid(IdentityRole::SubmitCommand)),
        flow.session,
        UserContent::try_text(String::from(SESSION_INPUT_FIXTURE)).map_err(|content_error| {
            error(format!(
                "benchmark input fixture was rejected: {content_error:?}"
            ))
        })?,
        signalbox_domain::DeliveryRequest::StartWhenNoActiveTurn {
            configuration: PerInputConfigurationChoices::new(
                SessionConfigurationDefaultsVersion::first(),
                ModelSelectionOverride::UseSessionDefault,
            ),
        },
    );
    let outcome = SubmitInputRepository::new(pool.clone())
        .handle_with_candidates(
            command,
            accepted_input,
            Some(flow.turn),
            CancelledModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(
                    ids.uuid(IdentityRole::CancelledModelCallEntry),
                ),
                ContextFrontierId::from_uuid(ids.uuid(IdentityRole::CancelledModelCallFrontier)),
            ),
            |_| TurnId::from_uuid(ids.uuid(IdentityRole::CancelledModelCallSuccessorTurn)),
            |requests| {
                let mut next_index = 0_u128;
                let entries = requests
                    .iter()
                    .map(|_| {
                        let entry = SemanticTranscriptEntryId::from_uuid(
                            ids.cancelled_tool_entry(next_index),
                        );
                        next_index = next_index.saturating_add(1);
                        entry
                    })
                    .collect();
                (
                    entries,
                    ContextFrontierId::from_uuid(ids.uuid(IdentityRole::CancelledEntriesFrontier)),
                )
            },
        )
        .await?;
    match outcome {
        SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Applied(
            SubmitInputAppliedResult::TurnOrigin(result),
        )) if result.turn() == flow.turn => {}
        SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Applied(
            SubmitInputAppliedResult::TurnOrigin(_),
        )) => {
            return Err(error("fresh input created a different turn"));
        }
        SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Applied(
            SubmitInputAppliedResult::PendingSteering(_),
        )) => {
            return Err(error("fresh input unexpectedly became pending steering"));
        }
        SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Rejected(_)) => {
            return Err(error("fresh input was rejected"));
        }
        SubmitInputHandlingOutcome::ConflictingReuse { .. } => {
            return Err(error("fresh input command identity conflicted"));
        }
    }
    Ok(flow)
}

async fn activate_turn(pool: &PgPool, ids: OperationIds, flow: TurnFlow) -> HarnessResult<()> {
    let outcome = StartEligibleTurnRepository::new(pool.clone())
        .handle(
            flow.session,
            AcceptedInputTurnActivationIdentities::new(
                SemanticTranscriptEntryId::from_uuid(ids.uuid(IdentityRole::ActivationUserEntry)),
                SemanticTranscriptEntryId::from_uuid(
                    ids.uuid(IdentityRole::ActivationAssistantEntry),
                ),
                ContextFrontierId::from_uuid(ids.uuid(IdentityRole::ActivationFrontier)),
                TurnAttemptId::from_uuid(ids.uuid(IdentityRole::TurnAttempt)),
            ),
        )
        .await?;
    match outcome {
        StartEligibleTurnOutcome::Activated(activated) if activated.turn() == flow.turn => Ok(()),
        StartEligibleTurnOutcome::Activated(_) => {
            Err(error("scheduler activated a different turn"))
        }
        StartEligibleTurnOutcome::NoEligibleTurn => {
            Err(error("fresh queued turn was not eligible"))
        }
    }
}

async fn full_path(pool: &PgPool, ids: OperationIds) -> HarnessResult<()> {
    let flow = create_and_submit_turn(pool, ids).await?;
    activate_turn(pool, ids, flow).await?;
    let instruction_manifest_id =
        TurnInstructionManifestId::from_uuid(ids.uuid(IdentityRole::InstructionManifest));
    let instruction_outcome = WorkspaceInstructionRepository::new(pool.clone())
        .record_turn_start(
            InstructionDiscoveryId::from_uuid(ids.uuid(IdentityRole::InstructionDiscovery)),
            TurnInstructionManifest::empty_turn_start(
                instruction_manifest_id,
                flow.session,
                flow.turn,
            ),
            &discover_workspace_instructions(Vec::new()),
            || InstructionBundleId::from_uuid(ids.uuid(IdentityRole::InstructionBundle)),
        )
        .await?;
    match instruction_outcome {
        RecordTurnInstructionSnapshotOutcome::Recorded(recorded)
            if recorded == instruction_manifest_id => {}
        RecordTurnInstructionSnapshotOutcome::Recorded(_) => {
            return Err(error("instruction preparation recorded another manifest"));
        }
        RecordTurnInstructionSnapshotOutcome::AlreadyRecorded(_)
        | RecordTurnInstructionSnapshotOutcome::DiscoveryIncomplete
        | RecordTurnInstructionSnapshotOutcome::TurnUnavailable => {
            return Err(error("instruction preparation did not record the manifest"));
        }
    }

    let provider = ProviderModelIdentity::from_uuid(ids.uuid(IdentityRole::ProviderModel));
    let targets = ModelTargetCatalog::try_from_definitions([ModelTargetDefinition::new(
        flow.selection,
        ResolvedProviderTarget::naming(provider),
    )])
    .map_err(|catalog_error| {
        error(format!(
            "benchmark model catalog was rejected: {catalog_error:?}"
        ))
    })?;
    let model_repository = PostgresModelCallRepository::new(
        pool.clone(),
        targets,
        ModelCallCredentialReference::new(MODEL_CREDENTIAL_FIXTURE),
    );
    let call = ModelCallId::from_uuid(ids.uuid(IdentityRole::ModelCall));
    let prepared = model_repository
        .prepare_initial_call(
            flow.session,
            call,
            FailedModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(ids.uuid(IdentityRole::FailedModelCallEntry)),
                ContextFrontierId::from_uuid(ids.uuid(IdentityRole::FailedModelCallFrontier)),
            ),
            ContextFrontierId::from_uuid(ids.uuid(IdentityRole::PreparedModelCallFrontier)),
            |_| {
                (
                    SemanticTranscriptEntryId::from_uuid(
                        ids.uuid(IdentityRole::FailedModelCallSuccessorEntry),
                    ),
                    TurnId::from_uuid(ids.uuid(IdentityRole::FailedModelCallSuccessorTurn)),
                )
            },
        )
        .await?;
    match prepared {
        PrepareInitialModelCallOutcome::Checkpointed(checkpointed) if checkpointed == call => {}
        PrepareInitialModelCallOutcome::Checkpointed(_) => {
            return Err(error(
                "initial model call checkpointed a different identity",
            ));
        }
        PrepareInitialModelCallOutcome::NoWork
        | PrepareInitialModelCallOutcome::RetryBackoff(_)
        | PrepareInitialModelCallOutcome::PoolExhausted(_)
        | PrepareInitialModelCallOutcome::Ready { .. }
        | PrepareInitialModelCallOutcome::TargetUnavailable(_) => {
            return Err(error("initial model call did not checkpoint"));
        }
    }
    let resumed = model_repository
        .prepare_initial_call(
            flow.session,
            ModelCallId::from_uuid(ids.uuid(IdentityRole::ResumeCandidateModelCall)),
            FailedModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(
                    ids.uuid(IdentityRole::ResumeFailedModelCallEntry),
                ),
                ContextFrontierId::from_uuid(ids.uuid(IdentityRole::ResumeFailedModelCallFrontier)),
            ),
            ContextFrontierId::from_uuid(ids.uuid(IdentityRole::ResumePreparedModelCallFrontier)),
            |_| {
                (
                    SemanticTranscriptEntryId::from_uuid(
                        ids.uuid(IdentityRole::ResumeFailedModelCallSuccessorEntry),
                    ),
                    TurnId::from_uuid(ids.uuid(IdentityRole::ResumeFailedModelCallSuccessorTurn)),
                )
            },
        )
        .await?;
    match resumed {
        PrepareInitialModelCallOutcome::Ready { .. } => {}
        PrepareInitialModelCallOutcome::NoWork
        | PrepareInitialModelCallOutcome::RetryBackoff(_)
        | PrepareInitialModelCallOutcome::PoolExhausted(_)
        | PrepareInitialModelCallOutcome::Checkpointed(_)
        | PrepareInitialModelCallOutcome::TargetUnavailable(_) => {
            return Err(error("checkpointed model call did not reload as ready"));
        }
    }
    let authorized = match model_repository.authorize_send(flow.session, call).await? {
        AuthorizeModelCallOutcome::Authorized(authorized) => *authorized,
        AuthorizeModelCallOutcome::NoSend => {
            return Err(error("checkpointed model call was not authorized"));
        }
    };

    let request = ToolRequestId::from_uuid(ids.uuid(IdentityRole::ToolRequest));
    let response =
        ToolUsingAssistantResponse::try_from_parts(vec![AssistantResponsePart::ToolCall(
            ToolCallProposal::new(
                ToolName::try_new(String::from(TOOL_NAME_FIXTURE)).map_err(|tool_error| {
                    error(format!("benchmark tool name was rejected: {tool_error:?}"))
                })?,
                NormalizedToolArguments::try_from_provider_text(String::from(
                    TOOL_ARGUMENTS_FIXTURE,
                ))
                .map_err(|arguments_error| {
                    error(format!(
                        "benchmark tool arguments were rejected: {arguments_error:?}"
                    ))
                })?,
            ),
        )])
        .map_err(|response_error| {
            error(format!(
                "benchmark tool response was rejected: {response_error:?}"
            ))
        })?;
    let observation = authorized
        .observation_correlation()
        .bind_terminal_observation(ModelCallTerminalObservation::CompletedWithTools {
            response,
            retained_input_tokens: None,
        });
    let model_outcome = model_repository
        .apply_terminal_observation(
            flow.session,
            observation,
            ModelCallTerminalIdentities::ToolRound(ToolRoundModelCallIdentities::new(
                vec![ToolResponsePartIdentity::tool_call(
                    SemanticTranscriptEntryId::from_uuid(ids.uuid(IdentityRole::ToolResponseEntry)),
                    request,
                    InitialToolApproval::Confirm,
                )],
                ContextFrontierId::from_uuid(ids.uuid(IdentityRole::ToolRoundFrontier)),
                None,
            )),
            |_| TurnId::from_uuid(ids.uuid(IdentityRole::ToolRoundSuccessorTurn)),
        )
        .await?;
    match model_outcome {
        ModelCallTerminalOutcome::ToolRound(_) => {}
        ModelCallTerminalOutcome::Completed(_)
        | ModelCallTerminalOutcome::CancelledWithToolResponse(_)
        | ModelCallTerminalOutcome::Failed(_)
        | ModelCallTerminalOutcome::Cancelled(_)
        | ModelCallTerminalOutcome::Refused(_)
        | ModelCallTerminalOutcome::ReconciliationRequired(_)
        | ModelCallTerminalOutcome::AwaitingRecovery(_) => {
            return Err(error("model call did not commit a tool round"));
        }
    }

    let tool_repository = PostgresToolLoopRepository::new(pool.clone());
    let decision = DecideToolRequest::try_new(
        DurableCommandId::from_uuid(ids.uuid(IdentityRole::ToolDecisionCommand)),
        request,
        ToolApprovalDecision::Approve,
    )
    .map_err(|decision_error| {
        error(format!(
            "benchmark tool decision was rejected: {decision_error:?}"
        ))
    })?;
    tool_repository
        .decide(decision, || {
            TurnAttemptId::from_uuid(ids.uuid(IdentityRole::ToolDecisionTurnAttempt))
        })
        .await?;
    let attempt = ToolAttemptId::from_uuid(ids.uuid(IdentityRole::ToolAttempt));
    let prepared_attempt = tool_repository
        .prepare_next_attempt(
            flow.session,
            flow.turn,
            attempt,
            ToolEffectClass::EffectFree,
        )
        .await?;
    let Some(prepared_attempt) = prepared_attempt else {
        return Err(error("approved tool request did not prepare an attempt"));
    };
    let first_reload = load_exact_prepared_tool_attempt(
        &tool_repository,
        flow.session,
        flow.turn,
        request,
        &prepared_attempt,
    )
    .await?;
    let _second_reload = load_exact_prepared_tool_attempt(
        &tool_repository,
        flow.session,
        flow.turn,
        request,
        &first_reload,
    )
    .await?;
    let authorized_attempt = tool_repository
        .authorize_attempt(flow.session, flow.turn, attempt)
        .await?;
    let ended = tool_repository
        .commit_observation(authorized_attempt.executor_fence().bind(
            ToolAttemptObservation::Completed {
                result: ToolResultContent::Text(
                    ToolResultText::try_new(String::from(TOOL_RESULT_FIXTURE)).map_err(
                        |result_error| {
                            error(format!(
                                "benchmark tool result was rejected: {result_error:?}"
                            ))
                        },
                    )?,
                ),
            },
        ))
        .await?;
    match ended.end() {
        ToolAttemptEnd::Completed { .. } => {}
        ToolAttemptEnd::KnownFailed { .. }
        | ToolAttemptEnd::AwaitingChild { .. }
        | ToolAttemptEnd::Ambiguous => {
            return Err(error("tool attempt did not commit completion"));
        }
    }
    Ok(())
}

async fn load_exact_prepared_tool_attempt(
    repository: &PostgresToolLoopRepository,
    session: SessionId,
    turn: TurnId,
    request: ToolRequestId,
    expected: &CurrentToolAttempt,
) -> HarnessResult<CurrentToolAttempt> {
    let batch = repository
        .load_active_batch(session, turn)
        .await?
        .ok_or_else(|| error("prepared tool attempt reload found no active batch"))?;
    if !batch
        .requests()
        .iter()
        .any(|candidate| candidate.id() == request)
    {
        return Err(error("prepared tool attempt reload omitted its request"));
    }
    match batch.attempt(request) {
        Some(ReconstitutedToolAttempt::Current(current))
            if current == expected && current.state() == CurrentToolAttemptState::Prepared =>
        {
            Ok(current.clone())
        }
        Some(ReconstitutedToolAttempt::Current(_))
        | Some(ReconstitutedToolAttempt::Ended(_))
        | None => Err(error(
            "prepared tool attempt reload did not preserve the exact prepared state",
        )),
    }
}

fn format_optional(value: Option<f64>) -> String {
    value.map_or_else(|| String::from("n/a"), |number| format!("{number:.3}"))
}

fn print_result_header() {
    println!(
        "| scenario | image | available parallelism | fsync | pool | server max | concurrency | \
         warmup (s) | offered (s) | elapsed (s) | completed in window | latency samples | ops/s | \
         p50 (ms) | p95 (ms) | p99 (ms) |"
    );
    println!("|---|---|---:|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|");
}

fn print_result(result: &PointResult) {
    println!(
        "| {} | postgres:{} | {} | {} | {} | {} | {} | {:.3} | {:.0} | {:.3} | {} | {} | {:.2} | {} | {} | {} |",
        result.scenario.label(),
        POSTGRES_IMAGE_TAG,
        result.available_parallelism,
        result.fsync.label(),
        result.pool_size,
        result.server_max_connections,
        result.concurrency,
        result.warmup_duration.as_secs_f64(),
        result.offered_duration.as_secs_f64(),
        result.elapsed.as_secs_f64(),
        result.completed_during_offered_load,
        result.measured_latencies(),
        result.throughput(),
        format_optional(result.percentile_milliseconds(50)),
        format_optional(result.percentile_milliseconds(95)),
        format_optional(result.percentile_milliseconds(99)),
    );
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> HarnessResult<()> {
    let config = match parse_args()? {
        ParsedArgs::Run(config) => config,
        ParsedArgs::Help => {
            print_help();
            return Ok(());
        }
        ParsedArgs::Skip => return Ok(()),
    };
    let available_parallelism = std::thread::available_parallelism()
        .map_err(|cpu_error| error(format!("available parallelism is unavailable: {cpu_error}")))?
        .get();
    let identities = Arc::new(LoadContext::new());
    let mut database_sequence = 0_u64;
    print_result_header();

    for fsync in config.fsync_modes.iter().copied() {
        for scenario in config.scenarios.iter().copied() {
            for concurrency in config.concurrencies.iter().copied() {
                database_sequence = database_sequence
                    .checked_add(1)
                    .ok_or_else(|| error("benchmark database sequence exhausted"))?;
                eprintln!(
                    "starting isolated postgres:{POSTGRES_IMAGE_TAG} with fsync={} server_max={}",
                    fsync.label(),
                    config.server_max_connections
                );
                let container_started = Instant::now();
                let environment =
                    PostgresEnvironment::start(fsync, config.server_max_connections).await?;
                eprintln!(
                    "running scenario={} fsync={} concurrency={} warmup={}s duration={}s pool={}",
                    scenario.label(),
                    fsync.label(),
                    concurrency,
                    WARMUP_DURATION.as_secs(),
                    config.duration.as_secs(),
                    config.pool_size
                );
                let pool = environment
                    .migrated_pool(database_sequence, config.pool_size)
                    .await?;
                // The parse-time check had to assume how long setup takes; this
                // one measures it, and measures it here because the database
                // creation and the migrations above are part of what it has to
                // account for. What remains has to cover the warmup, the
                // offered load, and one `OPERATION_TIMEOUT`, since an operation
                // started an instant before the load window closes is awaited
                // past it. Short of that, the sweep would remove this point's
                // database while the point is still using it.
                let point_remaining = Duration::from_secs(
                    DISPOSABLE_TEST_CONTAINER_LIFETIME_HOURS
                        .saturating_mul(60)
                        .saturating_mul(60),
                )
                .saturating_sub(container_started.elapsed());
                let point_needs = WARMUP_DURATION
                    .saturating_add(config.duration)
                    .saturating_add(OPERATION_TIMEOUT);
                if point_needs >= point_remaining {
                    return Err(error(format!(
                        "this point took {}s to become ready, leaving {}s before \
                         tooling/sweep-test-containers.sh would remove its database, \
                         short of the {}s this point still needs ({}s warmup, {}s offered \
                         load, {}s for an operation in flight when the window closes); \
                         lower --duration-seconds",
                        container_started.elapsed().as_secs(),
                        point_remaining.as_secs(),
                        point_needs.as_secs(),
                        WARMUP_DURATION.as_secs(),
                        config.duration.as_secs(),
                        OPERATION_TIMEOUT.as_secs()
                    )));
                }
                let point = run_point(
                    PointConfig {
                        scenario,
                        fsync,
                        pool_size: config.pool_size,
                        server_max_connections: config.server_max_connections,
                        concurrency,
                        duration: config.duration,
                        available_parallelism,
                    },
                    pool.clone(),
                    Arc::clone(&identities),
                )
                .await;
                pool.close().await;
                let point = point?;
                eprintln!(
                    "completed scenario={} fsync={} concurrency={} ops/s={:.2}",
                    scenario.label(),
                    fsync.label(),
                    concurrency,
                    point.throughput()
                );
                print_result(&point);
                environment.admin_pool.close().await;
            }
        }
    }
    Ok(())
}
