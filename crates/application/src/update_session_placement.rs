//! Explicit append-only session-placement update orchestration.

use std::future::Future;

use signalbox_domain::{
    DurableCommandId, SessionId, SessionPlacement, SessionPlacementVersion,
    UpdateSessionPlacement as DomainUpdateSessionPlacement, UpdateSessionPlacementResult,
};

use crate::InvalidDurableCommandId;

/// Complete admitted placement update request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UpdateSessionPlacementRequest {
    command_id: DurableCommandId,
    session: SessionId,
    expected_version: SessionPlacementVersion,
    replacement: SessionPlacement,
}

impl UpdateSessionPlacementRequest {
    /// Rejects reserved command identities before canonical construction.
    pub fn try_new(
        command_id: DurableCommandId,
        session: SessionId,
        expected_version: SessionPlacementVersion,
        replacement: SessionPlacement,
    ) -> Result<Self, InvalidDurableCommandId> {
        if command_id.as_uuid().is_nil() {
            return Err(InvalidDurableCommandId::Nil);
        }
        if command_id.as_uuid().is_max() {
            return Err(InvalidDurableCommandId::Max);
        }
        Ok(Self {
            command_id,
            session,
            expected_version,
            replacement,
        })
    }
}

#[cfg(test)]
mod tests {
    use signalbox_domain::{
        DurableCommandId, SessionId, SessionPlacement, SessionPlacementVersion,
    };
    use uuid::Uuid;

    use super::UpdateSessionPlacementRequest;
    use crate::InvalidDurableCommandId;

    /// S36: reserved command identities fail before a
    /// placement-update transaction can observe a canonical request.
    #[test]
    fn s36_request_rejects_reserved_command_identifiers() {
        let session = SessionId::from_uuid(Uuid::from_u128(1));

        assert_eq!(
            UpdateSessionPlacementRequest::try_new(
                DurableCommandId::from_uuid(Uuid::nil()),
                session,
                SessionPlacementVersion::INITIAL,
                SessionPlacement::pathless(),
            ),
            Err(InvalidDurableCommandId::Nil)
        );
        assert_eq!(
            UpdateSessionPlacementRequest::try_new(
                DurableCommandId::from_uuid(Uuid::max()),
                session,
                SessionPlacementVersion::INITIAL,
                SessionPlacement::pathless(),
            ),
            Err(InvalidDurableCommandId::Max)
        );
    }
}

/// Atomic durable handling boundary for placement updates.
pub trait UpdateSessionPlacementTransaction {
    type Error;
    fn handle(
        &mut self,
        command: DomainUpdateSessionPlacement,
    ) -> impl Future<Output = Result<UpdateSessionPlacementOutcome, Self::Error>> + Send;
}

/// First handling/equal replay or conflicting durable-command reuse.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UpdateSessionPlacementOutcome {
    Recorded(UpdateSessionPlacementResult),
    ConflictingReuse { command_id: DurableCommandId },
}

/// Coordinates one canonical placement update.
#[derive(Debug)]
pub struct UpdateSessionPlacementService<Transaction> {
    transaction: Transaction,
}

impl<Transaction> UpdateSessionPlacementService<Transaction> {
    pub const fn new(transaction: Transaction) -> Self {
        Self { transaction }
    }
}

impl<Transaction: UpdateSessionPlacementTransaction> UpdateSessionPlacementService<Transaction> {
    pub async fn execute(
        &mut self,
        request: UpdateSessionPlacementRequest,
    ) -> Result<UpdateSessionPlacementOutcome, Transaction::Error> {
        self.transaction
            .handle(DomainUpdateSessionPlacement::new(
                request.command_id,
                request.session,
                request.expected_version,
                request.replacement,
            ))
            .await
    }
}
