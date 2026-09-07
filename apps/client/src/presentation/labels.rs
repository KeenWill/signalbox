use super::*;

pub(super) const fn operator_status_lifecycle_state_label(
    state: OperatorStatusLifecycleState,
) -> &'static str {
    match state {
        OperatorStatusLifecycleState::Created => "created",
        OperatorStatusLifecycleState::Dispatched => "dispatched",
        OperatorStatusLifecycleState::Active => "active",
        OperatorStatusLifecycleState::Waiting => "waiting",
        OperatorStatusLifecycleState::Recovering => "recovering",
        OperatorStatusLifecycleState::Blocked => "blocked",
        OperatorStatusLifecycleState::Parked => "parked",
    }
}

/// Renders one metric as its exact counts beside its parts-per-million rate.
///
/// An empty population prints no rate rather than a zero.
pub(super) fn rate_label(counts: RateCounts) -> String {
    let RateCounts {
        numerator,
        denominator,
    } = counts;
    if denominator == 0 {
        return format!("{numerator}/0");
    }
    let ppm = u128::from(numerator) * 1_000_000 / u128::from(denominator);
    format!("{numerator}/{denominator}@{ppm}ppm")
}

pub(super) fn duration_label(seconds: u64) -> String {
    let days = seconds / 86_400;
    let hours = seconds % 86_400 / 3_600;
    let minutes = seconds % 3_600 / 60;
    let seconds = seconds % 60;
    if days > 0 {
        format!("{days}d{hours}h{minutes}m{seconds}s")
    } else if hours > 0 {
        format!("{hours}h{minutes}m{seconds}s")
    } else if minutes > 0 {
        format!("{minutes}m{seconds}s")
    } else {
        format!("{seconds}s")
    }
}

pub(super) const fn imported_speaker_label(source: ImportedSourceSpeaker) -> &'static str {
    match source {
        ImportedSourceSpeaker::NotAttested {} => "speaker_unattested",
        ImportedSourceSpeaker::AttestedAbsent {} => "speaker_absent",
        ImportedSourceSpeaker::Attested {
            speaker: ImportedSpeaker::User,
        } => "user",
        ImportedSourceSpeaker::Attested {
            speaker: ImportedSpeaker::Assistant,
        } => "assistant",
    }
}

/// Names the speaker attestation as a standalone field value, where the
/// transcript's `imported_<suffix>` composition does not supply the noun.
pub(super) const fn imported_speaker_attestation_label(
    source: ImportedSourceSpeaker,
) -> &'static str {
    match source {
        ImportedSourceSpeaker::NotAttested {} => "unattested",
        ImportedSourceSpeaker::AttestedAbsent {} => "absent",
        ImportedSourceSpeaker::Attested {
            speaker: ImportedSpeaker::User,
        } => "user",
        ImportedSourceSpeaker::Attested {
            speaker: ImportedSpeaker::Assistant,
        } => "assistant",
    }
}

pub(super) const fn imported_content_kind(kind: ImportedContentKind) -> &'static str {
    match kind {
        ImportedContentKind::SourceEvent => "source_event",
        ImportedContentKind::SourceMessageBlock => "source_message_block",
        ImportedContentKind::Text => "text",
        ImportedContentKind::ToolCall => "tool_call",
        ImportedContentKind::ToolResult => "tool_result",
        ImportedContentKind::Thinking => "thinking",
        ImportedContentKind::RedactedThinking => "redacted_thinking",
        ImportedContentKind::Document => "document",
        ImportedContentKind::MessageContentAbsent => "message_content_absent",
    }
}

pub(super) fn model_call_state(state: ModelCallState) -> &'static str {
    match state {
        ModelCallState::Prepared {} => "prepared",
        ModelCallState::InFlight {} => "in_flight",
        ModelCallState::CancellationRequested {} => "cancellation_requested",
        ModelCallState::Terminal { disposition } => match disposition {
            ModelCallDisposition::Completed => "terminal:completed",
            ModelCallDisposition::KnownFailed => "terminal:known_failed",
            ModelCallDisposition::Refused => "terminal:refused",
            ModelCallDisposition::Cancelled => "terminal:cancelled",
            ModelCallDisposition::Ambiguous => "terminal:ambiguous",
        },
    }
}

pub(super) const fn runner_sandbox_profile(profile: RunnerSandboxProfile) -> &'static str {
    match profile {
        RunnerSandboxProfile::Ambient => "ambient",
        RunnerSandboxProfile::WorkspaceRestricted => "workspace_restricted",
    }
}

pub(super) const fn runner_connection_health(health: RunnerConnectionHealth) -> &'static str {
    match health {
        RunnerConnectionHealth::Connected => "connected",
        RunnerConnectionHealth::Suspect => "suspect",
        RunnerConnectionHealth::Shutdown => "shutdown",
        RunnerConnectionHealth::Lost => "lost",
    }
}

