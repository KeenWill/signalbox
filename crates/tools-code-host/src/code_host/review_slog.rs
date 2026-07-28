//! Deterministic, bounded review-slog evidence and pure gate composition.

mod convergence;
mod gate;
mod inventory;
mod stack;

pub use convergence::{
    ConvergenceStateFields, ConvergenceStateResult, ConvergenceVerdict, ReviewCheck,
    ReviewThreadIdentity, ReviewerVerdictEvidence, ReviewerVerdictFields, ReviewerVerdictStatus,
};
pub use gate::{ReviewGateBlockerCode, ReviewGateCheckResult};
pub use inventory::{
    ReviewAuthorClass, ReviewDispositionClass, ReviewThreadInventoryFields,
    ReviewThreadInventoryItem, ThreadInventoryResult,
};
pub use stack::{ChildStackState, StackStateFields, StackStateResult};

/// Exact marker used when a review finding crosses the wave hard stop.
pub const ESCALATION_MARKER: &str = "Escalated without disposition";

pub(super) use convergence::{ReviewerActivity, reviewer_verdict_evidence};
pub(super) use inventory::{author_class, disposition_class, finding_title};

pub(super) fn convergence_into_value(result: ConvergenceStateResult) -> serde_json::Value {
    result.into_value()
}

pub(super) fn stack_into_value(result: StackStateResult) -> serde_json::Value {
    result.into_value()
}

pub(super) fn inventory_into_value(result: ThreadInventoryResult) -> serde_json::Value {
    result.into_value()
}

pub(super) fn gate_into_value(result: ReviewGateCheckResult) -> serde_json::Value {
    result.into_value()
}
