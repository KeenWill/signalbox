//! Exact pre-activation context-window guarding and automatic compaction.

use std::{error::Error, fmt, future::Future, sync::Arc};

use signalbox_application::{
    ClassifyOperatorFailure, EligibilityPass, InProcessEligibilityNudge, ModelCallInputTokenCount,
    ModelCallInputTokenCounter, OperatorFailureClass, SchedulerPassExpiryHandler, ToolCatalog,
};
use signalbox_domain::{
    AcceptedInputTurnActivationIdentities, ContextFrontierId, FailedModelCallTurnIdentities,
    ModelCallId, ResolvedContextFrontierSnapshot, SemanticTranscriptEntryId, SessionId,
    TurnAttemptId, TurnId, TurnTerminalCause,
};
use signalbox_model_provider_runtime::{ContextCompactionModel, RuntimeModelCatalog};
use signalbox_persistence::{
    goal::GoalExecutionFailureRecoveryCause,
    model_execution::{ModelCallRepositoryError, PostgresModelCallRepository},
    start_eligible_turn::{
        CommitActivationPreviewError, CommitActivationPreviewOutcome,
        CommitCompactionFailurePreviewOutcome, CommitCountedAttachmentFailurePreviewOutcome,
        PreparedActivationPreview, StartEligibleTurnRepository, StartEligibleTurnRepositoryError,
    },
};

use crate::{
    ActivatedTurnExecution, ExpiredPassRecoveryPolicy, HubModelConfiguration, ModelAdapter,
    SchedulerPassOccupancyRecovery, TurnPassExecutionStage, WorkspaceInstructionRuntime,
    WorkspaceInstructionRuntimeError,
    process_runtime::compact_automatically,
    report_ambiguous_commit,
    usage_limits::{
        ReportedInputCacheAxes, ReportedInputRetention, ReportedOutputRetention,
        reported_usage_requires_compaction,
    },
};
use tracing::Instrument;

const PROVIDER_COUNT_ADMISSION_PERCENT: u64 = 95;

/// Failure while reconciling provider-reported context growth before activation.
#[derive(Debug)]
pub enum ReportedUsageCompactionError {
    /// Read-only selection of the queued turn failed.
    Activation(StartEligibleTurnRepositoryError),
    /// Prospective operation or prior terminal usage could not be read.
    Model {
        /// Selected queued turn.
        turn: TurnId,
        /// Typed persistence failure.
        source: ModelCallRepositoryError,
    },
    /// The canonical prospective frontier could not be rendered.
    Render(TurnId),
    /// The prospective target was absent from immutable configuration.
    ContextWindowUnavailable(TurnId),
    /// The shared append-only compaction lifecycle failed.
    Compaction {
        /// Selected queued turn.
        turn: TurnId,
        /// Closed operator class retained across error erasure.
        failure_class: OperatorFailureClass,
        /// Closed operator cause retained across error erasure.
        cause_code: &'static str,
    },
    /// Closing the selected turn after compaction failure could not commit.
    CompactionFailureClosure {
        /// Selected queued turn.
        turn: TurnId,
        /// Typed activation-and-failure commit error.
        source: CommitActivationPreviewError,
    },
}

impl ReportedUsageCompactionError {
    /// Returns the selected queued turn when selection got that far.
    pub const fn turn(&self) -> Option<TurnId> {
        match self {
            Self::Activation(_) => None,
            Self::Model { turn, .. }
            | Self::Render(turn)
            | Self::ContextWindowUnavailable(turn) => Some(*turn),
            Self::Compaction { turn, .. } | Self::CompactionFailureClosure { turn, .. } => {
                Some(*turn)
            }
        }
    }
}

impl fmt::Display for ReportedUsageCompactionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("reported-usage context reconciliation failed")
    }
}

impl Error for ReportedUsageCompactionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Activation(error) => Some(error),
            Self::Model { source, .. } => Some(source),
            Self::CompactionFailureClosure { source, .. } => Some(source),
            Self::Render(_) | Self::ContextWindowUnavailable(_) | Self::Compaction { .. } => None,
        }
    }
}

impl ClassifyOperatorFailure for ReportedUsageCompactionError {
    fn operator_failure_class(&self) -> OperatorFailureClass {
        match self {
            Self::Activation(error) => error.operator_failure_class(),
            Self::Model { source, .. } => source.operator_failure_class(),
            Self::Render(_) => OperatorFailureClass::FailClosedCorruption,
            Self::ContextWindowUnavailable(_) => OperatorFailureClass::CallerOrHubBug,
            Self::Compaction { failure_class, .. } => *failure_class,
            Self::CompactionFailureClosure { source, .. } => source.operator_failure_class(),
        }
    }

    fn operator_failure_cause_code(&self) -> &'static str {
        match self {
            Self::Activation(_) => "reported_usage_activation_preview",
            Self::Model { source, .. } => source.operator_failure_cause_code(),
            Self::Render(_) => "reported_usage_frontier_rendering",
            Self::ContextWindowUnavailable(_) => "reported_usage_context_window_unavailable",
            Self::Compaction { cause_code, .. } => cause_code,
            Self::CompactionFailureClosure { source, .. } => source.operator_failure_cause_code(),
        }
    }
}

/// Conservative compaction preflight for adapters without a prospective count API.
#[derive(Clone)]
pub struct ReportedUsageCompaction {
    activation: StartEligibleTurnRepository,
    model_calls: PostgresModelCallRepository,
    tools: Arc<dyn ToolCatalog>,
    runtime_models: RuntimeModelCatalog,
    model_configuration: HubModelConfiguration,
    compaction_model: Arc<dyn ContextCompactionModel>,
}

struct ReportedUsageCompactionCandidate {
    preview: PreparedActivationPreview,
    turn: TurnId,
}

impl fmt::Debug for ReportedUsageCompaction {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ReportedUsageCompaction")
            .field("activation", &self.activation)
            .field("model_calls", &self.model_calls)
            .field("tools", &"[tool catalog]")
            .field("runtime_models", &self.runtime_models)
            .field("model_configuration", &self.model_configuration)
            .field("compaction_model", &"[context compaction model]")
            .finish()
    }
}

impl ReportedUsageCompaction {
    /// Composes the read-only queued-turn preflight and shared compaction path.
    pub fn new(
        activation: StartEligibleTurnRepository,
        model_calls: PostgresModelCallRepository,
        tools: impl ToolCatalog + 'static,
        runtime_models: RuntimeModelCatalog,
        model_configuration: HubModelConfiguration,
        compaction_model: Arc<dyn ContextCompactionModel>,
    ) -> Self {
        Self {
            activation,
            model_calls,
            tools: Arc::new(tools),
            runtime_models,
            model_configuration,
            compaction_model,
        }
    }

    /// Compacts once when the newest terminal call proves reserved headroom is gone.
    pub async fn compact_if_needed(
        &self,
        session: SessionId,
        observe_prepared: Option<&(dyn Fn(ModelCallId) + Send + Sync)>,
    ) -> Result<(), ReportedUsageCompactionError> {
        self.compact_if_needed_for(session, observe_prepared, false)
            .await
    }

