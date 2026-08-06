//! Explicit mappings between domain values and PostgreSQL-compatible values.

use std::{error::Error, fmt};

use rust_decimal::Decimal;
use serde::{Deserialize, Deserializer};
use serde_json::{Value, json};
use signalbox_application::{RepoWatchPullRequestLifecycle, RepoWatchThreadState};
use signalbox_domain::{
    AcceptedInputId, AnthropicServiceTier, CheckConclusion, ChecksOutcome, CodexCliServiceTier,
    DangerousToolAutoApproval, DelegateApprovalRecommendation, DirectModelSelection,
    DurableCommandId, EffectiveModelSettings, FastMode, FastModeOverlay, GoalBlockedReasonKind,
    GoalCommandRejection, GoalEventKind, GoalModelBlockedReasonKind, GoalUserAction,
    MergeableState, ModelChangeAdjustment, ModelSettingSource, ModelSettingsOverlay,
    ModelSettingsPrecedence, OpenAiServiceTier, ReactionChange, ReactionSubject, ReasoningLevel,
    RepoWatchEventKindNameV1, ReviewState, ServiceTier, SessionConfigurationDefaultsVersion,
    SessionId, SessionInputPosition, SessionPlacementEventKind, SettingOverlay,
    ToolApprovalPosture, ToolAttemptId, ToolPermissionDefault, ToolRequestId, TurnId,
    UpdateSessionPlacementRejectionKind, ValidatedModelSettings,
};
use signalbox_tools_plan::PlanStatus;
use sqlx::types::Uuid;

use crate::approval_judge::FailedApprovalJudgeDisposition;

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
}

pub(crate) const fn tool_approval_decision_source_to_str(
    value: ToolApprovalDecisionSourceStorageKind,
) -> &'static str {
    match value {
        ToolApprovalDecisionSourceStorageKind::UserCommand => "owner_command",
        ToolApprovalDecisionSourceStorageKind::PolicyAuto => "policy_auto",
        ToolApprovalDecisionSourceStorageKind::SessionBlanket => "session_blanket",
        ToolApprovalDecisionSourceStorageKind::Delegate => "delegate",
    }
}

pub(crate) fn tool_approval_decision_source_from_str(
    value: &str,
) -> Option<ToolApprovalDecisionSourceStorageKind> {
    match value {
        "owner_command" => Some(ToolApprovalDecisionSourceStorageKind::UserCommand),
        "policy_auto" => Some(ToolApprovalDecisionSourceStorageKind::PolicyAuto),
        "session_blanket" => Some(ToolApprovalDecisionSourceStorageKind::SessionBlanket),
        "delegate" => Some(ToolApprovalDecisionSourceStorageKind::Delegate),
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
        DurableCommandKind::ReviewWorkflow => "review_workflow",
        DurableCommandKind::ReviewOrchestration => "review_orchestration",
        DurableCommandKind::CompactSession => "compact_session",
        DurableCommandKind::Goal => "goal",
        DurableCommandKind::UpdateSessionPlacement => "update_session_placement",
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
        "review_workflow" => Some(DurableCommandKind::ReviewWorkflow),
        "review_orchestration" => Some(DurableCommandKind::ReviewOrchestration),
        "compact_session" => Some(DurableCommandKind::CompactSession),
        "goal" => Some(DurableCommandKind::Goal),
        "update_session_placement" => Some(DurableCommandKind::UpdateSessionPlacement),
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
        AcceptedInputId, CheckConclusion, ChecksOutcome, DelegateApprovalRecommendation,
        DirectModelSelection, DurableCommandId, FastMode, FastModeOverlay, FastModeSupport,
        MergeableState, ModelCapabilities, ModelChangeAdjustment, ModelSettingsOverlay,
        ModelSettingsPrecedence, OpenAiServiceTier, ReactionChange, ReasoningLevel,
        RepoWatchEventKindNameV1, ReviewState, ServiceTier, SessionConfigurationDefaultsVersion,
        SessionId, SessionInputPosition, SessionPlacementEventKind, SettingOverlay,
        ToolApprovalPosture, ToolPermissionDefault, TurnId,
    };
    use sqlx::types::Uuid;

    use super::{
        ApprovalJudgeStateStorageKind, ApprovalJudgeTerminalDispositionStorageKind,
        DurableCommandIdMappingError, DurableCommandKind, PlanEventStorageKind,
        PositiveOrdinalMappingError, SessionPlacementRejectionStorageKind,
        SessionPlacementResultStorageKind, StoredModelSettingsError, accepted_input_id_from_uuid,
        accepted_input_id_to_uuid, approval_judge_recommendation_from_str,
        approval_judge_recommendation_to_str, approval_judge_state_from_str,
        approval_judge_state_to_str, approval_judge_terminal_disposition_from_str,
        approval_judge_terminal_disposition_to_str, defaults_version_from_numeric,
        defaults_version_to_numeric, durable_command_id_from_uuid, durable_command_id_to_uuid,
        durable_command_kind_from_str, durable_command_kind_to_str, input_position_from_numeric,
        input_position_to_numeric, model_change_adjustments_from_json,
        model_change_adjustments_to_json, model_settings_from_json,
        model_settings_overlay_from_json, model_settings_to_json, plan_event_kind_from_str,
        plan_event_kind_to_str, repo_watch_check_conclusion_from_str,
        repo_watch_check_conclusion_to_str, repo_watch_checks_outcome_from_str,
        repo_watch_checks_outcome_to_str, repo_watch_event_kind_from_str,
        repo_watch_event_kind_to_str, repo_watch_mergeable_state_from_str,
        repo_watch_mergeable_state_to_str, repo_watch_pull_request_lifecycle_from_str,
        repo_watch_pull_request_lifecycle_to_str, repo_watch_reaction_change_from_str,
        repo_watch_reaction_change_to_str, repo_watch_review_state_from_str,
        repo_watch_review_state_to_str, repo_watch_thread_state_from_str,
        repo_watch_thread_state_to_str, session_id_from_uuid, session_id_to_uuid,
        session_placement_event_kind_from_str, session_placement_event_kind_to_str,
        session_placement_rejection_from_str, session_placement_result_kind_from_str,
        session_placement_result_kind_to_str, tool_approval_posture_from_str,
        tool_approval_posture_to_str, tool_permission_default_from_str,
        tool_permission_default_to_str, turn_id_from_uuid, turn_id_to_uuid,
    };
    use crate::approval_judge::FailedApprovalJudgeDisposition;

    const OUT_OF_U64_RANGE: &str = "18446744073709551616";
    const UNKNOWN_DISCRIMINATOR: &str = "outside-closed-set";

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
