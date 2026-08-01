//! Explicit append-only session-placement update orchestration.

use std::future::Future;

use signalbox_domain::{
    DurableCommandId, SessionId, SessionPlacement, SessionPlacementVersion,
    UpdateSessionPlacement as DomainUpdateSessionPlacement, UpdateSessionPlacementResult,
};

/// Complete admitted placement update request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UpdateSessionPlacementRequest {
    command_id: DurableCommandId,
    session: SessionId,
    expected_version: SessionPlacementVersion,
    replacement: SessionPlacement,
}

impl UpdateSessionPlacementRequest {
    pub const fn new(
        command_id: DurableCommandId,
        session: SessionId,
        expected_version: SessionPlacementVersion,
        replacement: SessionPlacement,
    ) -> Self {
        Self {
            command_id,
            session,
            expected_version,
            replacement,
        }
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
