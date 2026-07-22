//! Local scripted assistant-reply harness.
//!
//! This binary is deliberately not the ADR-0019 client protocol. It accepts
//! one input and one deterministic reply, runs the real scheduler and
//! PostgreSQL path, then prints the resulting semantic transcript.

use std::{
    env,
    error::Error,
    fmt,
    future::{Future, pending},
    process::ExitCode,
    time::Duration,
};

use signalbox_application::{
    ClassifyOperatorFailure, CreateSessionOutcome, CreateSessionRequest, CreateSessionService,
    EligibilityNudge, EligibilityNudgeOutcome, EligibilityWorkSource, InProcessAttemptDispatchGate,
    OperatorFailureClass, SchedulerLoop, StartEligibleTurnService, SubmitInputOutcome,
    SubmitInputRequest, SubmitInputService, UuidV7SessionIdGenerator,
    UuidV7StartEligibleTurnIdGenerator, UuidV7SubmitInputIdGenerator,
};
use signalbox_domain::{
    AssistantText, DeliveryRequest, DirectModelSelection, DurableCommandId, ModelSelectionOverride,
    ModelSelectionRequest, ModelTargetCatalog, ModelTargetDefinition, PerInputConfigurationChoices,
    ProviderModelIdentity, ResolvedProviderTarget, SessionConfigurationDefaults,
    SessionConfigurationDefaultsVersion, SessionId, SubmitInputAppliedResult, SubmitInputResult,
    TurnId, UserContent,
};
use signalbox_hubd::{ActivatedTurnPass, FatalExecutionSupervisor, PostgresScriptedModelExecution};
use signalbox_persistence::{
    create_session::CreateSessionRepository, local_test_connection_options, migrate,
    model_execution::PostgresModelCallRepository, start_eligible_turn::StartEligibleTurnRepository,
    submit_input::SubmitInputRepository,
};
use sqlx::postgres::PgPoolOptions;
use tokio::{sync::oneshot, task::JoinHandle, time::timeout};
use uuid::Uuid;

const DATABASE_URL_ENVIRONMENT: &str = "SIGNALBOX_DEBUG_DATABASE_URL";
const TRANSCRIPT_WAIT: Duration = Duration::from_secs(15);
const SCHEDULER_SHUTDOWN_WAIT: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DebugDriverError {
    Usage,
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
            Self::Usage => "set SIGNALBOX_DEBUG_DATABASE_URL and pass INPUT_TEXT SCRIPTED_REPLY",
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
    reply: String,
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
        let input = arguments.next().ok_or(DebugDriverError::Usage)?;
        let reply = arguments.next().ok_or(DebugDriverError::Usage)?;
        if arguments.next().is_some() {
            return Err(DebugDriverError::Usage);
        }
        Ok(Self {
            database_url,
            input,
            reply,
        })
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
                    accepted.content_text,
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
            ("origin_accepted_input", Some(text), None) => println!("user: {text}"),
            ("assistant_text", None, Some(text)) => println!("assistant: {text}"),
            _ => println!("event: {kind}"),
        }
    }
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
    let connection_options = local_test_connection_options(&arguments.database_url)
        .map_err(|_| DebugDriverError::Database)?;
    let pool = PgPoolOptions::new()
        .max_connections(8)
        .connect_with(connection_options)
        .await
        .map_err(|_| DebugDriverError::Database)?;
    migrate(&pool)
        .await
        .map_err(|_| DebugDriverError::Database)?;

    let selection = DirectModelSelection::from_uuid(Uuid::now_v7());
    let targets = ModelTargetCatalog::try_from_definitions([ModelTargetDefinition::new(
        selection,
        ResolvedProviderTarget::naming(ProviderModelIdentity::from_uuid(Uuid::now_v7())),
    )])
    .map_err(|_| DebugDriverError::UnexpectedOutcome)?;
    let mut create = CreateSessionService::new(
        UuidV7SessionIdGenerator,
        CreateSessionRepository::new(pool.clone()),
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
    );
    let content =
        UserContent::try_text(arguments.input).map_err(|_| DebugDriverError::InvalidText)?;
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

    let gate = InProcessAttemptDispatchGate::default();
    let (execution, fatal_execution) =
        FatalExecutionSupervisor::new(PostgresScriptedModelExecution::new(
            PostgresModelCallRepository::new(pool.clone(), targets),
            gate,
            AssistantText::try_new(arguments.reply).map_err(|_| DebugDriverError::InvalidText)?,
        ));
    let pass = ActivatedTurnPass::new(
        StartEligibleTurnService::new(
            UuidV7StartEligibleTurnIdGenerator,
            StartEligibleTurnRepository::new(pool.clone()),
        ),
        execution,
    );
    let mut scheduler = SchedulerLoop::new(work_source, pass);
    let (shutdown_sender, shutdown_receiver) = oneshot::channel();
    let fatal_shutdown = fatal_execution.clone();
    let scheduler_task = tokio::spawn(async move {
        scheduler
            .run_until(async move {
                tokio::select! {
                    _ = shutdown_receiver => {}
                    () = fatal_shutdown.wait() => {}
                }
            })
            .await
    });

    let fatal_observation = fatal_execution.clone();
    let transcript = timeout(TRANSCRIPT_WAIT, async {
        tokio::select! {
            transcript = poll_terminal_transcript(&pool, session, turn) => transcript,
            () = fatal_observation.wait() => Err(DebugDriverError::Scheduler),
        }
    })
    .await;
    stop_scheduler(shutdown_sender, scheduler_task).await?;
    if fatal_execution.is_triggered() {
        return Err(DebugDriverError::Scheduler);
    }
    let transcript = transcript.map_err(|_| DebugDriverError::TranscriptTimeout)??;
    print_transcript(transcript);

    pool.close().await;
    Ok(())
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
