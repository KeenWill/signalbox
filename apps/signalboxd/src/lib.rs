//! Hub-owned composition between turn activation and model execution.
//!
//! docs/spec/turn-lifecycle-and-scheduling.md owns this composition-root role.
//! The scheduler pass below hands each complete activated-turn outcome to a
//! fresh execution invocation; concrete provider selection remains an
//! injected composition choice.

use std::{error::Error, fmt, future::Future};

use signalbox_application::{
    ClassifyOperatorFailure, EligibilityPass, InProcessAttemptDispatchGate,
    InProcessToolDispatchGate, ModelCallExecutionError, ModelCallExecutionOutcome,
    ModelCallExecutionService, ModelCallProvider, OperatorFailureClass, ScriptedModelCallError,
    ScriptedModelCallProvider, ScriptedModelCallStep, StartEligibleTurnIdGenerator,
    StartEligibleTurnOutcome, StartEligibleTurnService, StartEligibleTurnTransaction, ToolCatalog,
    ToolExecutionService, ToolExecutionServiceError, ToolExecutionServiceOutcome, ToolExecutor,
    UuidV7ModelCallExecutionIdGenerator, UuidV7ToolLoopIdGenerator,
};
use signalbox_domain::{ActivatedAcceptedInputTurn, AssistantText, SessionId, TurnId};
use signalbox_persistence::model_execution::{
    ModelCallRepositoryError, PostgresModelCallRepository,
};
use signalbox_persistence::tool_loop::{PostgresToolLoopRepository, ToolLoopRepositoryError};
use tokio::sync::watch;

use tracing::Instrument;
mod configuration;
mod context_guard;
mod daemon_tools;
mod fenced_database;
mod local_socket;
mod process_runtime;
mod session_template_configuration;
mod single_hub;

pub use configuration::{
    ANTHROPIC_CREDENTIAL_REFERENCE, FileCredentialAccess, HubModelConfiguration,
    HubModelConfigurationError,
};
pub use context_guard::{ContextGuardedTurnPass, ContextGuardedTurnPassError};
pub use daemon_tools::{
    DaemonToolCatalog, DaemonToolExecutor, DaemonToolExecutorError, DaemonTools,
    DaemonToolsConstructionError,
};
pub use fenced_database::{FencedHubDatabase, FencedHubDatabaseError};
pub use local_socket::{LocalProcessListener, LocalSocketError};
pub use process_runtime::{ProcessProviderTextDeltaSink, ProcessRuntime, ProcessRuntimeError};
pub use session_template_configuration::{
    ResolvedSessionTemplate, SessionTemplateConfiguration, SessionTemplateConfigurationError,
};
pub use signalbox_tools_basic::{
    CurrentTimeClock, CurrentTimeExecutor, CurrentTimeExecutorError, CurrentTimeTool,
    CurrentTimeToolConstructionError, EchoExecutor, EchoExecutorError, EchoTool,
    EchoToolConstructionError, PostgresSessionStatusWriter, PostgresSessionStatusWriterError,
    ReqwestWebFetchConstructionError, ReqwestWebFetchTransport, SessionStatusExecutor,
    SessionStatusExecutorError, SessionStatusTool, SessionStatusToolConstructionError,
    SessionStatusWrite, SessionStatusWriteOutcome, SessionStatusWriter, SystemCurrentTimeClock,
    WebFetchBodyCompleteness, WebFetchEgressPolicy, WebFetchEgressPolicyError, WebFetchExecutor,
    WebFetchExecutorError, WebFetchRequest, WebFetchResponse, WebFetchTool,
    WebFetchToolConstructionError, WebFetchTransport, WebFetchTransportFailure,
};
pub use signalbox_tools_code_host::{
    CHANGE_REQUEST_CHANGED_FILES_NAME, CHANGE_REQUEST_CHECKS_STATUS_NAME,
    CHANGE_REQUEST_CI_JOB_LOG_NAME, CHANGE_REQUEST_COMMENT_NAME,
    CHANGE_REQUEST_CONVERGENCE_STATE_NAME, CHANGE_REQUEST_FILE_PATCH_NAME,
    CHANGE_REQUEST_RERUN_FAILED_JOBS_NAME, CHANGE_REQUEST_REVIEW_THREADS_NAME,
    CHANGE_REQUEST_STACK_STATE_NAME, CHANGE_REQUEST_SUMMARY_NAME,
    CHANGE_REQUEST_THREAD_INVENTORY_NAME, CHANGE_REQUEST_THREAD_REPLY_NAME,
    CHANGE_REQUEST_THREAD_RESOLVE_NAME, CODE_HOST_CREDENTIAL_REFERENCE, CODE_HOST_TOOL_NAMES,
    ChangeRequestCommentArguments, ChangeRequestCommentResult, ChangeRequestSummaryArguments,
    ChangeRequestSummaryFields, ChangeRequestSummaryResult, ChangedFile, ChangedFilesArguments,
    ChangedFilesResult, CheckStatus, ChecksStatusArguments, ChecksStatusResult, ChildStackState,
    CiJobLogArguments, CiJobLogResult, CodeHostChangeRequestNumber, CodeHostCommentBody,
    CodeHostCursor, CodeHostExecutor, CodeHostExecutorError, CodeHostFilePath, CodeHostOpaqueId,
    CodeHostOperation, CodeHostRepository, CodeHostResult, CodeHostResultCompleteness,
    CodeHostRevision, CodeHostTools, CodeHostToolsConstructionError, CodeHostTransport,
    CodeHostTransportFailure, ConvergenceStateArguments, ConvergenceStateFields,
    ConvergenceStateResult, ConvergenceVerdict, FilePatchArguments, FilePatchResult,
    GitHubCodeHostConstructionError, GitHubCodeHostTransport, REVIEW_GATE_CHECK_NAME,
    RerunFailedJobsArguments, RerunFailedJobsResult, ReviewAuthorClass, ReviewCheck,
    ReviewDispositionClass, ReviewGateBlockerCode, ReviewGateCheckArguments, ReviewGateCheckResult,
    ReviewGatePurpose, ReviewThread, ReviewThreadComment, ReviewThreadFields, ReviewThreadIdentity,
    ReviewThreadInventoryFields, ReviewThreadInventoryItem, ReviewThreadResolution,
    ReviewThreadsArguments, ReviewThreadsResult, ReviewerVerdictEvidence, ReviewerVerdictFields,
    ReviewerVerdictStatus, StackStateArguments, StackStateFields, StackStateResult,
    ThreadInventoryArguments, ThreadInventoryResult, ThreadReplyArguments, ThreadReplyResult,
    ThreadResolveArguments, ThreadResolveResult,
};
pub use single_hub::{SingleHubGuard, SingleHubGuardError};