pub(super) const fn runner_projection_state(state: RunnerProjectionState) -> &'static str {
    match state {
        RunnerProjectionState::Unpinned => "unpinned",
        RunnerProjectionState::Pinned => "pinned",
        RunnerProjectionState::RunnerLostBeforePin => "runner_lost_before_pin",
        RunnerProjectionState::RunnerLost => "runner_lost",
        RunnerProjectionState::RunnerAbandoned => "runner_abandoned",
    }
}

pub(super) const fn runner_state_transition_state(
    state: RunnerStateTransitionState,
) -> &'static str {
    match state {
        RunnerStateTransitionState::Pinned => "pinned",
        RunnerStateTransitionState::Suspect => "suspect",
        RunnerStateTransitionState::Connected => "connected",
        RunnerStateTransitionState::RunnerLostBeforePin => "runner_lost_before_pin",
        RunnerStateTransitionState::RunnerLost => "runner_lost",
        RunnerStateTransitionState::Replaced => "replaced",
        RunnerStateTransitionState::WorkingDirectoryChanged => "working_directory_changed",
        RunnerStateTransitionState::Abandoned => "abandoned",
    }
}

pub(super) const fn current_model_call_state(state: CurrentModelCallState) -> &'static str {
    match state {
        CurrentModelCallState::Prepared {} => "prepared",
        CurrentModelCallState::InFlight {} => "in_flight",
        CurrentModelCallState::CancellationRequested {} => "cancellation_requested",
    }
}

pub(super) const fn failed_model_call_disposition(
    disposition: FailedModelCallDisposition,
) -> &'static str {
    match disposition {
        FailedModelCallDisposition::KnownFailed => "known_failed",
        FailedModelCallDisposition::Cancelled => "cancelled",
    }
}

pub(super) const fn failed_model_call_cause(cause: FailedModelCallCause) -> &'static str {
    match cause {
        FailedModelCallCause::CredentialRejected => "credential_rejected",
        FailedModelCallCause::AttachmentTooLarge => "attachment_too_large",
        FailedModelCallCause::AttachmentMissing => "attachment_missing",
        FailedModelCallCause::AttachmentCorrupt => "attachment_corrupt",
        FailedModelCallCause::PermissionDenied => "permission_denied",
        FailedModelCallCause::InvalidRequest => "invalid_request",
        FailedModelCallCause::TargetNotFound => "target_not_found",
        FailedModelCallCause::RequestTooLarge => "request_too_large",
        FailedModelCallCause::RateLimited => "rate_limited",
        FailedModelCallCause::QuotaExhausted => "quota_exhausted",
        FailedModelCallCause::Overloaded => "overloaded",
        FailedModelCallCause::ProviderInternal => "provider_internal",
        FailedModelCallCause::Unrecognized => "unrecognized",
    }
}

pub(super) const fn bound_child_action(action: BoundChildAction) -> &'static str {
    match action {
        BoundChildAction::KeepRunning => "keep_running",
        BoundChildAction::Stop => "stop",
        BoundChildAction::Cancel => "cancel",
    }
}

pub(super) const fn delegation_outcome(outcome: DelegationOutcome) -> &'static str {
    match outcome {
        DelegationOutcome::Returned => "returned",
        DelegationOutcome::Failed => "failed",
        DelegationOutcome::Stopped => "stopped",
        DelegationOutcome::Cancelled => "cancelled",
        DelegationOutcome::ContinueRunning => "continue_running",
        DelegationOutcome::AlreadyTerminal => "already_terminal",
    }
}

pub(super) const fn delegation_reason(reason: DelegationReason) -> &'static str {
    match reason {
        DelegationReason::ChildCompleted => "child_completed",
        DelegationReason::ChildExecutionFailed => "child_execution_failed",
        DelegationReason::ChildResultUnavailable => "child_result_unavailable",
        DelegationReason::ChildCancelled => "child_cancelled",
        DelegationReason::ParentStopped => "parent_stopped",
        DelegationReason::ParentCancelled => "parent_cancelled",
    }
}

pub(super) const fn delegation_wait_mode(mode: DelegationWaitMode) -> &'static str {
    match mode {
        DelegationWaitMode::Foreground => "foreground",
        DelegationWaitMode::Background => "background",
    }
}

pub(super) const fn delegation_message_direction(
    direction: DelegationMessageDirection,
) -> &'static str {
    match direction {
        DelegationMessageDirection::ParentToChild => "parent_to_child",
        DelegationMessageDirection::ChildToParent => "child_to_parent",
    }
}

