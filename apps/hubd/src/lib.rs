//! Hub-owned composition between turn activation and model execution.
//!
//! ADR-0044 makes this package the composition root. The scheduler pass below
//! hands each complete activated-turn outcome to a fresh execution invocation;
//! concrete provider selection remains an injected composition choice.

use std::{error::Error, fmt, future::Future};

use signalbox_application::{
    ClassifyOperatorFailure, EligibilityPass, InProcessAttemptDispatchGate,
    ModelCallExecutionError, ModelCallExecutionOutcome, ModelCallExecutionService,
    ScriptedModelCallError, ScriptedModelCallProvider, ScriptedModelCallStep,
    StartEligibleTurnIdGenerator, StartEligibleTurnOutcome, StartEligibleTurnService,
    StartEligibleTurnTransaction, UuidV7ModelCallExecutionIdGenerator,
};
use signalbox_domain::{ActivatedAcceptedInputTurn, AssistantText, SessionId};
use signalbox_persistence::model_execution::{
    ModelCallRepositoryError, PostgresModelCallRepository,
};
use tokio::sync::watch;

/// Per-activation model execution constructed by the hub composition root.
pub trait ActivatedTurnExecution {
    /// Classified failure from the application service or provider adapter.
    type Error: ClassifyOperatorFailure;

    /// Consumes one exact activation outcome and drives its initial call.
    fn execute(
        &self,
        activated: Box<ActivatedAcceptedInputTurn>,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static;

    /// Reports that durable activation may require startup recovery.
    fn report_post_activation_failure(&self) {}
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
    Execution: ActivatedTurnExecution,
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

    fn report_post_activation_failure(&self) {
        self.fatal_signal.send_replace(true);
    }
}

async fn supervise_execution<Execution, ExecutionError>(
    fatal_signal: watch::Sender<bool>,
    execution: Execution,
) -> Result<(), ExecutionError>
where
    Execution: Future<Output = Result<(), ExecutionError>>,
{
    let result = execution.await;
    if result.is_err() {
        fatal_signal.send_replace(true);
    }
    result
}

/// Scheduler-pass failure retaining whether activation or execution failed.
#[derive(Debug)]
pub enum ActivatedTurnPassError<ActivationError, ExecutionError> {
    /// The authoritative activation transaction failed.
    Activation(ActivationError),
    /// A transaction, capability, or provider stage failed after activation.
    Execution(ExecutionError),
    /// The transaction returned an activation for another hinted session.
    ActivationSessionMismatch,
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
            Self::Execution(error) => write!(formatter, "activated turn execution failed: {error}"),
            Self::ActivationSessionMismatch => {
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
            Self::Execution(error) => error.operator_failure_class(),
            Self::ActivationSessionMismatch => {
                signalbox_application::OperatorFailureClass::CallerOrHubBug
            }
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

    fn run(
        &mut self,
        session: SessionId,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
        let activation = self.activation.execute_with_cloned_transaction(session);
        let execution = self.execution.clone();
        async move {
            let outcome = match activation.await {
                Ok(outcome) => outcome,
                Err(error) => {
                    if matches!(
                        error.operator_failure_class(),
                        signalbox_application::OperatorFailureClass::Infrastructure {
                            commit_ambiguous: true
                        }
                    ) {
                        execution.report_post_activation_failure();
                    }
                    return Err(ActivatedTurnPassError::Activation(error));
                }
            };
            match outcome {
                StartEligibleTurnOutcome::NoEligibleTurn => Ok(()),
                StartEligibleTurnOutcome::Activated(activated) => {
                    if !activation_session_matches(&execution, session, activated.session()) {
                        return Err(ActivatedTurnPassError::ActivationSessionMismatch);
                    }
                    execution
                        .execute(activated)
                        .await
                        .map_err(ActivatedTurnPassError::Execution)
                }
            }
        }
    }
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
pub type PostgresScriptedModelExecutionError = ModelCallExecutionError<
    ModelCallRepositoryError,
    ModelCallRepositoryError,
    ModelCallRepositoryError,
    ScriptedModelCallError,
    ModelCallRepositoryError,
>;

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
                match service.execute(session).await? {
                    ModelCallExecutionOutcome::Checkpointed(_) => continue,
                    ModelCallExecutionOutcome::NoWork
                    | ModelCallExecutionOutcome::TargetUnavailable(_)
                    | ModelCallExecutionOutcome::PendingSteering { .. }
                    | ModelCallExecutionOutcome::CapabilityKnownFailure(_)
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
        future::{Future, ready},
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
        activation_session_matches, supervise_execution,
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
}
