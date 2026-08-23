//! Explicit mappings between domain values and PostgreSQL-compatible values.

use std::{error::Error, fmt};

use crate::repo_watch_webhook::RepoWatchWebhookDisposition;
use rust_decimal::Decimal;
use serde::{Deserialize, Deserializer};
use serde_json::{Value, json};
use signalbox_application::{RepoWatchPullRequestLifecycle, RepoWatchThreadState};
use signalbox_domain::{
    AcceptedInputId, AnthropicServiceTier, BoundChildAction, CheckConclusion, ChecksOutcome,
    CodexCliServiceTier, DangerousToolAutoApproval, DelegateApprovalRecommendation,
    DelegationMessageDirection, DelegationOutcomeKind, DelegationOutcomeReason,
    DelegationTransitionFailure, DelegationWaitMode, DeliveryKind, DescendantTerminationScope,
    DirectModelSelection, DurableCommandId, EffectiveModelSettings, FastMode, FastModeOverlay,
    FaultCause, GoalBlockedReasonKind, GoalCommandRejection, GoalEventKind,
    GoalModelBlockedReasonKind, GoalUserAction, MergeableState, ModelChangeAdjustment,
    ModelSettingSource, ModelSettingsOverlay, ModelSettingsPrecedence, OpenAiServiceTier,
    ProgramCapability, ReactionChange, ReactionSubject, ReasoningLevel, RejectReason,
    RepoWatchEventKindNameV1, RequestKind, ReviewState, RunnerPlacementLossSource,
    RunnerSandboxProfile, ScopeOperation, ServiceTier, SessionConfigurationDefaultsVersion,
    SessionCreationCause, SessionId, SessionInputPosition, SessionPlacementEventKind,
    SettingOverlay, ToolApprovalPosture, ToolAttemptId, ToolPermissionDefault, ToolRequestId,
    TurnId, UpdateSessionPlacementRejectionKind, ValidatedModelSettings, WorkspaceOrigin,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ProgramRequestStorageKind {
    Now,
    Random,
    Sleep,
    AwaitEvent,
    Effect,
    Scope,
    Terminal,
}

pub(crate) fn program_request_kind_from_str(value: &str) -> Option<ProgramRequestStorageKind> {
    match value {
        "now" => Some(ProgramRequestStorageKind::Now),
        "random" => Some(ProgramRequestStorageKind::Random),
        "sleep" => Some(ProgramRequestStorageKind::Sleep),
        "await_event" => Some(ProgramRequestStorageKind::AwaitEvent),
        "effect" => Some(ProgramRequestStorageKind::Effect),
        "scope" => Some(ProgramRequestStorageKind::Scope),
        "terminal" => Some(ProgramRequestStorageKind::Terminal),
        _ => None,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ProgramDeliveryStorageKind {
    Answer,
    Wake,
    Reject,
    Cancel,
    RunCancel,
    Fault,
}

pub(crate) const fn program_delivery_kind_to_str(value: &DeliveryKind) -> &'static str {
    match value {
        DeliveryKind::Answer { .. } => "answer",
        DeliveryKind::Wake { .. } => "wake",
        DeliveryKind::Reject { .. } => "reject",
        DeliveryKind::Cancel { .. } => "cancel",
        DeliveryKind::RunCancel(_) => "run_cancel",
        DeliveryKind::Fault(_) => "fault",
    }
}

pub(crate) fn program_delivery_kind_from_str(value: &str) -> Option<ProgramDeliveryStorageKind> {
    match value {
        "answer" => Some(ProgramDeliveryStorageKind::Answer),
        "wake" => Some(ProgramDeliveryStorageKind::Wake),
        "reject" => Some(ProgramDeliveryStorageKind::Reject),
        "cancel" => Some(ProgramDeliveryStorageKind::Cancel),
        "run_cancel" => Some(ProgramDeliveryStorageKind::RunCancel),
        "fault" => Some(ProgramDeliveryStorageKind::Fault),
        _ => None,
    }
}

pub(crate) const fn program_fault_cause_to_str(value: FaultCause) -> &'static str {
    match value {
        FaultCause::Timeout => "timeout",
        FaultCause::Memory => "memory",
        FaultCause::Nondeterminism => "nondeterminism",
        FaultCause::ProgramError => "program_error",
        FaultCause::ContractRetired => "contract_retired",
        FaultCause::JournalBound => "journal_bound",
        FaultCause::PayloadTooLarge => "payload_too_large",
    }
}

pub(crate) fn program_fault_cause_from_str(value: &str) -> Option<FaultCause> {
    match value {
        "timeout" => Some(FaultCause::Timeout),
        "memory" => Some(FaultCause::Memory),
        "nondeterminism" => Some(FaultCause::Nondeterminism),
        "program_error" => Some(FaultCause::ProgramError),
        "contract_retired" => Some(FaultCause::ContractRetired),
        "journal_bound" => Some(FaultCause::JournalBound),
        "payload_too_large" => Some(FaultCause::PayloadTooLarge),
        _ => None,
    }
}

pub(crate) const fn program_request_kind_to_str(value: &RequestKind) -> &'static str {
    match value {
        RequestKind::Now(_) => "now",
        RequestKind::Random(_) => "random",
        RequestKind::Sleep(_) => "sleep",
        RequestKind::AwaitEvent(_) => "await_event",
        RequestKind::Effect(_) => "effect",
        RequestKind::Scope(_) => "scope",
        RequestKind::Terminal(_) => "terminal",
    }
}

pub(crate) const fn program_capability_to_str(value: ProgramCapability) -> &'static str {
    match value {
        ProgramCapability::Time => "time",
        ProgramCapability::Random => "random",
        ProgramCapability::Sleep => "sleep",
        ProgramCapability::Subscribe => "subscribe",
        ProgramCapability::Session => "session",
        ProgramCapability::Judge => "judge",
        ProgramCapability::ExecStage => "exec-stage",
        ProgramCapability::Corpus => "corpus",
        ProgramCapability::EvalRecord => "eval-record",
        ProgramCapability::Blob => "blob",
        ProgramCapability::Register => "register",
    }
}

pub(crate) fn program_capability_from_str(value: &str) -> Option<ProgramCapability> {
    match value {
        "time" => Some(ProgramCapability::Time),
        "random" => Some(ProgramCapability::Random),
        "sleep" => Some(ProgramCapability::Sleep),
        "subscribe" => Some(ProgramCapability::Subscribe),
        "session" => Some(ProgramCapability::Session),
        "judge" => Some(ProgramCapability::Judge),
        "exec-stage" => Some(ProgramCapability::ExecStage),
        "corpus" => Some(ProgramCapability::Corpus),
        "eval-record" => Some(ProgramCapability::EvalRecord),
        "blob" => Some(ProgramCapability::Blob),
        "register" => Some(ProgramCapability::Register),
        _ => None,
    }
}

pub(crate) const fn program_scope_operation_to_str(value: ScopeOperation) -> &'static str {
    match value {
        ScopeOperation::Open => "open",
        ScopeOperation::Close => "close",
    }
}

pub(crate) fn program_scope_operation_from_str(value: &str) -> Option<ScopeOperation> {
    match value {
        "open" => Some(ScopeOperation::Open),
        "close" => Some(ScopeOperation::Close),
        _ => None,
    }
}

pub(crate) const fn program_reject_reason_to_str(value: RejectReason) -> &'static str {
    match value {
        RejectReason::OutstandingRequests => "outstanding_requests",
    }
}

pub(crate) fn program_reject_reason_from_str(value: &str) -> Option<RejectReason> {
    match value {
        "outstanding_requests" => Some(RejectReason::OutstandingRequests),
        _ => None,
    }
}
use signalbox_tools_plan::PlanStatus;
use sqlx::types::Uuid;

use crate::{approval_judge::FailedApprovalJudgeDisposition, outbox::DispatchedRunnerState};

/// Closed stored states for one durable runner-loss propagation cursor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RunnerLossPropagationStateStorageKind {
    Pending,
    Completed,
}

pub(crate) const fn runner_loss_propagation_state_to_str(
    value: RunnerLossPropagationStateStorageKind,
) -> &'static str {
    match value {
        RunnerLossPropagationStateStorageKind::Pending => "pending",
        RunnerLossPropagationStateStorageKind::Completed => "completed",
    }
}

pub(crate) fn runner_loss_propagation_state_from_str(
    value: &str,
) -> Option<RunnerLossPropagationStateStorageKind> {
    match value {
        "pending" => Some(RunnerLossPropagationStateStorageKind::Pending),
        "completed" => Some(RunnerLossPropagationStateStorageKind::Completed),
        _ => None,
    }
}

/// Closed delegated-session relationship policy discriminators in PostgreSQL.
///
/// Public because the outbox decode tripwire drives these spellings with the
/// exact set the durable `CHECK` constraint admits.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DelegationPolicyStorageKind {
    /// The child outlives parent state changes.
    Background,
    /// The child follows the two explicit parent-state actions.
    Bound,
}

#[allow(
    dead_code,
    reason = "delegation relationship inserts are owned by the deferred placement writer"
)]
pub const fn delegation_policy_kind_to_str(value: DelegationPolicyStorageKind) -> &'static str {
    match value {
        DelegationPolicyStorageKind::Background => "background",
        DelegationPolicyStorageKind::Bound => "bound",
    }
}

/// Decodes one durable `policy_kind` spelling.
pub fn delegation_policy_kind_from_str(value: &str) -> Option<DelegationPolicyStorageKind> {
    match value {
        "background" => Some(DelegationPolicyStorageKind::Background),
        "bound" => Some(DelegationPolicyStorageKind::Bound),
        _ => None,
    }
}

/// Closed terminal dispositions stored for a tool attempt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ToolAttemptDispositionStorageKind {
    Completed,
    KnownFailed,
    AwaitingChild,
    Ambiguous,
}

pub(crate) const fn tool_attempt_disposition_to_str(
    value: ToolAttemptDispositionStorageKind,
) -> &'static str {
    match value {
        ToolAttemptDispositionStorageKind::Completed => "completed",
        ToolAttemptDispositionStorageKind::KnownFailed => "known_failed",
        ToolAttemptDispositionStorageKind::AwaitingChild => "awaiting_child",
        ToolAttemptDispositionStorageKind::Ambiguous => "ambiguous",
    }
}

pub(crate) fn tool_attempt_disposition_from_str(
    value: &str,
) -> Option<ToolAttemptDispositionStorageKind> {
    match value {
        "completed" => Some(ToolAttemptDispositionStorageKind::Completed),
        "known_failed" => Some(ToolAttemptDispositionStorageKind::KnownFailed),
        "awaiting_child" => Some(ToolAttemptDispositionStorageKind::AwaitingChild),
        "ambiguous" => Some(ToolAttemptDispositionStorageKind::Ambiguous),
        _ => None,
    }
}

/// Closed delegated-session update discriminators in PostgreSQL.
///
/// Public because the outbox decode tripwire drives these spellings with the
/// exact set the durable `CHECK` constraint admits.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DelegationUpdateStorageKind {
    /// A parent committed one child relationship and its spawn policy.
    ChildSpawned,
    /// A parent registered one foreground or background wait.
    ChildWaiting,
    /// Parent termination evaluated one relationship edge.
    ChildLifecycleDisposition,
    /// A terminal child result became durable for its parent.
    ChildResult,
    /// One relationship message became durable for its recipient.
    SessionMessage,
}

pub const fn delegation_update_kind_to_str(value: DelegationUpdateStorageKind) -> &'static str {
    match value {
        DelegationUpdateStorageKind::ChildSpawned => "child_spawned",
        DelegationUpdateStorageKind::ChildWaiting => "child_waiting",
        DelegationUpdateStorageKind::ChildLifecycleDisposition => "child_lifecycle_disposition",
        DelegationUpdateStorageKind::ChildResult => "child_result",
        DelegationUpdateStorageKind::SessionMessage => "session_message",
    }
}

/// Decodes one durable `update_kind` spelling.
pub fn delegation_update_kind_from_str(value: &str) -> Option<DelegationUpdateStorageKind> {
    match value {
        "child_spawned" => Some(DelegationUpdateStorageKind::ChildSpawned),
        "child_waiting" => Some(DelegationUpdateStorageKind::ChildWaiting),
        "child_lifecycle_disposition" => {
            Some(DelegationUpdateStorageKind::ChildLifecycleDisposition)
        }
        "child_result" => Some(DelegationUpdateStorageKind::ChildResult),
        "session_message" => Some(DelegationUpdateStorageKind::SessionMessage),
        _ => None,
    }
}

/// Closed delegation wake subject discriminators in PostgreSQL.
///
/// Public for the same reason as [`DelegationUpdateStorageKind`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DelegationWakeStorageKind {
    /// A child result is available to its parent.
    Result,
    /// One message is available to the event's recipient session.
    Message,
}

pub const fn delegation_wake_subject_to_str(value: DelegationWakeStorageKind) -> &'static str {
    match value {
        DelegationWakeStorageKind::Result => "result",
        DelegationWakeStorageKind::Message => "message",
    }
}

/// Decodes one durable `subject_kind` spelling.
pub fn delegation_wake_subject_from_str(value: &str) -> Option<DelegationWakeStorageKind> {
    match value {
        "result" => Some(DelegationWakeStorageKind::Result),
        "message" => Some(DelegationWakeStorageKind::Message),
        _ => None,
    }
}

pub(crate) const fn repo_watch_webhook_disposition_to_str(
    value: RepoWatchWebhookDisposition,
) -> &'static str {
    match value {
        RepoWatchWebhookDisposition::Projected => "projected",
        RepoWatchWebhookDisposition::DuplicateState => "duplicate_state",
        RepoWatchWebhookDisposition::Superseded => "superseded",
        RepoWatchWebhookDisposition::Ignored => "ignored",
        RepoWatchWebhookDisposition::Quarantined => "quarantined",
    }
}

/// Paired with the encoder above so a renamed or added disposition cannot
/// update the writer while leaving a reader interpreting the old spelling.
#[cfg(feature = "test-support")]
pub(crate) fn repo_watch_webhook_disposition_from_str(
    value: &str,
) -> Option<RepoWatchWebhookDisposition> {
    match value {
        "projected" => Some(RepoWatchWebhookDisposition::Projected),
        "duplicate_state" => Some(RepoWatchWebhookDisposition::DuplicateState),
        "superseded" => Some(RepoWatchWebhookDisposition::Superseded),
        "ignored" => Some(RepoWatchWebhookDisposition::Ignored),
        "quarantined" => Some(RepoWatchWebhookDisposition::Quarantined),
        _ => None,
    }
}

#[allow(
    dead_code,
    reason = "delegation relationship inserts are owned by the deferred placement writer"
)]
pub(crate) const fn bound_child_action_to_str(value: BoundChildAction) -> &'static str {
    match value {
        BoundChildAction::KeepRunning => "keep_running",
        BoundChildAction::Stop => "stop",
        BoundChildAction::Cancel => "cancel",
    }
}

pub(crate) fn bound_child_action_from_str(value: &str) -> Option<BoundChildAction> {
    match value {
        "keep_running" => Some(BoundChildAction::KeepRunning),
        "stop" => Some(BoundChildAction::Stop),
        "cancel" => Some(BoundChildAction::Cancel),
        _ => None,
    }
}

pub(crate) const fn delegation_wait_mode_to_str(value: DelegationWaitMode) -> &'static str {
    match value {
        DelegationWaitMode::Foreground => "foreground",
        DelegationWaitMode::Background => "background",
    }
}

pub(crate) fn delegation_wait_mode_from_str(value: &str) -> Option<DelegationWaitMode> {
    match value {
        "foreground" => Some(DelegationWaitMode::Foreground),
        "background" => Some(DelegationWaitMode::Background),
        _ => None,
    }
}

pub(crate) const fn delegation_message_direction_to_str(
    value: DelegationMessageDirection,
) -> &'static str {
    match value {
        DelegationMessageDirection::ParentToChild => "parent_to_child",
        DelegationMessageDirection::ChildToParent => "child_to_parent",
    }
}

pub(crate) fn delegation_message_direction_from_str(
    value: &str,
) -> Option<DelegationMessageDirection> {
    match value {
        "parent_to_child" => Some(DelegationMessageDirection::ParentToChild),
        "child_to_parent" => Some(DelegationMessageDirection::ChildToParent),
        _ => None,
    }
}

/// Closed delegated-operation rejection discriminators stored by PostgreSQL.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DelegationRejectionStorageKind {
    RelationshipNotFound,
    MessageIdentityCollision,
    DeliverySequenceExhausted,
    Transition,
}

pub(crate) const fn delegation_rejection_kind_to_str(
    value: DelegationRejectionStorageKind,
) -> &'static str {
    match value {
        DelegationRejectionStorageKind::RelationshipNotFound => "relationship_not_found",
        DelegationRejectionStorageKind::MessageIdentityCollision => "message_identity_collision",
        DelegationRejectionStorageKind::DeliverySequenceExhausted => "delivery_sequence_exhausted",
        DelegationRejectionStorageKind::Transition => "transition",
    }
}

