//! Deployment-scoped pending-runner promotion orchestration.

use std::future::Future;

use signalbox_domain::{
    DurableCommandId, PromotePendingRunner, PromotePendingRunnerResult, RunnerEnrollmentRequestId,
};

use crate::InvalidDurableCommandId;

/// Complete admitted request to activate one pending runner.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PromotePendingRunnerRequest {
    command: DurableCommandId,
    pending_request: RunnerEnrollmentRequestId,
}

impl PromotePendingRunnerRequest {
    /// Rejects reserved durable-command identities before transaction entry.
    pub fn try_new(
        command: DurableCommandId,
        pending_request: RunnerEnrollmentRequestId,
    ) -> Result<Self, InvalidDurableCommandId> {
        if command.as_uuid().is_nil() {
            return Err(InvalidDurableCommandId::Nil);
        }
        if command.as_uuid().is_max() {
            return Err(InvalidDurableCommandId::Max);
        }
        Ok(Self {
            command,
            pending_request,
        })
    }
}

/// Atomic durable handling boundary for pending-runner promotion.
pub trait PromotePendingRunnerTransaction {
    type Error;

    fn handle(
        &mut self,
        command: PromotePendingRunner,
    ) -> impl Future<Output = Result<PromotePendingRunnerOutcome, Self::Error>> + Send;
}

/// First handling/equal replay or conflicting durable-command reuse.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PromotePendingRunnerOutcome {
    /// The exact command already owns this terminal durable result.
    Recorded(PromotePendingRunnerResult),
    /// The user-global command identity names different intent.
    ConflictingReuse { command: DurableCommandId },
}

/// Coordinates one canonical deployment-scoped promotion.
#[derive(Debug)]
pub struct PromotePendingRunnerService<Transaction> {
    transaction: Transaction,
}

impl<Transaction> PromotePendingRunnerService<Transaction> {
    pub const fn new(transaction: Transaction) -> Self {
        Self { transaction }
    }
}

impl<Transaction: PromotePendingRunnerTransaction> PromotePendingRunnerService<Transaction> {
    pub async fn execute(
        &mut self,
        request: PromotePendingRunnerRequest,
    ) -> Result<PromotePendingRunnerOutcome, Transaction::Error> {
        self.transaction
            .handle(PromotePendingRunner::new(
                request.command,
                request.pending_request,
            ))
            .await
    }
}

#[cfg(test)]
mod tests {
    use signalbox_domain::{DurableCommandId, RunnerEnrollmentRequestId};
    use uuid::Uuid;

    use super::PromotePendingRunnerRequest;
    use crate::InvalidDurableCommandId;

    /// S32 / INV-001: reserved durable-command identities fail before the
    /// pending-runner promotion transaction can observe a request.
    #[test]
    fn inv001_promotion_request_rejects_reserved_command_identifiers() {
        let pending = RunnerEnrollmentRequestId::from_uuid(Uuid::from_u128(1));

        assert_eq!(
            PromotePendingRunnerRequest::try_new(DurableCommandId::from_uuid(Uuid::nil()), pending,),
            Err(InvalidDurableCommandId::Nil)
        );
        assert_eq!(
            PromotePendingRunnerRequest::try_new(DurableCommandId::from_uuid(Uuid::max()), pending,),
            Err(InvalidDurableCommandId::Max)
        );
    }
}
