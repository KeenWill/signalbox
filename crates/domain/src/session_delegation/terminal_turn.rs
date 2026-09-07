//! Session delegation terminal turn for `docs/spec/sessions-and-transcript.md`.

use super::content::{DelegationContent, delegation_content_digest};
use super::outcome::DelegationOutcomeReason;
use crate::{SessionId, TurnId};

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(super) enum TerminalChildTurnKind {
    Returned,
    Failed,
    Cancelled,
}

/// Exact child turn sealed by checked terminal scheduling evidence.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct TerminalChildTurn {
    pub(super) session: SessionId,
    pub(super) turn: TurnId,
    pub(super) kind: TerminalChildTurnKind,
    pub(super) reason: DelegationOutcomeReason,
    pub(super) result_digest: Option<[u8; 32]>,
}

impl TerminalChildTurn {
    /// Seals a live completed result from the completed execution candidate's
    /// own semantic entries. No independent content parameter is accepted.
    pub fn from_completed(value: &crate::CompletedModelCallTurn) -> Option<Self> {
        Some(terminal_from_completed_content(
            value.session(),
            value.turn(),
            delegation_content_from_live_completed(value),
        ))
    }

    /// Seals an execution failure from the exact failed-turn commit candidate.
    /// Unlike an accepted-input scheduling projection, this evidence may name
    /// any turn origin, including a delegated task.
    pub const fn from_failed(value: &crate::FailedModelCallTurn) -> Self {
        Self {
            session: value.session(),
            turn: value.turn(),
            kind: TerminalChildTurnKind::Failed,
            reason: DelegationOutcomeReason::ChildExecutionFailed,
            result_digest: None,
        }
    }

    /// Seals a reconciliation-required child as a failed result whose provider
    /// outcome cannot be made available to its waiting parent.
    pub const fn from_reconciliation_required(
        value: &crate::ReconciliationRequiredModelCallTurn,
    ) -> Self {
        Self {
            session: value.session(),
            turn: value.turn(),
            kind: TerminalChildTurnKind::Failed,
            reason: DelegationOutcomeReason::ChildResultUnavailable,
            result_digest: None,
        }
    }

    /// Seals cancellation from the exact cancelled-turn commit candidate.
    /// Unlike an accepted-input scheduling projection, this evidence may name
    /// any turn origin, including a delegated task.
    pub const fn from_cancelled(value: &crate::CancelledModelCallTurn) -> Self {
        Self {
            session: value.session(),
            turn: value.turn(),
            kind: TerminalChildTurnKind::Cancelled,
            reason: DelegationOutcomeReason::ChildCancelled,
            result_digest: None,
        }
    }

    /// Seals cancellation when an interrupt closed a tool-using response.
    pub const fn from_cancelled_tool_round(value: &crate::CancelledToolRoundModelCallTurn) -> Self {
        Self {
            session: value.session(),
            turn: value.turn(),
            kind: TerminalChildTurnKind::Cancelled,
            reason: DelegationOutcomeReason::ChildCancelled,
            result_digest: None,
        }
    }

    /// Seals a provider refusal from the exact refused-turn commit candidate.
    pub const fn from_refused(value: &crate::RefusedModelCallTurn) -> Self {
        Self {
            session: value.session(),
            turn: value.turn(),
            kind: TerminalChildTurnKind::Failed,
            reason: DelegationOutcomeReason::ChildExecutionFailed,
            result_digest: None,
        }
    }

    pub const fn session(self) -> SessionId {
        self.session
    }

    pub const fn turn(self) -> TurnId {
        self.turn
    }

    pub const fn reason(self) -> DelegationOutcomeReason {
        self.reason
    }
}

pub(super) fn delegation_content_from_live_completed(
    value: &crate::CompletedModelCallTurn,
) -> Option<DelegationContent> {
    let mut assistant_text = Vec::with_capacity(value.assistant_entries().len());
    for entry in value.assistant_entries() {
        let (producing_call, text) = match entry.payload() {
            crate::SemanticTranscriptEntryPayload::AssistantText {
                producing_call,
                value,
            } => (producing_call, value),
            crate::SemanticTranscriptEntryPayload::ProviderCompaction {
                producing_call, ..
            } => {
                if entry.source_session() != value.session() || *producing_call != value.call().id()
                {
                    return None;
                }
                continue;
            }
            crate::SemanticTranscriptEntryPayload::Imported { .. }
            | crate::SemanticTranscriptEntryPayload::DelegatedTask { .. }
            | crate::SemanticTranscriptEntryPayload::DelegationMessage { .. }
            | crate::SemanticTranscriptEntryPayload::DelegationResult { .. }
            | crate::SemanticTranscriptEntryPayload::OriginAcceptedInput { .. }
            | crate::SemanticTranscriptEntryPayload::SteeringAcceptedInput { .. }
            | crate::SemanticTranscriptEntryPayload::ModelIdentityChanged { .. }
            | crate::SemanticTranscriptEntryPayload::ContextSummary { .. }
            | crate::SemanticTranscriptEntryPayload::TurnFailed { .. }
            | crate::SemanticTranscriptEntryPayload::AssistantToolUse { .. }
            | crate::SemanticTranscriptEntryPayload::ToolExecutionResult { .. }
            | crate::SemanticTranscriptEntryPayload::ToolDenied { .. }
            | crate::SemanticTranscriptEntryPayload::ToolClosed { .. }
            | crate::SemanticTranscriptEntryPayload::TurnCompleted { .. }
            | crate::SemanticTranscriptEntryPayload::RunnerPlacementChanged { .. }
            | crate::SemanticTranscriptEntryPayload::TurnCancelled { .. } => return None,
        };
        if entry.source_session() != value.session() || *producing_call != value.call().id() {
            return None;
        }
        assistant_text.push(text);
    }
    let utf8_byte_length = assistant_text.iter().try_fold(0_usize, |total, text| {
        total.checked_add(text.as_str().len())
    })?;
    if utf8_byte_length > DelegationContent::MAX_UTF8_BYTES {
        return None;
    }
    let mut content = String::with_capacity(utf8_byte_length);
    for text in assistant_text {
        content.push_str(text.as_str());
    }
    DelegationContent::try_new(content).ok()
}

pub(super) fn terminal_from_completed_content(
    session: SessionId,
    turn: TurnId,
    content: Option<DelegationContent>,
) -> TerminalChildTurn {
    let (kind, reason, result_digest) = match content {
        Some(content) => (
            TerminalChildTurnKind::Returned,
            DelegationOutcomeReason::ChildCompleted,
            Some(delegation_content_digest(&content)),
        ),
        None => (
            TerminalChildTurnKind::Failed,
            DelegationOutcomeReason::ChildResultUnavailable,
            None,
        ),
    };
    TerminalChildTurn {
        session,
        turn,
        kind,
        reason,
        result_digest,
    }
}
