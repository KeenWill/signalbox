//! Evidence-bearing logical tool-batch transitions.
//!
//! `docs/spec/tool-loop.md` is normative. This aggregate validates one
//! producing call's complete request, approval, and attempt inventory before
//! it can expose an approval wait, prepare the next serialized physical
//! attempt, or project reference-only results.

mod batch;

#[cfg(test)]
mod tests;

pub use batch::{
    AwaitingToolApproval, AwaitingToolRecovery, DelegateToolApprovalTransitionError,
    DelegateToolApprovalTransitionFailure, PreparedDelegateToolApproval, PreparedToolAttempt,
    PreparedToolBatchDecision, PreparedToolResultProjection, ToolBatch, ToolBatchDecisionError,
    ToolBatchDecisionFailure, ToolBatchExecutionError, ToolBatchExecutionFailure, ToolBatchPhase,
    ToolBatchPhaseReconstitutionInput, ToolBatchReconstitutionError,
    ToolBatchReconstitutionFailure, ToolBatchReconstitutionInput, ToolResultProjectionError,
    ToolResultProjectionFailure,
};
