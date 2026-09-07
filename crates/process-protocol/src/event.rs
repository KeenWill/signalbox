//! Event wire representations and validation.

use crate::delegation::{
    DelegationOutcome, DelegationPolicy, DelegationProvenance, DelegationReason,
    DelegationWaitMode, ToolApprovalEventDecider, ToolApprovalEventDecision,
    child_result_shape_is_valid, delegation_content_is_valid, delegation_provenance_parent,
    parent_delegation_provenance_has_cascade, parent_delegation_provenance_is_cascade,
};
use crate::runner::{
    RunnerPlacementRevision, RunnerSandboxProfile, RunnerStateTransitionState,
    RunnerWorkingDirectory,
};
use crate::scalars::{
    CanonicalU64, CanonicalUuid, CommandId, FrameValidationError, deserialize_required_nullable,
};
use crate::settings::{
    ModelChangeAdjustment, ModelSelection, ModelSettingsOverlay, ModelSettingsPrecedence,
    ModelSettingsSnapshot, adjustments_target_explicit_overlay, apply_wire_adjustments,
    overlay_inheriting_from, snapshot_matches_model, validate_adjustments,
    validate_turn_settings_payload,
};
use crate::shared_validation::validate_tool_approval_event_shape;
use crate::transcript::{ModelCallState, ToolBatchState};
use crate::user_input::UserInputContent;
use serde::{Deserialize, Serialize};

