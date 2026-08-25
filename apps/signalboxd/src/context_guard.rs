//! Exact pre-activation context-window guarding and automatic compaction.

use std::{error::Error, fmt, future::Future, sync::Arc};

use signalbox_application::{
    ClassifyOperatorFailure, EligibilityPass, ModelCallInputTokenCount, ModelCallInputTokenCounter,
    OperatorFailureClass, ToolCatalog,
};
use signalbox_domain::{
    AcceptedInputTurnActivationIdentities, ContextFrontierId, ModelCallId,
    SemanticTranscriptEntryId, SessionId, TurnAttemptId, TurnId,
};
use signalbox_model_provider_runtime::{ContextCompactionModel, RuntimeModelCatalog};
use signalbox_persistence::{
    model_execution::{ModelCallRepositoryError, PostgresModelCallRepository},
    start_eligible_turn::{
        CommitActivationPreviewError, CommitActivationPreviewOutcome, StartEligibleTurnRepository,
        StartEligibleTurnRepositoryError,
    },
};

use crate::{
    ActivatedTurnExecution, HubModelConfiguration, TurnPassExecutionStage,
    WorkspaceInstructionRuntime, WorkspaceInstructionRuntimeError,
    process_runtime::compact_automatically, report_ambiguous_commit,
};
use tracing::Instrument;

/// Exact-guard failure before activation or during the resulting execution.
#[derive(Debug)]
pub enum ContextGuardedTurnPassError<CountError, ExecutionError> {
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
    /// Provider-native exact counting failed.
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
            Self::WorkspaceInstructions { source, .. } => source.operator_failure_class(),
            Self::Execution { source, .. } => source.operator_failure_class(),
        }
    }

    fn operator_failure_cause_code(&self) -> &'static str {
        match self {
            Self::Activation { .. } => "turn_activation_repository",
            Self::Operation { .. } => "model_call_repository",
            Self::Render { .. } => "model_frontier_rendering",
            Self::Count { source, .. } => source.operator_failure_cause_code(),
            Self::CountCancelled(_) => "model_input_count_cancelled",
            Self::ContextWindowUnavailable(_) => "context_window_unavailable",
            Self::ContextStillExceeded(_) => "context_window_exceeded",
            Self::Compaction { cause_code, .. } => cause_code,
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
    workspace_instructions: Option<WorkspaceInstructionRuntime>,
    execution: Execution,
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
            .field("workspace_instructions", &self.workspace_instructions)
            .field("execution", &self.execution)
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
            workspace_instructions: None,
            execution,
        }
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
            ContextGuardedTurnPassError::Activation { turn, .. }
            | ContextGuardedTurnPassError::Execution { turn, .. } => *turn,
            ContextGuardedTurnPassError::Operation { turn, .. }
            | ContextGuardedTurnPassError::Render { turn, .. }
            | ContextGuardedTurnPassError::Count { turn, .. }
            | ContextGuardedTurnPassError::Compaction { turn, .. }
            | ContextGuardedTurnPassError::WorkspaceInstructions { turn, .. } => Some(*turn),
            ContextGuardedTurnPassError::CountCancelled(turn)
            | ContextGuardedTurnPassError::ContextWindowUnavailable(turn)
            | ContextGuardedTurnPassError::ContextStillExceeded(turn)
            | ContextGuardedTurnPassError::ActivationSessionMismatch(turn) => Some(*turn),
        }
    }

    fn run(
        &mut self,
        session: SessionId,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
        let activation = self.activation.clone();
        let model_calls = self.model_calls.clone();
        let counter = self.counter.clone();
        let tools = self.tools.clone();
        let runtime_models = self.runtime_models.clone();
        let model_configuration = self.model_configuration.clone();
        let compaction_model = Arc::clone(&self.compaction_model);
        let workspace_instructions = self.workspace_instructions.clone();
        let execution = self.execution.clone();
        async move {
            execution.resume_active(session).await.map_err(|source| {
                ContextGuardedTurnPassError::Execution {
                    stage: TurnPassExecutionStage::ActiveTurnRecovery,
                    turn: Execution::active_resume_failure_turn(&source),
                    source,
                }
            })?;
            let outcome: Result<
                (),
                ContextGuardedTurnPassError<Counter::Error, Execution::Error>,
            > = async {
                let mut compacted = false;
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
                                report_guarded_turn_activation(activated.session(), activated.turn());
                                return execution
                                    .execute(activated)
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
                    };
                    let requested_tokens = input_tokens
                        .checked_add(u64::from(model.max_output_tokens()))
                        .ok_or(ContextGuardedTurnPassError::ContextStillExceeded(turn))?;
                    if requested_tokens > u64::from(model.context_window_tokens()) {
                        if compacted {
                            return Err(ContextGuardedTurnPassError::ContextStillExceeded(turn));
                        }
                        match compact_automatically(
                            &model_calls,
                            &model_configuration,
                            &compaction_model,
                            session,
                            turn,
                        )
                        .await
                        {
                            Ok(_) => {}
                            Err(crate::process_runtime::AutomaticContextCompactionError::AlreadyAttempted) => {
                                return Err(ContextGuardedTurnPassError::ContextStillExceeded(turn));
                            }
                            Err(error) => {
                                return Err(ContextGuardedTurnPassError::Compaction {
                                    turn,
                                    failure_class: error.operator_failure_class(),
                                    cause_code: error.operator_failure_cause_code(),
                                });
                            }
                        }
                        compacted = true;
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
                            call,
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
                            report_guarded_turn_activation(activated.session(), activated.turn());
                            return execution
                                .execute(activated)
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
        ContextGuardedTurnPassError::Activation { turn: None, .. } => "activation_preview",
        ContextGuardedTurnPassError::Activation { turn: Some(_), .. } => "activation_commit",
        ContextGuardedTurnPassError::Operation { .. } => "model_operation",
        ContextGuardedTurnPassError::Render { .. } => "frontier_rendering",
        ContextGuardedTurnPassError::Count { .. } => "input_token_count",
        ContextGuardedTurnPassError::CountCancelled(_) => "input_token_count",
        ContextGuardedTurnPassError::ContextWindowUnavailable(_) => "context_window",
        ContextGuardedTurnPassError::ContextStillExceeded(_) => "context_window",
        ContextGuardedTurnPassError::Compaction { .. } => "context_compaction",
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

fn activation_identities() -> AcceptedInputTurnActivationIdentities {
    AcceptedInputTurnActivationIdentities::new(
        SemanticTranscriptEntryId::from_uuid(uuid::Uuid::now_v7()),
        SemanticTranscriptEntryId::from_uuid(uuid::Uuid::now_v7()),
        ContextFrontierId::from_uuid(uuid::Uuid::now_v7()),
        TurnAttemptId::from_uuid(uuid::Uuid::now_v7()),
    )
}

#[cfg(test)]
mod tests {
    use std::{
        fmt,
        future::{Future, ready},
    };

    use signalbox_application::{ClassifyOperatorFailure, OperatorFailureClass};
    use signalbox_domain::{ActivatedTurn, TurnId};
    use signalbox_persistence::{
        context_compaction::ContextCompactionRepositoryError,
        start_eligible_turn::StartEligibleTurnRepositoryError,
    };

    use super::{ContextGuardedTurnPassError, guarded_failure_stage, report_guarded_ambiguity};
    use crate::{
        ActivatedTurnExecution, FatalExecutionSignal, FatalExecutionSupervisor,
        TurnPassExecutionStage, process_runtime::AutomaticContextCompactionError,
    };

    /// A classified failure whose durable commit outcome is unknown.
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    struct CommitAmbiguousFailure;

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