pub(super) fn delegation_provenance(provenance: &DelegationProvenance) -> String {
    match provenance {
        DelegationProvenance::ChildTurn {
            child_session_id,
            child_turn_id,
        } => format!("child_turn:{child_session_id}:{child_turn_id}"),
        DelegationProvenance::ParentTurnCommand {
            parent_session_id,
            parent_turn_id,
            command_id,
            descendant_scope,
        } => format!(
            "parent_turn_command:{parent_session_id}:{parent_turn_id}:{command_id}:{}",
            descendant_scope_label(*descendant_scope)
        ),
        DelegationProvenance::ParentGoalCommand {
            parent_session_id,
            goal_generation,
            command_id,
            descendant_scope,
        } => format!(
            "parent_goal_command:{parent_session_id}:{}:{command_id}:{}",
            goal_generation.value(),
            descendant_scope_label(*descendant_scope)
        ),
        DelegationProvenance::ParentLifecycleCommand {
            parent_session_id,
            command_id,
            descendant_scope,
        } => format!(
            "parent_lifecycle_command:{parent_session_id}:{command_id}:{}",
            descendant_scope_label(*descendant_scope)
        ),
    }
}

const fn descendant_scope_label(scope: DescendantTerminationScope) -> &'static str {
    match scope {
        DescendantTerminationScope::ParentAlone => "parent_alone",
        DescendantTerminationScope::ParentAndDescendants => "parent_and_descendants",
    }
}

pub(super) const fn goal_blocked_reason_label(reason: GoalBlockedReason) -> &'static str {
    match reason {
        GoalBlockedReason::UserInputRequired => "user_input_required",
        GoalBlockedReason::ExternalChangeRequired => "external_change_required",
        GoalBlockedReason::AuthorizationRequired => "authorization_required",
        GoalBlockedReason::ExecutionFailure => "execution_failure",
        GoalBlockedReason::FinishCheckFailed => "finish_check_failed",
    }
}

pub(super) const fn session_closure_outcome_label(outcome: SessionClosureOutcome) -> &'static str {
    match outcome {
        SessionClosureOutcome::FailedRetryable => "failed_retryable",
        SessionClosureOutcome::FailedStructural => "failed_structural",
        SessionClosureOutcome::FailedUnknown => "failed_unknown",
        SessionClosureOutcome::Superseded => "superseded",
        SessionClosureOutcome::Stopped => "stopped",
        SessionClosureOutcome::Abandoned => "abandoned",
        SessionClosureOutcome::Retired => "retired",
    }
}

pub(super) const fn lifecycle_actor_label(actor: LifecycleActorClass) -> &'static str {
    match actor {
        LifecycleActorClass::Core => "core",
        LifecycleActorClass::Operator => "operator",
        LifecycleActorClass::Module => "module",
        LifecycleActorClass::Watchdog => "watchdog",
    }
}

pub(super) const fn review_orchestration_state_label(
    state: ReviewOrchestrationState,
) -> &'static str {
    match state {
        ReviewOrchestrationState::AwaitingImport => "awaiting_import",
        ReviewOrchestrationState::ImportIncomplete => "import_incomplete",
        ReviewOrchestrationState::AwaitingConcerns => "awaiting_concerns",
        ReviewOrchestrationState::FanoutIncomplete => "fanout_incomplete",
        ReviewOrchestrationState::AwaitingJudgment => "awaiting_judgment",
        ReviewOrchestrationState::AwaitingJudgmentEffects => "awaiting_judgment_effects",
        ReviewOrchestrationState::JudgmentIncomplete => "judgment_incomplete",
        ReviewOrchestrationState::AwaitingRepair => "awaiting_repair",
        ReviewOrchestrationState::RepairIncomplete => "repair_incomplete",
        ReviewOrchestrationState::AwaitingPublication => "awaiting_publication",
        ReviewOrchestrationState::PublicationIncomplete => "publication_incomplete",
        ReviewOrchestrationState::Complete => "complete",
    }
}

pub(super) const fn review_orchestration_concern_status_label(
    status: ReviewOrchestrationConcernStatus,
) -> &'static str {
    match status {
        ReviewOrchestrationConcernStatus::Pending => "pending",
        ReviewOrchestrationConcernStatus::Succeeded => "succeeded",
        ReviewOrchestrationConcernStatus::Failed => "failed",
        ReviewOrchestrationConcernStatus::Blocked => "blocked",
        ReviewOrchestrationConcernStatus::Cancelled => "cancelled",
        ReviewOrchestrationConcernStatus::Superseded => "superseded",
    }
}

pub(super) const fn review_workflow_label(workflow: ReviewWorkflow) -> &'static str {
    match workflow {
        ReviewWorkflow::ImportExternalContext => "import_external_context",
        ReviewWorkflow::ReadOnlyReview => "read_only_review",
        ReviewWorkflow::JudgeFindings => "judge_findings",
        ReviewWorkflow::DedupeFindings => "dedupe_findings",
        ReviewWorkflow::PublishReview => "publish_review",
        ReviewWorkflow::FixFindings => "fix_findings",
        ReviewWorkflow::PropagateStack => "propagate_stack",
    }
}

