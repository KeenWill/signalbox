//! Deterministic, bounded review-slog evidence and pure gate composition.

mod inventory;
mod stack;

pub use inventory::{
    ReviewAuthorClass, ReviewDispositionClass, ReviewThreadInventoryFields,
    ReviewThreadInventoryItem, ThreadInventoryResult,
};
pub use stack::{ChildStackState, StackStateFields, StackStateResult};

/// Exact marker used when a review finding crosses the wave hard stop.
pub const ESCALATION_MARKER: &str = "Escalated without disposition";

pub(super) use inventory::{
    author_class, authorized_association, disposition_class, finding_title,
};

pub(super) fn stack_into_value(result: StackStateResult) -> serde_json::Value {
    result.into_value()
}

pub(super) fn inventory_into_value(result: ThreadInventoryResult) -> serde_json::Value {
    result.into_value()
}