    async fn compact_if_needed_for(
        &self,
        session: SessionId,
        observe_prepared: Option<&(dyn Fn(ModelCallId) + Send + Sync)>,
        include_anthropic: bool,
    ) -> Result<(), ReportedUsageCompactionError> {
        let Some(candidate) = self
            .compaction_candidate(session, include_anthropic)
            .await?
        else {
            return Ok(());
        };
        let ReportedUsageCompactionCandidate { preview, turn } = candidate;
        let applied = match compact_automatically(
            &self.model_calls,
            &self.model_configuration,
            &self.compaction_model,
            session,
            turn,
            observe_prepared,
        )
        .await
        {
            Ok(applied) => applied,
            Err(crate::process_runtime::AutomaticContextCompactionError::AlreadyAttempted) => {
                match close_failed_compaction_turn(
                    &self.activation,
                    &self.model_calls,
                    preview,
                    TurnTerminalCause::ReportedUsageContextCompactionExhausted,
                    None,
                )
                .await
                .map_err(|source| {
                    ReportedUsageCompactionError::CompactionFailureClosure { turn, source }
                })? {
                    CommitCompactionFailurePreviewOutcome::Failed(_) => {
                        tracing::warn!(
                            cause_code = "reported_usage_context_compaction_exhausted",
                            session_id = %session.as_uuid(),
                            turn_id = %turn.as_uuid(),
                            "the queued turn's bounded automatic compaction attempt was already spent; the turn was closed before provider dispatch"
                        );
                        return Err(ReportedUsageCompactionError::Compaction {
                            turn,
                            failure_class: OperatorFailureClass::CallerOrHubBug,
                            cause_code: "reported_usage_context_compaction_exhausted",
                        });
                    }
                    CommitCompactionFailurePreviewOutcome::Stale => return Ok(()),
                }
            }
            Err(error) => {
                let failure_class = error.operator_failure_class();
                let cause_code = error.operator_failure_cause_code();
                if failure_class
                    != (OperatorFailureClass::Infrastructure {
                        commit_ambiguous: true,
                    })
                {
                    match close_failed_compaction_turn(
                        &self.activation,
                        &self.model_calls,
                        preview,
                        compaction_terminal_cause(&error),
                        compaction_recovery_cause(&error),
                    )
                    .await
                    .map_err(|source| {
                        ReportedUsageCompactionError::CompactionFailureClosure { turn, source }
                    })? {
                        CommitCompactionFailurePreviewOutcome::Failed(_) => {}
                        CommitCompactionFailurePreviewOutcome::Stale => return Ok(()),
                    }
                }
                return Err(ReportedUsageCompactionError::Compaction {
                    turn,
                    failure_class,
                    cause_code,
                });
            }
        };
        tracing::warn!(
            cause_code = "reported_usage_context_compacted",
            session_id = %session.as_uuid(),
            turn_id = %turn.as_uuid(),
            context_compaction_id = %applied.compaction.into_uuid(),
            "provider-reported usage exhausted reserved context headroom; queued turn compacted before activation"
        );
        let Some(remaining) = self
            .compaction_candidate(session, include_anthropic)
            .await?
        else {
            return Ok(());
        };
        let remaining_turn = remaining.turn;
        match close_failed_compaction_turn(
            &self.activation,
            &self.model_calls,
            remaining.preview,
            TurnTerminalCause::ReportedUsageContextStillExceeded,
            None,
        )
        .await
        .map_err(
            |source| ReportedUsageCompactionError::CompactionFailureClosure {
                turn: remaining_turn,
                source,
            },
        )? {
            CommitCompactionFailurePreviewOutcome::Failed(_) => {
                tracing::warn!(
                    cause_code = "reported_usage_context_still_exceeded",
                    session_id = %session.as_uuid(),
                    turn_id = %remaining_turn.as_uuid(),
                    "automatic compaction did not restore reserved context headroom; the queued turn was closed before provider dispatch"
                );
                Err(ReportedUsageCompactionError::Compaction {
                    turn: remaining_turn,
                    failure_class: OperatorFailureClass::CallerOrHubBug,
                    cause_code: "reported_usage_context_still_exceeded",
                })
            }
            CommitCompactionFailurePreviewOutcome::Stale => Ok(()),
        }
    }

    async fn compaction_candidate(
        &self,
        session: SessionId,
        include_anthropic: bool,
    ) -> Result<Option<ReportedUsageCompactionCandidate>, ReportedUsageCompactionError> {
        let Some(preview) = self
            .activation
            .preview(session, activation_identities())
            .await
            .map_err(ReportedUsageCompactionError::Activation)?
        else {
            return Ok(None);
        };
        let turn = preview.prepared().turn().turn();
        let prospective = self
            .model_calls
            .preview_activation_operation(
                preview.prepared(),
                ModelCallId::from_uuid(uuid::Uuid::now_v7()),
            )
            .await
            .map_err(|source| ReportedUsageCompactionError::Model { turn, source })?;
        let Some(prospective) = prospective else {
            return Ok(None);
        };
        let operation = prospective
            .render(self.tools.definitions())
            .map_err(|_| ReportedUsageCompactionError::Render(turn))?;
        let target = operation.request().call().target();
        let selected = self
            .runtime_models
            .resolve(target)
            .ok_or(ReportedUsageCompactionError::ContextWindowUnavailable(turn))?;
        let definition = self
            .runtime_models
            .effective_definition(
                selected,
                operation.request().model_settings().effective().fast_mode(),
            )
            .ok_or(ReportedUsageCompactionError::ContextWindowUnavailable(turn))?;
        if !include_anthropic
            && self
                .model_configuration
                .adapter_for_provider_model(definition.provider_model())
                == Some(ModelAdapter::Anthropic)
        {
            return Ok(None);
        }
        // The preview's starting frontier is never committed, so it names the
        // model-visible input it would send rather than an identity no durable
        // membership resolves.
        let reported = self
            .model_calls
            .latest_reported_usage(session, target, prospective.prospective_input())
            .await
            .map_err(|source| ReportedUsageCompactionError::Model { turn, source })?;
        let reported_requires_compaction = reported.is_some_and(|reported| {
            reported_usage_requires_compaction(
                reported.usage(),
                ReportedInputCacheAxes::from_includes_cache_tokens(
                    reported.input_includes_cache_tokens(),
                ),
                ReportedInputRetention::from_retained(
                    reported.input_is_retained(),
                    reported.retained_input_tokens(),
                ),
                ReportedOutputRetention::from_retained(reported.output_is_retained()),
                reported.projected_unreported_content_bytes(),
                u64::from(definition.max_output_tokens()),
                u64::from(definition.context_window_tokens()),
            )
        });
        let failure_requires_compaction = if reported_requires_compaction {
            false
        } else if let Some(persisted_prefix) =
            persisted_preflight_prefix(preview.prepared().starting_snapshot())
        {
            self.model_calls
                .request_too_large_requires_compaction(session, target, persisted_prefix)
                .await
                .map_err(|source| ReportedUsageCompactionError::Model { turn, source })?
        } else {
            false
        };
        if !reported_requires_compaction && !failure_requires_compaction {
            return Ok(None);
        }
        Ok(Some(ReportedUsageCompactionCandidate { preview, turn }))
    }
}