pub(crate) fn delegation_rejection_kind_from_str(
    value: &str,
) -> Option<DelegationRejectionStorageKind> {
    match value {
        "relationship_not_found" => Some(DelegationRejectionStorageKind::RelationshipNotFound),
        "message_identity_collision" => {
            Some(DelegationRejectionStorageKind::MessageIdentityCollision)
        }
        "delivery_sequence_exhausted" => {
            Some(DelegationRejectionStorageKind::DeliverySequenceExhausted)
        }
        "transition" => Some(DelegationRejectionStorageKind::Transition),
        _ => None,
    }
}

pub(crate) const fn delegation_transition_failure_to_str(
    failure: DelegationTransitionFailure,
) -> &'static str {
    match failure {
        DelegationTransitionFailure::SameSession => "same_session",
        DelegationTransitionFailure::AlreadyTerminal => "already_terminal",
        DelegationTransitionFailure::MissingSpawnEvent => "missing_spawn_event",
        DelegationTransitionFailure::InvalidProvenance => "invalid_provenance",
        DelegationTransitionFailure::DescendantsNotSelected => "descendants_not_selected",
        DelegationTransitionFailure::DuplicateMessageIdentity => "duplicate_message_identity",
        DelegationTransitionFailure::ConflictingMessageReplay => "conflicting_message_replay",
        DelegationTransitionFailure::DuplicateOutcomeAuthority => "duplicate_outcome_authority",
        DelegationTransitionFailure::OutcomeReasonMismatch => "outcome_reason_mismatch",
        DelegationTransitionFailure::EventOrdinalExhausted => "event_ordinal_exhausted",
    }
}

pub(crate) fn delegation_transition_failure_from_str(
    value: &str,
) -> Option<DelegationTransitionFailure> {
    match value {
        "same_session" => Some(DelegationTransitionFailure::SameSession),
        "already_terminal" => Some(DelegationTransitionFailure::AlreadyTerminal),
        "missing_spawn_event" => Some(DelegationTransitionFailure::MissingSpawnEvent),
        "invalid_provenance" => Some(DelegationTransitionFailure::InvalidProvenance),
        "descendants_not_selected" => Some(DelegationTransitionFailure::DescendantsNotSelected),
        "duplicate_message_identity" => Some(DelegationTransitionFailure::DuplicateMessageIdentity),
        "conflicting_message_replay" => Some(DelegationTransitionFailure::ConflictingMessageReplay),
        "duplicate_outcome_authority" => {
            Some(DelegationTransitionFailure::DuplicateOutcomeAuthority)
        }
        "outcome_reason_mismatch" => Some(DelegationTransitionFailure::OutcomeReasonMismatch),
        "event_ordinal_exhausted" => Some(DelegationTransitionFailure::EventOrdinalExhausted),
        _ => None,
    }
}

pub(crate) const fn delegation_outcome_kind_to_str(value: DelegationOutcomeKind) -> &'static str {
    match value {
        DelegationOutcomeKind::ResultReturned => "result_returned",
        DelegationOutcomeKind::ChildFailed => "child_failed",
        DelegationOutcomeKind::ChildStopped => "child_stopped",
        DelegationOutcomeKind::ChildCancelled => "child_cancelled",
        DelegationOutcomeKind::ContinueRunning => "continue_running",
        DelegationOutcomeKind::AlreadyTerminal => "already_terminal",
    }
}

pub(crate) fn delegation_outcome_kind_from_str(value: &str) -> Option<DelegationOutcomeKind> {
    match value {
        "result_returned" => Some(DelegationOutcomeKind::ResultReturned),
        "child_failed" => Some(DelegationOutcomeKind::ChildFailed),
        "child_stopped" => Some(DelegationOutcomeKind::ChildStopped),
        "child_cancelled" => Some(DelegationOutcomeKind::ChildCancelled),
        "continue_running" => Some(DelegationOutcomeKind::ContinueRunning),
        "already_terminal" => Some(DelegationOutcomeKind::AlreadyTerminal),
        _ => None,
    }
}

pub(crate) const fn delegation_outcome_reason_to_str(
    value: DelegationOutcomeReason,
) -> Option<&'static str> {
    match value {
        DelegationOutcomeReason::ChildCompleted => Some("child_completed"),
        DelegationOutcomeReason::ChildExecutionFailed => Some("child_execution_failed"),
        DelegationOutcomeReason::ChildResultUnavailable => Some("child_result_unavailable"),
        DelegationOutcomeReason::ChildCancelled => Some("child_cancelled"),
        DelegationOutcomeReason::ParentStopped {
            scope: DescendantTerminationScope::ParentAndDescendants,
        } => Some("parent_stopped_parent_and_descendants"),
        DelegationOutcomeReason::ParentCancelled {
            scope: DescendantTerminationScope::ParentAndDescendants,
        } => Some("parent_cancelled_parent_and_descendants"),
        DelegationOutcomeReason::ParentStopped {
            scope: DescendantTerminationScope::ParentAlone,
        }
        | DelegationOutcomeReason::ParentCancelled {
            scope: DescendantTerminationScope::ParentAlone,
        } => None,
    }
}

pub(crate) fn delegation_outcome_reason_from_str(value: &str) -> Option<DelegationOutcomeReason> {
    match value {
        "child_completed" => Some(DelegationOutcomeReason::ChildCompleted),
        "child_execution_failed" => Some(DelegationOutcomeReason::ChildExecutionFailed),
        "child_result_unavailable" => Some(DelegationOutcomeReason::ChildResultUnavailable),
        "child_cancelled" => Some(DelegationOutcomeReason::ChildCancelled),
        "parent_stopped_parent_and_descendants" => Some(DelegationOutcomeReason::ParentStopped {
            scope: DescendantTerminationScope::ParentAndDescendants,
        }),
        "parent_cancelled_parent_and_descendants" => {
            Some(DelegationOutcomeReason::ParentCancelled {
                scope: DescendantTerminationScope::ParentAndDescendants,
            })
        }
        _ => None,
    }
}

/// Closed session-creation cause discriminators stored in PostgreSQL.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SessionCreationCauseStorageKind {
    UserInitiated,
    Delegated,
}

/// Encodes a session-creation cause as its closed PostgreSQL spelling.
pub(crate) const fn session_creation_cause_to_str(value: &SessionCreationCause) -> &'static str {
    match value {
        SessionCreationCause::UserInitiated => "user_initiated",
        SessionCreationCause::Delegated { .. } => "delegated",
    }
}

/// Decodes a closed session-creation cause discriminator from PostgreSQL.
pub(crate) fn session_creation_cause_from_str(
    value: &str,
) -> Option<SessionCreationCauseStorageKind> {
    match value {
        "user_initiated" => Some(SessionCreationCauseStorageKind::UserInitiated),
        "delegated" => Some(SessionCreationCauseStorageKind::Delegated),
        _ => None,
    }
}

/// Closed approval-judge lifecycle states stored by PostgreSQL.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ApprovalJudgeStateStorageKind {
    Prepared,
    InFlight,
    Terminal,
}

pub(crate) const fn approval_judge_state_to_str(
    value: ApprovalJudgeStateStorageKind,
) -> &'static str {
    match value {
        ApprovalJudgeStateStorageKind::Prepared => "prepared",
        ApprovalJudgeStateStorageKind::InFlight => "in_flight",
        ApprovalJudgeStateStorageKind::Terminal => "terminal",
    }
}

pub(crate) fn approval_judge_state_from_str(value: &str) -> Option<ApprovalJudgeStateStorageKind> {
    match value {
        "prepared" => Some(ApprovalJudgeStateStorageKind::Prepared),
        "in_flight" => Some(ApprovalJudgeStateStorageKind::InFlight),
        "terminal" => Some(ApprovalJudgeStateStorageKind::Terminal),
        _ => None,
    }
}

/// Closed approval-judge terminal dispositions stored by PostgreSQL.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ApprovalJudgeTerminalDispositionStorageKind {
    Completed,
    Failed(FailedApprovalJudgeDisposition),
}

pub(crate) const fn approval_judge_terminal_disposition_to_str(
    value: ApprovalJudgeTerminalDispositionStorageKind,
) -> &'static str {
    match value {
        ApprovalJudgeTerminalDispositionStorageKind::Completed => "completed",
        ApprovalJudgeTerminalDispositionStorageKind::Failed(
            FailedApprovalJudgeDisposition::KnownFailed,
        ) => "known_failed",
        ApprovalJudgeTerminalDispositionStorageKind::Failed(
            FailedApprovalJudgeDisposition::Refused,
        ) => "refused",
        ApprovalJudgeTerminalDispositionStorageKind::Failed(
            FailedApprovalJudgeDisposition::Cancelled,
        ) => "cancelled",
        ApprovalJudgeTerminalDispositionStorageKind::Failed(
            FailedApprovalJudgeDisposition::Ambiguous,
        ) => "ambiguous",
    }
}

pub(crate) fn approval_judge_terminal_disposition_from_str(
    value: &str,
) -> Option<ApprovalJudgeTerminalDispositionStorageKind> {
    match value {
        "completed" => Some(ApprovalJudgeTerminalDispositionStorageKind::Completed),
        "known_failed" => Some(ApprovalJudgeTerminalDispositionStorageKind::Failed(
            FailedApprovalJudgeDisposition::KnownFailed,
        )),
        "refused" => Some(ApprovalJudgeTerminalDispositionStorageKind::Failed(
            FailedApprovalJudgeDisposition::Refused,
        )),
        "cancelled" => Some(ApprovalJudgeTerminalDispositionStorageKind::Failed(
            FailedApprovalJudgeDisposition::Cancelled,
        )),
        "ambiguous" => Some(ApprovalJudgeTerminalDispositionStorageKind::Failed(
            FailedApprovalJudgeDisposition::Ambiguous,
        )),
        _ => None,
    }
}

pub(crate) const fn approval_judge_recommendation_to_str(
    value: DelegateApprovalRecommendation,
) -> &'static str {
    match value {
        DelegateApprovalRecommendation::Approve => "approve",
        DelegateApprovalRecommendation::Deny => "deny",
        DelegateApprovalRecommendation::EscalateToHuman => "escalate_to_human",
    }
}

pub(crate) fn approval_judge_recommendation_from_str(
    value: &str,
) -> Option<DelegateApprovalRecommendation> {
    match value {
        "approve" => Some(DelegateApprovalRecommendation::Approve),
        "deny" => Some(DelegateApprovalRecommendation::Deny),
        "escalate_to_human" => Some(DelegateApprovalRecommendation::EscalateToHuman),
        _ => None,
    }
}

/// Closed tool-approval decision sources stored by PostgreSQL.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ToolApprovalDecisionSourceStorageKind {
    UserCommand,
    PolicyAuto,
    SessionBlanket,
    Delegate,
    UserOverride,
}

pub(crate) const fn tool_approval_decision_source_to_str(
    value: ToolApprovalDecisionSourceStorageKind,
) -> &'static str {
    match value {
        ToolApprovalDecisionSourceStorageKind::UserCommand => "user_command",
        ToolApprovalDecisionSourceStorageKind::PolicyAuto => "policy_auto",
        ToolApprovalDecisionSourceStorageKind::SessionBlanket => "session_blanket",
        ToolApprovalDecisionSourceStorageKind::Delegate => "delegate",
        ToolApprovalDecisionSourceStorageKind::UserOverride => "user_override",
    }
}

pub(crate) fn tool_approval_decision_source_from_str(
    value: &str,
) -> Option<ToolApprovalDecisionSourceStorageKind> {
    match value {
        "user_command" => Some(ToolApprovalDecisionSourceStorageKind::UserCommand),
        "policy_auto" => Some(ToolApprovalDecisionSourceStorageKind::PolicyAuto),
        "session_blanket" => Some(ToolApprovalDecisionSourceStorageKind::SessionBlanket),
        "delegate" => Some(ToolApprovalDecisionSourceStorageKind::Delegate),
        "user_override" => Some(ToolApprovalDecisionSourceStorageKind::UserOverride),
        _ => None,
    }
}

/// Closed durable-command kinds stored by the user-global registry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DurableCommandKind {
    /// Session creation.
    CreateSession,
    /// Session creation from an imported frontier.
    CreateSessionFromImportedFrontier,
    /// Session-default replacement.
    ReplaceSessionDefaults,
    /// Session-metadata replacement.
    ReplaceSessionMetadata,
    /// Session input submission.
    SubmitInput,
    /// Tool-request decision.
    DecideToolRequest,
    /// Delegate-denial override.
    OverrideDeniedToolRequest,
    /// Review-workflow command.
    ReviewWorkflow,
    /// Review-orchestration command.
    ReviewOrchestration,
    /// Session context compaction.
    CompactSession,
    /// Session goal command.
    Goal,
    /// Session placement update.
    UpdateSessionPlacement,
    /// Workspace registration.
    RegisterWorkspace,
    /// Git remote mint.
    MintGitRemote,
    /// Git remote withdrawal.
    WithdrawGitRemote,
}

/// Encodes a durable-command kind as its closed PostgreSQL spelling.
pub(crate) const fn durable_command_kind_to_str(value: DurableCommandKind) -> &'static str {
    match value {
        DurableCommandKind::CreateSession => "create_session",
        DurableCommandKind::CreateSessionFromImportedFrontier => {
            "create_session_from_imported_frontier"
        }
        DurableCommandKind::ReplaceSessionDefaults => "replace_session_defaults",
        DurableCommandKind::ReplaceSessionMetadata => "replace_session_metadata",
        DurableCommandKind::SubmitInput => "submit_input",
        DurableCommandKind::DecideToolRequest => "decide_tool_request",
        DurableCommandKind::OverrideDeniedToolRequest => "override_denied_tool_request",
        DurableCommandKind::ReviewWorkflow => "review_workflow",
        DurableCommandKind::ReviewOrchestration => "review_orchestration",
        DurableCommandKind::CompactSession => "compact_session",
        DurableCommandKind::Goal => "goal",
        DurableCommandKind::UpdateSessionPlacement => "update_session_placement",
        DurableCommandKind::RegisterWorkspace => "register_workspace",
        DurableCommandKind::MintGitRemote => "mint_git_remote",
        DurableCommandKind::WithdrawGitRemote => "withdraw_git_remote",
    }
}

/// Decodes a closed durable-command kind from its PostgreSQL spelling.
pub(crate) fn durable_command_kind_from_str(value: &str) -> Option<DurableCommandKind> {
    match value {
        "create_session" => Some(DurableCommandKind::CreateSession),
        "create_session_from_imported_frontier" => {
            Some(DurableCommandKind::CreateSessionFromImportedFrontier)
        }
        "replace_session_defaults" => Some(DurableCommandKind::ReplaceSessionDefaults),
        "replace_session_metadata" => Some(DurableCommandKind::ReplaceSessionMetadata),
        "submit_input" => Some(DurableCommandKind::SubmitInput),
        "decide_tool_request" => Some(DurableCommandKind::DecideToolRequest),
        "override_denied_tool_request" => Some(DurableCommandKind::OverrideDeniedToolRequest),
        "review_workflow" => Some(DurableCommandKind::ReviewWorkflow),
        "review_orchestration" => Some(DurableCommandKind::ReviewOrchestration),
        "compact_session" => Some(DurableCommandKind::CompactSession),
        "goal" => Some(DurableCommandKind::Goal),
        "update_session_placement" => Some(DurableCommandKind::UpdateSessionPlacement),
        "register_workspace" => Some(DurableCommandKind::RegisterWorkspace),
        "mint_git_remote" => Some(DurableCommandKind::MintGitRemote),
        "withdraw_git_remote" => Some(DurableCommandKind::WithdrawGitRemote),
        _ => None,
    }
}

/// Encodes one durable `workspace.origin` spelling.
///
/// The column is authority-bearing: it says whether a workspace is a scope a
/// person registered or one the daemon's derivation produced. This module owns
/// the spelling so the store that lands next cannot restate it independently
/// and drift from the `CHECK` that admits exactly these two values.
#[allow(
    dead_code,
    reason = "the workspace store lands with the operator verbs; this mapping is the spelling those writers must use"
)]
pub(crate) const fn workspace_origin_to_str(value: WorkspaceOrigin) -> &'static str {
    match value {
        WorkspaceOrigin::OperatorRegistered => "operator_registered",
        WorkspaceOrigin::DaemonDerived => "daemon_derived",
    }
}

/// Decodes one durable `workspace.origin` spelling.
#[allow(
    dead_code,
    reason = "the workspace store lands with the operator verbs; this mapping is the spelling those readers must use"
)]
pub(crate) fn workspace_origin_from_str(value: &str) -> Option<WorkspaceOrigin> {
    match value {
        "operator_registered" => Some(WorkspaceOrigin::OperatorRegistered),
        "daemon_derived" => Some(WorkspaceOrigin::DaemonDerived),
        _ => None,
    }
}

/// Closed stored result kinds for placement-update commands.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SessionPlacementResultStorageKind {
    Applied,
    Rejected,
}

/// Closed stored rejection kinds for placement-update commands.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SessionPlacementRejectionStorageKind {
    SessionNotFound,
    CurrentVersionMismatch,
    VersionExhausted,
}

pub(crate) const fn session_placement_event_kind_to_str(
    value: SessionPlacementEventKind,
) -> &'static str {
    match value {
        SessionPlacementEventKind::Created => "created",
        SessionPlacementEventKind::Updated => "updated",
    }
}

