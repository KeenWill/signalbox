//! Local assistant-reply harness.
//!
//! This binary is deliberately not a client protocol; the client process
//! protocol remains future work. It accepts either one deterministic reply
//! or an explicit Anthropic smoke mode, runs the real scheduler and
//! PostgreSQL path, then prints the resulting semantic transcript.

use std::{
    env,
    error::Error,
    ffi::OsString,
    fmt,
    future::{Future, pending},
    path::PathBuf,
    process::ExitCode,
    time::Duration,
};

use signalbox_application::{
    ClassifyOperatorFailure, CreateSessionOutcome, CreateSessionRequest, CreateSessionService,
    EligibilityNudge, EligibilityNudgeOutcome, EligibilityPass, EligibilityWorkSource,
    InProcessAttemptDispatchGate, ModelCallCredentialReference, OperatorFailureClass,
    SchedulerLoop, StartEligibleTurnService, SubmitInputOutcome, SubmitInputRequest,
    SubmitInputService, UuidV7SessionIdGenerator, UuidV7StartEligibleTurnIdGenerator,
    UuidV7SubmitInputIdGenerator,
};
use signalbox_domain::{
    AssistantText, DeliveryRequest, DirectModelSelection, DurableCommandId, ModelSelectionOverride,
    ModelSelectionRequest, ModelTargetCatalog, ModelTargetDefinition, PerInputConfigurationChoices,
    ProviderModelIdentity, ResolvedProviderTarget, SessionConfigurationDefaults,
    SessionConfigurationDefaultsVersion, SessionId, SubmitInputAppliedResult, SubmitInputResult,
    TurnId, UserContent,
};
use signalbox_model_provider_runtime::RuntimeModelCallProvider;
use signalbox_model_runtime::CredentialReference;
use signalbox_model_runtime_anthropic::{AnthropicConfig, AnthropicRuntime};
use signalbox_persistence::{
    SessionCredentialPin, SessionModelCredential, create_session::CreateSessionRepository,
    local_test_connection_options, migrate, model_execution::PostgresModelCallRepository,
    start_eligible_turn::StartEligibleTurnRepository, submit_input::SubmitInputRepository,
};
use signalboxd::{
    ActivatedTurnPass, FatalExecutionSignal, FatalExecutionSupervisor, FileCredentialAccess,
    HubModelConfiguration, ModelAdapter, PostgresProviderModelExecution,
    PostgresScriptedModelExecution, WorkspaceInstructionPreparedExecution,
    WorkspaceInstructionRuntime,
};
use sqlx::postgres::PgPoolOptions;
use tokio::{
    sync::{oneshot, watch},
    task::JoinHandle,
    time::timeout,
};
use uuid::Uuid;

const DATABASE_URL_ENVIRONMENT: &str = "SIGNALBOX_DEBUG_DATABASE_URL";
const MODEL_CONFIGURATION_FILE_ENVIRONMENT: &str = "SIGNALBOX_CONFIG_FILE";
const TRANSCRIPT_WAIT: Duration = Duration::from_secs(120);
const SCHEDULER_SHUTDOWN_WAIT: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DebugDriverError {
    Usage,
    Configuration,
    Database,
    InvalidText,
    CreateSession,
    SubmitInput,
    UnexpectedOutcome,
    TranscriptTimeout,
    Scheduler,
}

impl fmt::Display for DebugDriverError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Usage => {
                "set SIGNALBOX_DEBUG_DATABASE_URL and pass INPUT_TEXT SCRIPTED_REPLY, or pass --anthropic SELECTION_UUID INPUT_TEXT with SIGNALBOX_CONFIG_FILE"
            }
            Self::Configuration => "debug provider configuration is invalid",
            Self::Database => "debug database operation failed",
            Self::InvalidText => "input or scripted reply is not admitted text",
            Self::CreateSession => "debug session creation failed",
            Self::SubmitInput => "debug input submission failed",
            Self::UnexpectedOutcome => "debug command returned an unexpected durable outcome",
            Self::TranscriptTimeout => "timed out waiting for a terminal transcript",
            Self::Scheduler => "debug scheduler task failed",
        })
    }
}

impl Error for DebugDriverError {}

struct DebugArguments {
    database_url: String,
    input: String,
    provider: DebugProvider,
}

enum DebugProvider {
    Scripted {
        reply: String,
    },
    Anthropic {
        selection: DirectModelSelection,
        model_configuration_file: PathBuf,
    },
}