/// Closed durable update event family.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum SessionEvent {
    /// Session creation committed.
    SessionCreated {},
    /// One defaults replacement changed model selection or settings.
    SessionModelSettingsChanged {
        command_id: CommandId,
        prior_defaults_version: CanonicalU64,
        installed_defaults_version: CanonicalU64,
        prior_model: ModelSelection,
        installed_model: ModelSelection,
        prior_settings: ModelSettingsSnapshot,
        installed_settings: ModelSettingsSnapshot,
        caller_override: ModelSettingsOverlay,
        adjustments: Vec<ModelChangeAdjustment>,
    },
    /// One accepted origin turn froze complete model settings.
    TurnModelSettingsResolved {
        accepted_input_id: CanonicalUuid,
        turn_id: CanonicalUuid,
        defaults_version: CanonicalU64,
        requested_model: ModelSelection,
        selected_direct_id: CanonicalUuid,
        per_call_override: ModelSettingsOverlay,
        settings: ModelSettingsSnapshot,
        adjusted_from_selection_id: Option<CanonicalUuid>,
        adjustments: Vec<ModelChangeAdjustment>,
    },
    /// User input acceptance and its queued turn committed.
    InputAccepted {
        /// Accepted input.
        accepted_input_id: CanonicalUuid,
        /// Queued origin turn.
        turn_id: CanonicalUuid,
        /// Immutable session acceptance position.
        acceptance_position: CanonicalU64,
        /// Exact ordered accepted user parts.
        content: UserInputContent,
    },
    /// A queued goal turn became intentionally ineligible.
    GoalTurnRetired {
        /// Exact immutable queued turn retired by a goal transition.
        turn_id: CanonicalUuid,
    },
    /// A queued turn became active.
    TurnActivated {
        /// Activated turn.
        turn_id: CanonicalUuid,
        /// Initial current attempt.
        current_attempt_id: CanonicalUuid,
    },
    /// Model call advanced.
    ModelCallTransition {
        /// Owning turn.
        turn_id: CanonicalUuid,
        /// Advancing call.
        model_call_id: CanonicalUuid,
        /// Exact committed state.
        state: ModelCallState,
    },
    /// A tool batch crossed one durable presentation boundary.
    ToolBatchTransition {
        /// Owning turn.
        turn_id: CanonicalUuid,
        /// Model call that proposed the batch.
        model_call_id: CanonicalUuid,
        /// Exact committed batch state.
        state: ToolBatchState,
    },
    /// A runner placement or its exact connection changed follower-visible state.
    RunnerStateTransition {
        /// Exact runner named by the transition.
        runner_id: CanonicalUuid,
        /// Positive placement revision whose immutable facts are projected.
        placement_revision: RunnerPlacementRevision,
        /// Placement-selected sandbox profile.
        sandbox_profile: RunnerSandboxProfile,
        /// Caller-selected directory, null when the runner default was selected.
        #[serde(deserialize_with = "deserialize_required_nullable")]
        working_directory: Option<RunnerWorkingDirectory>,
        /// Exact closed transition state.
        state: RunnerStateTransitionState,
    },
    /// One explicit tool approval decision committed with full provenance.
    ToolApprovalDecided {
        /// Owning turn.
        turn_id: CanonicalUuid,
        /// Exact logical tool request.
        tool_request_id: CanonicalUuid,
        /// Exact recorded decision.
        decision: ToolApprovalEventDecision,
        /// Exact user or delegate decider.
        decider: ToolApprovalEventDecider,
        /// Exact judge rationale, absent for a user decision.
        #[serde(deserialize_with = "deserialize_required_nullable")]
        rationale: Option<String>,
    },
    /// One append-only context compaction committed.
    ContextCompacted {
        /// Exact compaction provenance record.
        context_compaction_id: CanonicalUuid,
        /// Dedicated producing model call.
        model_call_id: CanonicalUuid,
        /// One-based final summarized position.
        through_position: CanonicalU64,
        /// Appended semantic summary entry.
        summary_entry_id: CanonicalUuid,
        /// Complete result frontier.
        result_frontier_id: CanonicalUuid,
    },
    /// Turn completed.
    TurnCompleted {
        /// Completed turn.
        turn_id: CanonicalUuid,
        /// Outcome-authoritative call.
        model_call_id: CanonicalUuid,
        /// Final completion marker.
        completion_entry_id: CanonicalUuid,
        /// Exact terminal frontier.
        terminal_frontier_id: CanonicalUuid,
    },
    /// Turn failed.
    TurnFailed {
        /// Failed turn.
        turn_id: CanonicalUuid,
        /// Failure marker.
        failure_entry_id: CanonicalUuid,
        /// Exact terminal frontier.
        terminal_frontier_id: CanonicalUuid,
    },
    /// Turn was refused.
    TurnRefused {
        /// Refused turn.
        turn_id: CanonicalUuid,
        /// Outcome-authoritative call.
        model_call_id: CanonicalUuid,
        /// Exact terminal frontier.
        terminal_frontier_id: CanonicalUuid,
    },
    /// Turn was cancelled.
    TurnCancelled {
        /// Cancelled turn.
        turn_id: CanonicalUuid,
        /// Semantic cancellation marker.
        cancellation_entry_id: CanonicalUuid,
        /// Exact terminal frontier.
        terminal_frontier_id: CanonicalUuid,
    },
    /// Turn stopped with an ambiguous model call requiring reconciliation.
    TurnReconciliationRequired {
        /// Reconciliation-required turn.
        turn_id: CanonicalUuid,
        /// Exact ambiguous terminal model call.
        model_call_id: CanonicalUuid,
        /// Exact terminal frontier.
        terminal_frontier_id: CanonicalUuid,
    },
    /// Turn stopped with an ambiguous tool attempt requiring reconciliation.
    TurnToolReconciliationRequired {
        /// Reconciliation-required turn.
        turn_id: CanonicalUuid,
        /// Exact ambiguous terminal tool attempt.
        tool_attempt_id: CanonicalUuid,
        /// Exact terminal frontier.
        terminal_frontier_id: CanonicalUuid,
    },
    /// A parent committed one child relationship and lifecycle policy.
    ChildSpawned {
        /// Exact spawning tool request and relationship identity.
        spawning_request_id: CanonicalUuid,
        /// Spawned child session.
        child_session_id: CanonicalUuid,
        /// Parent-chosen relationship lifecycle policy.
        relationship: DelegationPolicy,
    },
    /// A parent registered one foreground or background wait.
    ChildWaiting {
        /// Exact await tool request.
        await_request_id: CanonicalUuid,
        /// Relationship identity.
        spawning_request_id: CanonicalUuid,
        /// Child being awaited.
        child_session_id: CanonicalUuid,
        /// Wait delivery mode.
        mode: DelegationWaitMode,
    },
    /// One bidirectional relationship message became durable for its recipient.
    SessionMessage {
        /// Relationship identity.
        spawning_request_id: CanonicalUuid,
        /// Message identity.
        message_id: CanonicalUuid,
        /// Sending session.
        sender_session_id: CanonicalUuid,
        /// Receiving session.
        recipient_session_id: CanonicalUuid,
        /// Relationship-local message ordinal.
        ordinal: CanonicalU64,
        /// Recipient-wide delivery sequence.
        delivery_sequence: CanonicalU64,
        /// Exact delivered content.
        content: String,
    },
    /// A terminal child result became durable for its parent.
    ChildResult {
        /// Relationship identity.
        spawning_request_id: CanonicalUuid,
        /// Terminal child.
        child_session_id: CanonicalUuid,
        /// Typed terminal result outcome.
        outcome: DelegationOutcome,
        /// Delivered content for a successful result only.
        content: Option<String>,
        /// Typed reason for the terminal result.
        reason: DelegationReason,
        /// Exact child-turn or parent-command provenance.
        provenance: DelegationProvenance,
    },
    /// Parent termination evaluated one relationship edge.
    ChildLifecycleDisposition {
        /// Relationship identity.
        spawning_request_id: CanonicalUuid,
        /// Evaluated child.
        child_session_id: CanonicalUuid,
        /// Typed relationship outcome.
        outcome: DelegationOutcome,
        /// Typed reason for evaluating this relationship edge.
        reason: DelegationReason,
        /// Exact parent command provenance.
        provenance: DelegationProvenance,
    },
}

