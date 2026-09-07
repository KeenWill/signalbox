mod entry_output;
mod event_output;
mod session_output;
mod snapshot_output;

mod labels;
use labels::{
    RateCounts, bound_child_action, current_model_call_state, dangerous_tool_auto_approval_label,
    delegation_message_direction, delegation_outcome, delegation_provenance, delegation_reason,
    delegation_wait_mode, duration_label, failed_model_call_cause, failed_model_call_disposition,
    goal_blocked_reason_label, imported_content_kind, imported_speaker_attestation_label,
    imported_speaker_label, last_writer_micros_label, lifecycle_actor_label, model_call_state,
    operator_status_lifecycle_state_label, rate_label, review_diff_side_label,
    review_finding_status_label, review_orchestration_concern_status_label,
    review_orchestration_state_label, review_pass_kind_label, review_pass_state_label,
    review_run_state_label, review_severity_label, review_workflow_label, runner_connection_health,
    runner_projection_state, runner_sandbox_profile, runner_state_transition_state,
    session_closure_outcome_label,
};
pub(crate) use labels::{control_safe, last_writer_actor_label};
mod rows;
pub(crate) use rows::{
    ChatTurnStatus, ConversationRow, ImportedEntryRow, SessionMetadataRow, TextField,
};
mod cost;
#[cfg(test)]
pub(crate) use cost::{CostAggregateKey, DiskCostTotals};
use cost::{TokenUsageTotal, UsageAggregate, cost_label, usage_provenance_label};
mod selection;
pub(crate) use selection::SnapshotSelection;
#[cfg(test)]
use selection::SnapshotSelectionContext;

use std::{
    collections::HashSet,
    fs::File,
    io::{self, Read, Seek, SeekFrom, Write},
    path::Path,
    str::FromStr,
};

use rust_decimal::Decimal;
use signalbox_process_protocol::{
    BoundChildAction, CanonicalBlobDigest, CanonicalUuid, CurrentModelCallState,
    DelegationMessageDirection, DelegationOutcome, DelegationPolicy, DelegationProvenance,
    DelegationReason, DelegationWaitMode, DescendantTerminationScope, FailedModelCallCause,
    FailedModelCallDisposition, GoalBlockedProvenance, GoalBlockedReason, GoalHistoryEvent,
    GoalLifecycleState, ImportedContentKind, ImportedSourceSpeaker, ImportedSpeaker,
    ImportedTextPreview, LifecycleActorClass, MAX_RATE_VERSION_UTF8_BYTES, MetadataActor,
    MetadataLastWriter, ModelCallCostLabel, ModelCallDisposition, ModelCallState,
    OperatorStatusLifecycleDeadlineViolationMessage, OperatorStatusLifecycleState,
    OperatorStatusLifecycleWeekMessage, OperatorStatusMessage, ReviewDiffSide,
    ReviewFindingSnapshot, ReviewFindingStatus, ReviewOrchestrationConcernStatus,
    ReviewOrchestrationSnapshot, ReviewOrchestrationState, ReviewPassKind, ReviewPassLifecycle,
    ReviewRunLifecycle, ReviewRunSnapshot, ReviewSeverity, ReviewTargetSnapshot,
    ReviewTargetSubject, ReviewWorkflow, RunnerConnectionHealth, RunnerProjection,
    RunnerProjectionSelector, RunnerProjectionState, RunnerSandboxProfile,
    RunnerStateTransitionState, ServerMessage, SessionClosureOutcome, SessionEvent,
    ToolApprovalEventDecider, ToolApprovalEventDecision, ToolBatchState, ToolDecision,
    TranscriptEntry, TranscriptTextEntry, TurnState, UsageProvenance, UserInputContent,
    UserInputPart,
};

use crate::{
    ImportScanSummary,
    error::ClientError,
    transcript::{
        SnapshotEntry, SnapshotEntryKind, SnapshotIdentitySet, SnapshotRecord, TranscriptSnapshot,
        TranscriptTurn,
    },
};

pub(crate) struct ChildResultPresentation<'a> {
    pub(crate) await_request_id: CanonicalUuid,
    pub(crate) spawning_request_id: CanonicalUuid,
    pub(crate) child_session_id: CanonicalUuid,
    pub(crate) outcome: DelegationOutcome,
    pub(crate) content: Option<&'a String>,
    pub(crate) reason: DelegationReason,
    pub(crate) provenance: DelegationProvenance,
}

pub(crate) struct SessionSpawnedPresentation {
    pub(crate) tool_request_id: CanonicalUuid,
    pub(crate) child_session_id: CanonicalUuid,
    pub(crate) relationship: DelegationPolicy,
}

pub(crate) struct SessionAwaitRegisteredPresentation {
    pub(crate) tool_request_id: CanonicalUuid,
    pub(crate) child_session_id: CanonicalUuid,
    pub(crate) mode: DelegationWaitMode,
}

pub(crate) struct SessionMessageSentPresentation {
    pub(crate) tool_request_id: CanonicalUuid,
    pub(crate) peer_session_id: CanonicalUuid,
    pub(crate) message_id: CanonicalUuid,
    pub(crate) direction: DelegationMessageDirection,
    pub(crate) ordinal: u64,
    pub(crate) delivery_sequence: u64,
}

pub(crate) struct OperatorStatusPresentationCounts {
    pub(crate) lifecycle_weeks: u64,
    pub(crate) lifecycle_deadline_violations: u64,
}

pub(crate) enum BlobUploadPresentation {
    AlreadyPresent,
    Committed,
}

pub(crate) struct Output<'a> {
    stdout: &'a mut dyn Write,
    stderr: &'a mut dyn Write,
    raw: bool,
}

impl<'a> Output<'a> {
    pub(crate) fn oauth_credential(
        &mut self,
        message: &signalbox_process_protocol::ServerMessage,
    ) -> io::Result<()> {
        match message {
            signalbox_process_protocol::ServerMessage::OauthCredentialAuthorization {
                user_code,
                verification_uri,
                ..
            } => {
                self.text_field("user_code", user_code)?;
                self.text_field("verification_uri", verification_uri)
            }
            signalbox_process_protocol::ServerMessage::OauthCredentialReceipt {
                command_id,
                profile,
                outcome,
            } => {
                self.text_field("command_id", &command_id.into_uuid().to_string())?;
                self.text_field("profile", profile)?;
                let value = serde_json::to_string(outcome).map_err(io::Error::other)?;
                self.text_field("outcome", &value)
            }
            _ => Err(io::Error::other("unexpected OAuth message")),
        }
    }
    pub(crate) fn new(stdout: &'a mut dyn Write, stderr: &'a mut dyn Write, raw: bool) -> Self {
        Self {
            stdout,
            stderr,
            raw,
        }
    }

    pub(crate) fn flush(&mut self) -> io::Result<()> {
        self.stdout.flush()
    }

    fn text_field(&mut self, name: &str, value: &str) -> io::Result<()> {
        write!(self.stdout, "{name}=")?;
        self.stdout.write_all(
            self.render_field(value, TextField::TrailingOnLine)
                .as_bytes(),
        )?;
        self.stdout.write_all(b"\n")
    }

    fn render(&self, value: &str) -> String {
        self.render_field(value, TextField::Flowing)
    }

    fn render_field(&self, value: &str, field: TextField) -> String {
        if self.raw {
            value.to_owned()
        } else {
            control_safe(value, field)
        }
    }
}

#[cfg(test)]
mod tests;