/// Exact-guard failure before activation or during the resulting execution.
#[derive(Debug)]
pub enum ContextGuardedTurnPassError<CountError, ExecutionError> {
    /// Reported-usage preflight for an adapter without provider estimation failed.
    ReportedUsageCompaction(ReportedUsageCompactionError),
    /// Read-only activation preview or exact guarded commit failed.
    Activation {
        /// Selected turn, absent when preview failed before selection.
        turn: Option<TurnId>,
        /// Typed repository failure.
        source: StartEligibleTurnRepositoryError,
    },
    /// Prospective ordinary-call reconstitution failed.
    Operation {
        /// Selected turn.
        turn: TurnId,
        /// Typed model-call repository failure.
        source: ModelCallRepositoryError,
    },
    /// Canonical frontier rendering failed.
    Render {
        /// Selected turn.
        turn: TurnId,
        /// Typed frontier-rendering failure.
        source: signalbox_application::ModelFrontierRenderingError,
    },
    /// Provider-native token estimation failed.
    Count {
        /// Selected turn.
        turn: TurnId,
        /// Typed provider-counting failure.
        source: CountError,
    },
    /// A never-cancelled production count unexpectedly reported cancellation.
    CountCancelled(TurnId),
    /// The prospective target was absent from the immutable runtime catalog.
    ContextWindowUnavailable(TurnId),
    /// One automatic compaction still could not make the prospective input fit.
    ContextStillExceeded(TurnId),
    /// The shared append-only compaction lifecycle failed.
    Compaction {
        /// Selected turn.
        turn: TurnId,
        /// Classified compaction failure.
        failure_class: OperatorFailureClass,
        /// Closed cause retained before the compaction error is erased.
        cause_code: &'static str,
    },
    /// Closing the selected turn after compaction failure could not commit.
    CompactionFailureClosure {
        /// Selected turn.
        turn: TurnId,
        /// Typed activation-and-failure commit error.
        source: CommitActivationPreviewError,
    },
    /// Queued-turn discovery or durable manifest recording failed before the
    /// counted activation commit.
    WorkspaceInstructions {
        /// Selected queued turn.
        turn: TurnId,
        /// Typed instruction preparation failure.
        source: WorkspaceInstructionRuntimeError,
    },
    /// Execution after exact guarded activation failed.
    Execution {
        /// Stage at which execution orchestration failed.
        stage: TurnPassExecutionStage,
        /// Selected turn, absent when failure occurred before selection.
        turn: Option<TurnId>,
        /// Typed execution failure.
        source: ExecutionError,
    },
    /// The guarded commit returned an activation for another session.
    ActivationSessionMismatch(TurnId),
}

impl<CountError, ExecutionError> fmt::Display
    for ContextGuardedTurnPassError<CountError, ExecutionError>
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("context-guarded turn pass failed")
    }
}

impl<CountError, ExecutionError> Error for ContextGuardedTurnPassError<CountError, ExecutionError>
where
    CountError: Error + 'static,
    ExecutionError: Error + 'static,
{
}

impl<CountError, ExecutionError> ClassifyOperatorFailure
    for ContextGuardedTurnPassError<CountError, ExecutionError>
where
    CountError: ClassifyOperatorFailure,
    ExecutionError: ClassifyOperatorFailure,
{
    fn operator_failure_class(&self) -> OperatorFailureClass {
        match self {
            Self::ReportedUsageCompaction(error) => error.operator_failure_class(),
            Self::Activation { source, .. } => source.operator_failure_class(),
            Self::Operation { source, .. } => source.operator_failure_class(),
            Self::Render { .. } => OperatorFailureClass::FailClosedCorruption,
            Self::CountCancelled(_) | Self::ActivationSessionMismatch(_) => {
                OperatorFailureClass::CallerOrHubBug
            }
            Self::Count { source, .. } => source.operator_failure_class(),
            Self::ContextWindowUnavailable(_) | Self::ContextStillExceeded(_) => {
                OperatorFailureClass::CallerOrHubBug
            }
            Self::Compaction { failure_class, .. } => *failure_class,
            Self::CompactionFailureClosure { source, .. } => source.operator_failure_class(),
            Self::WorkspaceInstructions { source, .. } => source.operator_failure_class(),
            Self::Execution { source, .. } => source.operator_failure_class(),
        }
    }

    fn operator_failure_cause_code(&self) -> &'static str {
        match self {
            Self::ReportedUsageCompaction(error) => error.operator_failure_cause_code(),
            Self::Activation { .. } => "turn_activation_repository",
            Self::Operation { .. } => "model_call_repository",
            Self::Render { .. } => "model_frontier_rendering",
            Self::Count { source, .. } => source.operator_failure_cause_code(),
            Self::CountCancelled(_) => "model_input_count_cancelled",
            Self::ContextWindowUnavailable(_) => "context_window_unavailable",
            Self::ContextStillExceeded(_) => "context_window_exceeded",
            Self::Compaction { cause_code, .. } => cause_code,
            Self::CompactionFailureClosure { source, .. } => source.operator_failure_cause_code(),
            Self::WorkspaceInstructions { source, .. } => source.operator_failure_cause_code(),
            Self::Execution { source, .. } => source.operator_failure_cause_code(),
            Self::ActivationSessionMismatch(_) => "activation_session_mismatch",
        }
    }
}

/// Production eligibility pass that counts the exact prospective first call,
/// compacts once when required, then commits only that counted activation.
#[derive(Clone)]
pub struct ContextGuardedTurnPass<Counter, Catalog, Execution> {
    activation: StartEligibleTurnRepository,
    model_calls: PostgresModelCallRepository,
    counter: Counter,
    tools: Catalog,
    runtime_models: RuntimeModelCatalog,
    model_configuration: HubModelConfiguration,
    compaction_model: Arc<dyn ContextCompactionModel>,
    reported_usage_compaction: Option<ReportedUsageCompaction>,
    workspace_instructions: Option<WorkspaceInstructionRuntime>,
    execution: Execution,
    occupancy_recovery: Option<SchedulerPassOccupancyRecovery>,
    dispatch_start: bool,
}

impl<Counter, Catalog, Execution> fmt::Debug for ContextGuardedTurnPass<Counter, Catalog, Execution>
where
    Counter: fmt::Debug,
    Catalog: fmt::Debug,
    Execution: fmt::Debug,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ContextGuardedTurnPass")
            .field("activation", &self.activation)
            .field("model_calls", &self.model_calls)
            .field("counter", &self.counter)
            .field("tools", &self.tools)
            .field("runtime_models", &self.runtime_models)
            .field("model_configuration", &self.model_configuration)
            .field("compaction_model", &"[context compaction model]")
            .field("reported_usage_compaction", &self.reported_usage_compaction)
            .field("workspace_instructions", &self.workspace_instructions)
            .field("execution", &self.execution)
            .field("occupancy_recovery", &self.occupancy_recovery)
            .field("dispatch_start", &self.dispatch_start)
            .finish()
    }
}