pub(crate) fn validate_delegation_session_event(
    session_id: CanonicalUuid,
    event: &SessionEvent,
) -> Result<(), FrameValidationError> {
    let valid = match event {
        SessionEvent::ChildSpawned {
            child_session_id, ..
        }
        | SessionEvent::ChildWaiting {
            child_session_id, ..
        } => *child_session_id != session_id,
        SessionEvent::SessionMessage {
            sender_session_id,
            recipient_session_id,
            ordinal,
            delivery_sequence,
            content,
            ..
        } => {
            *recipient_session_id == session_id
                && sender_session_id != recipient_session_id
                && ordinal.value() > 0
                && delivery_sequence.value() > 0
                && delegation_content_is_valid(content)
        }
        SessionEvent::ChildResult {
            child_session_id,
            outcome,
            content,
            reason,
            provenance,
            ..
        } => {
            *child_session_id != session_id
                && child_result_shape_is_valid(
                    session_id,
                    *child_session_id,
                    *outcome,
                    content,
                    *reason,
                    provenance,
                )
        }
        SessionEvent::ChildLifecycleDisposition {
            child_session_id,
            outcome,
            reason,
            provenance,
            ..
        } => {
            matches!(
                reason,
                DelegationReason::ParentStopped | DelegationReason::ParentCancelled
            ) && if *child_session_id == session_id {
                // A descendant cascade also addresses the terminalization to
                // the child itself so that live child followers observe it.
                // That row carries the parent's cascade provenance, so the
                // provenance parent is a different session than this header.
                matches!(
                    outcome,
                    DelegationOutcome::Stopped | DelegationOutcome::Cancelled
                ) && delegation_provenance_parent(provenance)
                    .is_some_and(|parent| parent != session_id)
                    && parent_delegation_provenance_has_cascade(provenance)
            } else {
                matches!(
                    outcome,
                    DelegationOutcome::Stopped
                        | DelegationOutcome::Cancelled
                        | DelegationOutcome::AlreadyTerminal
                        | DelegationOutcome::ContinueRunning
                ) && parent_delegation_provenance_is_cascade(session_id, provenance)
            }
        }
        _ => true,
    };
    if valid {
        Ok(())
    } else {
        Err(FrameValidationError::DelegationShape)
    }
}