pub(crate) fn session_placement_event_kind_from_str(
    value: &str,
) -> Option<SessionPlacementEventKind> {
    match value {
        "created" => Some(SessionPlacementEventKind::Created),
        "updated" => Some(SessionPlacementEventKind::Updated),
        _ => None,
    }
}

pub(crate) const fn session_placement_result_kind_to_str(
    value: SessionPlacementResultStorageKind,
) -> &'static str {
    match value {
        SessionPlacementResultStorageKind::Applied => "applied",
        SessionPlacementResultStorageKind::Rejected => "rejected",
    }
}

pub(crate) fn session_placement_result_kind_from_str(
    value: &str,
) -> Option<SessionPlacementResultStorageKind> {
    match value {
        "applied" => Some(SessionPlacementResultStorageKind::Applied),
        "rejected" => Some(SessionPlacementResultStorageKind::Rejected),
        _ => None,
    }
}

pub(crate) const fn session_placement_rejection_to_str(
    value: UpdateSessionPlacementRejectionKind,
) -> &'static str {
    match value {
        UpdateSessionPlacementRejectionKind::SessionNotFound => "session_not_found",
        UpdateSessionPlacementRejectionKind::CurrentVersionMismatch => "current_version_mismatch",
        UpdateSessionPlacementRejectionKind::VersionExhausted => "version_exhausted",
    }
}

pub(crate) fn session_placement_rejection_from_str(
    value: &str,
) -> Option<SessionPlacementRejectionStorageKind> {
    match value {
        "session_not_found" => Some(SessionPlacementRejectionStorageKind::SessionNotFound),
        "current_version_mismatch" => {
            Some(SessionPlacementRejectionStorageKind::CurrentVersionMismatch)
        }
        "version_exhausted" => Some(SessionPlacementRejectionStorageKind::VersionExhausted),
        _ => None,
    }
}

/// Closed stored operation kinds for goal user commands.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum GoalOperationKind {
    Attach,
    Resume,
    Stop,
    Supersede,
}

pub(crate) const fn goal_operation_to_str(value: &GoalUserAction) -> &'static str {
    match value {
        GoalUserAction::Attach(_) => "attach",
        GoalUserAction::Resume(_) => "resume",
        GoalUserAction::Stop { .. } => "stop",
        GoalUserAction::Supersede(_) => "supersede",
    }
}

pub(crate) fn goal_operation_from_str(value: &str) -> Option<GoalOperationKind> {
    match value {
        "attach" => Some(GoalOperationKind::Attach),
        "resume" => Some(GoalOperationKind::Resume),
        "stop" => Some(GoalOperationKind::Stop),
        "supersede" => Some(GoalOperationKind::Supersede),
        _ => None,
    }
}

/// Closed stored event kinds for goal lineage events.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum GoalEventDiscriminator {
    Commissioned,
    Blocked,
    Resumed,
    Achieved,
    UserStopped,
    Superseded,
}

pub(crate) const fn goal_event_kind_to_str(value: &GoalEventKind) -> &'static str {
    match value {
        GoalEventKind::Commissioned { .. } => "commissioned",
        GoalEventKind::Blocked { .. } => "blocked",
        GoalEventKind::Resumed { .. } => "resumed",
        GoalEventKind::Achieved { .. } => "achieved",
        GoalEventKind::UserStopped { .. } => "user_stopped",
        GoalEventKind::Superseded { .. } => "superseded",
    }
}

pub(crate) fn goal_event_kind_from_str(value: &str) -> Option<GoalEventDiscriminator> {
    match value {
        "commissioned" => Some(GoalEventDiscriminator::Commissioned),
        "blocked" => Some(GoalEventDiscriminator::Blocked),
        "resumed" => Some(GoalEventDiscriminator::Resumed),
        "achieved" => Some(GoalEventDiscriminator::Achieved),
        "user_stopped" => Some(GoalEventDiscriminator::UserStopped),
        "superseded" => Some(GoalEventDiscriminator::Superseded),
        _ => None,
    }
}

pub(crate) const fn goal_blocked_reason_to_str(value: GoalBlockedReasonKind) -> &'static str {
    match value {
        GoalBlockedReasonKind::UserInputRequired => "user_input_required",
        GoalBlockedReasonKind::ExternalChangeRequired => "external_change_required",
        GoalBlockedReasonKind::AuthorizationRequired => "authorization_required",
        GoalBlockedReasonKind::ExecutionFailure => "execution_failure",
    }
}

pub(crate) fn goal_blocked_reason_from_str(value: &str) -> Option<GoalBlockedReasonKind> {
    match value {
        "user_input_required" => Some(GoalBlockedReasonKind::UserInputRequired),
        "external_change_required" => Some(GoalBlockedReasonKind::ExternalChangeRequired),
        "authorization_required" => Some(GoalBlockedReasonKind::AuthorizationRequired),
        "execution_failure" => Some(GoalBlockedReasonKind::ExecutionFailure),
        _ => None,
    }
}

pub(crate) fn goal_model_blocked_reason_from_str(
    value: &str,
) -> Option<GoalModelBlockedReasonKind> {
    match value {
        "user_input_required" => Some(GoalModelBlockedReasonKind::UserInputRequired),
        "external_change_required" => Some(GoalModelBlockedReasonKind::ExternalChangeRequired),
        "authorization_required" => Some(GoalModelBlockedReasonKind::AuthorizationRequired),
        _ => None,
    }
}

pub(crate) const fn goal_command_rejection_to_str(value: GoalCommandRejection) -> &'static str {
    match value {
        GoalCommandRejection::SessionNotFound => "session_not_found",
        GoalCommandRejection::GoalAlreadyAttached => "goal_already_attached",
        GoalCommandRejection::GoalNotAttached => "goal_not_attached",
        GoalCommandRejection::UnknownModelAlias => "unknown_model_alias",
        GoalCommandRejection::AcceptancePositionExhausted => "acceptance_position_exhausted",
        GoalCommandRejection::RequiresBlocked => "requires_blocked",
        GoalCommandRejection::RequiresPursuingOrBlocked => "requires_pursuing_or_blocked",
        GoalCommandRejection::GenerationExhausted => "generation_exhausted",
        GoalCommandRejection::EventOrdinalExhausted => "event_ordinal_exhausted",
    }
}

pub(crate) fn goal_command_rejection_from_str(value: &str) -> Option<GoalCommandRejection> {
    match value {
        "session_not_found" => Some(GoalCommandRejection::SessionNotFound),
        "goal_already_attached" => Some(GoalCommandRejection::GoalAlreadyAttached),
        "goal_not_attached" => Some(GoalCommandRejection::GoalNotAttached),
        "unknown_model_alias" => Some(GoalCommandRejection::UnknownModelAlias),
        "acceptance_position_exhausted" => Some(GoalCommandRejection::AcceptancePositionExhausted),
        "requires_blocked" => Some(GoalCommandRejection::RequiresBlocked),
        "requires_pursuing_or_blocked" => Some(GoalCommandRejection::RequiresPursuingOrBlocked),
        "generation_exhausted" => Some(GoalCommandRejection::GenerationExhausted),
        "event_ordinal_exhausted" => Some(GoalCommandRejection::EventOrdinalExhausted),
        _ => None,
    }
}

/// Closed repository-watch singleton scopes stored by PostgreSQL.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RepoWatchSingletonScopeStorageKind {
    PullRequest,
    Stack,
    Rule,
    Repository,
}

pub(crate) const fn repo_watch_singleton_scope_to_str(
    value: RepoWatchSingletonScopeStorageKind,
) -> &'static str {
    match value {
        RepoWatchSingletonScopeStorageKind::PullRequest => "pull_request",
        RepoWatchSingletonScopeStorageKind::Stack => "stack",
        RepoWatchSingletonScopeStorageKind::Rule => "rule",
        RepoWatchSingletonScopeStorageKind::Repository => "repo",
    }
}

pub(crate) fn repo_watch_singleton_scope_from_str(
    value: &str,
) -> Option<RepoWatchSingletonScopeStorageKind> {
    match value {
        "pull_request" => Some(RepoWatchSingletonScopeStorageKind::PullRequest),
        "stack" => Some(RepoWatchSingletonScopeStorageKind::Stack),
        "rule" => Some(RepoWatchSingletonScopeStorageKind::Rule),
        "repo" => Some(RepoWatchSingletonScopeStorageKind::Repository),
        _ => None,
    }
}

/// Closed lifecycle-cutoff dispositions stored by PostgreSQL.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RepoWatchLifecycleCutoffDispositionStorageKind {
    Terminal,
    Reopened,
}

pub(crate) const fn repo_watch_lifecycle_cutoff_disposition_to_str(
    value: RepoWatchLifecycleCutoffDispositionStorageKind,
) -> &'static str {
    match value {
        RepoWatchLifecycleCutoffDispositionStorageKind::Terminal => "terminal",
        RepoWatchLifecycleCutoffDispositionStorageKind::Reopened => "reopened",
    }
}

pub(crate) fn repo_watch_lifecycle_cutoff_disposition_from_str(
    value: &str,
) -> Option<RepoWatchLifecycleCutoffDispositionStorageKind> {
    match value {
        "terminal" => Some(RepoWatchLifecycleCutoffDispositionStorageKind::Terminal),
        "reopened" => Some(RepoWatchLifecycleCutoffDispositionStorageKind::Reopened),
        _ => None,
    }
}

/// Closed outcomes stored for one repository-watch rule evaluation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RepoWatchEvaluationOutcomeStorageKind {
    NotMatched,
    TargetClosed,
    Occupied,
    Coalesced,
    Cooldown,
    Dispatched,
}

pub(crate) const fn repo_watch_evaluation_outcome_to_str(
    value: RepoWatchEvaluationOutcomeStorageKind,
) -> &'static str {
    match value {
        RepoWatchEvaluationOutcomeStorageKind::NotMatched => "not_matched",
        RepoWatchEvaluationOutcomeStorageKind::TargetClosed => "target_closed",
        RepoWatchEvaluationOutcomeStorageKind::Occupied => "occupied",
        RepoWatchEvaluationOutcomeStorageKind::Coalesced => "coalesced",
        RepoWatchEvaluationOutcomeStorageKind::Cooldown => "cooldown",
        RepoWatchEvaluationOutcomeStorageKind::Dispatched => "dispatched",
    }
}

pub(crate) fn repo_watch_evaluation_outcome_from_str(
    value: &str,
) -> Option<RepoWatchEvaluationOutcomeStorageKind> {
    match value {
        "not_matched" => Some(RepoWatchEvaluationOutcomeStorageKind::NotMatched),
        "target_closed" => Some(RepoWatchEvaluationOutcomeStorageKind::TargetClosed),
        "occupied" => Some(RepoWatchEvaluationOutcomeStorageKind::Occupied),
        "coalesced" => Some(RepoWatchEvaluationOutcomeStorageKind::Coalesced),
        "cooldown" => Some(RepoWatchEvaluationOutcomeStorageKind::Cooldown),
        "dispatched" => Some(RepoWatchEvaluationOutcomeStorageKind::Dispatched),
        _ => None,
    }
}

/// Closed settlement kinds stored for one repository-watch dispatch obligation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RepoWatchObligationSettlementStorageKind {
    Deactivated,
    TargetClosed,
    Dispatched,
}

pub(crate) const fn repo_watch_obligation_settlement_to_str(
    value: RepoWatchObligationSettlementStorageKind,
) -> &'static str {
    match value {
        RepoWatchObligationSettlementStorageKind::Deactivated => "deactivated",
        RepoWatchObligationSettlementStorageKind::TargetClosed => "target_closed",
        RepoWatchObligationSettlementStorageKind::Dispatched => "dispatched",
    }
}

pub(crate) fn repo_watch_obligation_settlement_from_str(
    value: &str,
) -> Option<RepoWatchObligationSettlementStorageKind> {
    match value {
        "deactivated" => Some(RepoWatchObligationSettlementStorageKind::Deactivated),
        "target_closed" => Some(RepoWatchObligationSettlementStorageKind::TargetClosed),
        "dispatched" => Some(RepoWatchObligationSettlementStorageKind::Dispatched),
        _ => None,
    }
}

/// Stored target shape for one repository-watch event.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RepoWatchEventTargetStorageKind {
    PullRequest,
    Branch,
}

pub(crate) const fn repo_watch_event_target_to_str(
    value: RepoWatchEventTargetStorageKind,
) -> &'static str {
    match value {
        RepoWatchEventTargetStorageKind::PullRequest => "pull_request",
        RepoWatchEventTargetStorageKind::Branch => "branch",
    }
}

/// Which producer recorded one repository-watch event.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RepoWatchEventProducerStorageKind {
    Poll,
}

pub(crate) const fn repo_watch_event_producer_to_str(
    value: RepoWatchEventProducerStorageKind,
) -> &'static str {
    match value {
        RepoWatchEventProducerStorageKind::Poll => "poll",
    }
}

pub(crate) fn repo_watch_event_producer_from_str(
    value: &str,
) -> Option<RepoWatchEventProducerStorageKind> {
    match value {
        "poll" => Some(RepoWatchEventProducerStorageKind::Poll),
        _ => None,
    }
}

pub(crate) fn repo_watch_event_target_from_str(
    value: &str,
) -> Option<RepoWatchEventTargetStorageKind> {
    match value {
        "pull_request" => Some(RepoWatchEventTargetStorageKind::PullRequest),
        "branch" => Some(RepoWatchEventTargetStorageKind::Branch),
        _ => None,
    }
}

pub(crate) const fn repo_watch_event_kind_to_str(value: RepoWatchEventKindNameV1) -> &'static str {
    match value {
        RepoWatchEventKindNameV1::PullRequestOpened => "pull_request_opened",
        RepoWatchEventKindNameV1::PullRequestClosed => "pull_request_closed",
        RepoWatchEventKindNameV1::PullRequestMerged => "pull_request_merged",
        RepoWatchEventKindNameV1::HeadChanged => "head_changed",
        RepoWatchEventKindNameV1::MergeableStateChanged => "mergeable_state_changed",
        RepoWatchEventKindNameV1::ChecksCompleted => "checks_completed",
        RepoWatchEventKindNameV1::CheckRunCompleted => "check_run_completed",
        RepoWatchEventKindNameV1::BranchWorkflowRunCompleted => "branch_workflow_run_completed",
        RepoWatchEventKindNameV1::ReviewSubmitted => "review_submitted",
        RepoWatchEventKindNameV1::ThreadOpened => "thread_opened",
        RepoWatchEventKindNameV1::ThreadResolved => "thread_resolved",
        RepoWatchEventKindNameV1::Labeled => "labeled",
        RepoWatchEventKindNameV1::Unlabeled => "unlabeled",
        RepoWatchEventKindNameV1::BaseAdvanced => "base_advanced",
        RepoWatchEventKindNameV1::ReactionChanged => "reaction_changed",
    }
}

pub(crate) fn repo_watch_event_kind_from_str(value: &str) -> Option<RepoWatchEventKindNameV1> {
    match value {
        "pull_request_opened" => Some(RepoWatchEventKindNameV1::PullRequestOpened),
        "pull_request_closed" => Some(RepoWatchEventKindNameV1::PullRequestClosed),
        "pull_request_merged" => Some(RepoWatchEventKindNameV1::PullRequestMerged),
        "head_changed" => Some(RepoWatchEventKindNameV1::HeadChanged),
        "mergeable_state_changed" => Some(RepoWatchEventKindNameV1::MergeableStateChanged),
        "checks_completed" => Some(RepoWatchEventKindNameV1::ChecksCompleted),
        "check_run_completed" => Some(RepoWatchEventKindNameV1::CheckRunCompleted),
        "branch_workflow_run_completed" => {
            Some(RepoWatchEventKindNameV1::BranchWorkflowRunCompleted)
        }
        "review_submitted" => Some(RepoWatchEventKindNameV1::ReviewSubmitted),
        "thread_opened" => Some(RepoWatchEventKindNameV1::ThreadOpened),
        "thread_resolved" => Some(RepoWatchEventKindNameV1::ThreadResolved),
        "labeled" => Some(RepoWatchEventKindNameV1::Labeled),
        "unlabeled" => Some(RepoWatchEventKindNameV1::Unlabeled),
        "base_advanced" => Some(RepoWatchEventKindNameV1::BaseAdvanced),
        "reaction_changed" => Some(RepoWatchEventKindNameV1::ReactionChanged),
        _ => None,
    }
}

pub(crate) const fn repo_watch_pull_request_lifecycle_to_str(
    value: RepoWatchPullRequestLifecycle,
) -> &'static str {
    match value {
        RepoWatchPullRequestLifecycle::Open => "open",
        RepoWatchPullRequestLifecycle::Closed => "closed",
        RepoWatchPullRequestLifecycle::Merged => "merged",
    }
}

pub(crate) fn repo_watch_pull_request_lifecycle_from_str(
    value: &str,
) -> Option<RepoWatchPullRequestLifecycle> {
    match value {
        "open" => Some(RepoWatchPullRequestLifecycle::Open),
        "closed" => Some(RepoWatchPullRequestLifecycle::Closed),
        "merged" => Some(RepoWatchPullRequestLifecycle::Merged),
        _ => None,
    }
}