/// Per-activation model execution constructed by the hub composition root.
pub trait ActivatedTurnExecution {
    /// Classified failure from the application service or provider adapter.
    type Error: ClassifyOperatorFailure + Send + 'static;

    /// Consumes one exact activation outcome and drives its initial call.
    fn execute(
        &self,
        activated: Box<ActivatedAcceptedInputTurn>,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static;

    /// Reconciles a durable active tool turn for one scheduler hint.
    ///
    /// Implementations without tool orchestration have no active work to
    /// resume. PostgreSQL authority is rechecked by the concrete adapter.
    fn resume_active(
        &self,
        _session: SessionId,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
        std::future::ready(Ok(()))
    }

    /// Reports whether a failed active-turn resume may require startup
    /// recovery rather than ordinary scheduler retry.
    ///
    /// The fail-safe default treats every failure as potentially
    /// post-mutation. Implementations may return `false` only for a stage
    /// proven not to have entered durable execution.
    fn active_resume_failure_requires_recovery(_error: &Self::Error) -> bool {
        true
    }

    /// Reports that durable activation may require startup recovery.
    fn report_post_activation_failure(&self) {}
}

/// Cheap-clone handle that raises the daemon's fatal recovery signal.
///
/// The scheduler pass reaches the signal through its execution role, but the
/// connection runtime has no execution role and still observes durable
/// outcomes the running process cannot decide. Both raise the same signal
/// through this one handle rather than growing a second recovery mechanism.
#[derive(Clone, Debug)]
pub struct FatalRecoveryReporter {
    fatal_signal: watch::Sender<bool>,
}

impl FatalRecoveryReporter {
    /// Reports that durable state may require startup recovery.
    pub fn report_recovery_required(&self) {
        self.fatal_signal.send_replace(true);
    }
}

/// Cloneable signal raised when an activated turn may require recovery.
#[derive(Clone, Debug)]
pub struct FatalExecutionSignal {
    triggered: watch::Receiver<bool>,
}

impl FatalExecutionSignal {
    /// Waits until an activated-turn execution reports failure.
    pub async fn wait(&self) {
        let mut triggered = self.triggered.clone();
        while !*triggered.borrow_and_update() {
            if triggered.changed().await.is_err() {
                std::future::pending::<()>().await;
            }
        }
    }

    /// Reports whether activated-turn execution has failed.
    pub fn is_triggered(&self) -> bool {
        *self.triggered.borrow()
    }
}

/// Raises a fatal runtime signal when durable activation may require recovery.
///
/// The hub composition root uses the signal to stop scheduling and exit, so
/// startup recovery can regain authority over the active durable turn.
#[derive(Clone, Debug)]
pub struct FatalExecutionSupervisor<Execution> {
    execution: Execution,
    fatal_signal: watch::Sender<bool>,
}

impl<Execution> FatalExecutionSupervisor<Execution> {
    /// Returns a handle raising the same fatal signal this supervisor raises.
    pub fn recovery_reporter(&self) -> FatalRecoveryReporter {
        FatalRecoveryReporter {
            fatal_signal: self.fatal_signal.clone(),
        }
    }

    /// Wraps one execution role and returns its independently awaitable signal.
    pub fn new(execution: Execution) -> (Self, FatalExecutionSignal) {
        let (fatal_signal, triggered) = watch::channel(false);
        (
            Self {
                execution,
                fatal_signal,
            },
            FatalExecutionSignal { triggered },
        )
    }
}

impl<Execution> ActivatedTurnExecution for FatalExecutionSupervisor<Execution>
where
    Execution: ActivatedTurnExecution + 'static,
    Execution::Error: 'static,
{
    type Error = Execution::Error;

    fn execute(
        &self,
        activated: Box<ActivatedAcceptedInputTurn>,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
        let execution = self.execution.execute(activated);
        supervise_execution(self.fatal_signal.clone(), execution)
    }

    fn resume_active(
        &self,
        session: SessionId,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
        let execution = self.execution.resume_active(session);
        supervise_active_resume::<Execution, _>(self.fatal_signal.clone(), execution)
    }

    fn active_resume_failure_requires_recovery(error: &Self::Error) -> bool {
        Execution::active_resume_failure_requires_recovery(error)
    }

    fn report_post_activation_failure(&self) {
        self.recovery_reporter().report_recovery_required();
    }
}

async fn supervise_execution<Execution, ExecutionError>(
    fatal_signal: watch::Sender<bool>,
    execution: Execution,
) -> Result<(), ExecutionError>
where
    Execution: Future<Output = Result<(), ExecutionError>>,
{
    let fatal_on_drop = FatalOnIncompleteExecution(Some(fatal_signal));
    let result = execution.await;
    if result.is_ok() {
        fatal_on_drop.disarm();
    }
    result
}

async fn supervise_active_resume<Execution, Resume>(
    fatal_signal: watch::Sender<bool>,
    resume: Resume,
) -> Result<(), Execution::Error>
where
    Execution: ActivatedTurnExecution,
    Resume: Future<Output = Result<(), Execution::Error>>,
{
    let fatal_on_drop = FatalOnIncompleteExecution(Some(fatal_signal));
    let result = resume.await;
    let requires_recovery = match &result {
        Ok(()) => false,
        Err(error) => Execution::active_resume_failure_requires_recovery(error),
    };
    if !requires_recovery {
        fatal_on_drop.disarm();
    }
    result
}

async fn reconcile_retained_once<Outcome, ExecutionError, Execution>(
    original_error: ExecutionError,
    execution: Execution,
) -> Result<Outcome, RetainedExecutionError<ExecutionError>>
where
    Execution: Future<Output = Result<Outcome, ExecutionError>>,
{
    match execution.await {
        Ok(outcome) => Ok(outcome),
        Err(reconciliation_error) => Err(RetainedExecutionError::Reconciliation {
            original: original_error,
            reconciliation: reconciliation_error,
        }),
    }
}

/// Execution failure retaining both the causal stage error and a failed
/// same-incarnation retained-state reconciliation.
#[derive(Debug)]
pub enum RetainedExecutionError<ExecutionError> {
    /// Execution failed without a retained-state reconciliation failure.
    Primary(ExecutionError),
    /// The causal stage failed and its one authoritative reconciliation also
    /// failed.
    Reconciliation {
        /// Failure that created the retained evidence obligation.
        original: ExecutionError,
        /// Failure discovered by the authoritative reconciliation pass.
        reconciliation: ExecutionError,
    },
}

impl<ExecutionError> RetainedExecutionError<ExecutionError> {
    /// Borrows the causal stage failure.
    pub const fn original(&self) -> &ExecutionError {
        match self {
            Self::Primary(error)
            | Self::Reconciliation {
                original: error, ..
            } => error,
        }
    }