impl<Counter, Catalog, Execution> ContextGuardedTurnPass<Counter, Catalog, Execution> {
    /// Composes exact preview/count/commit, shared compaction, and execution.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        activation: StartEligibleTurnRepository,
        model_calls: PostgresModelCallRepository,
        counter: Counter,
        tools: Catalog,
        runtime_models: RuntimeModelCatalog,
        model_configuration: HubModelConfiguration,
        compaction_model: Arc<dyn ContextCompactionModel>,
        execution: Execution,
    ) -> Self {
        Self {
            activation,
            model_calls,
            counter,
            tools,
            runtime_models,
            model_configuration,
            compaction_model,
            reported_usage_compaction: None,
            workspace_instructions: None,
            execution,
            occupancy_recovery: None,
            dispatch_start: false,
        }
    }

    /// Keeps the reported-usage preflight for adapters without provider estimation.
    pub fn with_reported_usage_compaction(mut self, compaction: ReportedUsageCompaction) -> Self {
        self.reported_usage_compaction = Some(compaction);
        self
    }

    /// Installs daemon-owned recovery for passes ended by the occupancy bound.
    pub fn with_occupancy_recovery(
        mut self,
        pool: sqlx::PgPool,
        eligibility_nudge: InProcessEligibilityNudge,
        policy: ExpiredPassRecoveryPolicy,
        persistence_bounds: signalbox_persistence::turn_liveness::TurnLivenessPersistenceBounds,
    ) -> Self
    where
        Execution: ActivatedTurnExecution,
    {
        self.occupancy_recovery = Some(SchedulerPassOccupancyRecovery::new(
            pool,
            eligibility_nudge,
            &self.execution,
            policy,
            persistence_bounds,
        ));
        self
    }

    /// Records the empty queued-turn manifest needed by the atomic counted
    /// activation and first-call checkpoint.
    pub fn with_workspace_instructions(
        mut self,
        workspace_instructions: WorkspaceInstructionRuntime,
    ) -> Self {
        self.workspace_instructions = Some(workspace_instructions);
        self
    }
}

impl<Counter, Catalog, Execution> EligibilityPass
    for ContextGuardedTurnPass<Counter, Catalog, Execution>