pub(crate) const fn repo_watch_mergeable_state_to_str(value: MergeableState) -> &'static str {
    match value {
        MergeableState::Mergeable => "mergeable",
        MergeableState::Conflicting => "conflicting",
        MergeableState::Unknown => "unknown",
    }
}

pub(crate) fn repo_watch_mergeable_state_from_str(value: &str) -> Option<MergeableState> {
    match value {
        "mergeable" => Some(MergeableState::Mergeable),
        "conflicting" => Some(MergeableState::Conflicting),
        "unknown" => Some(MergeableState::Unknown),
        _ => None,
    }
}

pub(crate) const fn repo_watch_checks_outcome_to_str(value: ChecksOutcome) -> &'static str {
    match value {
        ChecksOutcome::Success => "success",
        ChecksOutcome::Failure => "failure",
    }
}

pub(crate) fn repo_watch_checks_outcome_from_str(value: &str) -> Option<ChecksOutcome> {
    match value {
        "success" => Some(ChecksOutcome::Success),
        "failure" => Some(ChecksOutcome::Failure),
        _ => None,
    }
}

pub(crate) const fn repo_watch_check_conclusion_to_str(value: CheckConclusion) -> &'static str {
    match value {
        CheckConclusion::Success => "success",
        CheckConclusion::Failure => "failure",
        CheckConclusion::Neutral => "neutral",
        CheckConclusion::Cancelled => "cancelled",
        CheckConclusion::Skipped => "skipped",
        CheckConclusion::TimedOut => "timed_out",
        CheckConclusion::ActionRequired => "action_required",
        CheckConclusion::Stale => "stale",
        CheckConclusion::StartupFailure => "startup_failure",
    }
}

pub(crate) fn repo_watch_check_conclusion_from_str(value: &str) -> Option<CheckConclusion> {
    match value {
        "success" => Some(CheckConclusion::Success),
        "failure" => Some(CheckConclusion::Failure),
        "neutral" => Some(CheckConclusion::Neutral),
        "cancelled" => Some(CheckConclusion::Cancelled),
        "skipped" => Some(CheckConclusion::Skipped),
        "timed_out" => Some(CheckConclusion::TimedOut),
        "action_required" => Some(CheckConclusion::ActionRequired),
        "stale" => Some(CheckConclusion::Stale),
        "startup_failure" => Some(CheckConclusion::StartupFailure),
        _ => None,
    }
}

pub(crate) const fn repo_watch_review_state_to_str(value: ReviewState) -> &'static str {
    match value {
        ReviewState::Approved => "approved",
        ReviewState::ChangesRequested => "changes_requested",
        ReviewState::Commented => "commented",
    }
}

pub(crate) fn repo_watch_review_state_from_str(value: &str) -> Option<ReviewState> {
    match value {
        "approved" => Some(ReviewState::Approved),
        "changes_requested" => Some(ReviewState::ChangesRequested),
        "commented" => Some(ReviewState::Commented),
        _ => None,
    }
}

pub(crate) const fn repo_watch_thread_state_to_str(value: RepoWatchThreadState) -> &'static str {
    match value {
        RepoWatchThreadState::Open => "open",
        RepoWatchThreadState::Resolved => "resolved",
    }
}

pub(crate) fn repo_watch_thread_state_from_str(value: &str) -> Option<RepoWatchThreadState> {
    match value {
        "open" => Some(RepoWatchThreadState::Open),
        "resolved" => Some(RepoWatchThreadState::Resolved),
        _ => None,
    }
}

pub(crate) const fn repo_watch_reaction_change_to_str(value: ReactionChange) -> &'static str {
    match value {
        ReactionChange::Added => "added",
        ReactionChange::Removed => "removed",
    }
}

pub(crate) fn repo_watch_reaction_change_from_str(value: &str) -> Option<ReactionChange> {
    match value {
        "added" => Some(ReactionChange::Added),
        "removed" => Some(ReactionChange::Removed),
        _ => None,
    }
}

/// Stored subject shape for one reaction observation or event.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RepoWatchReactionSubjectStorageKind {
    PullRequestBody,
    IssueComment,
    ReviewComment,
}

pub(crate) const fn repo_watch_reaction_subject_to_storage(
    value: ReactionSubject,
) -> (RepoWatchReactionSubjectStorageKind, Option<u64>) {
    match value {
        ReactionSubject::PullRequestBody => {
            (RepoWatchReactionSubjectStorageKind::PullRequestBody, None)
        }
        ReactionSubject::IssueComment { id } => (
            RepoWatchReactionSubjectStorageKind::IssueComment,
            Some(id.get()),
        ),
        ReactionSubject::ReviewComment { id } => (
            RepoWatchReactionSubjectStorageKind::ReviewComment,
            Some(id.get()),
        ),
    }
}

pub(crate) const fn repo_watch_reaction_subject_kind_to_str(
    value: RepoWatchReactionSubjectStorageKind,
) -> &'static str {
    match value {
        RepoWatchReactionSubjectStorageKind::PullRequestBody => "pull_request_body",
        RepoWatchReactionSubjectStorageKind::IssueComment => "issue_comment",
        RepoWatchReactionSubjectStorageKind::ReviewComment => "review_comment",
    }
}

pub(crate) fn repo_watch_reaction_subject_kind_from_str(
    value: &str,
) -> Option<RepoWatchReactionSubjectStorageKind> {
    match value {
        "pull_request_body" => Some(RepoWatchReactionSubjectStorageKind::PullRequestBody),
        "issue_comment" => Some(RepoWatchReactionSubjectStorageKind::IssueComment),
        "review_comment" => Some(RepoWatchReactionSubjectStorageKind::ReviewComment),
        _ => None,
    }
}
/// Why a PostgreSQL `numeric(20, 0)` value is not a positive domain ordinal.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PositiveOrdinalMappingError {
    /// The value is zero or negative.
    NonPositive,
    /// The value has a nonzero fractional component.
    Fractional,
    /// The positive integral value exceeds `u64::MAX`.
    OutOfRange,
}

impl fmt::Display for PositiveOrdinalMappingError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::NonPositive => "ordinal must be positive",
            Self::Fractional => "ordinal must not have a fractional component",
            Self::OutOfRange => "ordinal exceeds the u64 range",
        };
        formatter.write_str(message)
    }
}

impl Error for PositiveOrdinalMappingError {}

/// Encodes a defaults version as its exact PostgreSQL `numeric(20, 0)` value.
pub fn defaults_version_to_numeric(value: SessionConfigurationDefaultsVersion) -> Decimal {
    Decimal::from(value.as_u64())
}

/// Decodes a checked defaults version from a PostgreSQL `numeric(20, 0)` value.
pub fn defaults_version_from_numeric(
    value: Decimal,
) -> Result<SessionConfigurationDefaultsVersion, PositiveOrdinalMappingError> {
    let ordinal = positive_u64_from_numeric(value)?;
    SessionConfigurationDefaultsVersion::try_from_u64(ordinal)
        .ok_or(PositiveOrdinalMappingError::NonPositive)
}

/// Encodes an input position as its exact PostgreSQL `numeric(20, 0)` value.
pub fn input_position_to_numeric(value: SessionInputPosition) -> Decimal {
    Decimal::from(value.as_u64())
}

/// Decodes a checked input position from a PostgreSQL `numeric(20, 0)` value.
pub fn input_position_from_numeric(
    value: Decimal,
) -> Result<SessionInputPosition, PositiveOrdinalMappingError> {
    let ordinal = positive_u64_from_numeric(value)?;
    SessionInputPosition::try_from_u64(ordinal).ok_or(PositiveOrdinalMappingError::NonPositive)
}

/// Encodes the dangerous blanket posture as its closed storage spelling.
pub fn dangerous_tool_auto_approval_to_str(value: DangerousToolAutoApproval) -> &'static str {
    match value {
        DangerousToolAutoApproval::Disabled => "disabled",
        DangerousToolAutoApproval::ApproveAll => "approve_all",
    }
}

/// Decodes the closed dangerous blanket storage spelling.
pub fn dangerous_tool_auto_approval_from_str(value: &str) -> Option<DangerousToolAutoApproval> {
    match value {
        "disabled" => Some(DangerousToolAutoApproval::Disabled),
        "approve_all" => Some(DangerousToolAutoApproval::ApproveAll),
        _ => None,
    }
}

/// Encodes a tool permission default as its closed PostgreSQL spelling.
pub(crate) const fn tool_permission_default_to_str(value: ToolPermissionDefault) -> &'static str {
    match value {
        ToolPermissionDefault::Auto => "auto",
        ToolPermissionDefault::Confirm => "confirm",
        ToolPermissionDefault::AlwaysConfirm => "always_confirm",
    }
}

/// Decodes a tool permission default from its closed PostgreSQL spelling.
pub(crate) fn tool_permission_default_from_str(value: &str) -> Option<ToolPermissionDefault> {
    match value {
        "auto" => Some(ToolPermissionDefault::Auto),
        "confirm" => Some(ToolPermissionDefault::Confirm),
        "always_confirm" => Some(ToolPermissionDefault::AlwaysConfirm),
        _ => None,
    }
}

/// Encodes a runner placement loss source as its closed PostgreSQL spelling.
pub(crate) const fn runner_placement_loss_source_to_str(
    value: RunnerPlacementLossSource,
) -> &'static str {
    match value {
        RunnerPlacementLossSource::Connection => "connection",
        RunnerPlacementLossSource::Registration => "registration",
    }
}

/// Decodes a runner placement loss source from its closed PostgreSQL spelling.
pub(crate) fn runner_placement_loss_source_from_str(
    value: &str,
) -> Option<RunnerPlacementLossSource> {
    match value {
        "connection" => Some(RunnerPlacementLossSource::Connection),
        "registration" => Some(RunnerPlacementLossSource::Registration),
        _ => None,
    }
}

/// Encodes a follower-visible runner state as its closed PostgreSQL spelling.
pub(crate) const fn dispatched_runner_state_to_str(state: DispatchedRunnerState) -> &'static str {
    match state {
        DispatchedRunnerState::Pinned => "pinned",
        DispatchedRunnerState::Suspect => "suspect",
        DispatchedRunnerState::Connected => "connected",
        DispatchedRunnerState::RunnerLostBeforePin => "runner_lost_before_pin",
        DispatchedRunnerState::RunnerLost => "runner_lost",
        DispatchedRunnerState::Replaced => "replaced",
        DispatchedRunnerState::WorkingDirectoryChanged => "working_directory_changed",
        DispatchedRunnerState::Abandoned => "abandoned",
    }
}

/// Decodes a follower-visible runner state from its closed PostgreSQL spelling.
pub(crate) fn dispatched_runner_state_from_str(value: &str) -> Option<DispatchedRunnerState> {
    match value {
        "pinned" => Some(DispatchedRunnerState::Pinned),
        "suspect" => Some(DispatchedRunnerState::Suspect),
        "connected" => Some(DispatchedRunnerState::Connected),
        "runner_lost_before_pin" => Some(DispatchedRunnerState::RunnerLostBeforePin),
        "runner_lost" => Some(DispatchedRunnerState::RunnerLost),
        "replaced" => Some(DispatchedRunnerState::Replaced),
        "working_directory_changed" => Some(DispatchedRunnerState::WorkingDirectoryChanged),
        "abandoned" => Some(DispatchedRunnerState::Abandoned),
        _ => None,
    }
}

pub(crate) const fn runner_sandbox_to_str(value: RunnerSandboxProfile) -> &'static str {
    match value {
        RunnerSandboxProfile::Ambient => "ambient",
        RunnerSandboxProfile::WorkspaceRestricted => "workspace_restricted",
    }
}

pub(crate) fn runner_sandbox_from_str(value: &str) -> Option<RunnerSandboxProfile> {
    match value {
        "ambient" => Some(RunnerSandboxProfile::Ambient),
        "workspace_restricted" => Some(RunnerSandboxProfile::WorkspaceRestricted),
        _ => None,
    }
}

/// Encodes a frozen per-tool approval posture as its closed PostgreSQL spelling.
pub(crate) const fn tool_approval_posture_to_str(value: ToolApprovalPosture) -> &'static str {
    match value {
        ToolApprovalPosture::Auto => "auto",
        ToolApprovalPosture::Delegated => "delegated",
        ToolApprovalPosture::Human => "human",
    }
}

/// Decodes a frozen per-tool approval posture from its closed PostgreSQL spelling.
pub(crate) fn tool_approval_posture_from_str(value: &str) -> Option<ToolApprovalPosture> {
    match value {
        "auto" => Some(ToolApprovalPosture::Auto),
        "delegated" => Some(ToolApprovalPosture::Delegated),
        "human" => Some(ToolApprovalPosture::Human),
        _ => None,
    }
}

/// Closed plan-event kinds stored by PostgreSQL.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PlanEventStorageKind {
    /// Entry creation.
    Created,
    /// Text revision.
    TextRevised,
    /// Status change.
    StatusChanged,
    /// Entry dependency.
    DependsOn,
}

/// Encodes a plan-event kind as its closed PostgreSQL spelling.
pub(crate) const fn plan_event_kind_to_str(value: PlanEventStorageKind) -> &'static str {
    match value {
        PlanEventStorageKind::Created => "created",
        PlanEventStorageKind::TextRevised => "text_revised",
        PlanEventStorageKind::StatusChanged => "status_changed",
        PlanEventStorageKind::DependsOn => "depends_on",
    }
}

/// Decodes a closed plan-event kind from its PostgreSQL spelling.
pub(crate) fn plan_event_kind_from_str(value: &str) -> Option<PlanEventStorageKind> {
    match value {
        "created" => Some(PlanEventStorageKind::Created),
        "text_revised" => Some(PlanEventStorageKind::TextRevised),
        "status_changed" => Some(PlanEventStorageKind::StatusChanged),
        "depends_on" => Some(PlanEventStorageKind::DependsOn),
        _ => None,
    }
}

/// Encodes the closed durable plan-status spelling.
pub(crate) const fn plan_status_to_str(value: PlanStatus) -> &'static str {
    match value {
        PlanStatus::Pending => "pending",
        PlanStatus::InProgress => "in_progress",
        PlanStatus::Completed => "completed",
        PlanStatus::Abandoned => "abandoned",
    }
}

/// Decodes the closed durable plan-status spelling.
pub(crate) fn plan_status_from_str(value: &str) -> Option<PlanStatus> {
    match value {
        "pending" => Some(PlanStatus::Pending),
        "in_progress" => Some(PlanStatus::InProgress),
        "completed" => Some(PlanStatus::Completed),
        "abandoned" => Some(PlanStatus::Abandoned),
        _ => None,
    }
}

pub(crate) fn positive_u64_from_numeric(
    value: Decimal,
) -> Result<u64, PositiveOrdinalMappingError> {
    if !value.fract().is_zero() {
        return Err(PositiveOrdinalMappingError::Fractional);
    }
    if value <= Decimal::ZERO {
        return Err(PositiveOrdinalMappingError::NonPositive);
    }
    u64::try_from(value).map_err(|_| PositiveOrdinalMappingError::OutOfRange)
}

/// Encodes a session identity for a PostgreSQL `uuid` column.
pub fn session_id_to_uuid(value: SessionId) -> Uuid {
    value.into_uuid()
}

/// Decodes a session identity from a PostgreSQL `uuid` column.
pub fn session_id_from_uuid(value: Uuid) -> SessionId {
    SessionId::from_uuid(value)
}

/// Encodes an accepted-input identity for a PostgreSQL `uuid` column.
pub fn accepted_input_id_to_uuid(value: AcceptedInputId) -> Uuid {
    value.into_uuid()
}

/// Decodes an accepted-input identity from a PostgreSQL `uuid` column.
pub fn accepted_input_id_from_uuid(value: Uuid) -> AcceptedInputId {
    AcceptedInputId::from_uuid(value)
}

/// Encodes a turn identity for a PostgreSQL `uuid` column.
pub fn turn_id_to_uuid(value: TurnId) -> Uuid {
    value.into_uuid()
}

/// Decodes a turn identity from a PostgreSQL `uuid` column.
pub fn turn_id_from_uuid(value: Uuid) -> TurnId {
    TurnId::from_uuid(value)
}

/// Encodes a logical tool-request identity for a PostgreSQL `uuid` column.
pub fn tool_request_id_to_uuid(value: ToolRequestId) -> Uuid {
    value.into_uuid()
}

/// Decodes a logical tool-request identity from a PostgreSQL `uuid` column.
pub fn tool_request_id_from_uuid(value: Uuid) -> ToolRequestId {
    ToolRequestId::from_uuid(value)
}

/// Encodes a physical tool-attempt identity for a PostgreSQL `uuid` column.
pub fn tool_attempt_id_to_uuid(value: ToolAttemptId) -> Uuid {
    value.into_uuid()
}

/// Decodes a physical tool-attempt identity from a PostgreSQL `uuid` column.
pub fn tool_attempt_id_from_uuid(value: Uuid) -> ToolAttemptId {
    ToolAttemptId::from_uuid(value)
}

