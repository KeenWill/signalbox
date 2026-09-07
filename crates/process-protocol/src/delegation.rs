//! Delegation wire representations and validation.

use crate::goal::DescendantTerminationScope;
use crate::scalars::{
    CanonicalU64, CanonicalUuid, MAX_CONTENT_FRAGMENT_BYTES, deserialize_required_nullable,
};
use serde::{Deserialize, Serialize};

/// Action chosen for one bound child when its parent reaches a terminal state.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BoundChildAction {
    /// Leave the child running.
    KeepRunning,
    /// Stop the child with typed parent-policy provenance.
    Stop,
    /// Cancel the child with typed parent-policy provenance.
    Cancel,
}

/// Parent-chosen lifecycle policy carried by a child-spawned update.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum DelegationPolicy {
    /// The child keeps working independently of parent state.
    Background {},
    /// The child follows the two explicit parent-state actions.
    Bound {
        /// Action when the parent stops.
        on_parent_stopped: BoundChildAction,
        /// Action when the parent is cancelled.
        on_parent_cancelled: BoundChildAction,
    },
}

/// Delivery behavior chosen by one await request.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DelegationWaitMode {
    /// Keep the current parent turn open until delivery.
    Foreground,
    /// Return registration and deliver through a later wake.
    Background,
}

/// Direction of one message within its parent-child relationship.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DelegationMessageDirection {
    /// The relationship parent sent to its child.
    ParentToChild,
    /// The relationship child sent to its parent.
    ChildToParent,
}

/// Durable non-executable state of one delegation tool request.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DelegationToolRequestState {
    /// The request still requires an approval decision.
    AwaitingApproval,
    /// Approval was denied.
    Denied,
    /// Approval succeeded, but proposal-ordered execution has not prepared an attempt.
    Approved,
    /// A physical attempt exists but has not been authorized for execution.
    Prepared,
    /// The logical request already closed without executable work.
    Closed,
    /// Its current physical attempt already ended.
    AttemptEnded,
}

impl DelegationToolRequestState {
    /// Returns the stable wire spelling used by diagnostics.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AwaitingApproval => "awaiting_approval",
            Self::Denied => "denied",
            Self::Approved => "approved",
            Self::Prepared => "prepared",
            Self::Closed => "closed",
            Self::AttemptEnded => "attempt_ended",
        }
    }
}

/// Closed relationship outcome carried by delegation updates.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DelegationOutcome {
    /// Child content is available.
    Returned,
    /// Child execution failed or returned unusable content.
    Failed,
    /// Parent policy stopped the child.
    Stopped,
    /// Child or parent policy cancelled the child.
    Cancelled,
    /// Relationship policy left the child running.
    ContinueRunning,
    /// Parent policy reached an already-terminal child.
    AlreadyTerminal,
}

/// Exact reason carried alongside a delegation outcome.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DelegationReason {
    /// Child completed with delivered content.
    ChildCompleted,
    /// Child execution failed.
    ChildExecutionFailed,
    /// Completed child content could not form a result.
    ChildResultUnavailable,
    /// Child cancelled independently.
    ChildCancelled,
    /// A parent stop selected descendants.
    ParentStopped,
    /// A parent cancellation selected descendants.
    ParentCancelled,
}

/// Proof source retained by one lifecycle or result update.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum DelegationProvenance {
    /// Exact terminal child turn.
    ChildTurn {
        /// Child session.
        child_session_id: CanonicalUuid,
        /// Terminal delegated turn.
        child_turn_id: CanonicalUuid,
    },
    /// Exact parent turn command.
    ParentTurnCommand {
        /// Parent session.
        parent_session_id: CanonicalUuid,
        /// Parent turn named by the command.
        parent_turn_id: CanonicalUuid,
        /// Durable stop or interrupt command.
        command_id: CanonicalUuid,
        /// Explicit kill-time descendant choice.
        descendant_scope: DescendantTerminationScope,
    },
    /// Exact parent goal-generation command.
    ParentGoalCommand {
        /// Parent session.
        parent_session_id: CanonicalUuid,
        /// One-based goal generation.
        goal_generation: CanonicalU64,
        /// Durable goal stop command.
        command_id: CanonicalUuid,
        /// Explicit kill-time descendant choice.
        descendant_scope: DescendantTerminationScope,
    },
    /// Exact parent lifecycle command.
    ParentLifecycleCommand {
        /// Parent session.
        parent_session_id: CanonicalUuid,
        /// Durable lifecycle stop command.
        command_id: CanonicalUuid,
        /// Explicit kill-time descendant choice.
        descendant_scope: DescendantTerminationScope,
    },
}