    /// Borrows the later reconciliation failure when one occurred.
    pub const fn reconciliation(&self) -> Option<&ExecutionError> {
        match self {
            Self::Primary(_) => None,
            Self::Reconciliation { reconciliation, .. } => Some(reconciliation),
        }
    }
}

impl<ExecutionError> fmt::Display for RetainedExecutionError<ExecutionError>
where
    ExecutionError: fmt::Display,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Primary(error) => error.fmt(formatter),
            Self::Reconciliation {
                original,
                reconciliation,
            } => write!(
                formatter,
                "{original}; retained-state reconciliation also failed: {reconciliation}"
            ),
        }
    }
}

impl<ExecutionError> Error for RetainedExecutionError<ExecutionError>
where
    ExecutionError: Error + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(self.original())
    }
}

impl<ExecutionError> ClassifyOperatorFailure for RetainedExecutionError<ExecutionError>
where
    ExecutionError: ClassifyOperatorFailure,
{
    fn operator_failure_class(&self) -> OperatorFailureClass {
        let original = self.original().operator_failure_class();
        let Some(reconciliation) = self
            .reconciliation()
            .map(ClassifyOperatorFailure::operator_failure_class)
        else {
            return original;
        };
        if is_fatal_failure_class(reconciliation) {
            reconciliation
        } else {
            original
        }
    }

    fn operator_failure_cause_code(&self) -> &'static str {
        let original = self.original().operator_failure_cause_code();
        let Some(reconciliation) = self.reconciliation() else {
            return original;
        };
        if is_fatal_failure_class(reconciliation.operator_failure_class()) {
            reconciliation.operator_failure_cause_code()
        } else {
            original
        }
    }
}

/// Backwards-compatible name for retained model-call execution evidence.
pub type RetainedModelExecutionError<ExecutionError> = RetainedExecutionError<ExecutionError>;

const fn is_fatal_failure_class(failure: OperatorFailureClass) -> bool {
    matches!(
        failure,
        OperatorFailureClass::FailClosedCorruption | OperatorFailureClass::CallerOrHubBug
    )
}

struct FatalOnIncompleteExecution(Option<watch::Sender<bool>>);

impl FatalOnIncompleteExecution {
    fn disarm(mut self) {
        self.0 = None;
    }
}

impl Drop for FatalOnIncompleteExecution {
    fn drop(&mut self) {
        if let Some(fatal_signal) = self.0.take() {
            fatal_signal.send_replace(true);
        }
    }
}

/// Scheduler-pass failure retaining whether activation or execution failed.
#[derive(Debug)]
pub enum ActivatedTurnPassError<ActivationError, ExecutionError> {
    /// The authoritative activation transaction failed.
    Activation(ActivationError),
    /// A transaction, capability, or provider stage failed after activation.
    Execution {
        /// Selected turn, absent when active-turn recovery failed before selection.
        turn: Option<TurnId>,
        /// Typed application failure.
        source: ExecutionError,
    },
    /// The transaction returned an activation for another hinted session.
    ActivationSessionMismatch {
        /// Turn selected by the mismatched activation.
        turn: TurnId,
    },
}

impl<ActivationError, ExecutionError> fmt::Display
    for ActivatedTurnPassError<ActivationError, ExecutionError>
where
    ActivationError: fmt::Display,
    ExecutionError: fmt::Display,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Activation(error) => write!(formatter, "turn activation failed: {error}"),
            Self::Execution { source, .. } => {
                write!(formatter, "activated turn execution failed: {source}")
            }
            Self::ActivationSessionMismatch { .. } => {
                formatter.write_str("turn activation returned a different session")
            }
        }
    }
}

impl<ActivationError, ExecutionError> Error
    for ActivatedTurnPassError<ActivationError, ExecutionError>
where
    ActivationError: Error + 'static,
    ExecutionError: Error + 'static,
{
}

impl<ActivationError, ExecutionError> ClassifyOperatorFailure
    for ActivatedTurnPassError<ActivationError, ExecutionError>
where
    ActivationError: ClassifyOperatorFailure,
    ExecutionError: ClassifyOperatorFailure,
{
    fn operator_failure_class(&self) -> signalbox_application::OperatorFailureClass {
        match self {
            Self::Activation(error) => error.operator_failure_class(),
            Self::Execution { source, .. } => source.operator_failure_class(),
            Self::ActivationSessionMismatch { .. } => {
                signalbox_application::OperatorFailureClass::CallerOrHubBug
            }
        }
    }

    fn operator_failure_cause_code(&self) -> &'static str {
        match self {
            Self::Activation(error) => error.operator_failure_cause_code(),
            Self::Execution { source, .. } => source.operator_failure_cause_code(),
            Self::ActivationSessionMismatch { .. } => "activation_session_mismatch",
        }
    }
}

/// Authoritative eligibility pass followed by per-activation model execution.
#[derive(Clone, Debug)]
pub struct ActivatedTurnPass<Generator, Transaction, Execution> {
    activation: StartEligibleTurnService<Generator, Transaction>,
    execution: Execution,
}

impl<Generator, Transaction, Execution> ActivatedTurnPass<Generator, Transaction, Execution> {
    /// Composes the existing activation service with an execution factory.
    pub const fn new(
        activation: StartEligibleTurnService<Generator, Transaction>,
        execution: Execution,
    ) -> Self {
        Self {
            activation,
            execution,
        }
    }

    /// Returns both owned composition roles.
    pub fn into_parts(self) -> (StartEligibleTurnService<Generator, Transaction>, Execution) {
        (self.activation, self.execution)
    }
}

impl<Generator, Transaction, Execution> EligibilityPass
    for ActivatedTurnPass<Generator, Transaction, Execution>
