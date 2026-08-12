//! Transaction-only replacement of a runner lost before initial pinning.

use std::future::Future;

use signalbox_domain::{
    DurableCommandId, ReplaceLostRunner, ReplaceLostRunnerBeforePinResult, RunnerGeneration,
    RunnerId, RunnerReplacementTarget, SessionId,
};

use crate::InvalidDurableCommandId;

/// Complete admitted request for the pre-pin replacement transaction.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReplaceLostRunnerBeforePinRequest {
    command: DurableCommandId,
    session: SessionId,
    expected_placement_revision: RunnerGeneration,
    replacement: RunnerId,
}

impl ReplaceLostRunnerBeforePinRequest {
    /// Rejects reserved durable-command identities before transaction entry.
    pub fn try_new(
        command: DurableCommandId,
        session: SessionId,
        expected_placement_revision: RunnerGeneration,
        replacement: RunnerId,
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
            replacement,
        })
    }

    /// Returns the user-global durable command identity.
    pub const fn command(&self) -> DurableCommandId {
        self.command
    }

    /// Returns the exact session whose lost placement is targeted.
    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// Returns the placement revision observed by the caller.
    pub const fn expected_placement_revision(&self) -> RunnerGeneration {
        self.expected_placement_revision
    }

    /// Returns the exact user-selected successor target.
    pub const fn replacement(&self) -> RunnerId {
        self.replacement
    }
}

/// Atomic durable handling boundary for one pre-pin replacement.
pub trait ReplaceLostRunnerBeforePinTransaction {
    type Error;

    fn handle(
        &mut self,
        command: ReplaceLostRunner,
    ) -> impl Future<Output = Result<ReplaceLostRunnerBeforePinOutcome, Self::Error>> + Send;
}

/// First handling/equal replay or conflicting durable-command reuse.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReplaceLostRunnerBeforePinOutcome {
    /// The exact command owns this terminal durable result.
    Recorded(ReplaceLostRunnerBeforePinResult),
    /// The user-global command identity names different intent.
    ConflictingReuse { command: DurableCommandId },
}

/// Coordinates one canonical pre-pin replacement command.
#[derive(Debug)]
pub struct ReplaceLostRunnerBeforePinService<Transaction> {
    transaction: Transaction,
}

impl<Transaction> ReplaceLostRunnerBeforePinService<Transaction> {
    pub const fn new(transaction: Transaction) -> Self {
        Self { transaction }
    }
}

impl<Transaction: ReplaceLostRunnerBeforePinTransaction>
    ReplaceLostRunnerBeforePinService<Transaction>
{
    pub async fn execute(
        &mut self,
        request: ReplaceLostRunnerBeforePinRequest,
    ) -> Result<ReplaceLostRunnerBeforePinOutcome, Transaction::Error> {
        self.transaction
            .handle(ReplaceLostRunner::new(
                request.command,
                request.session,
                request.expected_placement_revision,
                RunnerReplacementTarget::Runner(request.replacement),
            ))
            .await
    }
}

#[cfg(test)]
mod tests {
    use signalbox_domain::{DurableCommandId, RunnerGeneration, RunnerId, SessionId};
    use uuid::Uuid;

    use super::ReplaceLostRunnerBeforePinRequest;
    use crate::InvalidDurableCommandId;

    /// INV-001: reserved durable-command identities fail before the pre-pin
    /// replacement transaction can observe a request.
    #[test]
    fn inv001_pre_pin_replacement_request_rejects_reserved_command_identifiers() {
        let session = SessionId::from_uuid(Uuid::from_u128(1));
        let revision = RunnerGeneration::try_from_u64(1).expect("positive fixture revision");
        let target = RunnerId::from_uuid(Uuid::from_u128(2));

        assert_eq!(
            ReplaceLostRunnerBeforePinRequest::try_new(
                DurableCommandId::from_uuid(Uuid::nil()),
                session,
                revision,
                target,
            ),
            Err(InvalidDurableCommandId::Nil)
        );
        assert_eq!(
            ReplaceLostRunnerBeforePinRequest::try_new(
                DurableCommandId::from_uuid(Uuid::max()),
                session,
                revision,
                target,
            ),
            Err(InvalidDurableCommandId::Max)
        );
    }
}