/// Exact decision recorded for one explicit tool approval.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ToolApprovalEventDecision {
    /// Execution is permitted subject to current aggregate guards.
    Approve {},
    /// Execution is permanently prohibited for this request.
    Deny {
        /// Exact user explanation, absent for a delegate denial.
        #[serde(deserialize_with = "deserialize_required_nullable")]
        reason: Option<String>,
    },
}

/// Exact actor provenance for one explicit tool approval decision.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ToolApprovalEventDecider {
    /// The user acted through the named durable command.
    User {
        /// Exact durable command provenance.
        command_id: CanonicalUuid,
    },
    /// A configured model acted through the named dedicated judge call.
    Delegate {
        /// Exact direct model selection used by the judge.
        model_selection_id: CanonicalUuid,
        /// Exact recorded judge model call.
        model_call_id: CanonicalUuid,
    },
    /// The user pre-approved the re-proposed command by overriding one exact
    /// delegate denial through the named durable command.
    UserOverride {
        /// Exact durable override-command provenance.
        command_id: CanonicalUuid,
        /// The delegate-denied request whose recorded override was consumed.
        overridden_tool_request_id: CanonicalUuid,
    },
}

/// One explicit approval decision retained in an authoritative transcript.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TranscriptToolApproval {
    /// Exact recorded decision.
    pub decision: ToolApprovalEventDecision,
    /// Exact user or delegate provenance.
    pub decider: ToolApprovalEventDecider,
    /// Exact judge rationale, absent for a user decision.
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub rationale: Option<String>,
}

pub(crate) fn child_result_shape_is_valid(
    parent_session_id: CanonicalUuid,
    child_session_id: CanonicalUuid,
    outcome: DelegationOutcome,
    content: &Option<String>,
    reason: DelegationReason,
    provenance: &DelegationProvenance,
) -> bool {
    match (outcome, reason, provenance, content) {
        (
            DelegationOutcome::Returned,
            DelegationReason::ChildCompleted,
            DelegationProvenance::ChildTurn {
                child_session_id: provenance_child,
                ..
            },
            Some(content),
        ) => *provenance_child == child_session_id && delegation_content_is_valid(content),
        (
            DelegationOutcome::Failed,
            DelegationReason::ChildExecutionFailed | DelegationReason::ChildResultUnavailable,
            DelegationProvenance::ChildTurn {
                child_session_id: provenance_child,
                ..
            },
            None,
        )
        | (
            DelegationOutcome::Cancelled,
            DelegationReason::ChildCancelled,
            DelegationProvenance::ChildTurn {
                child_session_id: provenance_child,
                ..
            },
            None,
        ) => *provenance_child == child_session_id,
        (
            DelegationOutcome::Stopped | DelegationOutcome::Cancelled,
            DelegationReason::ParentStopped | DelegationReason::ParentCancelled,
            provenance,
            None,
        ) => parent_delegation_provenance_is_cascade(parent_session_id, provenance),
        _ => false,
    }
}