where
    Generator: StartEligibleTurnIdGenerator + Send + 'static,
    Transaction: StartEligibleTurnTransaction + Clone + Send + 'static,
    Transaction::Error: ClassifyOperatorFailure + Send + 'static,
    Execution: ActivatedTurnExecution + Clone + Send + 'static,
    Execution::Error: Send + 'static,
{
    type Error = ActivatedTurnPassError<Transaction::Error, Execution::Error>;
    fn failure_stage(error: &Self::Error) -> &'static str {
        match error {
            ActivatedTurnPassError::Activation(_) => "activation",
            ActivatedTurnPassError::Execution { turn: None, .. } => "active_turn_recovery",
            ActivatedTurnPassError::Execution { turn: Some(_), .. } => "execution",
            ActivatedTurnPassError::ActivationSessionMismatch { .. } => "activation_correlation",
        }
    }

    fn failure_turn(error: &Self::Error) -> Option<TurnId> {
        match error {
            ActivatedTurnPassError::Activation(_) => None,
            ActivatedTurnPassError::Execution { turn, .. } => *turn,
            ActivatedTurnPassError::ActivationSessionMismatch { turn } => Some(*turn),
        }
    }

    fn run(
        &mut self,
        session: SessionId,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
        let activation = self.activation.execute_with_cloned_transaction(session);
        let execution = self.execution.clone();
        async move {
            execution
                .resume_active(session)
                .await
                .map_err(|source| ActivatedTurnPassError::Execution { turn: None, source })?;
            let outcome = match activation.await {
                Ok(outcome) => outcome,
                Err(error) => {
                    report_ambiguous_commit(&execution, &error);
                    return Err(ActivatedTurnPassError::Activation(error));
                }
            };
            match outcome {
                StartEligibleTurnOutcome::NoEligibleTurn => Ok(()),
                StartEligibleTurnOutcome::Activated(activated) => {
                    let turn = activated.turn();
                    if !activation_session_matches(&execution, session, activated.session()) {
                        return Err(ActivatedTurnPassError::ActivationSessionMismatch { turn });
                    }
                    execution
                        .execute(activated)
                        .instrument(turn_work_span(session, turn))
                        .await
                        .map_err(|source| ActivatedTurnPassError::Execution {
                            turn: Some(turn),
                            source,
                        })
                }
            }
        }
    }
}
/// Creates one turn child span beneath the scheduler's session span.
///
/// The hierarchy follows one selected turn through orchestration and keeps
/// stable names and fields for a future OpenTelemetry layer. Both values are
/// daemon-minted identities; no conversation content or adapter prose enters
/// this span.
fn turn_work_span(session: SessionId, turn: TurnId) -> tracing::Span {
    tracing::info_span!(
        "turn_work",
        session_id = %session.as_uuid(),
        turn_id = %turn.as_uuid(),
    )
}

/// Reports one classified failure whose durable commit outcome is unknown, so
/// startup recovery rather than ordinary scheduler retry regains authority.
///
/// `OperatorFailureClass::Infrastructure { commit_ambiguous: true }` is the
/// declared class for exactly that state. Every eligibility pass able to
/// observe it owes the same reported outcome, so the reaction is defined once
/// here instead of being restated — and diverging — per pass.
pub(crate) fn report_ambiguous_commit<Execution, Failure>(execution: &Execution, error: &Failure)
where
    Execution: ActivatedTurnExecution,
    Failure: ClassifyOperatorFailure,
{
    if commit_outcome_is_unknown(error) {
        execution.report_post_activation_failure();
    }
}

/// Whether one classified failure left a durable commit outcome the running
/// process cannot decide.
///
/// Surfaces without an execution role apply this to the same declared class
/// before raising the same signal, so the question has one answer wherever it
/// is asked.
pub(crate) fn commit_outcome_is_unknown(error: &impl ClassifyOperatorFailure) -> bool {
    matches!(
        error.operator_failure_class(),
        signalbox_application::OperatorFailureClass::Infrastructure {
            commit_ambiguous: true
        }
    )
}

fn activation_session_matches<Execution>(
    execution: &Execution,
    expected: SessionId,
    actual: SessionId,
) -> bool
where
    Execution: ActivatedTurnExecution,
{
    if actual == expected {
        true
    } else {
        execution.report_post_activation_failure();
        false
    }
}

/// Concrete error from the scripted PostgreSQL execution composition.
type PostgresScriptedModelExecutionStageError = ModelCallExecutionError<
    ModelCallRepositoryError,
    ModelCallRepositoryError,
    ModelCallRepositoryError,
    ScriptedModelCallError,
    ModelCallRepositoryError,
>;

/// Classified failure from scripted PostgreSQL execution, including a failed
/// same-incarnation retained-state reconciliation when one occurred.
pub type PostgresScriptedModelExecutionError =
    RetainedModelExecutionError<PostgresScriptedModelExecutionStageError>;

/// Classified provider execution failure, including a failed same-incarnation
/// retained-state reconciliation when one occurred.
pub type PostgresProviderModelExecutionError<ProviderError> = RetainedModelExecutionError<
    ModelCallExecutionError<
        ModelCallRepositoryError,
        ModelCallRepositoryError,
        ModelCallRepositoryError,
        ProviderError,
        ModelCallRepositoryError,
    >,
>;

/// Classified tool execution failure, including a failed same-incarnation
/// reconciliation of retained executor evidence.
pub type PostgresProviderToolExecutionError<ExecutorError> =
    RetainedExecutionError<ToolExecutionServiceError<ToolLoopRepositoryError, ExecutorError>>;

/// Classified failure while alternating provider calls and serialized tool
/// stages within one turn.
#[derive(Debug)]
pub enum PostgresProviderToolLoopExecutionError<ProviderError, ExecutorError> {
    /// Read-only active-turn lookup failed before durable execution began.
    ResumeLookup(ToolLoopRepositoryError),
    /// Model-call execution or same-incarnation reconciliation failed.
    Model(Box<PostgresProviderModelExecutionError<ProviderError>>),
    /// Tool preparation, execution, evidence commit, or continuation failed.
    Tool(Box<PostgresProviderToolExecutionError<ExecutorError>>),
}

impl<ProviderError, ExecutorError> fmt::Display
    for PostgresProviderToolLoopExecutionError<ProviderError, ExecutorError>
where
    ProviderError: fmt::Display,
    ExecutorError: fmt::Display,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ResumeLookup(error) => error.fmt(formatter),
            Self::Model(error) => error.fmt(formatter),
            Self::Tool(error) => error.fmt(formatter),
        }
    }
}

impl<ProviderError, ExecutorError> Error
    for PostgresProviderToolLoopExecutionError<ProviderError, ExecutorError>
where
    ProviderError: Error + 'static,
    ExecutorError: Error + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::ResumeLookup(error) => Some(error),
            Self::Model(error) => Some(error),
            Self::Tool(error) => Some(error),
        }
    }
}

impl<ProviderError, ExecutorError> ClassifyOperatorFailure
    for PostgresProviderToolLoopExecutionError<ProviderError, ExecutorError>
where
    ProviderError: ClassifyOperatorFailure,
    ExecutorError: ClassifyOperatorFailure,
{
    fn operator_failure_class(&self) -> OperatorFailureClass {
        match self {
            Self::ResumeLookup(error) => error.operator_failure_class(),
            Self::Model(error) => error.operator_failure_class(),
            Self::Tool(error) => error.operator_failure_class(),
        }
    }

    fn operator_failure_cause_code(&self) -> &'static str {
        match self {
            Self::ResumeLookup(_) => "tool_loop_resume_lookup",
            Self::Model(error) => error.operator_failure_cause_code(),
            Self::Tool(error) => error.operator_failure_cause_code(),
        }
    }
}

