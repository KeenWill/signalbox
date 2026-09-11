//! Call-free credential admission waits.

use crate::{ContextFrontierId, TurnAttemptId};

/// Why credential admission retains a turn without issuing a call.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CredentialAvailabilityWaitCause {
    /// Every otherwise-admissible member has its invocation capacity reserved.
    Contended,
    /// Every member is excluded and at least one can become available again.
    Exhausted,
    /// Every pool member exhausted provider-internal retries and awaits network recovery.
    NetworkUnavailable,
}

/// The immutable attempt and frontier from which credential admission parked.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CredentialAvailabilityWait {
    attempt: TurnAttemptId,
    frontier: ContextFrontierId,
    cause: CredentialAvailabilityWaitCause,
}

impl CredentialAvailabilityWait {
    /// Reconstitutes the phase from independently checked durable wait evidence.
    pub const fn new(
        attempt: TurnAttemptId,
        frontier: ContextFrontierId,
        cause: CredentialAvailabilityWaitCause,
    ) -> Self {
        Self {
            attempt,
            frontier,
            cause,
        }
    }

    /// The call-free attempt that yielded to this wait.
    pub const fn attempt(self) -> TurnAttemptId {
        self.attempt
    }

    /// The unchanged transcript frontier retained by the wait.
    pub const fn frontier(self) -> ContextFrontierId {
        self.frontier
    }

    /// The closed admission cause, independent of its wake sources.
    pub const fn cause(self) -> CredentialAvailabilityWaitCause {
        self.cause
    }
}
