//! Session-scoped lost-runner abandonment orchestration.

use std::future::Future;

use signalbox_domain::{
    AbandonLostRunner, AbandonLostRunnerResult, DurableCommandId, RunnerGeneration, SessionId,
};

use crate::InvalidDurableCommandId;

/// Complete admitted request to terminalize one exact lost runner placement.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AbandonLostRunnerRequest {
    command: DurableCommandId,
    session: SessionId,
    expected_placement_revision: RunnerGeneration,
}

impl AbandonLostRunnerRequest {
    /// Rejects reserved durable-command identities before transaction entry.
    pub fn try_new(
        command: DurableCommandId,
        session: SessionId,
        expected_placement_revision: RunnerGeneration,
    ) -> Result<Self, InvalidDurableCommandId> {
        if command.as_uuid().is_nil() {
            return Err(InvalidDurableCommandId::Nil);
        }
        if command.as_uuid().is_max() {
            return Err(InvalidDurableCommandId::Max);
        }
        Ok(Self {
            command,
            session,
            expected_placement_revision,
        })
    }
}

/// Atomic durable handling boundary for lost-runner abandonment.
pub trait AbandonLostRunnerTransaction {
    type Error;

    fn handle(
        &mut self,
        command: AbandonLostRunner,
    ) -> impl Future<Output = Result<AbandonLostRunnerOutcome, Self::Error>> + Send;
}

/// First handling/equal replay or conflicting durable-command reuse.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AbandonLostRunnerOutcome {
    /// The exact command owns this terminal durable result.
    Recorded(AbandonLostRunnerResult),
    /// The user-global command identity names different intent.
    ConflictingReuse { command: DurableCommandId },
}

/// Coordinates one canonical session-scoped abandonment.
#[derive(Debug)]
pub struct AbandonLostRunnerService<Transaction> {
    transaction: Transaction,
}

impl<Transaction> AbandonLostRunnerService<Transaction> {
    pub const fn new(transaction: Transaction) -> Self {
        Self { transaction }
    }
}

impl<Transaction: AbandonLostRunnerTransaction> AbandonLostRunnerService<Transaction> {
    pub async fn execute(
        &mut self,
        request: AbandonLostRunnerRequest,
    ) -> Result<AbandonLostRunnerOutcome, Transaction::Error> {
        self.transaction
            .handle(AbandonLostRunner::new(
                request.command,
                request.session,
                request.expected_placement_revision,
            ))
            .await
    }
}

#[cfg(test)]
mod tests {
    use signalbox_domain::{DurableCommandId, RunnerGeneration, SessionId};
    use uuid::Uuid;

    use super::AbandonLostRunnerRequest;
    use crate::InvalidDurableCommandId;

    /// INV-001: reserved durable-command identities fail before the
    /// lost-runner abandonment transaction can observe a request.
    #[test]
    fn inv001_abandonment_request_rejects_reserved_command_identifiers() {
        let session = SessionId::from_uuid(Uuid::from_u128(1));
        let revision = RunnerGeneration::try_from_u64(1).expect("positive fixture revision");

        assert_eq!(
            AbandonLostRunnerRequest::try_new(
                DurableCommandId::from_uuid(Uuid::nil()),
                session,
                revision,
            ),
            Err(InvalidDurableCommandId::Nil)
        );
        assert_eq!(
            AbandonLostRunnerRequest::try_new(
                DurableCommandId::from_uuid(Uuid::max()),
                session,
                revision,
            ),
            Err(InvalidDurableCommandId::Max)
        );
    }
}