where
    Counter: ModelCallInputTokenCounter + Clone + Send + Sync + 'static,
    Counter::Error: Send + 'static,
    Catalog: ToolCatalog + Clone + Send + Sync + 'static,
    Execution: ActivatedTurnExecution + Clone + Send + Sync + 'static,
{
    type Error = ContextGuardedTurnPassError<Counter::Error, Execution::Error>;

    fn failure_stage(error: &Self::Error) -> &'static str {
        guarded_failure_stage(error)
    }
    fn failure_turn(error: &Self::Error) -> Option<TurnId> {
        match error {
            ContextGuardedTurnPassError::ReportedUsageCompaction(error) => error.turn(),
            ContextGuardedTurnPassError::Activation { turn, .. }
            | ContextGuardedTurnPassError::Execution { turn, .. } => *turn,
            ContextGuardedTurnPassError::Operation { turn, .. }
            | ContextGuardedTurnPassError::Render { turn, .. }
            | ContextGuardedTurnPassError::Count { turn, .. }
            | ContextGuardedTurnPassError::Compaction { turn, .. }
            | ContextGuardedTurnPassError::CompactionFailureClosure { turn, .. }
            | ContextGuardedTurnPassError::WorkspaceInstructions { turn, .. } => Some(*turn),
            ContextGuardedTurnPassError::CountCancelled(turn)
            | ContextGuardedTurnPassError::ContextWindowUnavailable(turn)
            | ContextGuardedTurnPassError::ContextStillExceeded(turn)
            | ContextGuardedTurnPassError::ActivationSessionMismatch(turn) => Some(*turn),
        }
    }

    fn occupancy_expiry_handler(&self) -> Option<Arc<dyn SchedulerPassExpiryHandler>> {
        self.occupancy_recovery
            .clone()
            .map(|recovery| Arc::new(recovery) as _)
    }

    fn run(
        &mut self,
        session: SessionId,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
        let dispatch_start = std::mem::take(&mut self.dispatch_start);
        let activation = self.activation.clone();
        let model_calls = self.model_calls.clone();
        let counter = self.counter.clone();
        let tools = self.tools.clone();
        let runtime_models = self.runtime_models.clone();
        let model_configuration = self.model_configuration.clone();
        let compaction_model = Arc::clone(&self.compaction_model);
        let reported_usage_compaction = self.reported_usage_compaction.clone();
        let workspace_instructions = self.workspace_instructions.clone();
        let execution = self.execution.clone();
        let occupancy_recovery = self.occupancy_recovery.clone();
        async move {
            let occupancy_tracking = occupancy_recovery
                .as_ref()
                .map(|recovery| recovery.resume_turn_observer(session));
            let observe_turn = occupancy_tracking
                .as_ref()
                .map(|(_, observer)| Arc::clone(observer))
                .unwrap_or_else(|| Arc::new(|_| {}));
            let resumed = if dispatch_start {
                execution
                    .resume_dispatch_start_with_observer(session, Arc::clone(&observe_turn))
                    .await
            } else {
                execution
                    .resume_active_with_observer(session, Arc::clone(&observe_turn))
                    .await
            };
            resumed.map_err(|source| ContextGuardedTurnPassError::Execution {
                stage: TurnPassExecutionStage::ActiveTurnRecovery,
                turn: Execution::active_resume_failure_turn(&source),
                source,
            })?;
            if let Some(compaction) = &reported_usage_compaction {
                if dispatch_start {
                    match compaction.compaction_candidate(session, false).await {
                        Ok(Some(_)) => {
                            // A provider exchange cannot occupy the reserved
                            // start lane. The unchanged queued turn remains
                            // eligible for an ordinary pass to compact.
                            return Ok(());
                        }
                        Ok(None) => {}
                        Err(error) => {
                            let error = ContextGuardedTurnPassError::ReportedUsageCompaction(error);
                            report_guarded_ambiguity(&execution, &error);
                            return Err(error);
                        }
                    }
                }
                let compaction_window = occupancy_recovery
                    .as_ref()
                    .map(|recovery| recovery.compaction_window(session));
                let observe_prepared = compaction_window
                    .as_ref()
                    .map(|(_, observer)| Arc::clone(observer));
                let compacted = compaction
                    .compact_if_needed(session, observe_prepared.as_deref())
                    .await;
                drop(compaction_window);
                if let Err(error) = compacted {
                    let error = ContextGuardedTurnPassError::ReportedUsageCompaction(error);
                    report_guarded_ambiguity(&execution, &error);
                    return Err(error);
                }
            }
            let outcome: Result<
                (),
                ContextGuardedTurnPassError<Counter::Error, Execution::Error>,
            > = async {
                let mut compacted_turn = None;
                loop {
                    let identities = activation_identities();
                    let preview = match activation.preview(session, identities).await {
                        Ok(Some(preview)) => preview,
                        Ok(None) => return Ok(()),
                        Err(StartEligibleTurnRepositoryError::IdentityCollision(_)) => continue,
                        Err(source) => return Err(ContextGuardedTurnPassError::Activation { turn: None, source }),
                    };
                    let turn = preview.prepared().turn().turn();
                    let call = ModelCallId::from_uuid(uuid::Uuid::now_v7());
                    let prospective = match model_calls
                        .preview_activation_operation(preview.prepared(), call)
                        .await
                    {
                        Ok(prospective) => prospective,
                        Err(ModelCallRepositoryError::IdentityCollision(_)) => continue,
                        Err(source) => return Err(ContextGuardedTurnPassError::Operation { turn, source }),
                    };
                    // An exhausted credential pool leaves no account to
                    // authenticate the input-token count with, so this pass
                    // activates the turn call-free and lets ordinary
                    // preparation record the typed pool-exhaustion closure
                    // rather than failing the count against an excluded member.
                    let Some(prospective) = prospective else {
                        let committed = activation
                            .commit_preview(preview)
                            .await
                            .map_err(|source| ContextGuardedTurnPassError::Activation {
                                turn: Some(turn),
                                source,
                            })?;
                        match committed {
                            CommitActivationPreviewOutcome::Stale => continue,
                            CommitActivationPreviewOutcome::Activated(activated) => {
                                if activated.session() != session {
                                    execution.report_post_activation_failure();
                                    return Err(ContextGuardedTurnPassError::ActivationSessionMismatch(turn));
                                }
                                observe_turn(activated.turn());
                                report_guarded_turn_activation(activated.session(), activated.turn());
                                let execution = async {
                                    if dispatch_start {
                                        execution.execute_dispatch_start(activated).await
                                    } else {
                                        execution.execute(activated).await
                                    }
                                };
                                return execution
                                    .instrument(guarded_turn_span(session, turn))
                                    .await
                                    .map_err(|source| ContextGuardedTurnPassError::Execution {
                                        stage: TurnPassExecutionStage::Execution,
                                        turn: Some(turn),
                                        source,
                                    });
                            }
                        }
                    };
                    let operation = prospective
                        .render(tools.definitions())
                        .map_err(|source| ContextGuardedTurnPassError::Render { turn, source })?;
                    let target = operation.request().call().target();
                    let selected_model = runtime_models
                        .resolve(target)
                        .ok_or(ContextGuardedTurnPassError::ContextWindowUnavailable(turn))?;
                    if dispatch_start
                        && model_configuration
                            .adapter_for_provider_model(selected_model.provider_model())
                            == Some(ModelAdapter::Anthropic)
                    {
                        // Counting and attachment reads are provider/storage
                        // I/O. Preserve the queued preview so an ordinary pass
                        // performs them outside the reserved start lane.
                        return Ok(());
                    }
                    let model = runtime_models
                        .effective_definition(
                            selected_model,
                            operation.request().model_settings().effective().fast_mode(),
                        )
                        .ok_or(ContextGuardedTurnPassError::ContextWindowUnavailable(turn))?;
                    let input_tokens = match counter
                        .count_input_tokens(operation, std::future::pending())
                        .await
                        .map_err(|source| ContextGuardedTurnPassError::Count { turn, source })?
                    {
                        ModelCallInputTokenCount::Counted(count) => count,
                        ModelCallInputTokenCount::Cancelled => {
                            return Err(ContextGuardedTurnPassError::CountCancelled(turn));
                        }
                        ModelCallInputTokenCount::AttachmentUnavailable => {
                            // The preview is still uncommitted. Leave the turn
                            // queued so recovery must verify and recount the
                            // attachment before any later activation.
                            return Ok(());
                        }
                        ModelCallInputTokenCount::AttachmentFailure(failure) => {
                            let prepared_instructions = if let Some(workspace_instructions) = &workspace_instructions {
                                let Some(prepared) = workspace_instructions
                                    .prepare_counted_activation(session, turn)
                                    .await
                                    .map_err(|source| {
                                        ContextGuardedTurnPassError::WorkspaceInstructions {
                                            turn,
                                            source,
                                        }
                                    })?
                                else {
                                    continue;
                                };
                                Some(prepared)
                            } else {
                                None
                            };
                            let committed = close_counted_attachment_failure(
                                &activation,
                                &model_calls,
                                preview,
                                prospective,
                                failure,
                                prepared_instructions
                                    .as_ref()
                                    .map(|prepared| prepared.evidence()),
                            )
                                .await
                                .map_err(|error| match error {
                                    CommitActivationPreviewError::Activation(error) => {
                                        ContextGuardedTurnPassError::Activation { turn: Some(turn), source: error }
                                    }
                                    CommitActivationPreviewError::ModelCall(error) => {
                                        ContextGuardedTurnPassError::Operation { turn, source: error }
                                    }
                                    CommitActivationPreviewError::WorkspaceInstructions(error) => {
                                        ContextGuardedTurnPassError::WorkspaceInstructions {
                                            turn,
                                            source: WorkspaceInstructionRuntimeError::Persistence(error),
                                        }
                                    }
                                })?;
                            match committed {
                                CommitCountedAttachmentFailurePreviewOutcome::Stale => continue,
                                CommitCountedAttachmentFailurePreviewOutcome::Failed(failed_turn) => {
                                    observe_turn(failed_turn);
                                    report_guarded_turn_activation(session, failed_turn);
                                    return Ok(());
                                }
                            }
                        }
                        ModelCallInputTokenCount::Unavailable => {
                            if let Some(compaction) = &reported_usage_compaction
                                && compaction
                                    .compaction_candidate(session, true)
                                    .await
                                    .map_err(ContextGuardedTurnPassError::ReportedUsageCompaction)?
                                    .is_some()
                            {
                                if dispatch_start {
                                    // Preserve the queued preview and release
                                    // the reserved lane. An ordinary pass will
                                    // compact before falling through to send.
                                    return Ok(());
                                }
                                let compaction_window = occupancy_recovery
                                    .as_ref()
                                    .map(|recovery| recovery.compaction_window(session));
                                let observe_prepared = compaction_window
                                    .as_ref()
                                    .map(|(_, observer)| Arc::clone(observer));
                                let compacted = compaction
                                    .compact_if_needed_for(
                                        session,
                                        observe_prepared.as_deref(),
                                        true,
                                    )
                                    .await;
                                drop(compaction_window);
                                compacted.map_err(
                                    ContextGuardedTurnPassError::ReportedUsageCompaction,
                                )?;
                                continue;
                            }
                            let committed = activation
                                .commit_preview(preview)
                                .await
                                .map_err(|source| ContextGuardedTurnPassError::Activation {
                                    turn: Some(turn),
                                    source,
                                })?;
                            match committed {
                                CommitActivationPreviewOutcome::Stale => continue,
                                CommitActivationPreviewOutcome::Activated(activated) => {
                                    if activated.session() != session {
                                        execution.report_post_activation_failure();
                                        return Err(ContextGuardedTurnPassError::ActivationSessionMismatch(turn));
                                    }
                                    observe_turn(activated.turn());
                                    report_guarded_turn_activation(
                                        activated.session(),
                                        activated.turn(),
                                    );
                                    let execution = async {
                                        if dispatch_start {
                                            execution.execute_dispatch_start(activated).await
                                        } else {
                                            execution.execute(activated).await
                                        }
                                    };
                                    return execution
                                        .instrument(guarded_turn_span(session, turn))
                                        .await
                                        .map_err(|source| ContextGuardedTurnPassError::Execution {
                                            stage: TurnPassExecutionStage::Execution,
                                            turn: Some(turn),
                                            source,
                                        });
                                }
                            }
                        }
                    };
                    if !provider_count_admits(
                        input_tokens,
                        u64::from(model.max_output_tokens()),
                        u64::from(model.context_window_tokens()),
                    ) {
                        if dispatch_start {
                            // The estimate is advisory and creates no durable
                            // state. Leave the turn queued so an ordinary pass
                            // can recount and perform the provider compaction.
                            return Ok(());
                        }
                        if compacted_turn == Some(turn) {
                            match close_failed_compaction_turn(
                                &activation,
                                &model_calls,
                                preview,
                                TurnTerminalCause::ContextHeadroomExhausted,
                                None,
                            )
                            .await
                            .map_err(|source| {
                                ContextGuardedTurnPassError::CompactionFailureClosure {
                                    turn,
                                    source,
                                }
                            })? {
                                CommitCompactionFailurePreviewOutcome::Failed(_) => {}
                                CommitCompactionFailurePreviewOutcome::Stale => continue,
                            }
                            return Err(ContextGuardedTurnPassError::ContextStillExceeded(turn));
                        }
                        let compaction_window = occupancy_recovery
                            .as_ref()
                            .map(|recovery| recovery.compaction_window(session));
                        let observe_prepared = compaction_window
                            .as_ref()
                            .map(|(_, observer)| Arc::clone(observer));
                        let compaction_result = compact_automatically(
                            &model_calls,
                            &model_configuration,
                            &compaction_model,
                            session,
                            turn,
                            observe_prepared.as_deref(),
                        )
                        .await;
                        drop(compaction_window);
                        match compaction_result {
                            Ok(_) => {}
                            Err(crate::process_runtime::AutomaticContextCompactionError::AlreadyAttempted) => {
                                match close_failed_compaction_turn(
                                    &activation,
                                    &model_calls,
                                    preview,
                                    TurnTerminalCause::ContextHeadroomExhausted,
                                    None,
                                )
                                .await
                                .map_err(|source| {
                                    ContextGuardedTurnPassError::CompactionFailureClosure {
                                        turn,
                                        source,
                                    }
                                })? {
                                    CommitCompactionFailurePreviewOutcome::Failed(_) => {}
                                    CommitCompactionFailurePreviewOutcome::Stale => continue,
                                }
                                return Err(ContextGuardedTurnPassError::ContextStillExceeded(turn));
                            }
                            Err(error) => {
                                let failure_class = error.operator_failure_class();
                                let cause_code = error.operator_failure_cause_code();
                                if failure_class
                                    != (OperatorFailureClass::Infrastructure {
                                        commit_ambiguous: true,
                                    })
                                {
                                    match close_failed_compaction_turn(
                                        &activation,
                                        &model_calls,
                                        preview,
                                        compaction_terminal_cause(&error),
                                        compaction_recovery_cause(&error),
                                    )
                                    .await
                                    .map_err(|source| {
                                        ContextGuardedTurnPassError::CompactionFailureClosure {
                                            turn,
                                            source,
                                        }
                                    })? {
                                        CommitCompactionFailurePreviewOutcome::Failed(_) => {}
                                        CommitCompactionFailurePreviewOutcome::Stale => continue,
                                    }
                                }
                                return Err(ContextGuardedTurnPassError::Compaction {
                                    turn,
                                    failure_class,
                                    cause_code,
                                });
                            }
                        }
                        compacted_turn = Some(turn);
                        continue;
                    }
                    let prepared_instructions = if let Some(workspace_instructions) = &workspace_instructions {
                        let Some(prepared) = workspace_instructions
                            .prepare_counted_activation(session, turn)
                            .await
                            .map_err(|source| {
                                ContextGuardedTurnPassError::WorkspaceInstructions {
                                    turn,
                                    source,
                                }
                            })?
                        else {
                            continue;
                        };
                        Some(prepared)
                    } else {
                        None
                    };
                    let committed = activation
                        .commit_counted_preview(
                            preview,
                            prospective,
                            &model_calls,
                            prepared_instructions.as_ref().map(|prepared| prepared.evidence()),
                        )
                        .await
                        .map_err(|error| match error {
                            CommitActivationPreviewError::Activation(error) => {
                                ContextGuardedTurnPassError::Activation { turn: Some(turn), source: error }
                            }
                            CommitActivationPreviewError::ModelCall(error) => {
                                ContextGuardedTurnPassError::Operation { turn, source: error }
                            }
                            CommitActivationPreviewError::WorkspaceInstructions(error) => {
                                ContextGuardedTurnPassError::WorkspaceInstructions {
                                    turn,
                                    source: WorkspaceInstructionRuntimeError::Persistence(error),
                                }
                            }
                        })?;
                    match committed {
                        CommitActivationPreviewOutcome::Stale => continue,
                        CommitActivationPreviewOutcome::Activated(activated) => {
                            if activated.session() != session {
                                execution.report_post_activation_failure();
                                return Err(ContextGuardedTurnPassError::ActivationSessionMismatch(turn));
                            }
                            observe_turn(activated.turn());
                            report_guarded_turn_activation(activated.session(), activated.turn());
                            if dispatch_start {
                                // The counted commit already created the first
                                // durable call checkpoint. Returning releases
                                // the reserved start lane; ordinary scheduling
                                // resumes the prepared call.
                                return Ok(());
                            }
                            let execution = async {
                                execution.execute(activated).await
                            };
                            return execution
                                .instrument(guarded_turn_span(session, turn))
                                .await
                                .map_err(|source| ContextGuardedTurnPassError::Execution {
                                    stage: TurnPassExecutionStage::Execution,
                                    turn: Some(turn),
                                    source,
                                });
                        }
                    }
                }
            }
            .await;
            if let Err(error) = &outcome {
                report_guarded_ambiguity(&execution, error);
            }
            outcome
        }
    }

    fn run_dispatch_start(
        &mut self,
        session: SessionId,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
        self.dispatch_start = true;
        self.run(session)
    }
}

