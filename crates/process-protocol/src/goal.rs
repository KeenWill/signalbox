//! Goal wire representations and validation.

use crate::scalars::{
    CanonicalU64, CanonicalUuid, CommandId, FrameValidationError, MAX_CONTENT_FRAGMENT_BYTES,
    deserialize_required_nullable,
};
use serde::{Deserialize, Serialize};

/// Closed durable goal-command rejection vocabulary.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalCommandRejection {
    /// The target session does not exist.
    SessionNotFound,
    /// The session's closure is pending; the closure settles the goal.
    SessionClosing,
    /// A goal is already pursuing or blocked.
    GoalAlreadyAttached,
    /// The session has no goal lineage.
    GoalNotAttached,
    /// The session's selected model alias is absent from daemon configuration.
    UnknownModelAlias,
    /// The session accepted-input position cannot advance beyond `u64::MAX`.
    AcceptancePositionExhausted,
    /// Resume requires a blocked current generation.
    RequiresBlocked,
    /// Stop or supersede requires a pursuing or blocked generation.
    RequiresPursuingOrBlocked,
    /// No successor generation can be represented.
    GenerationExhausted,
    /// No successor event position can be represented.
    EventOrdinalExhausted,
}

/// Closed blocked-reason vocabulary at the process boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalBlockedReason {
    /// Progress requires information or a decision from the user.
    UserInputRequired,
    /// Progress requires an external state change.
    ExternalChangeRequired,
    /// Progress requires authority the session does not hold.
    AuthorizationRequired,
    /// The preceding goal turn failed and was not retried.
    ExecutionFailure,
    FinishCheckFailed,
}

/// Provenance for one blocked event at the process boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum GoalBlockedProvenance {
    /// The model declared the blocked transition through its correlated tool.
    Model {
        /// Exact invoking turn.
        turn_id: CanonicalUuid,
        /// Exact invoking tool request.
        tool_request_id: CanonicalUuid,
    },
    /// The scheduler observed one unsuccessfully terminalized goal turn.
    ExecutionFailure {
        /// Exact failed turn.
        turn_id: CanonicalUuid,
    },
}

/// One generation's derived lifecycle state at the process boundary.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum GoalLifecycleState {
    /// Autonomous scheduling continues.
    Pursuing {},
    /// Autonomous scheduling pauses pending an explicit user transition.
    Blocked {
        /// Closed blocked reason.
        reason: GoalBlockedReason,
        /// Exact statement of what is needed.
        need: String,
    },
    /// The commissioned work is complete.
    Achieved {
        /// Turn whose work was declared or verified complete.
        turn_id: CanonicalUuid,
        /// Model declaration request; absent for daemon verification.
        #[serde(deserialize_with = "deserialize_required_nullable")]
        tool_request_id: Option<CanonicalUuid>,
    },
    /// The user explicitly ended this generation.
    UserStopped {},
    /// Another immutable statement replaced this generation.
    Superseded {
        /// Successor generation commissioned by the same event.
        by_generation: CanonicalU64,
    },
    /// The session closed beneath this generation.
    SessionClosed {
        /// Closed session outcome that settled it.
        outcome: SessionClosureOutcome,
    },
}

/// The closed session outcomes that settle a live goal generation.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionClosureOutcome {
    /// Closed with a retryable cause standing.
    FailedRetryable,
    /// Closed with a structural cause standing.
    FailedStructural,
    /// Closed with no classified cause.
    FailedUnknown,
    /// A human or rule stopped the session.
    Stopped,
    /// A newer session owns the work, or the work is gone.
    Superseded,
    /// An operator wrote the session off.
    Abandoned,
    /// The session never did the work and never will.
    Retired,
}

/// The closed actor classification recorded with a lifecycle transition.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LifecycleActorClass {
    /// Daemon core.
    Core,
    /// The single user's authority.
    Operator,
    /// A module, without saying which: the classification is what the
    /// boundary carries, and the durable goal event keeps the exact module.
    Module,
    /// The recovery scan or liveness watchdog.
    Watchdog,
}