pub(super) const fn review_run_state_label(state: ReviewRunLifecycle) -> &'static str {
    match state {
        ReviewRunLifecycle::Queued => "queued",
        ReviewRunLifecycle::Running => "running",
        ReviewRunLifecycle::Succeeded => "succeeded",
        ReviewRunLifecycle::Failed => "failed",
        ReviewRunLifecycle::Blocked => "blocked",
        ReviewRunLifecycle::Cancelled => "cancelled",
    }
}

pub(super) const fn review_pass_kind_label(kind: ReviewPassKind) -> &'static str {
    match kind {
        ReviewPassKind::ImportExternalContext => "import_external_context",
        ReviewPassKind::ReadOnlyReview => "read_only_review",
        ReviewPassKind::Judge => "judge",
        ReviewPassKind::Dedupe => "dedupe",
        ReviewPassKind::Publish => "publish",
        ReviewPassKind::Fix => "fix",
        ReviewPassKind::PropagateStack => "propagate_stack",
    }
}

pub(super) const fn review_pass_state_label(state: ReviewPassLifecycle) -> &'static str {
    match state {
        ReviewPassLifecycle::Queued => "queued",
        ReviewPassLifecycle::Running => "running",
        ReviewPassLifecycle::Succeeded => "succeeded",
        ReviewPassLifecycle::Failed => "failed",
        ReviewPassLifecycle::Blocked => "blocked",
        ReviewPassLifecycle::Cancelled => "cancelled",
    }
}

pub(super) const fn review_diff_side_label(side: ReviewDiffSide) -> &'static str {
    match side {
        ReviewDiffSide::Left => "left",
        ReviewDiffSide::Right => "right",
    }
}

pub(super) const fn review_severity_label(severity: ReviewSeverity) -> &'static str {
    match severity {
        ReviewSeverity::Info => "info",
        ReviewSeverity::Low => "low",
        ReviewSeverity::Medium => "medium",
        ReviewSeverity::High => "high",
        ReviewSeverity::Critical => "critical",
    }
}

pub(super) const fn review_finding_status_label(status: ReviewFindingStatus) -> &'static str {
    match status {
        ReviewFindingStatus::Open => "open",
        ReviewFindingStatus::Accepted => "accepted",
        ReviewFindingStatus::Rejected => "rejected",
        ReviewFindingStatus::Duplicate => "duplicate",
        ReviewFindingStatus::Superseded => "superseded",
        ReviewFindingStatus::Stale => "stale",
        ReviewFindingStatus::Posted => "posted",
        ReviewFindingStatus::Fixed => "fixed",
        ReviewFindingStatus::BlockedWithReason => "blocked_with_reason",
    }
}

pub(super) const fn dangerous_tool_auto_approval_label(
    dangerous_tool_auto_approval: bool,
) -> &'static str {
    if dangerous_tool_auto_approval {
        "approve-all"
    } else {
        "disabled"
    }
}

pub(crate) const fn last_writer_actor_label(
    last_writer: Option<MetadataLastWriter>,
) -> &'static str {
    match last_writer {
        Some(last_writer) => match last_writer.actor() {
            MetadataActor::User {} => "user",
            MetadataActor::Core {} => "core",
            MetadataActor::Model { .. } => "model",
            MetadataActor::Recovery {} => "recovery",
            MetadataActor::Tool { .. } => "tool",
        },
        None => "none",
    }
}

pub(super) fn last_writer_micros_label(last_writer: Option<MetadataLastWriter>) -> String {
    match last_writer {
        Some(last_writer) => last_writer.updated_at_unix_micros().value().to_string(),
        None => String::from("none"),
    }
}

pub(crate) fn control_safe(value: &str, field: TextField) -> String {
    let mut rendered = String::with_capacity(value.len());
    for character in value.chars() {
        let code = character as u32;
        let preserved_line_feed = character == '\n' && field == TextField::Flowing;
        let control = code <= 0x1f || (0x7f..=0x9f).contains(&code);
        // A delimited field escapes the introducer too, so every backslash in
        // its output opens an escape this renderer wrote and the field decodes
        // back to the exact values it was given.
        let delimiter =
            matches!(character, ' ' | ',' | '\\') && field == TextField::DelimitedOnLine;
        if delimiter || (control && !preserved_line_feed) {
            rendered.push_str(&format!("\\u{{{code:x}}}"));
        } else {
            rendered.push(character);
        }
    }
    rendered
}

/// One metric's two counts: how much of a population the metric names.
#[derive(Clone, Copy)]
pub(super) struct RateCounts {
    pub(super) numerator: u64,
    pub(super) denominator: u64,
}