/// Returns one closed guarded-pass stage without inspecting payload-bearing
/// errors.
///
/// The explicit execution marker keeps retained-turn recovery distinct even
/// when that recovery already knows the turn identity.
fn guarded_failure_stage<CountError, ExecutionError>(
    error: &ContextGuardedTurnPassError<CountError, ExecutionError>,
) -> &'static str {
    match error {
        ContextGuardedTurnPassError::ReportedUsageCompaction(_) => "context_compaction",
        ContextGuardedTurnPassError::Activation { turn: None, .. } => "activation_preview",
        ContextGuardedTurnPassError::Activation { turn: Some(_), .. } => "activation_commit",
        ContextGuardedTurnPassError::Operation { .. } => "model_operation",
        ContextGuardedTurnPassError::Render { .. } => "frontier_rendering",
        ContextGuardedTurnPassError::Count { .. } => "input_token_count",
        ContextGuardedTurnPassError::CountCancelled(_) => "input_token_count",
        ContextGuardedTurnPassError::ContextWindowUnavailable(_) => "context_window",
        ContextGuardedTurnPassError::ContextStillExceeded(_) => "context_window",
        ContextGuardedTurnPassError::Compaction { .. } => "context_compaction",
        ContextGuardedTurnPassError::CompactionFailureClosure { .. } => {
            "context_compaction_failure_closure"
        }
        ContextGuardedTurnPassError::WorkspaceInstructions { .. } => "workspace_instructions",
        ContextGuardedTurnPassError::Execution { stage, .. } => stage.operator_label(),
        ContextGuardedTurnPassError::ActivationSessionMismatch(_) => "activation_correlation",
    }
}