/// One append-only goal event payload at the process boundary.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum GoalHistoryEvent {
    /// The user commissioned an immutable statement.
    Commissioned {
        /// Exact immutable statement.
        statement: String,
        /// Durable user command provenance.
        command_id: CommandId,
    },
    /// Pursuit paused with a typed reason and exact need.
    Blocked {
        /// Closed reason.
        reason: GoalBlockedReason,
        /// Exact statement of what is needed.
        need: String,
        /// Typed transition provenance.
        provenance: GoalBlockedProvenance,
    },
    /// The user resumed blocked pursuit.
    Resumed {
        /// Optional exact next-turn guidance.
        #[serde(deserialize_with = "deserialize_required_nullable")]
        guidance: Option<String>,
        /// Durable user command provenance.
        command_id: CommandId,
    },
    /// Achievement with its final report.
    Achieved {
        /// Exact final report.
        report: String,
        /// Invoking turn.
        turn_id: CanonicalUuid,
        /// Model declaration request; absent for daemon verification.
        #[serde(deserialize_with = "deserialize_required_nullable")]
        tool_request_id: Option<CanonicalUuid>,
    },
    /// The user explicitly ended the generation.
    UserStopped {
        /// Durable user command provenance.
        command_id: CommandId,
        /// The turn whose physical stop settlement belongs to this closure.
        #[serde(deserialize_with = "deserialize_required_nullable")]
        settling_turn_id: Option<CanonicalUuid>,
        /// Approved actions abandoned at settlement; absent while settling.
        #[serde(deserialize_with = "deserialize_required_nullable")]
        abandoned_actions: Option<CanonicalU64>,
    },
    /// The user atomically replaced the active statement.
    Superseded {
        /// Newly commissioned immutable statement.
        replacement_statement: String,
        /// Durable user command provenance.
        command_id: CommandId,
    },
    /// The session closed, settling this generation.
    SessionClosed {
        /// Closed session outcome that settled it.
        outcome: SessionClosureOutcome,
        /// Classified actor that closed the session.
        actor: LifecycleActorClass,
    },
}

pub(crate) fn validate_goal_text(value: &str) -> Result<(), FrameValidationError> {
    if value.is_empty() || value.len() > MAX_CONTENT_FRAGMENT_BYTES || value.contains('\0') {
        return Err(FrameValidationError::GoalShape);
    }
    Ok(())
}

pub(crate) fn validate_goal_state(state: &GoalLifecycleState) -> Result<(), FrameValidationError> {
    match state {
        GoalLifecycleState::Blocked { need, .. } => validate_goal_text(need),
        GoalLifecycleState::Superseded { by_generation } if by_generation.value() == 0 => {
            Err(FrameValidationError::GoalShape)
        }
        GoalLifecycleState::Pursuing {}
        | GoalLifecycleState::Achieved { .. }
        | GoalLifecycleState::UserStopped {}
        | GoalLifecycleState::Superseded { .. }
        | GoalLifecycleState::SessionClosed { .. } => Ok(()),
    }
}

pub(crate) fn validate_goal_event(event: &GoalHistoryEvent) -> Result<(), FrameValidationError> {
    match event {
        GoalHistoryEvent::Commissioned { statement, .. } => validate_goal_text(statement),
        GoalHistoryEvent::Blocked {
            reason,
            need,
            provenance,
        } => {
            validate_goal_text(need)?;
            let scheduler_reason = match reason {
                GoalBlockedReason::UserInputRequired
                | GoalBlockedReason::ExternalChangeRequired
                | GoalBlockedReason::AuthorizationRequired
                | GoalBlockedReason::FinishCheckFailed => false,
                GoalBlockedReason::ExecutionFailure => true,
            };
            let scheduler_provenance = match provenance {
                GoalBlockedProvenance::Model { .. } => false,
                GoalBlockedProvenance::ExecutionFailure { .. } => true,
            };
            if scheduler_reason != scheduler_provenance {
                return Err(FrameValidationError::GoalShape);
            }
            Ok(())
        }
        GoalHistoryEvent::Resumed {
            guidance: Some(guidance),
            ..
        } => validate_goal_text(guidance),
        GoalHistoryEvent::Achieved { report, .. } => validate_goal_text(report),
        GoalHistoryEvent::Superseded {
            replacement_statement,
            ..
        } => validate_goal_text(replacement_statement),
        GoalHistoryEvent::Resumed { guidance: None, .. }
        | GoalHistoryEvent::UserStopped { .. }
        | GoalHistoryEvent::SessionClosed { .. } => Ok(()),
    }
}