/// Production execution factory over PostgreSQL orchestration and one cloned
/// provider-port adapter per activation.
#[derive(Clone, Debug)]
pub struct PostgresProviderModelExecution<Provider> {
    repository: PostgresModelCallRepository,
    gate: InProcessAttemptDispatchGate,
    provider: Provider,
}

impl<Provider> PostgresProviderModelExecution<Provider> {
    /// Supplies shared persistence, the per-attempt gate, and provider port.
    pub const fn new(
        repository: PostgresModelCallRepository,
        gate: InProcessAttemptDispatchGate,
        provider: Provider,
    ) -> Self {
        Self {
            repository,
            gate,
            provider,
        }
    }

    /// Adds serialized tool execution and continuation to the provider
    /// composition without changing the provider-facing application boundary.
    pub fn with_tool_loop<Catalog, Executor>(
        self,
        tool_dispatch_gate: InProcessToolDispatchGate,
        catalog: Catalog,
        executor: Executor,
    ) -> PostgresProviderToolLoopExecution<Provider, Catalog, Executor> {
        let tool_repository = self.repository.tool_loop_repository();
        PostgresProviderToolLoopExecution {
            model_repository: self.repository,
            tool_repository,
            model_gate: self.gate,
            tool_gate: tool_dispatch_gate,
            provider: self.provider,
            catalog,
            executor,
        }
    }
}

impl<Provider> ActivatedTurnExecution for PostgresProviderModelExecution<Provider>
where
    Provider: ModelCallProvider + Clone + Send + 'static,
    Provider::Capability: Send,
    Provider::Error: Send + 'static,
{
    type Error = PostgresProviderModelExecutionError<Provider::Error>;

    fn execute(
        &self,
        activated: Box<ActivatedAcceptedInputTurn>,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
        let repository = self.repository.clone();
        let gate = self.gate.clone();
        let provider = self.provider.clone();
        async move {
            let session = activated.session();
            drop(activated);
            let mut service = ModelCallExecutionService::new(
                UuidV7ModelCallExecutionIdGenerator,
                repository.clone(),
                repository.clone(),
                repository.clone(),
                repository,
                provider,
                gate,
            );
            loop {
                let outcome = match service.execute(session).await {
                    Ok(outcome) => outcome,
                    Err(error) if service.retained_state().is_some() => {
                        // Preserve same-incarnation evidence for one
                        // authoritative reconciliation pass before fatal
                        // supervision hands authority to startup recovery.
                        reconcile_retained_once(error, service.execute(session)).await?
                    }
                    Err(error) => return Err(RetainedModelExecutionError::Primary(error)),
                };
                match outcome {
                    ModelCallExecutionOutcome::Checkpointed(_) => continue,
                    ModelCallExecutionOutcome::NoWork
                    | ModelCallExecutionOutcome::TargetUnavailable(_)
                    | ModelCallExecutionOutcome::PendingSteering { .. }
                    | ModelCallExecutionOutcome::CapabilityKnownFailure(_)
                    | ModelCallExecutionOutcome::CapabilityFailureAlreadyCommitted(_)
                    | ModelCallExecutionOutcome::ObservationCommitted(_)
                    | ModelCallExecutionOutcome::ObservationAlreadyCommitted(_) => return Ok(()),
                }
            }
        }
    }
}

/// Production execution factory alternating provider calls with serialized
/// PostgreSQL-backed tool stages until the turn parks or terminalizes.
#[derive(Clone, Debug)]
pub struct PostgresProviderToolLoopExecution<Provider, Catalog, Executor> {
    model_repository: PostgresModelCallRepository,
    tool_repository: PostgresToolLoopRepository,
    model_gate: InProcessAttemptDispatchGate,
    tool_gate: InProcessToolDispatchGate,
    provider: Provider,
    catalog: Catalog,
    executor: Executor,
}

impl<Provider, Catalog, Executor> PostgresProviderToolLoopExecution<Provider, Catalog, Executor>
where
    Provider: ModelCallProvider + Clone + Send + 'static,
    Provider::Capability: Send,
    Provider::Error: Send + 'static,
    Catalog: ToolCatalog + Clone + Send + 'static,
    Executor: ToolExecutor + Clone + Send + 'static,
    Executor::Error: Send + 'static,
{
    fn execute_scope(
        &self,
        session: SessionId,
        turn: signalbox_domain::TurnId,
    ) -> impl Future<
        Output = Result<
            (),
            PostgresProviderToolLoopExecutionError<Provider::Error, Executor::Error>,
        >,
    > + Send
    + 'static {
        let model_repository = self.model_repository.clone();
        let tool_repository = self.tool_repository.clone();
        let model_gate = self.model_gate.clone();
        let tool_gate = self.tool_gate.clone();
        let provider = self.provider.clone();
        let catalog = self.catalog.clone();
        let executor = self.executor.clone();
        async move {
            let mut model = ModelCallExecutionService::new(
                UuidV7ModelCallExecutionIdGenerator,
                model_repository.clone(),
                model_repository.clone(),
                model_repository.clone(),
                model_repository,
                provider,
                model_gate,
            )
            .with_tool_catalog(catalog.clone());
            let mut tools = ToolExecutionService::new(
                UuidV7ToolLoopIdGenerator,
                tool_repository,
                catalog,
                executor,
                tool_gate,
            );
            let mut run_tools = true;
            let mut return_if_tools_absent = false;

            loop {
                if run_tools {
                    let tool_outcome = match tools.execute(session, turn).await {
                        Ok(outcome) => outcome,
                        Err(error) if tools.retained_state().is_some() => {
                            reconcile_retained_once(error, tools.execute(session, turn))
                                .await
                                .map_err(|error| {
                                    PostgresProviderToolLoopExecutionError::Tool(Box::new(error))
                                })?
                        }
                        Err(error) => {
                            return Err(PostgresProviderToolLoopExecutionError::Tool(Box::new(
                                RetainedExecutionError::Primary(error),
                            )));
                        }
                    };
                    match tool_outcome {
                        ToolExecutionServiceOutcome::AttemptCheckpointed(_)
                        | ToolExecutionServiceOutcome::PreflightFailed(_)
                        | ToolExecutionServiceOutcome::ObservationCommitted(_)
                        | ToolExecutionServiceOutcome::ObservationAlreadyCommitted(_)
                        | ToolExecutionServiceOutcome::CrashClassified(_) => {
                            return_if_tools_absent = true;
                            continue;
                        }
                        ToolExecutionServiceOutcome::ContinuationCheckpointed(_) => {
                            run_tools = false;
                        }
                        ToolExecutionServiceOutcome::NoWork => {
                            if return_if_tools_absent {
                                return Ok(());
                            }
                            run_tools = false;
                        }
                        ToolExecutionServiceOutcome::AwaitingApproval(_)
                        | ToolExecutionServiceOutcome::AwaitingRecovery(_)
                        | ToolExecutionServiceOutcome::ContinuationTargetUnavailable(_) => {
                            return Ok(());
                        }
                    }
                }

                let model_outcome = match model.execute(session).await {
                    Ok(outcome) => outcome,
                    Err(error) if model.retained_state().is_some() => {
                        reconcile_retained_once(error, model.execute(session))
                            .await
                            .map_err(|error| {
                                PostgresProviderToolLoopExecutionError::Model(Box::new(error))
                            })?
                    }
                    Err(error) => {
                        return Err(PostgresProviderToolLoopExecutionError::Model(Box::new(
                            RetainedModelExecutionError::Primary(error),
                        )));
                    }
                };
                match model_outcome {
                    ModelCallExecutionOutcome::Checkpointed(_) => {}
                    ModelCallExecutionOutcome::TargetUnavailable(_)
                    | ModelCallExecutionOutcome::PendingSteering { .. }
                    | ModelCallExecutionOutcome::CapabilityKnownFailure(_)
                    | ModelCallExecutionOutcome::CapabilityFailureAlreadyCommitted(_) => {
                        return Ok(());
                    }
                    ModelCallExecutionOutcome::NoWork => return Ok(()),
                    ModelCallExecutionOutcome::ObservationCommitted(_)
                    | ModelCallExecutionOutcome::ObservationAlreadyCommitted(_) => {
                        run_tools = true;
                        return_if_tools_absent = true;
                    }
                }
            }
        }
    }
}

