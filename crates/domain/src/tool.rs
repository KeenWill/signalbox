//! Logical tool requests, approval provenance, and result values.
//!
//! `docs/spec/tool-loop.md` is normative. This module owns bounded,
//! provider-neutral request content and the approval algebra. Physical
//! execution lives in `tool_attempt`; persistence, registry lookup, and
//! executor selection remain outside the domain boundary.

mod approval;
mod arguments;
mod decide;
mod name;
mod override_denial;
mod policy;
mod proposal;
mod request;
mod result;

#[cfg(test)]
mod fixtures;
#[cfg(test)]
mod tests;

pub use approval::{
    InitialToolApproval, ToolApprovalDecision, ToolApprovalResolution,
    ToolApprovalResolutionReconstitutionError, ToolApprovalResolutionReconstitutionInput,
};
pub use arguments::{
    NormalizedToolArguments, ToolArgumentsError, ToolArgumentsFailure, ToolArgumentsKind,
};
pub use decide::{
    DecideToolRequest, DecideToolRequestAppliedResult, DecideToolRequestConstructionError,
    DecideToolRequestPreparationError, DecideToolRequestRejectedResult, DecideToolRequestResult,
    PreparedDecideToolRequest,
};
pub use name::{ToolName, ToolNameError, ToolNameFailure};
pub use override_denial::{
    OverrideDeniedToolRequest, OverrideDeniedToolRequestAppliedResult,
    OverrideDeniedToolRequestConstructionError, OverrideDeniedToolRequestPreparationError,
    OverrideDeniedToolRequestRejectedResult, OverrideDeniedToolRequestResult,
    PreparedOverrideDeniedToolRequest, RecordedUserOverride,
};
pub use policy::{
    DangerousToolAutoApproval, DelegateApprovalRecommendation, DelegateToolApproval,
    DelegateToolApprovalError, ToolApprovalDecider, ToolApprovalPosture, ToolDecisionRationale,
    ToolDecisionRationaleError, ToolDecisionSource, ToolDenialReason, ToolDenialReasonError,
    ToolDenialReasonFailure, ToolEffectClass, ToolPermissionDefault,
};
pub use proposal::{
    AssistantResponsePart, ToolCallProposal, ToolRequestOrdinal, ToolUsingAssistantResponse,
    ToolUsingAssistantResponseError,
};
pub use request::{ToolRequest, ToolRequestReconstitutionInput};
pub use result::{
    ToolRequestResolution, ToolResultContent, ToolResultText, ToolResultTextError,
    ToolResultTextFailure,
};

pub(crate) use proposal::MAX_TOOL_REQUESTS_PER_RESPONSE;