/// Explicit delegated-child scope selected by a parent termination request.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DescendantTerminationScope {
    /// Apply the stop only to the named parent session.
    ParentAlone,
    /// Evaluate every reachable delegated-child relationship.
    ParentAndDescendants,
}

/// Immutable authority fence a commissioned-session request records.
///
/// The shapes mirror the repository-watch dispatch fence: a pull-request fence
/// names the pull request, its exact head commit, the repository and branch
/// holding that head, and the base branch; a branch fence names the repository
/// and branch alone. Field admission (slug, commit, and branch grammar) is the
/// daemon's, at command construction.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "target", rename_all = "snake_case", deny_unknown_fields)]
pub enum CommissionedSessionFence {
    /// Exact pull-request authority for the commissioned session.
    PullRequest {
        /// Repository whose pull request the session is commissioned against.
        repository: String,
        /// Positive pull-request number within the repository.
        pull_request: CanonicalU64,
        /// Exact head commit authorized at commissioning time.
        head_sha: String,
        /// Repository containing the authorized head branch.
        head_repository: String,
        /// Authorized head branch.
        head_branch: String,
        /// Authorized base branch.
        base_branch: String,
    },
    /// Exact branch authority for the commissioned session.
    Branch {
        /// Repository whose branch the session is commissioned against.
        repository: String,
        /// Authorized branch.
        branch: String,
    },
}

/// Whether a creation holds its start gate.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StartGate {
    /// The session may dispatch as soon as it has input.
    #[default]
    Open,
    /// The session stays `created` until `release_start` or gate expiry.
    Held,
}

/// Whether the daemon holds a liveness obligation for the session.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionOwnership {
    /// The daemon drives the session to a declared terminal outcome.
    Owned,
    /// A conversation the daemon does not drive.
    #[default]
    Unmonitored,
}

/// Closed finish condition an owned session owes.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum FinishCondition {
    /// Completion is declared outside the session.
    ExternalGate,
    /// Completion is checked against exact declared text.
    Declared {
        /// Exact statement the finish check evaluates.
        statement: String,
    },
}

/// The lifecycle members of a creation: omission means an open gate, an
/// unmonitored conversation, and no finish condition.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionLifecycleMembers {
    /// Whether the creation holds its start gate.
    #[serde(default)]
    pub start_gate: StartGate,
    /// The ownership the creation establishes.
    #[serde(default)]
    pub ownership: SessionOwnership,
    /// The finish condition an owned session owes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finish_condition: Option<FinishCondition>,
}

impl SessionLifecycleMembers {
    /// Whether every member holds its omission value.
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

/// The standing failure cause a parked session closes with.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionFailureCause {
    ProviderTransient,
    ProviderQuotaExhausted,
    ProviderOverloaded,
    InfrastructureFailure,
    RetryBudgetExhausted,
    ContextCompactionWall,
    ContextHeadroomExhausted,
    BrokenToolchain,
    ModerationBlock,
}

/// Closed session-lifecycle command rejection vocabulary.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionLifecycleCommandRejection {
    SessionNotFound,
    TransitionNotAdmitted,
    RequiresParked,
    ReleaseWhileParked,
    OwnershipUnchanged,
    FinishConditionAlreadyDeclared,
    StandingCauseMismatch,
    SuccessorNotFound,
    SuccessorIsSelf,
    GoalResumeRequired,
    GoalOutcomeMismatch,
    PendingTerminalConflict,
}

/// What an applied lifecycle command did.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum SessionLifecycleEffect {
    /// The held start gate opened.
    StartReleased {},
    /// The session recorded terminal.
    Closed {},
    /// The outcome is committed; the named live turn settles first.
    ClosurePending {
        /// The turn the committed interrupt machinery settles.
        live_turn_id: CanonicalUuid,
    },
    /// The park lifted.
    Resumed {},
    /// The ownership bit flipped.
    OwnershipChanged {},
}