impl<Provider, Catalog, Executor> ActivatedTurnExecution
    for PostgresProviderToolLoopExecution<Provider, Catalog, Executor>
where
    Provider: ModelCallProvider + Clone + Send + 'static,
    Provider::Capability: Send,
    Provider::Error: Send + 'static,
    Catalog: ToolCatalog + Clone + Send + 'static,
    Executor: ToolExecutor + Clone + Send + 'static,
    Executor::Error: Send + 'static,
{
    type Error = PostgresProviderToolLoopExecutionError<Provider::Error, Executor::Error>;

    fn execute(
        &self,
        activated: Box<ActivatedAcceptedInputTurn>,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
        let session = activated.session();
        let turn = activated.turn();
        drop(activated);
        self.execute_scope(session, turn)
    }

    fn resume_active(
        &self,
        session: SessionId,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
        let tool_repository = self.tool_repository.clone();
        let execution = self.clone();
        async move {
            let turn = tool_repository
                .find_resumable_turn(session)
                .await
                .map_err(PostgresProviderToolLoopExecutionError::ResumeLookup)?;
            match turn {
                Some(turn) => execution.execute_scope(session, turn).await,
                None => Ok(()),
            }
        }
    }

    fn active_resume_failure_requires_recovery(error: &Self::Error) -> bool {
        !matches!(
            error,
            PostgresProviderToolLoopExecutionError::ResumeLookup(_)
        )
    }
}

/// Debug/test-only execution factory using the deterministic scripted provider.
#[derive(Clone, Debug)]
pub struct PostgresScriptedModelExecution {
    repository: PostgresModelCallRepository,
    gate: InProcessAttemptDispatchGate,
    assistant_reply: AssistantText,
}

impl PostgresScriptedModelExecution {
    /// Supplies shared persistence, dispatch gate, and one exact scripted reply.
    pub const fn new(
        repository: PostgresModelCallRepository,
        gate: InProcessAttemptDispatchGate,
        assistant_reply: AssistantText,
    ) -> Self {
        Self {
            repository,
            gate,
            assistant_reply,
        }
    }
}

impl ActivatedTurnExecution for PostgresScriptedModelExecution {
    type Error = PostgresScriptedModelExecutionError;

