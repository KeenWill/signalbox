//! Shared operator-facing failure classification.
//!
//! ADR-0044 owns this closed taxonomy. Adapter-specific errors retain their
//! diagnostic detail while exposing only this user-content-free classification
//! to shared runtime telemetry.

/// The closed operator-facing classification for adapter/runtime failures.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum OperatorFailureClass {
    /// Infrastructure prevented the operation from completing.
    Infrastructure {
        /// The connection failed where durable commit may or may not have won.
        commit_ambiguous: bool,
    },
    /// Committed records cannot construct the accepted domain value.
    FailClosedCorruption,
    /// A fresh hub-minted identity collided with a durable identity.
    IdentityCollision,
    /// The request or an internal guard can fail only because of a defect.
    CallerOrHubBug,
}

/// Maps an adapter/runtime error into the shared operator taxonomy.
pub trait ClassifyOperatorFailure {
    /// Returns a user-content-free classification for shared telemetry.
    fn operator_failure_class(&self) -> OperatorFailureClass;
}
