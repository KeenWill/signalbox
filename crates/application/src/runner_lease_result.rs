//! Atomic durable admission of one runner terminal result.

use std::future::Future;

use signalbox_domain::{RunnerLeaseCompletion, RunnerLeaseCorrelation, ToolAttemptObservation};

/// Exact claimed-lease correlation and terminal evidence returned by a runner.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunnerLeaseResultRequest {
    correlation: RunnerLeaseCorrelation,
    observation: ToolAttemptObservation,
}

impl RunnerLeaseResultRequest {
    /// Retains the complete immutable lease fence and bounded observation.
    pub const fn new(
        correlation: RunnerLeaseCorrelation,
        observation: ToolAttemptObservation,
    ) -> Self {
        Self {
            correlation,
            observation,
        }
    }

    /// Borrows the complete claimed-lease correlation.
    pub const fn correlation(&self) -> &RunnerLeaseCorrelation {
        &self.correlation
    }

    /// Borrows the terminal runner observation.
    pub const fn observation(&self) -> &ToolAttemptObservation {
        &self.observation
    }

    /// Returns the complete result-admission input.
    pub fn into_parts(self) -> (RunnerLeaseCorrelation, ToolAttemptObservation) {
        (self.correlation, self.observation)
    }
}

/// Atomic result boundary that precedes `result_recorded` delivery.
pub trait RunnerLeaseResultTransaction {
    /// Adapter-specific transaction failure.
    type Error;

    /// Commits the exact lease and physical-attempt terminal pair.
    fn commit_result(
        &mut self,
        request: RunnerLeaseResultRequest,
    ) -> impl Future<Output = Result<RunnerLeaseCompletion, Self::Error>> + Send;
}

/// Coordinates one exact runner result admission.
#[derive(Debug)]
pub struct RunnerLeaseResultService<Transaction> {
    transaction: Transaction,
}

impl<Transaction> RunnerLeaseResultService<Transaction> {
    /// Uses the supplied durable result boundary.
    pub const fn new(transaction: Transaction) -> Self {
        Self { transaction }
    }
}

impl<Transaction> RunnerLeaseResultService<Transaction>
where
    Transaction: RunnerLeaseResultTransaction,
{
    /// Commits one exact result before any acknowledgement is emitted.
    pub async fn execute(
        &mut self,
        request: RunnerLeaseResultRequest,
    ) -> Result<RunnerLeaseCompletion, Transaction::Error> {
        self.transaction.commit_result(request).await
    }
}

#[cfg(test)]
mod tests {
    use std::future::ready;

    use signalbox_domain::{
        RunnerGeneration, RunnerId, RunnerLeaseCompletion, RunnerLeaseCorrelation, RunnerLeaseId,
        RunnerSandboxProfile, RunnerWorkingDirectory, SessionId, ToolAttemptDispatchCorrelation,
        ToolAttemptDispatchCorrelationReconstitutionInput, ToolAttemptId, ToolAttemptObservation,
        ToolDispatchGeneration, ToolName, ToolRequestId, TurnAttemptId, TurnId,
    };
    use uuid::Uuid;

    use super::{RunnerLeaseResultRequest, RunnerLeaseResultService, RunnerLeaseResultTransaction};

    const LEASE_ID: u128 = 1;
    const RUNNER_ID: u128 = 2;
    const SESSION_ID: u128 = 3;
    const TURN_ID: u128 = 4;
    const ISSUING_ATTEMPT_ID: u128 = 5;
    const REQUEST_ID: u128 = 6;
    const ATTEMPT_ID: u128 = 7;
    const TRANSACTION_REJECTION: &str = "rejected";

    #[derive(Debug)]
    struct RejectingTransaction;

    impl RunnerLeaseResultTransaction for RejectingTransaction {
        type Error = &'static str;

        fn commit_result(
            &mut self,
            _request: RunnerLeaseResultRequest,
        ) -> impl Future<Output = Result<RunnerLeaseCompletion, Self::Error>> + Send {
            ready(Err(TRANSACTION_REJECTION))
        }
    }

    fn correlation() -> RunnerLeaseCorrelation {
        RunnerLeaseCorrelation {
            lease: RunnerLeaseId::from_uuid(Uuid::from_u128(LEASE_ID)),
            runner: RunnerId::from_uuid(Uuid::from_u128(RUNNER_ID)),
            registration_revision: RunnerGeneration::one(),
            placement_revision: RunnerGeneration::one(),
            working_directory: RunnerWorkingDirectory::try_new("workspace".to_owned())
                .expect("the fixture working directory is exact"),
            sandbox: RunnerSandboxProfile::WorkspaceRestricted,
            tool: ToolName::try_new("sandboxed_exec".to_owned())
                .expect("the fixture tool name is portable"),
            dispatch: ToolAttemptDispatchCorrelation::reconstitute(
                ToolAttemptDispatchCorrelationReconstitutionInput {
                    session: SessionId::from_uuid(Uuid::from_u128(SESSION_ID)),
                    turn: TurnId::from_uuid(Uuid::from_u128(TURN_ID)),
                    issuing_attempt: TurnAttemptId::from_uuid(Uuid::from_u128(ISSUING_ATTEMPT_ID)),
                    request: ToolRequestId::from_uuid(Uuid::from_u128(REQUEST_ID)),
                    attempt: ToolAttemptId::from_uuid(Uuid::from_u128(ATTEMPT_ID)),
                    generation: ToolDispatchGeneration::first(),
                },
            ),
            generation: RunnerGeneration::one(),
        }
    }

    #[test]
    fn request_retains_the_complete_result_input() {
        let expected_correlation = correlation();
        let expected_observation = ToolAttemptObservation::Ambiguous;
        let request = RunnerLeaseResultRequest::new(
            expected_correlation.clone(),
            expected_observation.clone(),
        );

        assert_eq!(request.correlation(), &expected_correlation);
        assert_eq!(request.observation(), &expected_observation);
        assert_eq!(
            request.into_parts(),
            (expected_correlation, expected_observation)
        );
    }

    #[tokio::test]
    async fn service_propagates_the_transaction_refusal() {
        let mut service = RunnerLeaseResultService::new(RejectingTransaction);

        assert_eq!(
            service
                .execute(RunnerLeaseResultRequest::new(
                    correlation(),
                    ToolAttemptObservation::Ambiguous,
                ))
                .await,
            Err(TRANSACTION_REJECTION)
        );
    }
}
