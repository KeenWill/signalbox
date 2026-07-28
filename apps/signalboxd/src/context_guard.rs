//! Exact pre-activation context-window guarding and automatic compaction.

use std::{error::Error, fmt, future::Future, sync::Arc};

use signalbox_application::{
    ClassifyOperatorFailure, EligibilityPass, ModelCallInputTokenCount, ModelCallInputTokenCounter,
    OperatorFailureClass, ToolCatalog,
};
use signalbox_domain::{
    AcceptedInputTurnActivationIdentities, ContextFrontierId, ModelCallId,
    SemanticTranscriptEntryId, SessionId, TurnAttemptId,
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
    ActivatedTurnExecution, HubModelConfiguration, process_runtime::compact_automatically,
    report_ambiguous_commit,
};

/// Exact-guard failure before activation or during the resulting execution.
#[derive(Debug)]
pub enum ContextGuardedTurnPassError<CountError, ExecutionError> {
    /// Read-only activation preview or exact guarded commit failed.
    Activation(StartEligibleTurnRepositoryError),
    /// Prospective ordinary-call reconstitution failed.
    Operation(ModelCallRepositoryError),
    /// Canonical frontier rendering failed.
    Render(signalbox_application::ModelFrontierRenderingError),
    /// Provider-native exact counting failed.
    Count(CountError),
    /// A never-cancelled production count unexpectedly reported cancellation.
    CountCancelled,
    /// The prospective target was absent from the immutable runtime catalog.
    ContextWindowUnavailable,
    /// One automatic compaction still could not make the prospective input fit.
    ContextStillExceeded,
    /// The shared append-only compaction lifecycle failed.
    Compaction(OperatorFailureClass),
    /// Execution after exact guarded activation failed.
    Execution(ExecutionError),
    /// The guarded commit returned an activation for another session.
    ActivationSessionMismatch,
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
            Self::Activation(error) => error.operator_failure_class(),
            Self::Operation(error) => error.operator_failure_class(),
            Self::Render(_) | Self::CountCancelled | Self::ActivationSessionMismatch => {
                OperatorFailureClass::FailClosedCorruption
            }
            Self::Count(error) => error.operator_failure_class(),
            Self::ContextWindowUnavailable | Self::ContextStillExceeded => {
                OperatorFailureClass::CallerOrHubBug
            }
            Self::Compaction(failure_class) => *failure_class,
            Self::Execution(error) => error.operator_failure_class(),
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
    compaction_credential_reference: Arc<str>,
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
            .field(
                "compaction_credential_reference",
                &self.compaction_credential_reference,
            )
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
        compaction_credential_reference: impl Into<Arc<str>>,
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
            compaction_credential_reference: compaction_credential_reference.into(),
            execution,
        }
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
        let compaction_credential_reference = Arc::clone(&self.compaction_credential_reference);
        let execution = self.execution.clone();
        async move {
            execution
                .resume_active(session)
                .await
                .map_err(ContextGuardedTurnPassError::Execution)?;
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
                        Err(error) => return Err(ContextGuardedTurnPassError::Activation(error)),
                    };
                    let call = ModelCallId::from_uuid(uuid::Uuid::now_v7());
                    let prospective = match model_calls
                        .preview_activation_operation(preview.prepared(), call)
                        .await
                    {
                        Ok(prospective) => prospective,
                        Err(ModelCallRepositoryError::IdentityCollision(_)) => continue,
                        Err(error) => return Err(ContextGuardedTurnPassError::Operation(error)),
                    };
                    let operation = prospective
                        .render(tools.definitions())
                        .map_err(ContextGuardedTurnPassError::Render)?;
                    let target = operation.request().call().target();
                    let model = runtime_models
                        .resolve(target)
                        .ok_or(ContextGuardedTurnPassError::ContextWindowUnavailable)?;
                    let input_tokens = match counter
                        .count_input_tokens(operation, std::future::pending())
                        .await
                        .map_err(ContextGuardedTurnPassError::Count)?
                    {
                        ModelCallInputTokenCount::Counted(count) => count,
                        ModelCallInputTokenCount::Cancelled => {
                            return Err(ContextGuardedTurnPassError::CountCancelled);
                        }
                    };
                    let requested_tokens = input_tokens
                        .checked_add(u64::from(model.max_output_tokens()))
                        .ok_or(ContextGuardedTurnPassError::ContextStillExceeded)?;
                    if requested_tokens > u64::from(model.context_window_tokens()) {
                        if compacted {
                            return Err(ContextGuardedTurnPassError::ContextStillExceeded);
                        }
                        let turn = preview.prepared().turn().turn();
                        match compact_automatically(
                            model_calls.pool(),
                            &model_configuration,
                            &compaction_model,
                            &compaction_credential_reference,
                            session,
                            turn,
                        )
                        .await
                        {
                            Ok(_) => {}
                            Err(crate::process_runtime::AutomaticContextCompactionError::AlreadyAttempted) => {
                                return Err(ContextGuardedTurnPassError::ContextStillExceeded);
                            }
                            Err(error) => {
                                return Err(ContextGuardedTurnPassError::Compaction(
                                    error.operator_failure_class(),
                                ));
                            }
                        }
                        compacted = true;
                        continue;
                    }
                    let committed = activation
                        .commit_counted_preview(preview, call, &model_calls)
                        .await
                        .map_err(|error| match error {
                            CommitActivationPreviewError::Activation(error) => {
                                ContextGuardedTurnPassError::Activation(error)
                            }
                            CommitActivationPreviewError::ModelCall(error) => {
                                ContextGuardedTurnPassError::Operation(error)
                            }
                        })?;
                    match committed {
                        CommitActivationPreviewOutcome::Stale => continue,
                        CommitActivationPreviewOutcome::Activated(activated) => {
                            if activated.session() != session {
                                execution.report_post_activation_failure();
                                return Err(ContextGuardedTurnPassError::ActivationSessionMismatch);
                            }
                            return execution
                                .execute(activated)
                                .await
                                .map_err(ContextGuardedTurnPassError::Execution);
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
    if matches!(error, ContextGuardedTurnPassError::Execution(_)) {
        return;
    }
    report_ambiguous_commit(execution, error);
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
    use signalbox_domain::ActivatedAcceptedInputTurn;
    use signalbox_persistence::{
        context_compaction::ContextCompactionRepositoryError,
        start_eligible_turn::StartEligibleTurnRepositoryError,
    };

    use super::{ContextGuardedTurnPassError, report_guarded_ambiguity};
    use crate::{
        ActivatedTurnExecution, FatalExecutionSignal, FatalExecutionSupervisor,
        process_runtime::AutomaticContextCompactionError,
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
            _activated: Box<ActivatedAcceptedInputTurn>,
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

    /// The exact lost-acknowledgement failure the activation repository reports
    /// when a guarded counted commit cannot be proven.
    fn ambiguous_activation() -> GuardedFailure {
        ContextGuardedTurnPassError::Activation(StartEligibleTurnRepositoryError::Database {
            source: sqlx::Error::PoolClosed,
            commit_ambiguous: true,
        })
    }

    /// The exact failure automatic compaction preparation reports when its own
    /// durable prepare cannot be proven committed.
    fn ambiguous_compaction() -> GuardedFailure {
        ContextGuardedTurnPassError::Compaction(
            AutomaticContextCompactionError::Repository(
                ContextCompactionRepositoryError::CommitAmbiguous(sqlx::Error::PoolClosed),
            )
            .operator_failure_class(),
        )
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

        report_guarded_ambiguity(&execution, &ambiguous_compaction());

        assert!(signal.is_triggered());
    }

    /// S03 / INV-034: a database failure before any commit boundary is ordinary
    /// scheduler retry work and raises no recovery signal.
    #[test]
    fn s03_inv034_activation_failure_before_the_commit_boundary_reports_nothing() {
        let (execution, signal) = supervised();

        let error: GuardedFailure =
            ContextGuardedTurnPassError::Activation(StartEligibleTurnRepositoryError::Database {
                source: sqlx::Error::PoolClosed,
                commit_ambiguous: false,
            });

        report_guarded_ambiguity(&execution, &error);

        assert!(!signal.is_triggered());
    }

    /// S03 / INV-034: execution failures keep their own supervision rule, so
    /// the guarded pass adds no second reaction to them.
    #[test]
    fn s03_inv034_execution_failure_keeps_its_own_supervision_rule() {
        let (execution, signal) = supervised();

        let error: GuardedFailure = ContextGuardedTurnPassError::Execution(CommitAmbiguousFailure);

        report_guarded_ambiguity(&execution, &error);

        assert!(!signal.is_triggered());
    }
}