/// Why a PostgreSQL `uuid` value is not a valid durable-command identity.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DurableCommandIdMappingError {
    /// The value is the nil or max sentinel UUID, rejected as an invalid
    /// command identity before canonical command construction
    /// (docs/spec/identity-and-commands.md).
    SentinelUuid,
}

impl fmt::Display for DurableCommandIdMappingError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::SentinelUuid => "durable-command identity must not be the nil or max UUID",
        };
        formatter.write_str(message)
    }
}

impl Error for DurableCommandIdMappingError {}

/// Encodes a durable-command identity for a PostgreSQL `uuid` column.
pub fn durable_command_id_to_uuid(value: DurableCommandId) -> Uuid {
    value.into_uuid()
}

/// Decodes a checked durable-command identity from a PostgreSQL `uuid` column.
///
/// Per docs/spec/identity-and-commands.md, the nil and max UUIDs are invalid
/// sentinel-like command identities and are rejected before a
/// `DurableCommandId` is constructed.
pub fn durable_command_id_from_uuid(
    value: Uuid,
) -> Result<DurableCommandId, DurableCommandIdMappingError> {
    if value == Uuid::nil() || value == Uuid::max() {
        return Err(DurableCommandIdMappingError::SentinelUuid);
    }
    Ok(DurableCommandId::from_uuid(value))
}

/// A stored settings document that cannot reconstruct domain validation
/// evidence. Dynamic document content is deliberately omitted from display.
#[derive(Debug)]
pub(crate) enum StoredModelSettingsError {
    Json(serde_json::Error),
    Invalid(&'static str),
}

impl fmt::Display for StoredModelSettingsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Json(_) => formatter.write_str("stored model settings have an invalid shape"),
            Self::Invalid(field) => write!(formatter, "stored model settings have invalid {field}"),
        }
    }
}

