//! Atomic durable admission of one runner lease claim.

use std::future::Future;

use signalbox_domain::{RunnerLease, RunnerLeaseCorrelation};

/// Exact offered correlation supplied by one runner claim frame.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunnerLeaseClaimRequest {
    correlation: RunnerLeaseCorrelation,
}

impl RunnerLeaseClaimRequest {
    /// Retains the complete immutable offered-lease correlation.
    pub const fn new(correlation: RunnerLeaseCorrelation) -> Self {
        Self { correlation }
    }

    /// Borrows the complete offered-lease correlation.
    pub const fn correlation(&self) -> &RunnerLeaseCorrelation {
        &self.correlation
    }

    /// Returns the complete offered-lease correlation.
    pub fn into_correlation(self) -> RunnerLeaseCorrelation {
        self.correlation
    }
}

/// Atomic durable claim boundary that precedes `lease_claimed` delivery.
pub trait RunnerLeaseClaimTransaction {
    /// Adapter-specific transaction failure.
    type Error;

    /// Commits the exact claim and returns its canonical claimed lease.
    fn claim(
        &mut self,
        request: RunnerLeaseClaimRequest,
    ) -> impl Future<Output = Result<RunnerLease, Self::Error>> + Send;
}

/// Coordinates one exact runner lease claim.
#[derive(Debug)]
pub struct RunnerLeaseClaimService<Transaction> {
    transaction: Transaction,
}

impl<Transaction> RunnerLeaseClaimService<Transaction> {
    /// Uses the supplied durable claim boundary.
    pub const fn new(transaction: Transaction) -> Self {
        Self { transaction }
    }
}

impl<Transaction> RunnerLeaseClaimService<Transaction>
where
    Transaction: RunnerLeaseClaimTransaction,
{
    /// Commits one exact claim before any acknowledgement is emitted.
    pub async fn execute(
        &mut self,
        request: RunnerLeaseClaimRequest,
    ) -> Result<RunnerLease, Transaction::Error> {
        self.transaction.claim(request).await
    }
}

#[cfg(test)]
mod tests {
    use std::future::ready;

    use signalbox_domain::{
        RunnerGeneration, RunnerId, RunnerLease, RunnerLeaseCorrelation, RunnerLeaseId,
        RunnerSandboxProfile, RunnerWorkingDirectory, SessionId, ToolAttemptDispatchCorrelation,
        ToolAttemptDispatchCorrelationReconstitutionInput, ToolAttemptId, ToolDispatchGeneration,
        ToolName, ToolRequestId, TurnAttemptId, TurnId,
    };
    use uuid::Uuid;

    use super::{RunnerLeaseClaimRequest, RunnerLeaseClaimService, RunnerLeaseClaimTransaction};

    const LEASE: u128 = 1;
    const RUNNER: u128 = 2;
    const SESSION: u128 = 3;
    const TURN: u128 = 4;
    const TURN_ATTEMPT: u128 = 5;
    const REQUEST: u128 = 6;
    const TOOL_ATTEMPT: u128 = 7;

    #[derive(Debug)]
    struct RejectingTransaction;

    impl RunnerLeaseClaimTransaction for RejectingTransaction {
        type Error = &'static str;

        fn claim(
            &mut self,
            _request: RunnerLeaseClaimRequest,
        ) -> impl Future<Output = Result<RunnerLease, Self::Error>> + Send {
            ready(Err("rejected"))
        }
    }

    fn correlation() -> RunnerLeaseCorrelation {
        RunnerLeaseCorrelation {
            lease: RunnerLeaseId::from_uuid(Uuid::from_u128(LEASE)),
            runner: RunnerId::from_uuid(Uuid::from_u128(RUNNER)),
            registration_revision: RunnerGeneration::one(),
            placement_revision: RunnerGeneration::one(),
            working_directory: RunnerWorkingDirectory::try_new("workspace".to_owned())
                .expect("the fixture working directory is exact"),
            sandbox: RunnerSandboxProfile::WorkspaceRestricted,
            tool: ToolName::try_new("sandboxed_exec".to_owned())
                .expect("the fixture tool name is portable"),
            dispatch: ToolAttemptDispatchCorrelation::reconstitute(
                ToolAttemptDispatchCorrelationReconstitutionInput {
                    session: SessionId::from_uuid(Uuid::from_u128(SESSION)),
                    turn: TurnId::from_uuid(Uuid::from_u128(TURN)),
                    issuing_attempt: TurnAttemptId::from_uuid(Uuid::from_u128(TURN_ATTEMPT)),
                    request: ToolRequestId::from_uuid(Uuid::from_u128(REQUEST)),
                    attempt: ToolAttemptId::from_uuid(Uuid::from_u128(TOOL_ATTEMPT)),
                    generation: ToolDispatchGeneration::first(),
                },
            ),
            generation: RunnerGeneration::one(),
        }
    }

    #[test]
    fn request_retains_the_complete_correlation() {
        let expected = correlation();
        let request = RunnerLeaseClaimRequest::new(expected.clone());

        assert_eq!(request.correlation(), &expected);
        assert_eq!(request.into_correlation(), expected);
    }

    #[tokio::test]
    async fn service_propagates_the_transaction_refusal() {
        let mut service = RunnerLeaseClaimService::new(RejectingTransaction);

        assert_eq!(
            service
                .execute(RunnerLeaseClaimRequest::new(correlation()))
                .await,
            Err("rejected")
        );
    }
}