pub(crate) fn direct_child_result_shape_is_valid(
    child_session_id: CanonicalUuid,
    outcome: DelegationOutcome,
    content: &Option<String>,
    reason: DelegationReason,
    provenance: &DelegationProvenance,
) -> bool {
    match provenance {
        DelegationProvenance::ChildTurn { .. } => child_result_shape_is_valid(
            child_session_id,
            child_session_id,
            outcome,
            content,
            reason,
            provenance,
        ),
        DelegationProvenance::ParentTurnCommand {
            parent_session_id, ..
        }
        | DelegationProvenance::ParentGoalCommand {
            parent_session_id, ..
        }
        | DelegationProvenance::ParentLifecycleCommand {
            parent_session_id, ..
        } => {
            *parent_session_id != child_session_id
                && child_result_shape_is_valid(
                    *parent_session_id,
                    child_session_id,
                    outcome,
                    content,
                    reason,
                    provenance,
                )
        }
    }
}

pub(crate) fn parent_delegation_provenance_is_cascade(
    parent_session_id: CanonicalUuid,
    provenance: &DelegationProvenance,
) -> bool {
    match provenance {
        DelegationProvenance::ParentTurnCommand {
            parent_session_id: provenance_parent,
            descendant_scope: DescendantTerminationScope::ParentAndDescendants,
            ..
        } => {
            *provenance_parent == parent_session_id
                && parent_delegation_provenance_has_cascade(provenance)
        }
        DelegationProvenance::ParentGoalCommand {
            parent_session_id: provenance_parent,
            goal_generation,
            descendant_scope: DescendantTerminationScope::ParentAndDescendants,
            ..
        } => {
            *provenance_parent == parent_session_id
                && goal_generation.value() > 0
                && parent_delegation_provenance_has_cascade(provenance)
        }
        DelegationProvenance::ParentLifecycleCommand {
            parent_session_id: provenance_parent,
            descendant_scope: DescendantTerminationScope::ParentAndDescendants,
            ..
        } => {
            *provenance_parent == parent_session_id
                && parent_delegation_provenance_has_cascade(provenance)
        }
        _ => false,
    }
}

/// Reads the commanding parent session out of a cascade provenance.
pub(crate) fn delegation_provenance_parent(
    provenance: &DelegationProvenance,
) -> Option<CanonicalUuid> {
    match provenance {
        DelegationProvenance::ParentTurnCommand {
            parent_session_id, ..
        }
        | DelegationProvenance::ParentGoalCommand {
            parent_session_id, ..
        }
        | DelegationProvenance::ParentLifecycleCommand {
            parent_session_id, ..
        } => Some(*parent_session_id),
        _ => None,
    }
}

pub(crate) fn parent_delegation_provenance_has_cascade(provenance: &DelegationProvenance) -> bool {
    match provenance {
        DelegationProvenance::ParentTurnCommand {
            descendant_scope: DescendantTerminationScope::ParentAndDescendants,
            ..
        } => true,
        DelegationProvenance::ParentGoalCommand {
            descendant_scope: DescendantTerminationScope::ParentAndDescendants,
            goal_generation,
            ..
        } => goal_generation.value() > 0,
        DelegationProvenance::ParentLifecycleCommand {
            descendant_scope: DescendantTerminationScope::ParentAndDescendants,
            ..
        } => true,
        _ => false,
    }
}

/// Admits every terminal outcome a parent cascade can impose on a child.
///
/// A bound relationship carries its own termination policy, so the child
/// outcome is not required to match the parent reason: a parent cancellation
/// may map to a child `stop`, and a parent stop may map to a child `cancel`.
/// All four crossed pairs are therefore valid, exactly as `process_read`
/// projects them.
pub(crate) fn delegation_terminal_outcome_reason_is_admissible(
    outcome: DelegationOutcome,
    reason: DelegationReason,
) -> bool {
    matches!(
        outcome,
        DelegationOutcome::Stopped | DelegationOutcome::Cancelled
    ) && matches!(
        reason,
        DelegationReason::ParentStopped | DelegationReason::ParentCancelled
    )
}

pub(crate) fn delegation_content_is_valid(content: &str) -> bool {
    !content.is_empty() && content.len() <= MAX_CONTENT_FRAGMENT_BYTES && !content.contains('\0')
}