/// Applies the shared ambiguous-commit recovery report to one guarded-pass
/// failure.
///
/// Every durable stage this pass owns — activation preview, prospective call
/// reconstitution, the guarded counted commit, and automatic compaction
/// preparation — can raise the declared ambiguous-commit class, and each leaves
/// durable state whose outcome ordinary scheduler retry cannot decide. One
/// reaction point keeps them from diverging. Execution failures are excluded:
/// [`ActivatedTurnExecution`] already owns how its own failures are supervised.
fn report_guarded_ambiguity<CountError, Execution>(
    execution: &Execution,
    error: &ContextGuardedTurnPassError<CountError, Execution::Error>,
) where
    CountError: ClassifyOperatorFailure,
    Execution: ActivatedTurnExecution,
{
    if matches!(error, ContextGuardedTurnPassError::Execution { .. }) {
        return;
    }
    report_ambiguous_commit(execution, error);
}

/// Creates the selected turn's child span under scheduler session work.
///
/// Stable span and field names preserve the hierarchy for a future exporter.
/// Only daemon-minted aggregate identities are recorded; no model, prompt, or
/// tool content enters the span.
fn guarded_turn_span(session: SessionId, turn: TurnId) -> tracing::Span {
    tracing::info_span!(
        "turn_work",
        session_id = %session.as_uuid(),
        turn_id = %turn.as_uuid(),
    )
}

/// Records activation from the production counted-preview commit path.
///
/// The generic start-eligible-turn service records its own committed outcome,
/// but this exact-counting composition commits through the persistence preview
/// boundary directly. The two daemon-minted identities are the complete event;
/// no accepted input, prompt, model content, or adapter prose is recorded.
fn report_guarded_turn_activation(session: SessionId, turn: TurnId) {
    tracing::info!(
        session_id = %session.as_uuid(),
        turn_id = %turn.as_uuid(),
        "turn activated"
    );
}

fn provider_count_admits(
    input_tokens: u64,
    max_output_tokens: u64,
    context_window_tokens: u64,
) -> bool {
    let admission_ceiling =
        context_window_tokens.saturating_mul(PROVIDER_COUNT_ADMISSION_PERCENT) / 100;
    input_tokens
        .checked_add(max_output_tokens)
        .is_some_and(|requested| requested <= admission_ceiling)
}

fn activation_identities() -> AcceptedInputTurnActivationIdentities {
    AcceptedInputTurnActivationIdentities::new(
        SemanticTranscriptEntryId::from_uuid(uuid::Uuid::now_v7()),
        SemanticTranscriptEntryId::from_uuid(uuid::Uuid::now_v7()),
        ContextFrontierId::from_uuid(uuid::Uuid::now_v7()),
        TurnAttemptId::from_uuid(uuid::Uuid::now_v7()),
    )
}

fn persisted_preflight_prefix(
    prospective: &ResolvedContextFrontierSnapshot,
) -> Option<ContextFrontierId> {
    prospective
        .immediate_semantic_prefix()
        .map(|prefix| prefix.snapshot())
}

async fn close_failed_compaction_turn(
    activation: &StartEligibleTurnRepository,
    model_calls: &PostgresModelCallRepository,
    preview: PreparedActivationPreview,
    terminal_cause: TurnTerminalCause,
    recovery_cause: Option<GoalExecutionFailureRecoveryCause>,
) -> Result<CommitCompactionFailurePreviewOutcome, CommitActivationPreviewError> {
    loop {
        let identities = FailedModelCallTurnIdentities::new(
            SemanticTranscriptEntryId::from_uuid(uuid::Uuid::now_v7()),
            ContextFrontierId::from_uuid(uuid::Uuid::now_v7()),
        );
        match activation
            .commit_compaction_failure_preview(
                preview.clone(),
                model_calls,
                identities,
                terminal_cause,
                recovery_cause,
            )
            .await
        {
            Err(error) if compaction_failure_closure_collision_is_retryable(&error) => {}
            outcome => return outcome,
        }
    }
}

async fn close_counted_attachment_failure(
    activation: &StartEligibleTurnRepository,
    model_calls: &PostgresModelCallRepository,
    preview: PreparedActivationPreview,
    prospective: signalbox_persistence::model_execution::ProspectiveModelCall,
    failure: signalbox_application::AttachmentPreparationFailure,
    instruction_evidence: Option<
        signalbox_persistence::workspace_instructions::CountedActivationInstructionEvidence<'_>,
    >,
) -> Result<CommitCountedAttachmentFailurePreviewOutcome, CommitActivationPreviewError> {
    loop {
        let identities = FailedModelCallTurnIdentities::new(
            SemanticTranscriptEntryId::from_uuid(uuid::Uuid::now_v7()),
            ContextFrontierId::from_uuid(uuid::Uuid::now_v7()),
        );
        match activation
            .commit_counted_attachment_failure_preview(
                preview.clone(),
                prospective.clone(),
                model_calls,
                failure,
                identities,
                instruction_evidence,
            )
            .await
        {
            Err(error) if compaction_failure_closure_collision_is_retryable(&error) => {}
            outcome => return outcome,
        }
    }
}

fn compaction_recovery_cause(
    error: &crate::process_runtime::AutomaticContextCompactionError,
) -> Option<GoalExecutionFailureRecoveryCause> {
    matches!(
        error,
        crate::process_runtime::AutomaticContextCompactionError::InputDoesNotFit
    )
    .then_some(GoalExecutionFailureRecoveryCause::ContextCompactionInputDoesNotFit)
}

/// Classifies the turn-terminal cause a failed automatic compaction records.
///
/// An input the compactor cannot fit is the compaction wall and keeps its own
/// spelling; every other compaction failure is recorded as such rather than
/// borrowed from the wall.
const fn compaction_terminal_cause(
    error: &crate::process_runtime::AutomaticContextCompactionError,
) -> TurnTerminalCause {
    match error {
        crate::process_runtime::AutomaticContextCompactionError::InputDoesNotFit => {
            TurnTerminalCause::ContextCompactionWall
        }
        _ => TurnTerminalCause::ContextCompactionFailed,
    }
}

fn compaction_failure_closure_collision_is_retryable(error: &CommitActivationPreviewError) -> bool {
    matches!(
        error,
        CommitActivationPreviewError::ModelCall(ModelCallRepositoryError::IdentityCollision(_))
    )
}

#[cfg(test)]
mod tests {
    use std::{
        fmt,
        future::{Future, ready},
    };

    use signalbox_application::{ClassifyOperatorFailure, OperatorFailureClass};
    use signalbox_domain::{
        ActivatedTurn, ContextFrontierId, ResolvedContextFrontierReconstitutionInput, SessionId,
        TurnId,
    };
    use signalbox_persistence::{
        context_compaction::ContextCompactionRepositoryError,
        goal::GoalExecutionFailureRecoveryCause,
        model_execution::{ModelCallIdentityCollision, ModelCallRepositoryError},
        start_eligible_turn::{
            CommitActivationPreviewError, StartEligibleTurnIdentityCollision,
            StartEligibleTurnRepositoryError,
        },
    };

    use super::{
        ContextGuardedTurnPassError, compaction_failure_closure_collision_is_retryable,
        compaction_recovery_cause, guarded_failure_stage, persisted_preflight_prefix,
        provider_count_admits, report_guarded_ambiguity,
    };

    #[test]
    fn no_fitting_compaction_input_requires_operator_recovery() {
        assert_eq!(
            compaction_recovery_cause(&AutomaticContextCompactionError::InputDoesNotFit),
            Some(GoalExecutionFailureRecoveryCause::ContextCompactionInputDoesNotFit)
        );
    }