#[derive(Clone, Copy, Debug)]
struct DroppedDebugNudge;

impl EligibilityNudge for DroppedDebugNudge {
    fn nudge(&self, _session: SessionId) -> EligibilityNudgeOutcome {
        EligibilityNudgeOutcome::DroppedAtCapacity
    }
}

#[derive(Clone, Copy, Debug)]
struct DebugSessionWorkSource {
    pending: Option<SessionId>,
}

impl DebugSessionWorkSource {
    const fn new(session: SessionId) -> Self {
        Self {
            pending: Some(session),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DebugWorkSourceError {}

impl fmt::Display for DebugWorkSourceError {
    fn fmt(&self, _formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {}
    }
}

impl Error for DebugWorkSourceError {}

impl ClassifyOperatorFailure for DebugWorkSourceError {
    fn operator_failure_class(&self) -> OperatorFailureClass {
        match *self {}
    }
}

impl EligibilityWorkSource for DebugSessionWorkSource {
    type Error = DebugWorkSourceError;

    fn next(&mut self) -> impl Future<Output = Result<SessionId, Self::Error>> + Send {
        let session = self.pending.take();
        async move {
            match session {
                Some(session) => Ok(session),
                None => pending().await,
            }
        }
    }
}

#[derive(Clone, Debug)]
struct DebugPassFailureSignal {
    triggered: watch::Receiver<bool>,
}

impl DebugPassFailureSignal {
    async fn wait(&self) {
        let mut triggered = self.triggered.clone();
        while !*triggered.borrow_and_update() {
            if triggered.changed().await.is_err() {
                pending::<()>().await;
            }
        }
    }

    fn is_triggered(&self) -> bool {
        *self.triggered.borrow()
    }
}

#[derive(Clone, Debug)]
struct ObservableDebugPass<Pass> {
    pass: Pass,
    failure: watch::Sender<bool>,
}

impl<Pass> ObservableDebugPass<Pass> {
    fn new(pass: Pass) -> (Self, DebugPassFailureSignal) {
        let (failure, triggered) = watch::channel(false);
        (Self { pass, failure }, DebugPassFailureSignal { triggered })
    }
}

impl<Pass> EligibilityPass for ObservableDebugPass<Pass>
where
    Pass: EligibilityPass,
{
    type Error = Pass::Error;

    fn occupancy_expiry_handler(
        &self,
    ) -> Option<std::sync::Arc<dyn signalbox_application::SchedulerPassExpiryHandler>> {
        self.pass.occupancy_expiry_handler()
    }

    fn run(
        &mut self,
        session: SessionId,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
        let execution = self.pass.run(session);
        let failure = self.failure.clone();
        async move {
            let result = execution.await;
            if result.is_err() {
                failure.send_replace(true);
            }
            result
        }
    }
}

impl DebugArguments {
    fn from_process() -> Result<Self, DebugDriverError> {
        let database_url = env::var(DATABASE_URL_ENVIRONMENT)
            .map_err(|_| DebugDriverError::Usage)
            .and_then(|value| {
                if value.is_empty() {
                    Err(DebugDriverError::Usage)
                } else {
                    Ok(value)
                }
            })?;
        let mut arguments = env::args().skip(1);
        let first = arguments.next().ok_or(DebugDriverError::Usage)?;
        if first == "--anthropic" {
            let selection = arguments
                .next()
                .and_then(|value| Uuid::parse_str(&value).ok())
                .map(DirectModelSelection::from_uuid)
                .ok_or(DebugDriverError::Usage)?;
            let input = arguments.next().ok_or(DebugDriverError::Usage)?;
            if arguments.next().is_some() {
                return Err(DebugDriverError::Usage);
            }
            Ok(Self {
                database_url,
                input,
                provider: DebugProvider::Anthropic {
                    selection,
                    model_configuration_file: required_environment_path(
                        MODEL_CONFIGURATION_FILE_ENVIRONMENT,
                    )?,
                },
            })
        } else {
            let reply = arguments.next().ok_or(DebugDriverError::Usage)?;
            if arguments.next().is_some() {
                return Err(DebugDriverError::Usage);
            }
            Ok(Self {
                database_url,
                input: first,
                provider: DebugProvider::Scripted { reply },
            })
        }
    }
}

fn required_environment_path(name: &str) -> Result<PathBuf, DebugDriverError> {
    let value = env::var_os(name).ok_or(DebugDriverError::Usage)?;
    if value == OsString::new() {
        Err(DebugDriverError::Usage)
    } else {
        Ok(PathBuf::from(value))
    }
}

type TranscriptRow = (String, Option<String>, Option<String>);

async fn poll_terminal_transcript(
    pool: &sqlx::PgPool,
    session: SessionId,
    turn: TurnId,
) -> Result<Vec<TranscriptRow>, DebugDriverError> {
    loop {
        let rows = sqlx::query_as::<_, TranscriptRow>(
            "SELECT entry.payload_kind,
                    accepted_part.text_value,
                    entry.assistant_text_value
               FROM turn_lifecycle AS lifecycle
               JOIN context_frontier_member AS member
                 ON member.owning_session_id = lifecycle.session_id
                AND member.context_frontier_id = lifecycle.terminal_frontier_id
               JOIN semantic_transcript_entry AS entry
                 ON entry.source_session_id = member.source_session_id
                AND entry.semantic_entry_id = member.semantic_entry_id
               LEFT JOIN accepted_input AS accepted
                 ON accepted.session_id = entry.source_session_id
                AND accepted.accepted_input_id = entry.origin_accepted_input_id
               LEFT JOIN accepted_input_content_part AS accepted_part
                 ON accepted_part.accepted_input_id = accepted.accepted_input_id
                AND accepted_part.position = 0
                AND accepted_part.part_kind = 'text'
              WHERE lifecycle.session_id = $1
                AND lifecycle.turn_id = $2
                AND lifecycle.state_kind = 'terminal'
              ORDER BY member.member_position",
        )
        .bind(session.into_uuid())
        .bind(turn.into_uuid())
        .fetch_all(pool)
        .await
        .map_err(|_| DebugDriverError::Database)?;
        if !rows.is_empty() {
            return Ok(rows);
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

fn print_transcript(rows: Vec<TranscriptRow>) {
    for (kind, user_text, assistant_text) in rows {
        match (kind.as_str(), user_text, assistant_text) {
            ("origin_accepted_input", Some(text), None) => {
                println!("{}", format_transcript_text("user", &text));
            }
            ("assistant_text", None, Some(text)) => {
                println!("{}", format_transcript_text("assistant", &text));
            }
            _ => println!("event: {kind}"),
        }
    }
}

fn format_transcript_text(role: &str, text: &str) -> String {
    format!("{role}: {text:?}")
}

async fn drive_scheduler<WorkSource, Pass>(
    mut scheduler: SchedulerLoop<WorkSource, Pass>,
    fatal_execution: FatalExecutionSignal,
    pass_failure: DebugPassFailureSignal,
    pool: &sqlx::PgPool,
    session: SessionId,
    turn: TurnId,
) -> Result<Vec<TranscriptRow>, DebugDriverError>
where
    WorkSource: EligibilityWorkSource + Send + 'static,
    WorkSource::Error: ClassifyOperatorFailure,
    Pass: EligibilityPass + Clone + Send + 'static,
    Pass::Error: ClassifyOperatorFailure + Send + 'static,
{
    let (shutdown_sender, shutdown_receiver) = oneshot::channel();
    let fatal_shutdown = fatal_execution.clone();
    let pass_failure_shutdown = pass_failure.clone();
    let scheduler_task = tokio::spawn(async move {
        scheduler
            .run_until(async move {
                tokio::select! {
                    _ = shutdown_receiver => {}
                    () = fatal_shutdown.wait() => {}
                    () = pass_failure_shutdown.wait() => {}
                }
            })
            .await
    });
    let fatal_observation = fatal_execution.clone();
    let pass_failure_observation = pass_failure.clone();
    let transcript = timeout(TRANSCRIPT_WAIT, async {
        tokio::select! {
            transcript = poll_terminal_transcript(pool, session, turn) => transcript,
            () = fatal_observation.wait() => Err(DebugDriverError::Scheduler),
            () = pass_failure_observation.wait() => Err(DebugDriverError::Scheduler),
        }
    })
    .await;
    stop_scheduler(shutdown_sender, scheduler_task).await?;
    if fatal_execution.is_triggered() || pass_failure.is_triggered() {
        return Err(DebugDriverError::Scheduler);
    }
    transcript.map_err(|_| DebugDriverError::TranscriptTimeout)?
}

async fn stop_scheduler(
    shutdown_sender: oneshot::Sender<()>,
    mut scheduler_task: JoinHandle<signalbox_application::SchedulerLoopExit>,
) -> Result<(), DebugDriverError> {
    let _ = shutdown_sender.send(());
    match timeout(SCHEDULER_SHUTDOWN_WAIT, &mut scheduler_task).await {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(_)) => Err(DebugDriverError::Scheduler),
        Err(_) => {
            scheduler_task.abort();
            Err(DebugDriverError::Scheduler)
        }
    }
}

async fn run(arguments: DebugArguments) -> Result<(), DebugDriverError> {
    let DebugArguments {
        database_url,
        input,
        provider,
    } = arguments;
    let content = UserContent::try_text(input).map_err(|_| DebugDriverError::InvalidText)?;
    let (
        selection,
        targets,
        credential_reference,
        credential_pin,
        credential_families,
        automatic_tool_round_limit,
        instruction_roots,
        provider,
    ) = match provider {
        DebugProvider::Scripted { reply } => {
            let selection = DirectModelSelection::from_uuid(Uuid::now_v7());
            let targets = ModelTargetCatalog::try_from_definitions([ModelTargetDefinition::new(
                selection,
                ResolvedProviderTarget::naming(ProviderModelIdentity::from_uuid(Uuid::now_v7())),
            )])
            .map_err(|_| DebugDriverError::UnexpectedOutcome)?;
            (
                selection,
                targets,
                ModelCallCredentialReference::new("scripted-test"),
                SessionCredentialPin::try_new(vec![SessionModelCredential::new(
                    "scripted-debug",
                    "scripted-test",
                )])
                .map_err(|_| DebugDriverError::Configuration)?,
                // The scripted provider routes no real family. It carries no
                // catalog at all rather than an empty one: an empty catalog
                // resolves no family and fails the call as corruption, while
                // `None` is what selects the fallback reference.
                None,
                None,
                Vec::new(),
                DebugProviderRuntime::Scripted(
                    AssistantText::try_new(reply).map_err(|_| DebugDriverError::InvalidText)?,
                ),
            )
        }
        DebugProvider::Anthropic {
            selection,
            model_configuration_file,
        } => {
            let configuration = HubModelConfiguration::read(&model_configuration_file)
                .map_err(|_| DebugDriverError::Configuration)?;
            require_anthropic_selection(&configuration, selection)?;
            let credential_profile = configuration
                .resolve_direct_model(selection)
                .ok_or(DebugDriverError::Configuration)?
                .credential_profile()
                .to_owned();
            let credential_access = FileCredentialAccess::from_files(
                configuration
                    .file_credential_profiles(ModelAdapter::Anthropic)
                    .map(|(reference, path)| {
                        (CredentialReference::new(reference), path.to_path_buf())
                    }),
            );
            let credential_reference = ModelCallCredentialReference::new(credential_profile);
            let native_message_limit = configuration
                .numeric_bounds()
                .integer("max_native_message_bytes")
                .ok_or(DebugDriverError::Configuration)?
                .map(usize::try_from)
                .transpose()
                .map_err(|_| DebugDriverError::Configuration)?;
            let mut adapter_configuration = AnthropicConfig::new(native_message_limit);
            adapter_configuration.provider_compaction_targets =
                configuration.anthropic_provider_compaction_targets();
            adapter_configuration.exchange_timeout = configuration
                .numeric_bounds()
                .duration("model_exchange_timeout")
                .ok_or(DebugDriverError::Configuration)?;
            adapter_configuration.model_capabilities =
                configuration.runtime_model_capability_catalog();
            let runtime = AnthropicRuntime::new(adapter_configuration, credential_access)
                .map_err(|_| DebugDriverError::Configuration)?;
            let diagnostic_model_identity_limit = configuration
                .numeric_bounds()
                .integer("diagnostic_model_identity_limit")
                .flatten()
                .and_then(|value| usize::try_from(value).ok());
            let automatic_tool_round_limit = configuration
                .numeric_bounds()
                .integer("max_automatic_tool_rounds_per_turn")
                .ok_or(DebugDriverError::Configuration)?
                .map(usize::try_from)
                .transpose()
                .map_err(|_| DebugDriverError::Configuration)?;
            let provider = RuntimeModelCallProvider::new(
                runtime,
                configuration.runtime_model_catalog(),
                diagnostic_model_identity_limit,
            );
            let instruction_roots = configuration.workspace_instructions().roots().to_vec();
            (
                selection,
                configuration.target_catalog(),
                credential_reference,
                configuration.session_credential_pin(),
                Some(configuration.credential_family_catalog()),
                automatic_tool_round_limit,
                instruction_roots,
                DebugProviderRuntime::Anthropic(provider),
            )
        }
    };
    let connection_options =
        local_test_connection_options(&database_url).map_err(|_| DebugDriverError::Database)?;
    let pool = PgPoolOptions::new()
        .max_connections(8)
        .connect_with(connection_options)
        .await
        .map_err(|_| DebugDriverError::Database)?;
    migrate(&pool)
        .await
        .map_err(|_| DebugDriverError::Database)?;

    let mut create = CreateSessionService::new(
        UuidV7SessionIdGenerator,
        CreateSessionRepository::new(pool.clone(), credential_pin),
    );
    let CreateSessionOutcome::Applied(created) = create
        .execute(
            CreateSessionRequest::try_new(
                DurableCommandId::from_uuid(Uuid::now_v7()),
                SessionConfigurationDefaults::new(ModelSelectionRequest::Direct(selection)),
            )
            .map_err(|_| DebugDriverError::CreateSession)?,
        )
        .await
        .map_err(|_| DebugDriverError::CreateSession)?
    else {
        return Err(DebugDriverError::UnexpectedOutcome);
    };
    let session = created.session();

    let mut submit = SubmitInputService::new(
        UuidV7SubmitInputIdGenerator,
        SubmitInputRepository::new(pool.clone()),
        DroppedDebugNudge,
        signalbox_application::InProcessToolDispatchGate::default(),
    );
    let SubmitInputOutcome::Recorded(SubmitInputResult::Applied(
        SubmitInputAppliedResult::TurnOrigin(origin),
    )) = submit
        .execute(
            SubmitInputRequest::try_new(
                DurableCommandId::from_uuid(Uuid::now_v7()),
                session,
                content,
                DeliveryRequest::StartWhenNoActiveTurn {
                    configuration: PerInputConfigurationChoices::new(
                        SessionConfigurationDefaultsVersion::first(),
                        ModelSelectionOverride::UseSessionDefault,
                    ),
                },
            )
            .map_err(|_| DebugDriverError::SubmitInput)?,
        )
        .await
        .map_err(|_| DebugDriverError::SubmitInput)?
    else {
        return Err(DebugDriverError::UnexpectedOutcome);
    };
    let turn = origin.turn();
    let work_source = DebugSessionWorkSource::new(session);

    // The diagnostic composes the same session-credential catalog production
    // does. Without it a model whose enabled fast mode routes to an alternate
    // serving target resolves the base family's profile while the runtime
    // switches families, so the call would authenticate — and bill — against
    // an account the route did not select.
    let repository = PostgresModelCallRepository::new(pool.clone(), targets, credential_reference);
    let repository = match credential_families {
        Some(families) => repository.with_session_credentials(families),
        None => repository,
    };
    let activation = StartEligibleTurnService::new(
        UuidV7StartEligibleTurnIdGenerator,
        StartEligibleTurnRepository::new(pool.clone()),
    );
    let workspace_instructions =
        WorkspaceInstructionRuntime::new(pool.clone(), None, instruction_roots);
    let transcript = match provider {
        DebugProviderRuntime::Scripted(reply) => {
            let (execution, fatal_execution) =
                FatalExecutionSupervisor::new(WorkspaceInstructionPreparedExecution::new(
                    PostgresScriptedModelExecution::new(
                        repository,
                        InProcessAttemptDispatchGate::default(),
                        reply,
                    ),
                    workspace_instructions,
                ));
            let (pass, pass_failure) =
                ObservableDebugPass::new(ActivatedTurnPass::new(activation, execution));
            drive_scheduler(
                SchedulerLoop::new(work_source, pass),
                fatal_execution,
                pass_failure,
                &pool,
                session,
                turn,
            )
            .await?
        }
        DebugProviderRuntime::Anthropic(provider) => {
            let (execution, fatal_execution) =
                FatalExecutionSupervisor::new(WorkspaceInstructionPreparedExecution::new(
                    PostgresProviderModelExecution::new(
                        repository,
                        InProcessAttemptDispatchGate::default(),
                        provider,
                        automatic_tool_round_limit,
                    ),
                    workspace_instructions,
                ));
            let (pass, pass_failure) =
                ObservableDebugPass::new(ActivatedTurnPass::new(activation, execution));
            drive_scheduler(
                SchedulerLoop::new(work_source, pass),
                fatal_execution,
                pass_failure,
                &pool,
                session,
                turn,
            )
            .await?
        }
    };
    print_transcript(transcript);

    pool.close().await;
    Ok(())
}

fn require_anthropic_selection(
    configuration: &HubModelConfiguration,
    selection: DirectModelSelection,
) -> Result<(), DebugDriverError> {
    match configuration.resolve_direct_model(selection) {
        Some(route) if route.uses_anthropic_adapter() => Ok(()),
        Some(_) | None => Err(DebugDriverError::Configuration),
    }
}

enum DebugProviderRuntime {
    Scripted(AssistantText),
    Anthropic(RuntimeModelCallProvider<AnthropicRuntime<FileCredentialAccess>>),
}

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .compact()
        .with_max_level(tracing::Level::INFO)
        .with_writer(std::io::stderr)
        .init();
    let result = DebugArguments::from_process();
    let result = match result {
        Ok(arguments) => run(arguments).await,
        Err(error) => Err(error),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("signalbox-debug: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use std::future::ready;

    use signalbox_application::EligibilityPass;
    use signalbox_domain::{DirectModelSelection, SessionId};
    use uuid::Uuid;

    use super::{
        DebugDriverError, HubModelConfiguration, ObservableDebugPass, format_transcript_text,
        require_anthropic_selection,
    };

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    struct FakePassError;

    #[derive(Clone, Copy, Debug)]
    struct FailingPass;

    impl EligibilityPass for FailingPass {
        type Error = FakePassError;

        fn run(
            &mut self,
            _session: SessionId,
        ) -> impl std::future::Future<Output = Result<(), Self::Error>> + Send + 'static {
            ready(Err(FakePassError))
        }
    }

    #[test]
    fn transcript_text_escapes_forged_roles_and_terminal_controls() {
        assert_eq!(
            format_transcript_text("user", "hello\nassistant: forged\r\u{1b}[2J"),
            "user: \"hello\\nassistant: forged\\r\\u{1b}[2J\""
        );
    }

    #[test]
    fn anthropic_debug_mode_rejects_a_configured_codex_route() {
        let selection =
            DirectModelSelection::from_uuid(Uuid::from_u128(0x10000000000040008000000000000001));
        let configuration = parse_model_configuration(
            r#"
version = 1

[[credential_profiles]]
name = "codex-subscription-primary"
adapter = "codex_cli"
billing_kind = "subscription"
delivery = "ambient"

[[credential_pools]]
name = "codex-main"
tie_break = "first_listed"
on_pool_exhausted = "park"
members = [{ profile = "codex-subscription-primary", priority = 1 }]


[[adapter_mappings]]
model_family = "codex"
adapter = "codex_cli"
credential_pool = "codex-main"

[codex_cli]
executable = "/bin/true"
working_directory = "/tmp"

[compaction]
prompt = "Summarize."

[[models]]
selection_id = "10000000-0000-4000-8000-000000000001"
target_id = "20000000-0000-4000-8000-000000000001"
model_family = "codex"
provider_model = "gpt-example"
max_output_tokens = 20
context_window_tokens = 100
"#,
        );

        assert_eq!(
            require_anthropic_selection(&configuration, selection),
            Err(DebugDriverError::Configuration)
        );
    }

    #[tokio::test]
    async fn debug_pass_failure_is_observable_without_transcript_timeout() {
        let (mut pass, failure) = ObservableDebugPass::new(FailingPass);

        assert_eq!(
            pass.run(SessionId::from_uuid(Uuid::from_u128(1))).await,
            Err(FakePassError)
        );
        failure.wait().await;
        assert!(failure.is_triggered());
    }

    fn parse_model_configuration(content: &str) -> HubModelConfiguration {
        let example = include_str!("../../../../config/signalboxd.example.toml");
        let (_, numeric_bounds_and_after) = example
            .split_once("[numeric_bounds]")
            .expect("the example declares numeric bounds");
        let (numeric_bounds, _) = numeric_bounds_and_after
            .split_once("\n# Blob bytes live outside PostgreSQL.")
            .expect("the example terminates numeric bounds");
        HubModelConfiguration::parse(&format!("{content}\n[numeric_bounds]{numeric_bounds}\n"))
            .expect("the model configuration fixture is valid")
    }
}
