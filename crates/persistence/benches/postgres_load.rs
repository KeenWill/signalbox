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
};
use signalbox_domain::{
    AcceptedInputId, AcceptedInputTurnActivationIdentities, AssistantResponsePart,
    CancelledModelCallTurnIdentities, ContextFrontierId, CreateSession, DecideToolRequest,
    DirectModelSelection, DurableCommandId, FailedModelCallTurnIdentities, InitialToolApproval,
    ModelCallId, ModelCallTerminalIdentities, ModelCallTerminalObservation,
    ModelCallTerminalOutcome, ModelSelectionOverride, ModelSelectionRequest, ModelTargetCatalog,
    ModelTargetDefinition, NormalizedToolArguments, PerInputConfigurationChoices,
    ProviderModelIdentity, ResolvedProviderTarget, SemanticTranscriptEntryId,
    SessionConfigurationDefaults, SessionConfigurationDefaultsVersion, SessionCreationCause,
    SessionCreationProvenance, SessionId, SubmitInput, SubmitInputAppliedResult, SubmitInputResult,
    ToolApprovalDecision, ToolAttemptEnd, ToolAttemptId, ToolAttemptObservation, ToolCallProposal,
    ToolEffectClass, ToolName, ToolRequestId, ToolResponsePartIdentity, ToolResultContent,
    ToolResultText, ToolRoundModelCallIdentities, ToolUsingAssistantResponse, TranscriptAncestry,
    TurnAttemptId, TurnId, UserContent,
};
use signalbox_persistence::{
    create_session::{CreateSessionHandlingOutcome, CreateSessionRepository},
    local_test_connection_options, migrate,
    model_execution::{PostgresModelCallRepository, PrepareInitialModelCallOutcome},
    start_eligible_turn::StartEligibleTurnRepository,
    submit_input::{SubmitInputHandlingOutcome, SubmitInputRepository},
    tool_loop::PostgresToolLoopRepository,
};
use sqlx::{PgPool, postgres::PgPoolOptions, types::Uuid};
use testcontainers_modules::{
    postgres::Postgres,
    testcontainers::{ContainerAsync, ImageExt, runners::AsyncRunner},
};
use tokio::{sync::Barrier, task::JoinSet};