    #[test]
    fn provider_count_retains_conservative_headroom_for_admission() {
        assert!(provider_count_admits(79, 16, 100));
        assert!(!provider_count_admits(80, 16, 100));
        assert!(!provider_count_admits(u64::MAX, 1, u64::MAX));
    }

    #[test]
    fn transient_compaction_failure_keeps_automatic_recovery() {
        assert_eq!(
            compaction_recovery_cause(&AutomaticContextCompactionError::Model),
            None
        );
    }
    use crate::{
        ActivatedTurnExecution, FatalExecutionSignal, FatalExecutionSupervisor,
        TurnPassExecutionStage, process_runtime::AutomaticContextCompactionError,
    };

    /// A classified failure whose durable commit outcome is unknown.
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    struct CommitAmbiguousFailure;

    #[test]
    fn reminted_compaction_failure_identity_collision_is_retryable() {
        let error = CommitActivationPreviewError::ModelCall(
            ModelCallRepositoryError::IdentityCollision(ModelCallIdentityCollision::SemanticEntry),
        );

        assert!(compaction_failure_closure_collision_is_retryable(&error));
    }

    #[test]
    fn immutable_activation_identity_collision_is_not_retryable() {
        let error = CommitActivationPreviewError::Activation(
            StartEligibleTurnRepositoryError::IdentityCollision(
                StartEligibleTurnIdentityCollision::StartingFrontier,
            ),
        );

        assert!(!compaction_failure_closure_collision_is_retryable(&error));
    }

    impl fmt::Display for CommitAmbiguousFailure {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("commit acknowledgement was lost")
        }
    }

    impl std::error::Error for CommitAmbiguousFailure {}

    impl ClassifyOperatorFailure for CommitAmbiguousFailure {
        fn operator_failure_class(&self) -> OperatorFailureClass {
            OperatorFailureClass::Infrastructure {
                commit_ambiguous: true,
            }
        }
    }

    #[derive(Clone, Copy, Debug)]
    struct NoopExecution;

    impl ActivatedTurnExecution for NoopExecution {
        type Error = CommitAmbiguousFailure;

        fn execute(
            &self,
            _activated: Box<ActivatedTurn>,
        ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
            ready(Ok(()))
        }
    }

    /// One guarded-pass failure, as the supervised production composition would
    /// observe it.
    type GuardedFailure =
        ContextGuardedTurnPassError<CommitAmbiguousFailure, CommitAmbiguousFailure>;

    fn supervised() -> (
        FatalExecutionSupervisor<NoopExecution>,
        FatalExecutionSignal,
    ) {
        FatalExecutionSupervisor::new(NoopExecution)
    }

    fn turn() -> TurnId {
        TurnId::from_uuid(uuid::Uuid::from_u128(1))
    }

    #[test]
    fn request_failure_preflight_uses_the_durable_immediate_prefix() {
        let session = SessionId::from_uuid(uuid::Uuid::from_u128(2));
        let persisted = ContextFrontierId::from_uuid(uuid::Uuid::from_u128(3));
        let prospective = ContextFrontierId::from_uuid(uuid::Uuid::from_u128(4));
        let snapshot = ResolvedContextFrontierReconstitutionInput::new(session, persisted, vec![])
            .derive_appending(prospective, vec![])
            .reconstitute()
            .expect("derived prospective frontier is valid");

        assert_eq!(persisted_preflight_prefix(&snapshot), Some(persisted));
    }

    /// The exact lost-acknowledgement failure the activation repository reports
    /// when a guarded counted commit cannot be proven.
    fn ambiguous_activation() -> GuardedFailure {
        ContextGuardedTurnPassError::Activation {
            turn: Some(turn()),
            source: StartEligibleTurnRepositoryError::Database {
                source: sqlx::Error::PoolClosed,
                commit_ambiguous: true,
            },
        }
    }

    /// The exact failure automatic compaction preparation reports when its own
    /// durable prepare cannot be proven committed.
    fn ambiguous_compaction_source() -> AutomaticContextCompactionError {
        AutomaticContextCompactionError::Repository(
            ContextCompactionRepositoryError::CommitAmbiguous(sqlx::Error::PoolClosed),
        )
    }

    fn ambiguous_compaction() -> GuardedFailure {
        let source = ambiguous_compaction_source();
        ContextGuardedTurnPassError::Compaction {
            turn: turn(),
            failure_class: source.operator_failure_class(),
            cause_code: source.operator_failure_cause_code(),
        }
    }

    /// S03 / INV-034: the production guarded pass replaced the activated pass,
    /// so an unprovable guarded activation commit must still raise the fatal
    /// recovery signal — every turn depends on it, not only compacted ones.
    #[test]
    fn s03_inv034_ambiguous_guarded_activation_commit_reports_post_activation_failure() {
        let (execution, signal) = supervised();

        report_guarded_ambiguity(&execution, &ambiguous_activation());

        assert!(signal.is_triggered());
    }

    /// S03 / INV-034: an automatic compaction whose durable preparation cannot
    /// be proven committed reports the same outcome as an ambiguous activation
    /// commit, rather than failing silently on the compaction path.
    #[test]
    fn s03_inv034_ambiguous_compaction_preparation_reports_post_activation_failure() {
        let (execution, signal) = supervised();

        let error = ambiguous_compaction();
        report_guarded_ambiguity(&execution, &error);

        assert_eq!(
            error.operator_failure_cause_code(),
            ambiguous_compaction_source().operator_failure_cause_code()
        );
        assert!(signal.is_triggered());
    }

    /// S03 / INV-034: a database failure before any commit boundary is ordinary
    /// scheduler retry work and raises no recovery signal.
    #[test]
    fn s03_inv034_activation_failure_before_the_commit_boundary_reports_nothing() {
        let (execution, signal) = supervised();

        let error: GuardedFailure = ContextGuardedTurnPassError::Activation {
            turn: None,
            source: StartEligibleTurnRepositoryError::Database {
                source: sqlx::Error::PoolClosed,
                commit_ambiguous: false,
            },
        };

        report_guarded_ambiguity(&execution, &error);

        assert!(!signal.is_triggered());
    }

    /// S03 / INV-034: execution failures keep their own supervision rule, so
    /// the guarded pass adds no second reaction to them.
    #[test]
    fn s03_inv034_execution_failure_keeps_its_own_supervision_rule() {
        let (execution, signal) = supervised();

        let error: GuardedFailure = ContextGuardedTurnPassError::Execution {
            stage: TurnPassExecutionStage::Execution,
            turn: Some(turn()),
            source: CommitAmbiguousFailure,
        };

        report_guarded_ambiguity(&execution, &error);

        assert!(!signal.is_triggered());
    }

    #[test]
    fn known_turn_recovery_failure_keeps_the_recovery_stage() {
        let error: GuardedFailure = ContextGuardedTurnPassError::Execution {
            stage: TurnPassExecutionStage::ActiveTurnRecovery,
            turn: Some(turn()),
            source: CommitAmbiguousFailure,
        };

        assert_eq!(
            guarded_failure_stage(&error),
            TurnPassExecutionStage::ActiveTurnRecovery.operator_label(),
        );
    }
}