    fn execute(
        &self,
        activated: Box<ActivatedAcceptedInputTurn>,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
        let repository = self.repository.clone();
        let gate = self.gate.clone();
        let assistant_reply = self.assistant_reply.clone();
        async move {
            let session = activated.session();
            drop(activated);
            let mut service = ModelCallExecutionService::new(
                UuidV7ModelCallExecutionIdGenerator,
                repository.clone(),
                repository.clone(),
                repository.clone(),
                repository,
                ScriptedModelCallProvider::new([ScriptedModelCallStep::Return(
                    signalbox_domain::ModelCallTerminalObservation::Completed {
                        assistant_text: vec![assistant_reply],
                    },
                )]),
                gate,
            );
            loop {
                let outcome = match service.execute(session).await {
                    Ok(outcome) => outcome,
                    Err(error) if service.retained_state().is_some() => {
                        // docs/spec/model-call-execution.md gives
                        // same-incarnation evidence one authoritative
                        // reconciliation pass before fatal supervision hands
                        // authority to startup recovery. A second failure
                        // does not replace the causal stage error that
                        // created the retained obligation.
                        reconcile_retained_once(error, service.execute(session)).await?
                    }
                    Err(error) => return Err(RetainedModelExecutionError::Primary(error)),
                };
                match outcome {
                    ModelCallExecutionOutcome::Checkpointed(_) => continue,
                    ModelCallExecutionOutcome::NoWork
                    | ModelCallExecutionOutcome::TargetUnavailable(_)
                    | ModelCallExecutionOutcome::PendingSteering { .. }
                    | ModelCallExecutionOutcome::CapabilityKnownFailure(_)
                    | ModelCallExecutionOutcome::CapabilityFailureAlreadyCommitted(_)
                    | ModelCallExecutionOutcome::ObservationCommitted(_)
                    | ModelCallExecutionOutcome::ObservationAlreadyCommitted(_) => return Ok(()),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fmt,
        future::{Future, pending, ready},
        sync::{Arc, Mutex},
    };

    use signalbox_application::{
        ClassifyOperatorFailure, EligibilityPass, OperatorFailureClass,
        StartEligibleTurnIdGenerator, StartEligibleTurnOutcome, StartEligibleTurnService,
        StartEligibleTurnTransaction,
    };
    use signalbox_domain::{
        AcceptedInputTurnActivationIdentities, ActivatedAcceptedInputTurn, ContextFrontierId,
        SemanticTranscriptEntryId, SessionId, TurnAttemptId,
    };
    use tokio::sync::watch;
    use uuid::Uuid;

    use super::{
        ActivatedTurnExecution, ActivatedTurnPass, FatalExecutionSignal, FatalExecutionSupervisor,
        activation_session_matches, reconcile_retained_once, supervise_execution,
    };

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    struct ExecutionFailure;

    impl fmt::Display for ExecutionFailure {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("execution failure")
        }
    }

    impl std::error::Error for ExecutionFailure {}

    impl ClassifyOperatorFailure for ExecutionFailure {
        fn operator_failure_class(&self) -> OperatorFailureClass {
            OperatorFailureClass::CallerOrHubBug
        }
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    struct CommitAmbiguousActivationFailure;

    impl fmt::Display for CommitAmbiguousActivationFailure {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("activation commit acknowledgement was lost")
        }
    }

    impl std::error::Error for CommitAmbiguousActivationFailure {}

    impl ClassifyOperatorFailure for CommitAmbiguousActivationFailure {
        fn operator_failure_class(&self) -> OperatorFailureClass {
            OperatorFailureClass::Infrastructure {
                commit_ambiguous: true,
            }
        }
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum StagedExecutionFailure {
        Infrastructure,
        Corruption,
        CallerBug,
    }

    impl fmt::Display for StagedExecutionFailure {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str(match self {
                Self::Infrastructure => "initial infrastructure failure",
                Self::Corruption => "reconciliation corruption",
                Self::CallerBug => "reconciliation caller bug",
            })
        }
    }

    impl std::error::Error for StagedExecutionFailure {}

    impl ClassifyOperatorFailure for StagedExecutionFailure {
        fn operator_failure_class(&self) -> OperatorFailureClass {
            match self {
                Self::Infrastructure => OperatorFailureClass::Infrastructure {
                    commit_ambiguous: false,
                },
                Self::Corruption => OperatorFailureClass::FailClosedCorruption,
                Self::CallerBug => OperatorFailureClass::CallerOrHubBug,
            }
        }

        fn operator_failure_cause_code(&self) -> &'static str {
            match self {
                Self::Infrastructure => "initial_execution",
                Self::Corruption => "reconciliation_corruption",
                Self::CallerBug => "reconciliation_caller_bug",
            }
        }
    }

    #[derive(Debug)]
    struct AdvancingIds {
        next: u128,
    }

    impl AdvancingIds {
        const fn new() -> Self {
            Self { next: 1 }
        }

        fn take(&mut self) -> Uuid {
            let value = self.next;
            self.next += 1;
            Uuid::from_u128(value)
        }
    }

    impl StartEligibleTurnIdGenerator for AdvancingIds {
        fn next_model_identity_entry_id(&mut self) -> SemanticTranscriptEntryId {
            SemanticTranscriptEntryId::from_uuid(self.take())
        }

        fn next_origin_entry_id(&mut self) -> SemanticTranscriptEntryId {
            SemanticTranscriptEntryId::from_uuid(self.take())
        }

        fn next_starting_frontier_id(&mut self) -> ContextFrontierId {
            ContextFrontierId::from_uuid(self.take())
        }

        fn next_initial_attempt_id(&mut self) -> TurnAttemptId {
            TurnAttemptId::from_uuid(self.take())
        }
    }

    #[derive(Clone, Debug, Default)]
    struct RecordingTransaction {
        observed: Arc<Mutex<Vec<AcceptedInputTurnActivationIdentities>>>,
    }

    impl StartEligibleTurnTransaction for RecordingTransaction {
        type Error = ExecutionFailure;

        fn handle(
            &mut self,
            _session: SessionId,
            identities: AcceptedInputTurnActivationIdentities,
        ) -> impl Future<Output = Result<StartEligibleTurnOutcome, Self::Error>> + Send {
            self.observed
                .lock()
                .expect("recording transaction lock")
                .push(identities);
            ready(Ok(StartEligibleTurnOutcome::NoEligibleTurn))
        }
    }

    #[derive(Clone, Copy, Debug)]
    struct CommitAmbiguousTransaction;

    impl StartEligibleTurnTransaction for CommitAmbiguousTransaction {
        type Error = CommitAmbiguousActivationFailure;

        fn handle(
            &mut self,
            _session: SessionId,
            _identities: AcceptedInputTurnActivationIdentities,
        ) -> impl Future<Output = Result<StartEligibleTurnOutcome, Self::Error>> + Send {
            ready(Err(CommitAmbiguousActivationFailure))
        }
    }

    #[derive(Clone, Copy, Debug)]
    struct NoopExecution;

    impl ActivatedTurnExecution for NoopExecution {
        type Error = ExecutionFailure;

        fn execute(
            &self,
            _activated: Box<ActivatedAcceptedInputTurn>,
        ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
            ready(Ok(()))
        }
    }

    #[derive(Clone, Copy, Debug)]
    struct ReadOnlyResumeFailureExecution;

    impl ActivatedTurnExecution for ReadOnlyResumeFailureExecution {
        type Error = ExecutionFailure;

        fn execute(
            &self,
            _activated: Box<ActivatedAcceptedInputTurn>,
        ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
            ready(Ok(()))
        }

        fn resume_active(
            &self,
            _session: SessionId,
        ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
            ready(Err(ExecutionFailure))
        }

        fn active_resume_failure_requires_recovery(_error: &Self::Error) -> bool {
            false
        }
    }

    #[derive(Clone, Copy, Debug)]
    struct PostMutationResumeFailureExecution;

    impl ActivatedTurnExecution for PostMutationResumeFailureExecution {
        type Error = ExecutionFailure;

        fn execute(
            &self,
            _activated: Box<ActivatedAcceptedInputTurn>,
        ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
            ready(Ok(()))
        }

        fn resume_active(
            &self,
            _session: SessionId,
        ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
            ready(Err(ExecutionFailure))
        }
    }

    #[derive(Clone, Debug)]
    struct OrderedResumeTransaction {
        events: Arc<Mutex<Vec<&'static str>>>,
    }

    impl StartEligibleTurnTransaction for OrderedResumeTransaction {
        type Error = ExecutionFailure;

        fn handle(
            &mut self,
            _session: SessionId,
            _identities: AcceptedInputTurnActivationIdentities,
        ) -> impl Future<Output = Result<StartEligibleTurnOutcome, Self::Error>> + Send {
            self.events
                .lock()
                .expect("ordered event lock")
                .push("activate");
            ready(Ok(StartEligibleTurnOutcome::NoEligibleTurn))
        }
    }

    #[derive(Clone, Debug)]
    struct RecordingResumeExecution {
        events: Arc<Mutex<Vec<&'static str>>>,
    }

    impl ActivatedTurnExecution for RecordingResumeExecution {
        type Error = ExecutionFailure;

        fn execute(
            &self,
            _activated: Box<ActivatedAcceptedInputTurn>,
        ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
            ready(Ok(()))
        }

        fn resume_active(
            &self,
            _session: SessionId,
        ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
            self.events
                .lock()
                .expect("ordered event lock")
                .push("resume");
            ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn active_tool_reconciliation_precedes_queued_activation() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let mut pass = ActivatedTurnPass::new(
            StartEligibleTurnService::new(
                AdvancingIds::new(),
                OrderedResumeTransaction {
                    events: Arc::clone(&events),
                },
            ),
            RecordingResumeExecution {
                events: Arc::clone(&events),
            },
        );

        pass.run(SessionId::from_uuid(Uuid::from_u128(9)))
            .await
            .expect("active reconciliation and activation check both finish");

        assert_eq!(
            *events.lock().expect("ordered event lock"),
            ["resume", "activate"]
        );
    }

    #[tokio::test]
    async fn repeated_passes_advance_the_owned_identity_generator() {
        let transaction = RecordingTransaction::default();
        let observed = Arc::clone(&transaction.observed);
        let mut pass = ActivatedTurnPass::new(
            StartEligibleTurnService::new(AdvancingIds::new(), transaction),
            NoopExecution,
        );
        let session = SessionId::from_uuid(Uuid::from_u128(9));

        pass.run(session).await.expect("first pass succeeds");
        pass.run(session).await.expect("second pass succeeds");

        let observed = observed.lock().expect("recording transaction lock");
        assert_eq!(observed.len(), 2);
        assert_ne!(observed[0], observed[1]);
    }

    #[tokio::test]
    async fn inv034_commit_ambiguous_activation_raises_the_fatal_recovery_signal() {
        let (execution, signal) = FatalExecutionSupervisor::new(NoopExecution);
        let mut pass = ActivatedTurnPass::new(
            StartEligibleTurnService::new(AdvancingIds::new(), CommitAmbiguousTransaction),
            execution,
        );

        let error = pass
            .run(SessionId::from_uuid(Uuid::from_u128(9)))
            .await
            .expect_err("a lost commit acknowledgement remains an activation failure");

        assert!(matches!(
            error,
            super::ActivatedTurnPassError::Activation(CommitAmbiguousActivationFailure)
        ));
        assert!(signal.is_triggered());
    }

    #[test]
    fn activation_session_mismatch_raises_the_fatal_signal() {
        let (execution, signal) = FatalExecutionSupervisor::new(NoopExecution);

        assert!(!activation_session_matches(
            &execution,
            SessionId::from_uuid(Uuid::from_u128(1)),
            SessionId::from_uuid(Uuid::from_u128(2)),
        ));
        assert!(signal.is_triggered());
    }

    #[tokio::test]
    async fn post_activation_failure_raises_the_fatal_signal() {
        let (fatal_signal, triggered) = watch::channel(false);
        let signal = FatalExecutionSignal { triggered };
        assert_eq!(
            supervise_execution(fatal_signal, ready(Err(ExecutionFailure))).await,
            Err(ExecutionFailure)
        );
        signal.wait().await;
        assert!(signal.is_triggered());
    }

    #[tokio::test]
    async fn read_only_active_resume_failure_remains_an_ordinary_pass_error() {
        let (execution, signal) = FatalExecutionSupervisor::new(ReadOnlyResumeFailureExecution);

        assert_eq!(
            execution
                .resume_active(SessionId::from_uuid(Uuid::from_u128(9)))
                .await,
            Err(ExecutionFailure)
        );
        assert!(!signal.is_triggered());
    }

    #[tokio::test]
    async fn post_mutation_active_resume_failure_raises_the_fatal_signal() {
        let (execution, signal) = FatalExecutionSupervisor::new(PostMutationResumeFailureExecution);

        assert_eq!(
            execution
                .resume_active(SessionId::from_uuid(Uuid::from_u128(9)))
                .await,
            Err(ExecutionFailure)
        );
        signal.wait().await;
        assert!(signal.is_triggered());
    }

    #[tokio::test]
    #[allow(
        clippy::panic,
        reason = "the test deliberately exercises unwind supervision"
    )]
    async fn activated_execution_unwind_raises_the_fatal_signal() {
        let (fatal_signal, triggered) = watch::channel(false);
        let signal = FatalExecutionSignal { triggered };
        let execution = tokio::spawn(supervise_execution(fatal_signal, async {
            panic!("simulated activated-turn execution unwind");
            #[allow(unreachable_code)]
            Ok::<(), ExecutionFailure>(())
        }));

        assert!(execution.await.is_err());
        signal.wait().await;
        assert!(signal.is_triggered());
    }