impl Error for StoredModelSettingsError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Json(error) => Some(error),
            Self::Invalid(_) => None,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredModelSettings {
    precedence: StoredModelSettingsPrecedence,
    effective: StoredEffectiveModelSettings,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    reasoning_source: Option<StoredModelSettingSource>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    fast_mode_source: Option<StoredModelSettingSource>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    service_tier_source: Option<StoredModelSettingSource>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    validated_for_selection_id: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredModelSettingsPrecedence {
    per_call: StoredModelSettingsOverlay,
    session: StoredModelSettingsOverlay,
    profile: StoredModelSettingsOverlay,
    global_default: StoredModelSettingsOverlay,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredModelSettingsOverlay {
    reasoning_level: StoredSetting<StoredReasoningLevel>,
    fast_mode: StoredFastModeOverlay,
    service_tier: StoredSetting<StoredServiceTier>,
}

#[derive(Deserialize)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
enum StoredFastModeOverlay {
    Inherit,
    Value(StoredFastMode),
}

#[derive(Deserialize)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
enum StoredSetting<T> {
    Inherit,
    ProviderDefault,
    Value(T),
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum StoredReasoningLevel {
    None,
    Minimal,
    Low,
    Medium,
    High,
    XHigh,
    Max,
    Ultra,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum StoredFastMode {
    Disabled,
    Enabled,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(
    tag = "provider",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
enum StoredServiceTier {
    Anthropic(StoredAnthropicServiceTier),
    OpenAi(StoredOpenAiServiceTier),
    CodexCli(StoredCodexCliServiceTier),
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum StoredAnthropicServiceTier {
    Auto,
    StandardOnly,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum StoredOpenAiServiceTier {
    Auto,
    Default,
    Flex,
    Scale,
    Priority,
    Fast,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum StoredCodexCliServiceTier {
    Default,
    Priority,
    Flex,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredEffectiveModelSettings {
    #[serde(deserialize_with = "deserialize_required_nullable")]
    reasoning_level: Option<StoredReasoningLevel>,
    fast_mode: StoredFastMode,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    service_tier: Option<StoredServiceTier>,
}

fn deserialize_required_nullable<'de, DeserializerT, ValueT>(
    deserializer: DeserializerT,
) -> Result<Option<ValueT>, DeserializerT::Error>
where
    DeserializerT: Deserializer<'de>,
    ValueT: Deserialize<'de>,
{
    Option::<ValueT>::deserialize(deserializer)
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum StoredModelSettingSource {
    PerCall,
    Session,
    Profile,
    GlobalDefault,
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum StoredModelChangeAdjustment {
    ReasoningLevelClamped {
        from: StoredReasoningLevel,
        to: StoredReasoningLevel,
    },
    ReasoningLevelCleared {
        from: StoredReasoningLevel,
    },
    FastModeDisabled,
    ServiceTierCleared {
        from: StoredServiceTier,
    },
}

pub(crate) fn model_settings_to_json(settings: ValidatedModelSettings) -> Value {
    let precedence = settings.precedence();
    let resolved = settings.resolved();
    let effective = settings.effective();
    json!({
        "precedence": {
            "per_call": model_settings_overlay_to_json(precedence.per_call()),
            "session": model_settings_overlay_to_json(precedence.session()),
            "profile": model_settings_overlay_to_json(precedence.profile()),
            "global_default": model_settings_overlay_to_json(precedence.global_default()),
        },
        "effective": {
            "reasoning_level": effective.reasoning_level().map(reasoning_level_to_json),
            "fast_mode": fast_mode_to_json(effective.fast_mode()),
            "service_tier": effective.service_tier().map(service_tier_to_json),
        },
        "reasoning_source": resolved.reasoning_source().map(model_setting_source_to_str),
        "fast_mode_source": resolved.fast_mode_source().map(model_setting_source_to_str),
        "service_tier_source": resolved.service_tier_source().map(model_setting_source_to_str),
        "validated_for_selection_id": settings.validated_for().map(|selection| selection.into_uuid().to_string()),
    })
}

pub(crate) fn model_settings_from_json(
    value: Value,
) -> Result<ValidatedModelSettings, StoredModelSettingsError> {
    let stored: StoredModelSettings =
        serde_json::from_value(value).map_err(StoredModelSettingsError::Json)?;
    let precedence = ModelSettingsPrecedence::new(
        stored.precedence.per_call.into_domain(),
        stored.precedence.session.into_domain(),
        stored.precedence.profile.into_domain(),
        stored.precedence.global_default.into_domain(),
    );
    let effective = EffectiveModelSettings::new(
        stored
            .effective
            .reasoning_level
            .map(StoredReasoningLevel::into_domain),
        stored.effective.fast_mode.into_domain(),
        stored
            .effective
            .service_tier
            .map(StoredServiceTier::into_domain),
    );
    let validated_for = stored
        .validated_for_selection_id
        .map(|value| {
            Uuid::parse_str(&value)
                .map(DirectModelSelection::from_uuid)
                .map_err(|_| StoredModelSettingsError::Invalid("validation selection"))
        })
        .transpose()?;
    ValidatedModelSettings::reconstitute(
        precedence,
        effective,
        stored
            .reasoning_source
            .map(StoredModelSettingSource::into_domain),
        stored
            .fast_mode_source
            .map(StoredModelSettingSource::into_domain),
        stored
            .service_tier_source
            .map(StoredModelSettingSource::into_domain),
        validated_for,
    )
    .ok_or(StoredModelSettingsError::Invalid("precedence correlation"))
}

pub(crate) fn model_settings_overlay_to_json(settings: ModelSettingsOverlay) -> Value {
    json!({
        "reasoning_level": setting_to_json(settings.reasoning_level(), reasoning_level_to_json),
        "fast_mode": fast_mode_overlay_to_json(settings.fast_mode()),
        "service_tier": setting_to_json(settings.service_tier(), service_tier_to_json),
    })
}

pub(crate) fn model_settings_overlay_from_json(
    value: Value,
) -> Result<ModelSettingsOverlay, StoredModelSettingsError> {
    let stored: StoredModelSettingsOverlay =
        serde_json::from_value(value).map_err(StoredModelSettingsError::Json)?;
    Ok(stored.into_domain())
}

pub(crate) fn model_change_adjustments_to_json(adjustments: &[ModelChangeAdjustment]) -> Value {
    Value::Array(
        adjustments
            .iter()
            .map(|adjustment| match *adjustment {
                ModelChangeAdjustment::ReasoningLevelClamped { from, to } => json!({
                    "kind": "reasoning_level_clamped",
                    "from": reasoning_level_to_json(from),
                    "to": reasoning_level_to_json(to),
                }),
                ModelChangeAdjustment::ReasoningLevelCleared { from } => json!({
                    "kind": "reasoning_level_cleared",
                    "from": reasoning_level_to_json(from),
                }),
                ModelChangeAdjustment::FastModeDisabled => {
                    json!({ "kind": "fast_mode_disabled" })
                }
                ModelChangeAdjustment::ServiceTierCleared { from } => json!({
                    "kind": "service_tier_cleared",
                    "from": service_tier_to_json(from),
                }),
            })
            .collect(),
    )
}

pub(crate) fn model_change_adjustments_from_json(
    value: Value,
) -> Result<Vec<ModelChangeAdjustment>, StoredModelSettingsError> {
    let stored: Vec<StoredModelChangeAdjustment> =
        serde_json::from_value(value).map_err(StoredModelSettingsError::Json)?;
    Ok(stored
        .into_iter()
        .map(StoredModelChangeAdjustment::into_domain)
        .collect())
}

fn setting_to_json<T: Copy>(
    setting: SettingOverlay<T>,
    value_to_json: impl FnOnce(T) -> Value,
) -> Value {
    match setting {
        SettingOverlay::Inherit => json!({ "kind": "inherit" }),
        SettingOverlay::ProviderDefault => json!({ "kind": "provider_default" }),
        SettingOverlay::Value(value) => json!({ "kind": "value", "value": value_to_json(value) }),
    }
}

fn fast_mode_overlay_to_json(setting: FastModeOverlay) -> Value {
    match setting {
        FastModeOverlay::Inherit => json!({ "kind": "inherit" }),
        FastModeOverlay::Value(value) => {
            json!({ "kind": "value", "value": fast_mode_to_json(value) })
        }
    }
}

fn reasoning_level_to_json(value: ReasoningLevel) -> Value {
    Value::String(String::from(match value {
        ReasoningLevel::None => "none",
        ReasoningLevel::Minimal => "minimal",
        ReasoningLevel::Low => "low",
        ReasoningLevel::Medium => "medium",
        ReasoningLevel::High => "high",
        ReasoningLevel::XHigh => "x_high",
        ReasoningLevel::Max => "max",
        ReasoningLevel::Ultra => "ultra",
    }))
}

fn fast_mode_to_json(value: FastMode) -> Value {
    Value::String(String::from(match value {
        FastMode::Disabled => "disabled",
        FastMode::Enabled => "enabled",
    }))
}

fn service_tier_to_json(value: ServiceTier) -> Value {
    match value {
        ServiceTier::Anthropic(value) => {
            json!({"provider":"anthropic","value":match value { AnthropicServiceTier::Auto=>"auto", AnthropicServiceTier::StandardOnly=>"standard_only" }})
        }
        ServiceTier::OpenAi(value) => {
            json!({"provider":"open_ai","value":match value { OpenAiServiceTier::Auto=>"auto", OpenAiServiceTier::Default=>"default", OpenAiServiceTier::Flex=>"flex", OpenAiServiceTier::Scale=>"scale", OpenAiServiceTier::Priority=>"priority", OpenAiServiceTier::Fast=>"fast" }})
        }
        ServiceTier::CodexCli(value) => {
            json!({"provider":"codex_cli","value":match value { CodexCliServiceTier::Default=>"default", CodexCliServiceTier::Priority=>"priority", CodexCliServiceTier::Flex=>"flex" }})
        }
    }
}

const fn model_setting_source_to_str(value: ModelSettingSource) -> &'static str {
    match value {
        ModelSettingSource::PerCall => "per_call",
        ModelSettingSource::Session => "session",
        ModelSettingSource::Profile => "profile",
        ModelSettingSource::GlobalDefault => "global_default",
    }
}

impl StoredModelSettingsOverlay {
    fn into_domain(self) -> ModelSettingsOverlay {
        ModelSettingsOverlay::new(
            self.reasoning_level.map(StoredReasoningLevel::into_domain),
            self.fast_mode.into_domain(),
            self.service_tier.map(StoredServiceTier::into_domain),
        )
    }
}

impl StoredFastModeOverlay {
    const fn into_domain(self) -> FastModeOverlay {
        match self {
            Self::Inherit => FastModeOverlay::Inherit,
            Self::Value(value) => FastModeOverlay::Value(value.into_domain()),
        }
    }
}

impl<T> StoredSetting<T> {
    fn map<U>(self, convert: impl FnOnce(T) -> U) -> SettingOverlay<U> {
        match self {
            Self::Inherit => SettingOverlay::Inherit,
            Self::ProviderDefault => SettingOverlay::ProviderDefault,
            Self::Value(value) => SettingOverlay::Value(convert(value)),
        }
    }
}

impl StoredReasoningLevel {
    const fn into_domain(self) -> ReasoningLevel {
        match self {
            Self::None => ReasoningLevel::None,
            Self::Minimal => ReasoningLevel::Minimal,
            Self::Low => ReasoningLevel::Low,
            Self::Medium => ReasoningLevel::Medium,
            Self::High => ReasoningLevel::High,
            Self::XHigh => ReasoningLevel::XHigh,
            Self::Max => ReasoningLevel::Max,
            Self::Ultra => ReasoningLevel::Ultra,
        }
    }
}

impl StoredFastMode {
    const fn into_domain(self) -> FastMode {
        match self {
            Self::Disabled => FastMode::Disabled,
            Self::Enabled => FastMode::Enabled,
        }
    }
}

impl StoredServiceTier {
    const fn into_domain(self) -> ServiceTier {
        match self {
            Self::Anthropic(StoredAnthropicServiceTier::Auto) => {
                ServiceTier::Anthropic(AnthropicServiceTier::Auto)
            }
            Self::Anthropic(StoredAnthropicServiceTier::StandardOnly) => {
                ServiceTier::Anthropic(AnthropicServiceTier::StandardOnly)
            }
            Self::OpenAi(StoredOpenAiServiceTier::Auto) => {
                ServiceTier::OpenAi(OpenAiServiceTier::Auto)
            }
            Self::OpenAi(StoredOpenAiServiceTier::Default) => {
                ServiceTier::OpenAi(OpenAiServiceTier::Default)
            }
            Self::OpenAi(StoredOpenAiServiceTier::Flex) => {
                ServiceTier::OpenAi(OpenAiServiceTier::Flex)
            }
            Self::OpenAi(StoredOpenAiServiceTier::Scale) => {
                ServiceTier::OpenAi(OpenAiServiceTier::Scale)
            }
            Self::OpenAi(StoredOpenAiServiceTier::Priority) => {
                ServiceTier::OpenAi(OpenAiServiceTier::Priority)
            }
            Self::OpenAi(StoredOpenAiServiceTier::Fast) => {
                ServiceTier::OpenAi(OpenAiServiceTier::Fast)
            }
            Self::CodexCli(StoredCodexCliServiceTier::Default) => {
                ServiceTier::CodexCli(CodexCliServiceTier::Default)
            }
            Self::CodexCli(StoredCodexCliServiceTier::Priority) => {
                ServiceTier::CodexCli(CodexCliServiceTier::Priority)
            }
            Self::CodexCli(StoredCodexCliServiceTier::Flex) => {
                ServiceTier::CodexCli(CodexCliServiceTier::Flex)
            }
        }
    }
}

impl StoredModelSettingSource {
    const fn into_domain(self) -> ModelSettingSource {
        match self {
            Self::PerCall => ModelSettingSource::PerCall,
            Self::Session => ModelSettingSource::Session,
            Self::Profile => ModelSettingSource::Profile,
            Self::GlobalDefault => ModelSettingSource::GlobalDefault,
        }
    }
}

impl StoredModelChangeAdjustment {
    const fn into_domain(self) -> ModelChangeAdjustment {
        match self {
            Self::ReasoningLevelClamped { from, to } => {
                ModelChangeAdjustment::ReasoningLevelClamped {
                    from: from.into_domain(),
                    to: to.into_domain(),
                }
            }
            Self::ReasoningLevelCleared { from } => ModelChangeAdjustment::ReasoningLevelCleared {
                from: from.into_domain(),
            },
            Self::FastModeDisabled => ModelChangeAdjustment::FastModeDisabled,
            Self::ServiceTierCleared { from } => ModelChangeAdjustment::ServiceTierCleared {
                from: from.into_domain(),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeSet, str::FromStr};

    use rust_decimal::Decimal;
    use signalbox_application::{RepoWatchPullRequestLifecycle, RepoWatchThreadState};
    use signalbox_domain::{
        AcceptedInputId, BoundChildAction, CheckConclusion, ChecksOutcome,
        DelegateApprovalRecommendation, DelegationMessageDirection, DelegationOutcomeKind,
        DelegationOutcomeReason, DelegationTransitionFailure, DelegationWaitMode,
        DescendantTerminationScope, DirectModelSelection, DurableCommandId, FastMode,
        FastModeOverlay, FastModeSupport, MergeableState, ModelCapabilities, ModelChangeAdjustment,
        ModelSettingsOverlay, ModelSettingsPrecedence, OpenAiServiceTier, ReactionChange,
        ReasoningLevel, RepoWatchEventKindNameV1, ReviewState, RunnerPlacementLossSource,
        RunnerSandboxProfile, ServiceTier, SessionConfigurationDefaultsVersion,
        SessionCreationCause, SessionId, SessionInputPosition, SessionPlacementEventKind,
        SettingOverlay, ToolApprovalPosture, ToolPermissionDefault, TurnId,
    };
    use sqlx::types::Uuid;

    use crate::outbox::DispatchedRunnerState;

    use super::{
        ApprovalJudgeStateStorageKind, ApprovalJudgeTerminalDispositionStorageKind,
        DelegationPolicyStorageKind, DelegationRejectionStorageKind, DelegationUpdateStorageKind,
        DelegationWakeStorageKind, DurableCommandIdMappingError, DurableCommandKind,
        PlanEventStorageKind, PositiveOrdinalMappingError, RepoWatchEvaluationOutcomeStorageKind,
        RepoWatchLifecycleCutoffDispositionStorageKind, RepoWatchObligationSettlementStorageKind,
        RunnerLossPropagationStateStorageKind, SessionCreationCauseStorageKind,
        SessionPlacementRejectionStorageKind, SessionPlacementResultStorageKind,
        StoredModelSettingsError, ToolApprovalDecisionSourceStorageKind,
        ToolAttemptDispositionStorageKind, accepted_input_id_from_uuid, accepted_input_id_to_uuid,
        approval_judge_recommendation_from_str, approval_judge_recommendation_to_str,
        approval_judge_state_from_str, approval_judge_state_to_str,
        approval_judge_terminal_disposition_from_str, approval_judge_terminal_disposition_to_str,
        bound_child_action_from_str, bound_child_action_to_str, defaults_version_from_numeric,
        defaults_version_to_numeric, delegation_message_direction_from_str,
        delegation_message_direction_to_str, delegation_outcome_kind_from_str,
        delegation_outcome_kind_to_str, delegation_outcome_reason_from_str,
        delegation_outcome_reason_to_str, delegation_policy_kind_from_str,
        delegation_policy_kind_to_str, delegation_rejection_kind_from_str,
        delegation_rejection_kind_to_str, delegation_transition_failure_from_str,
        delegation_transition_failure_to_str, delegation_update_kind_from_str,
        delegation_update_kind_to_str, delegation_wait_mode_from_str, delegation_wait_mode_to_str,
        delegation_wake_subject_from_str, delegation_wake_subject_to_str,
        dispatched_runner_state_from_str, dispatched_runner_state_to_str,
        durable_command_id_from_uuid, durable_command_id_to_uuid, durable_command_kind_from_str,
        durable_command_kind_to_str, input_position_from_numeric, input_position_to_numeric,
        model_change_adjustments_from_json, model_change_adjustments_to_json,
        model_settings_from_json, model_settings_overlay_from_json, model_settings_to_json,
        plan_event_kind_from_str, plan_event_kind_to_str, repo_watch_check_conclusion_from_str,
        repo_watch_check_conclusion_to_str, repo_watch_checks_outcome_from_str,
        repo_watch_checks_outcome_to_str, repo_watch_evaluation_outcome_from_str,
        repo_watch_evaluation_outcome_to_str, repo_watch_event_kind_from_str,
        repo_watch_event_kind_to_str, repo_watch_lifecycle_cutoff_disposition_from_str,
        repo_watch_lifecycle_cutoff_disposition_to_str, repo_watch_mergeable_state_from_str,
        repo_watch_mergeable_state_to_str, repo_watch_obligation_settlement_from_str,
        repo_watch_obligation_settlement_to_str, repo_watch_pull_request_lifecycle_from_str,
        repo_watch_pull_request_lifecycle_to_str, repo_watch_reaction_change_from_str,
        repo_watch_reaction_change_to_str, repo_watch_review_state_from_str,
        repo_watch_review_state_to_str, repo_watch_thread_state_from_str,
        repo_watch_thread_state_to_str, runner_loss_propagation_state_from_str,
        runner_loss_propagation_state_to_str, runner_placement_loss_source_from_str,
        runner_placement_loss_source_to_str, runner_sandbox_from_str, runner_sandbox_to_str,
        session_creation_cause_from_str, session_creation_cause_to_str, session_id_from_uuid,
        session_id_to_uuid, session_placement_event_kind_from_str,
        session_placement_event_kind_to_str, session_placement_rejection_from_str,
        session_placement_result_kind_from_str, session_placement_result_kind_to_str,
        tool_approval_decision_source_from_str, tool_approval_decision_source_to_str,
        tool_approval_posture_from_str, tool_approval_posture_to_str,
        tool_attempt_disposition_from_str, tool_attempt_disposition_to_str,
        tool_permission_default_from_str, tool_permission_default_to_str, turn_id_from_uuid,
        turn_id_to_uuid,
    };

    #[test]
    fn runner_loss_propagation_state_mapping_is_closed() {
        assert_eq!(
            runner_loss_propagation_state_from_str(runner_loss_propagation_state_to_str(
                RunnerLossPropagationStateStorageKind::Pending,
            )),
            Some(RunnerLossPropagationStateStorageKind::Pending)
        );
        assert_eq!(
            runner_loss_propagation_state_from_str(runner_loss_propagation_state_to_str(
                RunnerLossPropagationStateStorageKind::Completed,
            )),
            Some(RunnerLossPropagationStateStorageKind::Completed)
        );
        assert_eq!(runner_loss_propagation_state_from_str("unknown"), None);
    }

    #[test]
    fn repository_watch_target_closed_mappings_are_closed() {
        assert_eq!(
            repo_watch_evaluation_outcome_from_str(repo_watch_evaluation_outcome_to_str(
                RepoWatchEvaluationOutcomeStorageKind::TargetClosed,
            )),
            Some(RepoWatchEvaluationOutcomeStorageKind::TargetClosed)
        );
        assert_eq!(repo_watch_evaluation_outcome_from_str("unknown"), None);
        assert_eq!(
            repo_watch_obligation_settlement_from_str(repo_watch_obligation_settlement_to_str(
                RepoWatchObligationSettlementStorageKind::TargetClosed,
            )),
            Some(RepoWatchObligationSettlementStorageKind::TargetClosed)
        );
        assert_eq!(repo_watch_obligation_settlement_from_str("unknown"), None);
        assert_eq!(
            repo_watch_lifecycle_cutoff_disposition_from_str(
                repo_watch_lifecycle_cutoff_disposition_to_str(
                    RepoWatchLifecycleCutoffDispositionStorageKind::Terminal,
                ),
            ),
            Some(RepoWatchLifecycleCutoffDispositionStorageKind::Terminal)
        );
        assert_eq!(
            repo_watch_lifecycle_cutoff_disposition_from_str(
                repo_watch_lifecycle_cutoff_disposition_to_str(
                    RepoWatchLifecycleCutoffDispositionStorageKind::Reopened,
                ),
            ),
            Some(RepoWatchLifecycleCutoffDispositionStorageKind::Reopened)
        );
        assert_eq!(
            repo_watch_lifecycle_cutoff_disposition_from_str("unknown"),
            None
        );
    }

    #[test]
    fn tool_attempt_dispositions_pin_each_storage_spelling() {
        assert_eq!(
            tool_attempt_disposition_to_str(ToolAttemptDispositionStorageKind::Completed),
            "completed"
        );
        assert_eq!(
            tool_attempt_disposition_from_str("completed"),
            Some(ToolAttemptDispositionStorageKind::Completed)
        );
        assert_eq!(
            tool_attempt_disposition_to_str(ToolAttemptDispositionStorageKind::KnownFailed),
            "known_failed"
        );
        assert_eq!(
            tool_attempt_disposition_from_str("known_failed"),
            Some(ToolAttemptDispositionStorageKind::KnownFailed)
        );
        assert_eq!(
            tool_attempt_disposition_to_str(ToolAttemptDispositionStorageKind::AwaitingChild),
            "awaiting_child"
        );
        assert_eq!(
            tool_attempt_disposition_from_str("awaiting_child"),
            Some(ToolAttemptDispositionStorageKind::AwaitingChild)
        );
        assert_eq!(
            tool_attempt_disposition_to_str(ToolAttemptDispositionStorageKind::Ambiguous),
            "ambiguous"
        );
        assert_eq!(
            tool_attempt_disposition_from_str("ambiguous"),
            Some(ToolAttemptDispositionStorageKind::Ambiguous)
        );
        assert_eq!(
            tool_attempt_disposition_from_str(UNKNOWN_DISCRIMINATOR),
            None
        );
    }
    use crate::approval_judge::FailedApprovalJudgeDisposition;

    const OUT_OF_U64_RANGE: &str = "18446744073709551616";
    const DELEGATED_REQUEST_ID: u128 = 1;
    const UNKNOWN_DISCRIMINATOR: &str = "outside-closed-set";

    #[test]
    fn delegation_policy_kind_mapping_is_closed() {
        assert_eq!(
            delegation_policy_kind_from_str(delegation_policy_kind_to_str(
                DelegationPolicyStorageKind::Background,
            )),
            Some(DelegationPolicyStorageKind::Background)
        );
        assert_eq!(
            delegation_policy_kind_from_str(delegation_policy_kind_to_str(
                DelegationPolicyStorageKind::Bound,
            )),
            Some(DelegationPolicyStorageKind::Bound)
        );
        assert_eq!(delegation_policy_kind_from_str(UNKNOWN_DISCRIMINATOR), None);
    }

    /// Every delegation update spelling survives a round trip, and only those.
    ///
    /// The outbox dispatcher decodes this column for every committed
    /// delegation update, and a spelling it cannot read stalls the singleton
    /// cursor for every session, so the closed set is pinned here.
    #[test]
    fn delegation_update_kind_mapping_is_closed() {
        assert_eq!(
            delegation_update_kind_from_str(delegation_update_kind_to_str(
                DelegationUpdateStorageKind::ChildSpawned,
            )),
            Some(DelegationUpdateStorageKind::ChildSpawned)
        );
        assert_eq!(
            delegation_update_kind_from_str(delegation_update_kind_to_str(
                DelegationUpdateStorageKind::ChildWaiting,
            )),
            Some(DelegationUpdateStorageKind::ChildWaiting)
        );
        assert_eq!(
            delegation_update_kind_from_str(delegation_update_kind_to_str(
                DelegationUpdateStorageKind::ChildLifecycleDisposition,
            )),
            Some(DelegationUpdateStorageKind::ChildLifecycleDisposition)
        );
        assert_eq!(
            delegation_update_kind_from_str(delegation_update_kind_to_str(
                DelegationUpdateStorageKind::ChildResult,
            )),
            Some(DelegationUpdateStorageKind::ChildResult)
        );
        assert_eq!(
            delegation_update_kind_from_str(delegation_update_kind_to_str(
                DelegationUpdateStorageKind::SessionMessage,
            )),
            Some(DelegationUpdateStorageKind::SessionMessage)
        );
        assert_eq!(delegation_update_kind_from_str(UNKNOWN_DISCRIMINATOR), None);
    }

    /// Every delegation wake subject survives a round trip, and only those.
    #[test]
    fn delegation_wake_subject_mapping_is_closed() {
        assert_eq!(
            delegation_wake_subject_from_str(delegation_wake_subject_to_str(
                DelegationWakeStorageKind::Result,
            )),
            Some(DelegationWakeStorageKind::Result)
        );
        assert_eq!(
            delegation_wake_subject_from_str(delegation_wake_subject_to_str(
                DelegationWakeStorageKind::Message,
            )),
            Some(DelegationWakeStorageKind::Message)
        );
        assert_eq!(
            delegation_wake_subject_from_str(UNKNOWN_DISCRIMINATOR),
            None
        );
    }

    #[test]
    fn delegation_bound_action_mapping_is_closed() {
        assert_eq!(
            bound_child_action_from_str(bound_child_action_to_str(BoundChildAction::KeepRunning,)),
            Some(BoundChildAction::KeepRunning)
        );
        assert_eq!(
            bound_child_action_from_str(bound_child_action_to_str(BoundChildAction::Stop)),
            Some(BoundChildAction::Stop)
        );
        assert_eq!(
            bound_child_action_from_str(bound_child_action_to_str(BoundChildAction::Cancel)),
            Some(BoundChildAction::Cancel)
        );
        assert_eq!(bound_child_action_from_str(UNKNOWN_DISCRIMINATOR), None);
    }

    #[test]
    fn delegation_wait_mode_mapping_is_closed() {
        assert_eq!(
            delegation_wait_mode_from_str(delegation_wait_mode_to_str(
                DelegationWaitMode::Foreground,
            )),
            Some(DelegationWaitMode::Foreground)
        );
        assert_eq!(
            delegation_wait_mode_from_str(delegation_wait_mode_to_str(
                DelegationWaitMode::Background,
            )),
            Some(DelegationWaitMode::Background)
        );
        assert_eq!(delegation_wait_mode_from_str(UNKNOWN_DISCRIMINATOR), None);
    }

    #[test]
    fn delegation_message_direction_mapping_is_closed() {
        assert_eq!(
            delegation_message_direction_from_str(delegation_message_direction_to_str(
                DelegationMessageDirection::ParentToChild,
            )),
            Some(DelegationMessageDirection::ParentToChild)
        );
        assert_eq!(
            delegation_message_direction_from_str(delegation_message_direction_to_str(
                DelegationMessageDirection::ChildToParent,
            )),
            Some(DelegationMessageDirection::ChildToParent)
        );
        assert_eq!(
            delegation_message_direction_from_str(UNKNOWN_DISCRIMINATOR),
            None
        );
    }

    #[test]
    fn delegation_rejection_kind_mapping_is_closed() {
        assert_eq!(
            delegation_rejection_kind_from_str(delegation_rejection_kind_to_str(
                DelegationRejectionStorageKind::RelationshipNotFound,
            )),
            Some(DelegationRejectionStorageKind::RelationshipNotFound)
        );
        assert_eq!(
            delegation_rejection_kind_from_str(delegation_rejection_kind_to_str(
                DelegationRejectionStorageKind::MessageIdentityCollision,
            )),
            Some(DelegationRejectionStorageKind::MessageIdentityCollision)
        );
        assert_eq!(
            delegation_rejection_kind_from_str(delegation_rejection_kind_to_str(
                DelegationRejectionStorageKind::DeliverySequenceExhausted,
            )),
            Some(DelegationRejectionStorageKind::DeliverySequenceExhausted)
        );
        assert_eq!(
            delegation_rejection_kind_from_str(delegation_rejection_kind_to_str(
                DelegationRejectionStorageKind::Transition,
            )),
            Some(DelegationRejectionStorageKind::Transition)
        );
        assert_eq!(
            delegation_rejection_kind_from_str(UNKNOWN_DISCRIMINATOR),
            None
        );
    }

    #[test]
    fn delegation_transition_failure_mapping_is_closed() {
        assert_eq!(
            delegation_transition_failure_from_str(delegation_transition_failure_to_str(
                DelegationTransitionFailure::SameSession,
            )),
            Some(DelegationTransitionFailure::SameSession)
        );
        assert_eq!(
            delegation_transition_failure_from_str(delegation_transition_failure_to_str(
                DelegationTransitionFailure::AlreadyTerminal,
            )),
            Some(DelegationTransitionFailure::AlreadyTerminal)
        );
        assert_eq!(
            delegation_transition_failure_from_str(delegation_transition_failure_to_str(
                DelegationTransitionFailure::MissingSpawnEvent,
            )),
            Some(DelegationTransitionFailure::MissingSpawnEvent)
        );
        assert_eq!(
            delegation_transition_failure_from_str(delegation_transition_failure_to_str(
                DelegationTransitionFailure::InvalidProvenance,
            )),
            Some(DelegationTransitionFailure::InvalidProvenance)
        );
        assert_eq!(
            delegation_transition_failure_from_str(delegation_transition_failure_to_str(
                DelegationTransitionFailure::DescendantsNotSelected,
            )),
            Some(DelegationTransitionFailure::DescendantsNotSelected)
        );
        assert_eq!(
            delegation_transition_failure_from_str(delegation_transition_failure_to_str(
                DelegationTransitionFailure::DuplicateMessageIdentity,
            )),
            Some(DelegationTransitionFailure::DuplicateMessageIdentity)
        );
        assert_eq!(
            delegation_transition_failure_from_str(delegation_transition_failure_to_str(
                DelegationTransitionFailure::ConflictingMessageReplay,
            )),
            Some(DelegationTransitionFailure::ConflictingMessageReplay)
        );
        assert_eq!(
            delegation_transition_failure_from_str(delegation_transition_failure_to_str(
                DelegationTransitionFailure::DuplicateOutcomeAuthority,
            )),
            Some(DelegationTransitionFailure::DuplicateOutcomeAuthority)
        );
        assert_eq!(
            delegation_transition_failure_from_str(delegation_transition_failure_to_str(
                DelegationTransitionFailure::OutcomeReasonMismatch,
            )),
            Some(DelegationTransitionFailure::OutcomeReasonMismatch)
        );
        assert_eq!(
            delegation_transition_failure_from_str(delegation_transition_failure_to_str(
                DelegationTransitionFailure::EventOrdinalExhausted,
            )),
            Some(DelegationTransitionFailure::EventOrdinalExhausted)
        );
        assert_eq!(
            delegation_transition_failure_from_str(UNKNOWN_DISCRIMINATOR),
            None
        );
    }

    #[test]
    fn delegation_outcome_kind_mapping_is_closed() {
        assert_eq!(
            delegation_outcome_kind_from_str(delegation_outcome_kind_to_str(
                DelegationOutcomeKind::ResultReturned,
            )),
            Some(DelegationOutcomeKind::ResultReturned)
        );
        assert_eq!(
            delegation_outcome_kind_from_str(delegation_outcome_kind_to_str(
                DelegationOutcomeKind::ChildFailed,
            )),
            Some(DelegationOutcomeKind::ChildFailed)
        );
        assert_eq!(
            delegation_outcome_kind_from_str(delegation_outcome_kind_to_str(
                DelegationOutcomeKind::ChildStopped,
            )),
            Some(DelegationOutcomeKind::ChildStopped)
        );
        assert_eq!(
            delegation_outcome_kind_from_str(delegation_outcome_kind_to_str(
                DelegationOutcomeKind::ChildCancelled,
            )),
            Some(DelegationOutcomeKind::ChildCancelled)
        );
        assert_eq!(
            delegation_outcome_kind_from_str(delegation_outcome_kind_to_str(
                DelegationOutcomeKind::ContinueRunning,
            )),
            Some(DelegationOutcomeKind::ContinueRunning)
        );
        assert_eq!(
            delegation_outcome_kind_from_str(delegation_outcome_kind_to_str(
                DelegationOutcomeKind::AlreadyTerminal,
            )),
            Some(DelegationOutcomeKind::AlreadyTerminal)
        );
        assert_eq!(
            delegation_outcome_kind_from_str(UNKNOWN_DISCRIMINATOR),
            None
        );
    }

    #[test]
    fn delegation_outcome_reason_mapping_is_closed() {
        assert_eq!(
            delegation_outcome_reason_from_str(
                delegation_outcome_reason_to_str(DelegationOutcomeReason::ChildCompleted)
                    .expect("child completion is stored"),
            ),
            Some(DelegationOutcomeReason::ChildCompleted)
        );
        assert_eq!(
            delegation_outcome_reason_from_str(
                delegation_outcome_reason_to_str(DelegationOutcomeReason::ChildExecutionFailed)
                    .expect("child failure is stored"),
            ),
            Some(DelegationOutcomeReason::ChildExecutionFailed)
        );
        assert_eq!(
            delegation_outcome_reason_from_str(
                delegation_outcome_reason_to_str(DelegationOutcomeReason::ChildResultUnavailable)
                    .expect("unavailable child result is stored"),
            ),
            Some(DelegationOutcomeReason::ChildResultUnavailable)
        );
        assert_eq!(
            delegation_outcome_reason_from_str(
                delegation_outcome_reason_to_str(DelegationOutcomeReason::ChildCancelled)
                    .expect("child cancellation is stored"),
            ),
            Some(DelegationOutcomeReason::ChildCancelled)
        );
        assert_eq!(
            delegation_outcome_reason_from_str(
                delegation_outcome_reason_to_str(DelegationOutcomeReason::ParentStopped {
                    scope: DescendantTerminationScope::ParentAndDescendants,
                })
                .expect("descendant stop is stored"),
            ),
            Some(DelegationOutcomeReason::ParentStopped {
                scope: DescendantTerminationScope::ParentAndDescendants,
            })
        );
        assert_eq!(
            delegation_outcome_reason_from_str(
                delegation_outcome_reason_to_str(DelegationOutcomeReason::ParentCancelled {
                    scope: DescendantTerminationScope::ParentAndDescendants,
                })
                .expect("descendant cancellation is stored"),
            ),
            Some(DelegationOutcomeReason::ParentCancelled {
                scope: DescendantTerminationScope::ParentAndDescendants,
            })
        );
        assert_eq!(
            delegation_outcome_reason_to_str(DelegationOutcomeReason::ParentStopped {
                scope: DescendantTerminationScope::ParentAlone,
            }),
            None
        );
        assert_eq!(
            delegation_outcome_reason_to_str(DelegationOutcomeReason::ParentCancelled {
                scope: DescendantTerminationScope::ParentAlone,
            }),
            None
        );
        assert_eq!(
            delegation_outcome_reason_from_str(UNKNOWN_DISCRIMINATOR),
            None
        );
    }

    #[test]
    fn session_creation_cause_mapping_is_closed() {
        assert_eq!(
            session_creation_cause_from_str(session_creation_cause_to_str(
                &SessionCreationCause::UserInitiated,
            )),
            Some(SessionCreationCauseStorageKind::UserInitiated)
        );
        assert_eq!(
            session_creation_cause_from_str(session_creation_cause_to_str(
                &SessionCreationCause::Delegated {
                    spawning_request: signalbox_domain::ToolRequestId::from_uuid(Uuid::from_u128(
                        DELEGATED_REQUEST_ID,
                    )),
                },
            )),
            Some(SessionCreationCauseStorageKind::Delegated)
        );
        assert_eq!(session_creation_cause_from_str("unknown"), None);
    }

    #[test]
    fn tool_approval_decision_source_mapping_is_closed() {
        assert_eq!(
            tool_approval_decision_source_from_str(tool_approval_decision_source_to_str(
                ToolApprovalDecisionSourceStorageKind::UserCommand,
            )),
            Some(ToolApprovalDecisionSourceStorageKind::UserCommand)
        );
        assert_eq!(
            tool_approval_decision_source_from_str(tool_approval_decision_source_to_str(
                ToolApprovalDecisionSourceStorageKind::PolicyAuto,
            )),
            Some(ToolApprovalDecisionSourceStorageKind::PolicyAuto)
        );
        assert_eq!(
            tool_approval_decision_source_from_str(tool_approval_decision_source_to_str(
                ToolApprovalDecisionSourceStorageKind::SessionBlanket,
            )),
            Some(ToolApprovalDecisionSourceStorageKind::SessionBlanket)
        );
        assert_eq!(
            tool_approval_decision_source_from_str(tool_approval_decision_source_to_str(
                ToolApprovalDecisionSourceStorageKind::Delegate,
            )),
            Some(ToolApprovalDecisionSourceStorageKind::Delegate)
        );
        assert_eq!(
            tool_approval_decision_source_from_str(UNKNOWN_DISCRIMINATOR),
            None
        );
    }

    /// Pins the stored spellings literally rather than only round-tripping
    /// them. A rename that moved the encoder and the decoder together would
    /// round-trip perfectly and still fail against every row already written,
    /// so the round-trip tests above cannot catch it on their own. These are
    /// the exact strings the closed `CHECK` constraints admit.
    #[test]
    fn storage_spells_the_human_principal_user() {
        assert_eq!(
            session_creation_cause_to_str(&SessionCreationCause::UserInitiated),
            "user_initiated"
        );
        assert_eq!(
            tool_approval_decision_source_to_str(
                ToolApprovalDecisionSourceStorageKind::UserCommand
            ),
            "user_command"
        );
    }

    #[test]
    fn repository_watch_event_kind_mapping_is_closed() {
        assert_eq!(
            repo_watch_event_kind_from_str(repo_watch_event_kind_to_str(
                RepoWatchEventKindNameV1::PullRequestOpened
            )),
            Some(RepoWatchEventKindNameV1::PullRequestOpened)
        );
        assert_eq!(
            repo_watch_event_kind_from_str(repo_watch_event_kind_to_str(
                RepoWatchEventKindNameV1::PullRequestClosed
            )),
            Some(RepoWatchEventKindNameV1::PullRequestClosed)
        );
        assert_eq!(
            repo_watch_event_kind_from_str(repo_watch_event_kind_to_str(
                RepoWatchEventKindNameV1::PullRequestMerged
            )),
            Some(RepoWatchEventKindNameV1::PullRequestMerged)
        );
        assert_eq!(
            repo_watch_event_kind_from_str(repo_watch_event_kind_to_str(
                RepoWatchEventKindNameV1::HeadChanged
            )),
            Some(RepoWatchEventKindNameV1::HeadChanged)
        );
        assert_eq!(
            repo_watch_event_kind_from_str(repo_watch_event_kind_to_str(
                RepoWatchEventKindNameV1::MergeableStateChanged
            )),
            Some(RepoWatchEventKindNameV1::MergeableStateChanged)
        );
        assert_eq!(
            repo_watch_event_kind_from_str(repo_watch_event_kind_to_str(
                RepoWatchEventKindNameV1::ChecksCompleted
            )),
            Some(RepoWatchEventKindNameV1::ChecksCompleted)
        );
        assert_eq!(
            repo_watch_event_kind_from_str(repo_watch_event_kind_to_str(
                RepoWatchEventKindNameV1::CheckRunCompleted
            )),
            Some(RepoWatchEventKindNameV1::CheckRunCompleted)
        );
        assert_eq!(
            repo_watch_event_kind_from_str(repo_watch_event_kind_to_str(
                RepoWatchEventKindNameV1::BranchWorkflowRunCompleted
            )),
            Some(RepoWatchEventKindNameV1::BranchWorkflowRunCompleted)
        );
        assert_eq!(
            repo_watch_event_kind_from_str(repo_watch_event_kind_to_str(
                RepoWatchEventKindNameV1::ReviewSubmitted
            )),
            Some(RepoWatchEventKindNameV1::ReviewSubmitted)
        );
        assert_eq!(
            repo_watch_event_kind_from_str(repo_watch_event_kind_to_str(
                RepoWatchEventKindNameV1::ThreadOpened
            )),
            Some(RepoWatchEventKindNameV1::ThreadOpened)
        );
        assert_eq!(
            repo_watch_event_kind_from_str(repo_watch_event_kind_to_str(
                RepoWatchEventKindNameV1::ThreadResolved
            )),
            Some(RepoWatchEventKindNameV1::ThreadResolved)
        );
        assert_eq!(
            repo_watch_event_kind_from_str(repo_watch_event_kind_to_str(
                RepoWatchEventKindNameV1::Labeled
            )),
            Some(RepoWatchEventKindNameV1::Labeled)
        );
        assert_eq!(
            repo_watch_event_kind_from_str(repo_watch_event_kind_to_str(
                RepoWatchEventKindNameV1::Unlabeled
            )),
            Some(RepoWatchEventKindNameV1::Unlabeled)
        );
        assert_eq!(
            repo_watch_event_kind_from_str(repo_watch_event_kind_to_str(
                RepoWatchEventKindNameV1::BaseAdvanced
            )),
            Some(RepoWatchEventKindNameV1::BaseAdvanced)
        );
        assert_eq!(
            repo_watch_event_kind_from_str(repo_watch_event_kind_to_str(
                RepoWatchEventKindNameV1::ReactionChanged
            )),
            Some(RepoWatchEventKindNameV1::ReactionChanged)
        );
        assert_eq!(repo_watch_event_kind_from_str(UNKNOWN_DISCRIMINATOR), None);
    }

    #[test]
    fn repository_watch_payload_mapping_is_closed() {
        assert_eq!(
            repo_watch_pull_request_lifecycle_from_str(repo_watch_pull_request_lifecycle_to_str(
                RepoWatchPullRequestLifecycle::Merged
            )),
            Some(RepoWatchPullRequestLifecycle::Merged)
        );
        assert_eq!(
            repo_watch_mergeable_state_from_str(repo_watch_mergeable_state_to_str(
                MergeableState::Conflicting
            )),
            Some(MergeableState::Conflicting)
        );
        assert_eq!(
            repo_watch_checks_outcome_from_str(repo_watch_checks_outcome_to_str(
                ChecksOutcome::Failure
            )),
            Some(ChecksOutcome::Failure)
        );
        assert_eq!(
            repo_watch_check_conclusion_from_str(repo_watch_check_conclusion_to_str(
                CheckConclusion::StartupFailure
            )),
            Some(CheckConclusion::StartupFailure)
        );
        assert_eq!(
            repo_watch_review_state_from_str(repo_watch_review_state_to_str(
                ReviewState::ChangesRequested
            )),
            Some(ReviewState::ChangesRequested)
        );
        assert_eq!(
            repo_watch_thread_state_from_str(repo_watch_thread_state_to_str(
                RepoWatchThreadState::Resolved
            )),
            Some(RepoWatchThreadState::Resolved)
        );
        assert_eq!(
            repo_watch_reaction_change_from_str(repo_watch_reaction_change_to_str(
                ReactionChange::Removed
            )),
            Some(ReactionChange::Removed)
        );
        assert_eq!(
            repo_watch_pull_request_lifecycle_from_str(UNKNOWN_DISCRIMINATOR),
            None
        );
        assert_eq!(
            repo_watch_mergeable_state_from_str(UNKNOWN_DISCRIMINATOR),
            None
        );
        assert_eq!(
            repo_watch_checks_outcome_from_str(UNKNOWN_DISCRIMINATOR),
            None
        );
        assert_eq!(
            repo_watch_check_conclusion_from_str(UNKNOWN_DISCRIMINATOR),
            None
        );
        assert_eq!(
            repo_watch_review_state_from_str(UNKNOWN_DISCRIMINATOR),
            None
        );
        assert_eq!(
            repo_watch_thread_state_from_str(UNKNOWN_DISCRIMINATOR),
            None
        );
        assert_eq!(
            repo_watch_reaction_change_from_str(UNKNOWN_DISCRIMINATOR),
            None
        );
    }

    /// INV-003 / INV-053: the JSONB mapping preserves complete model-settings
    /// precedence, effective value, source evidence, and validation identity.
    #[test]
    fn inv003_inv053_model_settings_json_round_trips_complete_evidence() {
        let selection = DirectModelSelection::from_uuid(Uuid::from_u128(0x51));
        let capabilities = ModelCapabilities::new(
            BTreeSet::from([ReasoningLevel::High]),
            FastModeSupport::RequestControl,
            BTreeSet::new(),
        );
        let precedence = ModelSettingsPrecedence::new(
            ModelSettingsOverlay::new(
                SettingOverlay::Value(ReasoningLevel::High),
                FastModeOverlay::Value(FastMode::Enabled),
                SettingOverlay::ProviderDefault,
            ),
            ModelSettingsOverlay::inherit_all(),
            ModelSettingsOverlay::inherit_all(),
            ModelSettingsOverlay::inherit_all(),
        );
        let settings = capabilities
            .validate_precedence(selection, precedence)
            .expect("the fixture capability admits every explicit value");

        let decoded = model_settings_from_json(model_settings_to_json(settings))
            .expect("the encoded complete document reconstitutes");

        assert_eq!(decoded, settings);
    }

    /// INV-003: unknown stored settings members fail closed instead of being
    /// silently ignored during reconstitution.
    #[test]
    fn inv003_model_settings_json_rejects_unknown_members() {
        let mut encoded =
            model_settings_to_json(signalbox_domain::ValidatedModelSettings::provider_defaults());
        encoded
            .as_object_mut()
            .expect("the fixture encoder produces an object")
            .insert(String::from("unknown_member"), serde_json::Value::Null);

        let error = model_settings_from_json(encoded)
            .expect_err("an unknown settings member must fail closed");

        assert!(matches!(error, StoredModelSettingsError::Json(_)));

        let nested_tier = serde_json::json!({
            "reasoning_level": {"kind": "inherit"},
            "fast_mode": {"kind": "inherit"},
            "service_tier": {
                "kind": "value",
                "value": {"provider": "open_ai", "value": "flex", "extra": true}
            }
        });
        let nested_error = model_settings_overlay_from_json(nested_tier)
            .expect_err("an unknown nested service-tier member must fail closed");

        assert!(matches!(nested_error, StoredModelSettingsError::Json(_)));
    }

    /// INV-003: every nullable member remains required durable evidence, so
    /// omission cannot normalize a truncated document into provider defaults.
    #[test]
    fn inv003_model_settings_json_rejects_missing_nullable_members() {
        let mut missing_source =
            model_settings_to_json(signalbox_domain::ValidatedModelSettings::provider_defaults());
        missing_source
            .as_object_mut()
            .expect("the fixture encoder produces an object")
            .remove("reasoning_source");

        let source_error = model_settings_from_json(missing_source)
            .expect_err("an omitted nullable source must fail closed");

        assert!(matches!(source_error, StoredModelSettingsError::Json(_)));

        let mut missing_effective =
            model_settings_to_json(signalbox_domain::ValidatedModelSettings::provider_defaults());
        missing_effective
            .get_mut("effective")
            .expect("the fixture contains effective settings")
            .as_object_mut()
            .expect("effective settings are an object")
            .remove("service_tier");

        let effective_error = model_settings_from_json(missing_effective)
            .expect_err("an omitted nullable effective value must fail closed");

        assert!(matches!(effective_error, StoredModelSettingsError::Json(_)));
    }

    /// INV-003: fast mode has no provider-default state in the domain, so a
    /// durable spelling that invents one fails closed.
    #[test]
    fn inv003_model_settings_overlay_rejects_provider_default_fast_mode() {
        let encoded = serde_json::json!({
            "reasoning_level": {"kind": "inherit"},
            "fast_mode": {"kind": "provider_default"},
            "service_tier": {"kind": "inherit"}
        });

        let error = model_settings_overlay_from_json(encoded)
            .expect_err("provider-default fast mode must fail closed");

        assert!(matches!(error, StoredModelSettingsError::Json(_)));
    }

    #[test]
    fn model_change_adjustment_json_round_trips_every_variant() {
        let adjustments = vec![
            ModelChangeAdjustment::ReasoningLevelClamped {
                from: ReasoningLevel::High,
                to: ReasoningLevel::Low,
            },
            ModelChangeAdjustment::ReasoningLevelCleared {
                from: ReasoningLevel::Medium,
            },
            ModelChangeAdjustment::FastModeDisabled,
            ModelChangeAdjustment::ServiceTierCleared {
                from: ServiceTier::OpenAi(OpenAiServiceTier::Priority),
            },
        ];

        let decoded =
            model_change_adjustments_from_json(model_change_adjustments_to_json(&adjustments))
                .expect("the closed adjustment document decodes");

        assert_eq!(decoded, adjustments);
    }

    #[test]
    fn model_change_adjustment_json_rejects_unknown_variants() {
        let encoded = serde_json::json!([{"kind": "unknown"}]);

        let error = model_change_adjustments_from_json(encoded)
            .expect_err("an unknown adjustment variant must fail closed");

        assert!(matches!(error, StoredModelSettingsError::Json(_)));
    }

    #[test]
    fn approval_judge_discriminator_mappings_are_closed() {
        assert_eq!(
            approval_judge_state_from_str(approval_judge_state_to_str(
                ApprovalJudgeStateStorageKind::Prepared,
            )),
            Some(ApprovalJudgeStateStorageKind::Prepared)
        );
        assert_eq!(
            approval_judge_state_from_str(approval_judge_state_to_str(
                ApprovalJudgeStateStorageKind::InFlight,
            )),
            Some(ApprovalJudgeStateStorageKind::InFlight)
        );
        assert_eq!(
            approval_judge_state_from_str(approval_judge_state_to_str(
                ApprovalJudgeStateStorageKind::Terminal,
            )),
            Some(ApprovalJudgeStateStorageKind::Terminal)
        );
        assert_eq!(approval_judge_state_from_str("unknown"), None);
        assert_eq!(
            approval_judge_terminal_disposition_from_str(
                approval_judge_terminal_disposition_to_str(
                    ApprovalJudgeTerminalDispositionStorageKind::Completed,
                ),
            ),
            Some(ApprovalJudgeTerminalDispositionStorageKind::Completed)
        );
        assert_eq!(
            approval_judge_terminal_disposition_from_str(
                approval_judge_terminal_disposition_to_str(
                    ApprovalJudgeTerminalDispositionStorageKind::Failed(
                        FailedApprovalJudgeDisposition::KnownFailed,
                    ),
                ),
            ),
            Some(ApprovalJudgeTerminalDispositionStorageKind::Failed(
                FailedApprovalJudgeDisposition::KnownFailed,
            ))
        );
        assert_eq!(
            approval_judge_terminal_disposition_from_str(
                approval_judge_terminal_disposition_to_str(
                    ApprovalJudgeTerminalDispositionStorageKind::Failed(
                        FailedApprovalJudgeDisposition::Refused,
                    ),
                ),
            ),
            Some(ApprovalJudgeTerminalDispositionStorageKind::Failed(
                FailedApprovalJudgeDisposition::Refused,
            ))
        );
        assert_eq!(
            approval_judge_terminal_disposition_from_str(
                approval_judge_terminal_disposition_to_str(
                    ApprovalJudgeTerminalDispositionStorageKind::Failed(
                        FailedApprovalJudgeDisposition::Cancelled,
                    ),
                ),
            ),
            Some(ApprovalJudgeTerminalDispositionStorageKind::Failed(
                FailedApprovalJudgeDisposition::Cancelled,
            ))
        );
        assert_eq!(
            approval_judge_terminal_disposition_from_str(
                approval_judge_terminal_disposition_to_str(
                    ApprovalJudgeTerminalDispositionStorageKind::Failed(
                        FailedApprovalJudgeDisposition::Ambiguous,
                    ),
                ),
            ),
            Some(ApprovalJudgeTerminalDispositionStorageKind::Failed(
                FailedApprovalJudgeDisposition::Ambiguous,
            ))
        );
        assert_eq!(
            approval_judge_terminal_disposition_from_str("unknown"),
            None
        );
        assert_eq!(
            approval_judge_recommendation_from_str(approval_judge_recommendation_to_str(
                DelegateApprovalRecommendation::Approve,
            )),
            Some(DelegateApprovalRecommendation::Approve)
        );
        assert_eq!(
            approval_judge_recommendation_from_str(approval_judge_recommendation_to_str(
                DelegateApprovalRecommendation::Deny,
            )),
            Some(DelegateApprovalRecommendation::Deny)
        );
        assert_eq!(
            approval_judge_recommendation_from_str(approval_judge_recommendation_to_str(
                DelegateApprovalRecommendation::EscalateToHuman,
            )),
            Some(DelegateApprovalRecommendation::EscalateToHuman)
        );
        assert_eq!(approval_judge_recommendation_from_str("unknown"), None);
    }

    #[test]
    fn plan_event_kind_mapping_is_closed() {
        assert_eq!(
            plan_event_kind_from_str(plan_event_kind_to_str(PlanEventStorageKind::Created)),
            Some(PlanEventStorageKind::Created)
        );
        assert_eq!(
            plan_event_kind_from_str(plan_event_kind_to_str(PlanEventStorageKind::TextRevised)),
            Some(PlanEventStorageKind::TextRevised)
        );
        assert_eq!(
            plan_event_kind_from_str(plan_event_kind_to_str(PlanEventStorageKind::StatusChanged)),
            Some(PlanEventStorageKind::StatusChanged)
        );
        assert_eq!(
            plan_event_kind_from_str(plan_event_kind_to_str(PlanEventStorageKind::DependsOn)),
            Some(PlanEventStorageKind::DependsOn)
        );
        assert_eq!(plan_event_kind_from_str("unknown"), None);
    }

    #[test]
    fn compact_session_command_kind_mapping_is_closed() {
        assert_eq!(
            durable_command_kind_to_str(DurableCommandKind::CompactSession),
            "compact_session"
        );
        assert_eq!(
            durable_command_kind_from_str("compact_session"),
            Some(DurableCommandKind::CompactSession)
        );
        assert_eq!(durable_command_kind_from_str("unknown"), None);
    }

    #[test]
    fn session_placement_event_kind_mapping_is_closed() {
        assert_eq!(
            session_placement_event_kind_from_str(session_placement_event_kind_to_str(
                SessionPlacementEventKind::Created,
            )),
            Some(SessionPlacementEventKind::Created)
        );
        assert_eq!(
            session_placement_event_kind_from_str(session_placement_event_kind_to_str(
                SessionPlacementEventKind::Updated,
            )),
            Some(SessionPlacementEventKind::Updated)
        );
        assert_eq!(session_placement_event_kind_from_str("unknown"), None);
    }

    #[test]
    fn session_placement_result_kind_mapping_is_closed() {
        assert_eq!(
            session_placement_result_kind_from_str(session_placement_result_kind_to_str(
                SessionPlacementResultStorageKind::Applied,
            )),
            Some(SessionPlacementResultStorageKind::Applied)
        );
        assert_eq!(
            session_placement_result_kind_from_str(session_placement_result_kind_to_str(
                SessionPlacementResultStorageKind::Rejected,
            )),
            Some(SessionPlacementResultStorageKind::Rejected)
        );
        assert_eq!(session_placement_result_kind_from_str("unknown"), None);
    }

    #[test]
    fn session_placement_rejection_kind_mapping_is_closed() {
        assert_eq!(
            session_placement_rejection_from_str("session_not_found"),
            Some(SessionPlacementRejectionStorageKind::SessionNotFound)
        );
        assert_eq!(
            session_placement_rejection_from_str("current_version_mismatch"),
            Some(SessionPlacementRejectionStorageKind::CurrentVersionMismatch)
        );
        assert_eq!(
            session_placement_rejection_from_str("version_exhausted"),
            Some(SessionPlacementRejectionStorageKind::VersionExhausted)
        );
        assert_eq!(session_placement_rejection_from_str("unknown"), None);
    }

    #[test]
    fn always_confirm_permission_mapping_is_closed() {
        let encoded = tool_permission_default_to_str(ToolPermissionDefault::AlwaysConfirm);
        let decoded = tool_permission_default_from_str(encoded)
            .expect("the additive permission encoding is canonical");

        assert_eq!(encoded, "always_confirm");
        assert_eq!(decoded, ToolPermissionDefault::AlwaysConfirm);
        assert_eq!(tool_permission_default_from_str("unknown"), None);
    }

    #[test]
    fn runner_placement_loss_source_mapping_is_closed() {
        assert_eq!(
            runner_placement_loss_source_from_str(runner_placement_loss_source_to_str(
                RunnerPlacementLossSource::Connection,
            )),
            Some(RunnerPlacementLossSource::Connection),
        );
        assert_eq!(
            runner_placement_loss_source_from_str(runner_placement_loss_source_to_str(
                RunnerPlacementLossSource::Registration,
            )),
            Some(RunnerPlacementLossSource::Registration),
        );
        assert_eq!(runner_placement_loss_source_from_str("unknown"), None);
    }

    #[test]
    fn runner_sandbox_mapping_is_closed() {
        assert_eq!(
            runner_sandbox_from_str(runner_sandbox_to_str(RunnerSandboxProfile::Ambient)),
            Some(RunnerSandboxProfile::Ambient),
        );
        assert_eq!(
            runner_sandbox_from_str(runner_sandbox_to_str(
                RunnerSandboxProfile::WorkspaceRestricted,
            )),
            Some(RunnerSandboxProfile::WorkspaceRestricted),
        );
        assert_eq!(runner_sandbox_from_str("unknown"), None);
    }

    #[test]
    fn dispatched_runner_state_mapping_is_closed() {
        assert_eq!(
            dispatched_runner_state_from_str(dispatched_runner_state_to_str(
                DispatchedRunnerState::Pinned,
            )),
            Some(DispatchedRunnerState::Pinned),
        );
        assert_eq!(
            dispatched_runner_state_from_str(dispatched_runner_state_to_str(
                DispatchedRunnerState::Suspect,
            )),
            Some(DispatchedRunnerState::Suspect),
        );
        assert_eq!(
            dispatched_runner_state_from_str(dispatched_runner_state_to_str(
                DispatchedRunnerState::Connected,
            )),
            Some(DispatchedRunnerState::Connected),
        );
        assert_eq!(
            dispatched_runner_state_from_str(dispatched_runner_state_to_str(
                DispatchedRunnerState::RunnerLostBeforePin,
            )),
            Some(DispatchedRunnerState::RunnerLostBeforePin),
        );
        assert_eq!(
            dispatched_runner_state_from_str(dispatched_runner_state_to_str(
                DispatchedRunnerState::RunnerLost,
            )),
            Some(DispatchedRunnerState::RunnerLost),
        );
        assert_eq!(
            dispatched_runner_state_from_str(dispatched_runner_state_to_str(
                DispatchedRunnerState::Replaced,
            )),
            Some(DispatchedRunnerState::Replaced),
        );
        assert_eq!(
            dispatched_runner_state_from_str(dispatched_runner_state_to_str(
                DispatchedRunnerState::WorkingDirectoryChanged,
            )),
            Some(DispatchedRunnerState::WorkingDirectoryChanged),
        );
        assert_eq!(
            dispatched_runner_state_from_str(dispatched_runner_state_to_str(
                DispatchedRunnerState::Abandoned,
            )),
            Some(DispatchedRunnerState::Abandoned),
        );
        assert_eq!(dispatched_runner_state_from_str("unknown"), None);
    }

    #[test]
    fn tool_approval_posture_mapping_round_trips() {
        assert_eq!(
            tool_approval_posture_from_str(tool_approval_posture_to_str(
                ToolApprovalPosture::Delegated,
            )),
            Some(ToolApprovalPosture::Delegated)
        );
    }

    #[test]
    fn unknown_tool_approval_posture_is_rejected() {
        const UNKNOWN_POSTURE: &str = "unknown";

        assert_eq!(tool_approval_posture_from_str(UNKNOWN_POSTURE), None);
    }

    /// INV-002: PostgreSQL numeric values are decoded and checked before a
    /// domain defaults version exists.
    #[test]
    fn inv002_defaults_version_numeric_boundary() {
        assert_eq!(
            defaults_version_from_numeric(Decimal::ZERO),
            Err(PositiveOrdinalMappingError::NonPositive)
        );
        assert_eq!(
            defaults_version_from_numeric(Decimal::NEGATIVE_ONE),
            Err(PositiveOrdinalMappingError::NonPositive)
        );
        assert_eq!(
            defaults_version_from_numeric(Decimal::new(15, 1)),
            Err(PositiveOrdinalMappingError::Fractional)
        );
        assert_eq!(
            defaults_version_from_numeric(Decimal::ONE),
            Ok(SessionConfigurationDefaultsVersion::first())
        );

        let maximum = Decimal::from(u64::MAX);
        let mapped = defaults_version_from_numeric(maximum).expect("maximum must round-trip");
        assert_eq!(mapped.as_u64(), u64::MAX);
        assert_eq!(defaults_version_to_numeric(mapped), maximum);

        let out_of_range = Decimal::from_str(OUT_OF_U64_RANGE).expect("representable decimal");
        assert_eq!(
            defaults_version_from_numeric(out_of_range),
            Err(PositiveOrdinalMappingError::OutOfRange)
        );
    }

    /// INV-002: PostgreSQL numeric values are decoded and checked before a
    /// domain input position exists.
    #[test]
    fn inv002_input_position_numeric_boundary() {
        assert_eq!(
            input_position_from_numeric(Decimal::ZERO),
            Err(PositiveOrdinalMappingError::NonPositive)
        );
        assert_eq!(
            input_position_from_numeric(Decimal::NEGATIVE_ONE),
            Err(PositiveOrdinalMappingError::NonPositive)
        );
        assert_eq!(
            input_position_from_numeric(Decimal::new(15, 1)),
            Err(PositiveOrdinalMappingError::Fractional)
        );
        assert_eq!(
            input_position_from_numeric(Decimal::ONE),
            Ok(SessionInputPosition::first())
        );

        let maximum = Decimal::from(u64::MAX);
        let mapped = input_position_from_numeric(maximum).expect("maximum must round-trip");
        assert_eq!(mapped.as_u64(), u64::MAX);
        assert_eq!(input_position_to_numeric(mapped), maximum);

        let out_of_range = Decimal::from_str(OUT_OF_U64_RANGE).expect("representable decimal");
        assert_eq!(
            input_position_from_numeric(out_of_range),
            Err(PositiveOrdinalMappingError::OutOfRange)
        );
    }

    /// INV-002: each CreateSession identity kind crosses the persistence
    /// boundary through its own typed conversion.
    #[test]
    fn inv002_create_session_identity_mappings_remain_kind_specific() {
        let session_uuid = Uuid::from_u128(1);
        let command_uuid = Uuid::from_u128(2);

        let session = session_id_from_uuid(session_uuid);
        let command = durable_command_id_from_uuid(command_uuid).expect("non-sentinel command");

        assert_eq!(session, SessionId::from_uuid(session_uuid));
        assert_eq!(command, DurableCommandId::from_uuid(command_uuid));
        assert_eq!(session_id_to_uuid(session), session_uuid);
        assert_eq!(durable_command_id_to_uuid(command), command_uuid);
    }

    /// INV-002: accepted-input and future-turn identities cross the SQL
    /// boundary through distinct mappings even though both use native UUIDs.
    #[test]
    fn inv002_submit_input_identity_mappings_remain_kind_specific() {
        let accepted_uuid = Uuid::from_u128(3);
        let turn_uuid = Uuid::from_u128(4);

        let accepted = accepted_input_id_from_uuid(accepted_uuid);
        let turn = turn_id_from_uuid(turn_uuid);

        assert_eq!(accepted, AcceptedInputId::from_uuid(accepted_uuid));
        assert_eq!(turn, TurnId::from_uuid(turn_uuid));
        assert_eq!(accepted_input_id_to_uuid(accepted), accepted_uuid);
        assert_eq!(turn_id_to_uuid(turn), turn_uuid);
    }

    /// INV-002: the durable-command boundary rejects the nil and max sentinel
    /// UUIDs rather than admitting them as command identities.
    #[test]
    fn inv002_durable_command_mapping_rejects_sentinel_uuids() {
        assert_eq!(
            durable_command_id_from_uuid(Uuid::nil()),
            Err(DurableCommandIdMappingError::SentinelUuid)
        );
        assert_eq!(
            durable_command_id_from_uuid(Uuid::max()),
            Err(DurableCommandIdMappingError::SentinelUuid)
        );

        let valid = Uuid::from_u128(7);
        assert_eq!(
            durable_command_id_from_uuid(valid),
            Ok(DurableCommandId::from_uuid(valid))
        );
    }
}
