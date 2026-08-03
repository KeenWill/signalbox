//! Authorization boundary for one dedicated approval-judge provider call.

use signalbox_domain::{DirectModelSelection, ModelCallId, ResolvedProviderTarget, ToolRequest};

/// Exact durable binding authorized to enter an approval-judge provider.
pub trait ApprovalJudgeAuthorization {
    /// Borrows the exact parked request being judged.
    fn request(&self) -> &ToolRequest;

    /// Returns the dedicated model-call identity.
    fn call(&self) -> ModelCallId;

    /// Returns the direct selection frozen for the judge call.
    fn selection(&self) -> DirectModelSelection;

    /// Returns the exact resolved provider target.
    fn target(&self) -> ResolvedProviderTarget;

    /// Borrows the pinned non-secret credential reference.
    fn credential_reference(&self) -> &str;
}