    #[tokio::test]
    async fn retained_reconciliation_preserves_cause_and_reports_fatal_classification() {
        let primary =
            super::RetainedModelExecutionError::Primary(StagedExecutionFailure::Infrastructure);
        assert_eq!(primary.operator_failure_cause_code(), "initial_execution");

        let corruption = reconcile_retained_once(
            StagedExecutionFailure::Infrastructure,
            ready(Err::<(), _>(StagedExecutionFailure::Corruption)),
        )
        .await
        .expect_err("the corruption reconciliation also fails");
        assert_reconciliation_preserves_cause_and_reports_classification(
            corruption,
            StagedExecutionFailure::Corruption,
            OperatorFailureClass::FailClosedCorruption,
            "reconciliation_corruption",
        );

        let caller_bug = reconcile_retained_once(
            StagedExecutionFailure::Infrastructure,
            ready(Err::<(), _>(StagedExecutionFailure::CallerBug)),
        )
        .await
        .expect_err("the caller-bug reconciliation also fails");
        assert_reconciliation_preserves_cause_and_reports_classification(
            caller_bug,
            StagedExecutionFailure::CallerBug,
            OperatorFailureClass::CallerOrHubBug,
            "reconciliation_caller_bug",
        );
    }

    #[track_caller]
    fn assert_reconciliation_preserves_cause_and_reports_classification(
        error: super::RetainedModelExecutionError<StagedExecutionFailure>,
        reconciliation: StagedExecutionFailure,
        expected_class: OperatorFailureClass,
        expected_cause: &'static str,
    ) {
        assert_eq!(error.original(), &StagedExecutionFailure::Infrastructure);
        assert_eq!(error.reconciliation(), Some(&reconciliation));
        assert_eq!(error.operator_failure_class(), expected_class);
        assert_eq!(error.operator_failure_cause_code(), expected_cause);
    }

    #[tokio::test]
    async fn cancelled_activated_execution_raises_the_fatal_signal() {
        let (fatal_signal, triggered) = watch::channel(false);
        let signal = FatalExecutionSignal { triggered };
        let entered = Arc::new(tokio::sync::Notify::new());
        let execution_entered = Arc::clone(&entered);
        let execution = tokio::spawn(supervise_execution(fatal_signal, async move {
            execution_entered.notify_one();
            pending::<Result<(), ExecutionFailure>>().await
        }));
        entered.notified().await;

        execution.abort();
        assert!(
            execution
                .await
                .expect_err("the execution task is cancelled")
                .is_cancelled()
        );
        signal.wait().await;
        assert!(signal.is_triggered());
    }
}