const POSTGRES_IMAGE_TAG: &str = "18.4-alpine3.23";
const DATABASE_USER: &str = "signalbox";
const DATABASE_PASSWORD: &str = "signalbox-benchmark-only";
const ADMIN_DATABASE: &str = "signalbox_benchmark";
const DEFAULT_DURATION_SECONDS: u64 = 10;
const DEFAULT_POOL_SIZE: u32 = 80;
const DEFAULT_CONCURRENCIES: [usize; 7] = [1, 2, 4, 8, 16, 32, 64];
const IDENTITY_PREFIX: u128 = 0x5b00_0000_u128 << 96;

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

    Ok(ParsedArgs::Run(Config {
        duration: Duration::from_secs(duration_seconds),
        pool_size,
        concurrencies,
        fsync_modes,
        scenarios,
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
           --duration-seconds N   Positive offered-load duration per point, in seconds \
         (default: 10)\n\
           --pool-size N          Pre-opened pool size, above the highest concurrency \
         (default: 80)\n\
           --concurrency LIST     Comma-separated positive sweep (default: \
         1,2,4,8,16,32,64)\n\
           --fsync MODE           both, on, or off (default: both)\n\
           --scenario NAME        all, session-creation, full-path, or scheduler-lock\n\
           -h, --help             Show this help"
    );
}

struct PostgresEnvironment {
    _container: ContainerAsync<Postgres>,
    admin_pool: PgPool,
    host: String,
    port: u16,
    fsync: FsyncMode,
}

impl PostgresEnvironment {
    async fn start(fsync: FsyncMode) -> HarnessResult<Self> {
        let image = Postgres::default()
            .with_db_name(ADMIN_DATABASE)
            .with_user(DATABASE_USER)
            .with_password(DATABASE_PASSWORD);
        let image = if fsync == FsyncMode::On {
            image.with_fsync_enabled()
        } else {
            image
        };
        let container = image.with_tag(POSTGRES_IMAGE_TAG).start().await?;
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
    ToolRequest = 140,
    ToolResponseEntry = 141,
    ToolRoundFrontier = 142,
    ToolRoundSuccessorTurn = 143,
    ToolDecisionCommand = 144,
    ToolDecisionTurnAttempt = 145,
    ToolAttempt = 146,
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
}

struct PointResult {
    scenario: Scenario,
    fsync: FsyncMode,
    concurrency: usize,
    offered_duration: Duration,
    elapsed: Duration,
    pool_size: u32,
    host_cpus: usize,
    latencies: Vec<Duration>,
}

#[derive(Clone, Copy)]
struct PointConfig {
    scenario: Scenario,
    fsync: FsyncMode,
    pool_size: u32,
    concurrency: usize,
    duration: Duration,
    host_cpus: usize,
}

impl PointResult {
    fn completed(&self) -> usize {
        self.latencies.len()
    }

    fn throughput(&self) -> f64 {
        self.completed() as f64 / self.elapsed.as_secs_f64()
    }

    fn percentile_milliseconds(&self, percentile: usize) -> Option<f64> {
        if self.latencies.is_empty() {
            return None;
        }
        let index = (self.latencies.len() - 1) * percentile / 100;
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
    let barrier = Arc::new(Barrier::new(config.concurrency + 1));
    let deadline = Arc::new(OnceLock::new());
    let mut workers = JoinSet::new();

    for _ in 0..config.concurrency {
        let worker_pool = pool.clone();
        let worker_barrier = Arc::clone(&barrier);
        let worker_deadline = Arc::clone(&deadline);
        let worker_identities = Arc::clone(&identities);
        workers.spawn(async move {
            worker_barrier.wait().await;
            let stop = worker_deadline
                .get()
                .copied()
                .ok_or_else(|| error("measurement deadline was not initialized"))?;
            let mut measurements = WorkerMeasurements::default();
            while Instant::now() < stop {
                let operation_ids = worker_identities.next_ids();
                let started = Instant::now();
                perform_operation(config.scenario, &worker_pool, operation_ids).await?;
                let completed = Instant::now();
                measurements
                    .latencies
                    .push(completed.duration_since(started));
            }
            Ok::<_, Box<dyn Error + Send + Sync + 'static>>(measurements)
        });
    }

    let measurement_started = Instant::now();
    deadline
        .set(measurement_started + config.duration)
        .map_err(|_| error("measurement deadline was initialized twice"))?;
    barrier.wait().await;
    let mut latencies = Vec::new();
    while let Some(worker) = workers.join_next().await {
        let measurements = worker
            .map_err(|join_error| error(format!("load worker failed to join: {join_error}")))??;
        latencies.extend(measurements.latencies);
    }
    latencies.sort_unstable();
    let elapsed = measurement_started.elapsed();

    Ok(PointResult {
        scenario: config.scenario,
        fsync: config.fsync,
        concurrency: config.concurrency,
        offered_duration: config.duration,
        elapsed,
        pool_size: config.pool_size,
        host_cpus: config.host_cpus,
        latencies,
    })
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
        SessionCreationProvenance::new(
            SessionCreationCause::OwnerInitiated,
            TranscriptAncestry::None,
        ),
        SessionConfigurationDefaults::new(ModelSelectionRequest::Direct(selection)),
    )
    .prepare(session)
    .map_err(|preparation_error| {
        error(format!(
            "session creation fixture was rejected: {preparation_error:?}"
        ))
    })?;
    let outcome = CreateSessionRepository::new(pool.clone())
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
        UserContent::try_text(String::from("Measure the current state.")).map_err(
            |content_error| {
                error(format!(
                    "benchmark input fixture was rejected: {content_error:?}"
                ))
            },
        )?,
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
        SubmitInputHandlingOutcome::Recorded(_) => {
            return Err(error("fresh input did not create the requested turn"));
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
        ModelCallCredentialReference::new("benchmark-provider-primary"),
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
    if !matches!(
        prepared,
        PrepareInitialModelCallOutcome::Checkpointed(checkpointed) if checkpointed == call
    ) {
        return Err(error("initial model call did not checkpoint"));
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
                ToolName::try_new(String::from("current_time")).map_err(|tool_error| {
                    error(format!("benchmark tool name was rejected: {tool_error:?}"))
                })?,
                NormalizedToolArguments::try_from_provider_text(String::from("{}")).map_err(
                    |arguments_error| {
                        error(format!(
                            "benchmark tool arguments were rejected: {arguments_error:?}"
                        ))
                    },
                )?,
            ),
        )])
        .map_err(|response_error| {
            error(format!(
                "benchmark tool response was rejected: {response_error:?}"
            ))
        })?;
    let observation = authorized
        .observation_correlation()
        .bind_terminal_observation(ModelCallTerminalObservation::CompletedWithTools { response });
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
    if !matches!(model_outcome, ModelCallTerminalOutcome::ToolRound(_)) {
        return Err(error("model call did not commit a tool round"));
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
    if prepared_attempt.is_none() {
        return Err(error("approved tool request did not prepare an attempt"));
    }
    let authorized_attempt = tool_repository
        .authorize_attempt(flow.session, flow.turn, attempt)
        .await?;
    let ended = tool_repository
        .commit_observation(authorized_attempt.executor_fence().bind(
            ToolAttemptObservation::Completed {
                result: ToolResultContent::Text(
                    ToolResultText::try_new(String::from("2026-07-30T12:00:00Z")).map_err(
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
    if !matches!(ended.end(), ToolAttemptEnd::Completed { .. }) {
        return Err(error("tool attempt did not commit completion"));
    }
    Ok(())
}

fn format_optional(value: Option<f64>) -> String {
    value.map_or_else(|| String::from("n/a"), |number| format!("{number:.3}"))
}

fn print_results(results: &[PointResult]) {
    println!(
        "| scenario | image | host CPUs | fsync | pool | concurrency | offered (s) | elapsed (s) \
         | completed | ops/s | p50 (ms) | p95 (ms) | p99 (ms) |"
    );
    println!("|---|---|---:|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|");
    for result in results {
        println!(
            "| {} | postgres:{} | {} | {} | {} | {} | {:.0} | {:.3} | {} | {:.2} | {} | {} | {} |",
            result.scenario.label(),
            POSTGRES_IMAGE_TAG,
            result.host_cpus,
            result.fsync.label(),
            result.pool_size,
            result.concurrency,
            result.offered_duration.as_secs_f64(),
            result.elapsed.as_secs_f64(),
            result.completed(),
            result.throughput(),
            format_optional(result.percentile_milliseconds(50)),
            format_optional(result.percentile_milliseconds(95)),
            format_optional(result.percentile_milliseconds(99)),
        );
    }
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
    let host_cpus = std::thread::available_parallelism()
        .map_err(|cpu_error| error(format!("host CPU count is unavailable: {cpu_error}")))?
        .get();
    let identities = Arc::new(LoadContext::new());
    let mut results = Vec::new();
    let mut database_sequence = 0_u64;

    for fsync in config.fsync_modes.iter().copied() {
        eprintln!(
            "starting postgres:{POSTGRES_IMAGE_TAG} with fsync={}",
            fsync.label()
        );
        let environment = PostgresEnvironment::start(fsync).await?;
        for scenario in config.scenarios.iter().copied() {
            for concurrency in config.concurrencies.iter().copied() {
                database_sequence = database_sequence
                    .checked_add(1)
                    .ok_or_else(|| error("benchmark database sequence exhausted"))?;
                eprintln!(
                    "running scenario={} fsync={} concurrency={} duration={}s pool={}",
                    scenario.label(),
                    fsync.label(),
                    concurrency,
                    config.duration.as_secs(),
                    config.pool_size
                );
                let pool = environment
                    .migrated_pool(database_sequence, config.pool_size)
                    .await?;
                let point = run_point(
                    PointConfig {
                        scenario,
                        fsync,
                        pool_size: config.pool_size,
                        concurrency,
                        duration: config.duration,
                        host_cpus,
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
                results.push(point);
            }
        }
        environment.admin_pool.close().await;
    }

    print_results(&results);
    Ok(())
}