pub(crate) fn validate_settings_event(event: &SessionEvent) -> Result<(), FrameValidationError> {
    match event {
        SessionEvent::SessionModelSettingsChanged {
            prior_defaults_version,
            installed_defaults_version,
            prior_model,
            installed_model,
            prior_settings,
            installed_settings,
            caller_override,
            adjustments,
            ..
        } => {
            prior_settings.validate_defaults()?;
            installed_settings.validate_defaults()?;
            validate_adjustments(adjustments)?;
            let validation_changed = matches!(
                (
                    prior_settings.validated_for_selection_id,
                    installed_settings.validated_for_selection_id,
                ),
                (Some(prior), Some(installed)) if prior != installed
            );
            let copied_precedence = ModelSettingsPrecedence {
                per_call: prior_settings.precedence.per_call,
                session: prior_settings.precedence.session,
                profile: installed_settings.precedence.profile,
                global_default: installed_settings.precedence.global_default,
            };
            let unadjusted_precedence = ModelSettingsPrecedence {
                session: overlay_inheriting_from(
                    *caller_override,
                    prior_settings.precedence.session,
                ),
                ..copied_precedence
            };
            let provenance_matches = apply_wire_adjustments(unadjusted_precedence, adjustments)
                .is_some_and(|expected| expected == installed_settings.precedence);
            if prior_defaults_version.value() == 0
                || prior_defaults_version.value().checked_add(1)
                    != Some(installed_defaults_version.value())
                || (prior_model == installed_model && prior_settings == installed_settings)
                || !snapshot_matches_model(prior_model, prior_settings)
                || !snapshot_matches_model(installed_model, installed_settings)
                || !provenance_matches
                || (!adjustments.is_empty() && !validation_changed)
                || adjustments_target_explicit_overlay(*caller_override, adjustments)
            {
                return Err(FrameValidationError::ModelSettingsShape);
            }
        }
        SessionEvent::TurnModelSettingsResolved {
            defaults_version,
            requested_model,
            selected_direct_id,
            per_call_override,
            settings,
            adjusted_from_selection_id,
            adjustments,
            ..
        } => validate_turn_settings_payload(
            *defaults_version,
            requested_model,
            *selected_direct_id,
            *per_call_override,
            settings,
            *adjusted_from_selection_id,
            adjustments,
        )?,
        SessionEvent::ToolApprovalDecided {
            decision,
            decider,
            rationale,
            ..
        } => validate_tool_approval_event_shape(decision, decider, rationale)?,
        SessionEvent::InputAccepted { content, .. } => content.validate()?,
        SessionEvent::SessionCreated {}
        | SessionEvent::GoalTurnRetired { .. }
        | SessionEvent::TurnActivated { .. }
        | SessionEvent::ModelCallTransition { .. }
        | SessionEvent::ToolBatchTransition { .. }
        | SessionEvent::RunnerStateTransition { .. }
        | SessionEvent::ContextCompacted { .. }
        | SessionEvent::TurnCompleted { .. }
        | SessionEvent::TurnFailed { .. }
        | SessionEvent::TurnRefused { .. }
        | SessionEvent::TurnCancelled { .. }
        | SessionEvent::TurnReconciliationRequired { .. }
        | SessionEvent::TurnToolReconciliationRequired { .. }
        | SessionEvent::ChildSpawned { .. }
        | SessionEvent::ChildWaiting { .. }
        | SessionEvent::SessionMessage { .. }
        | SessionEvent::ChildResult { .. }
        | SessionEvent::ChildLifecycleDisposition { .. } => {}
    }
    Ok(())
}
