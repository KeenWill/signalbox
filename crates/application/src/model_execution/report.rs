use super::{
    BTreeSet, ModelCallTerminalOutcome, ModelConversationMessage, PreparedModelCallFailureCause,
    SessionId, TurnId,
};

/// Closed terminal labels admitted to the turn lifecycle event.
///
/// Callers select a typed variant from their exhaustive domain outcome instead
/// of supplying a positional string that could drift from committed state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum TurnTerminalOutcome {
    Completed,
    CancelledWithToolResponse,
    Failed,
    Cancelled,
    Refused,
    TargetUnavailable,
    CapabilityKnownFailure,
    ToolRoundLimitReached,
}

impl TurnTerminalOutcome {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::CancelledWithToolResponse => "cancelled_with_tool_response",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Refused => "refused",
            Self::TargetUnavailable => "target_unavailable",
            Self::CapabilityKnownFailure => "capability_known_failure",
            Self::ToolRoundLimitReached => "tool_round_limit_reached",
        }
    }
}

impl From<PreparedModelCallFailureCause> for TurnTerminalOutcome {
    fn from(cause: PreparedModelCallFailureCause) -> Self {
        match cause {
            PreparedModelCallFailureCause::CapabilityKnownFailure => Self::CapabilityKnownFailure,
            PreparedModelCallFailureCause::ToolRoundLimitReached => Self::ToolRoundLimitReached,
        }
    }
}

/// Records terminal model-call commits while excluding nonterminal waits.
///
/// Each arm is exhaustive over the domain-owned outcome, keeping the label
/// derived from the committed state rather than supplied independently.
pub(super) fn report_model_call_terminalization(outcome: &ModelCallTerminalOutcome) {
    let (session, turn, terminal_outcome) = match outcome {
        ModelCallTerminalOutcome::Completed(value) => (
            value.session(),
            value.turn(),
            TurnTerminalOutcome::Completed,
        ),
        ModelCallTerminalOutcome::CancelledWithToolResponse(value) => (
            value.session(),
            value.turn(),
            TurnTerminalOutcome::CancelledWithToolResponse,
        ),
        ModelCallTerminalOutcome::Failed(value) => {
            (value.session(), value.turn(), TurnTerminalOutcome::Failed)
        }
        ModelCallTerminalOutcome::Cancelled(value) => (
            value.session(),
            value.turn(),
            TurnTerminalOutcome::Cancelled,
        ),
        ModelCallTerminalOutcome::Refused(value) => {
            (value.session(), value.turn(), TurnTerminalOutcome::Refused)
        }
        ModelCallTerminalOutcome::ReconciliationRequired(value) => {
            report_turn_parked_for_reconciliation(value.session(), value.turn());
            return;
        }
        ModelCallTerminalOutcome::ToolRound(_) | ModelCallTerminalOutcome::AwaitingRecovery(_) => {
            return;
        }
    };
    report_turn_terminalization(session, turn, terminal_outcome);
}

/// Emits one content-free record for a turn parked on user reconciliation.
///
/// Session and turn are daemon-minted identities, while the event name is a
/// closed lifecycle state. Ambiguity details and model content remain absent.
fn report_turn_parked_for_reconciliation(session: SessionId, turn: TurnId) {
    tracing::warn!(
        session_id = %session.into_uuid(),
        turn_id = %turn.into_uuid(),
        "turn parked awaiting bounded reconciliation"
    );
}

/// Emits one content-free terminal lifecycle record for an operator.
///
/// Session, turn, and the closed outcome token are sufficient to distinguish
/// completed work from an active or parked daemon without exposing payloads.
pub(super) fn report_turn_terminalization(
    session: SessionId,
    turn: TurnId,
    terminal_outcome: TurnTerminalOutcome,
) {
    tracing::info!(
        session_id = %session.as_uuid(),
        turn_id = %turn.as_uuid(),
        terminal_outcome = terminal_outcome.as_str(),
        "turn terminalized"
    );
}
/// Counts one turn's distinct automatic tool rounds in a rendered frontier.
///
/// The count is the quantity a deployment's configured ceiling is compared
/// against; the comparison itself stays at the checkpoint that owns the
/// configured limit.
pub(super) fn automatic_tool_round_count(
    turn: TurnId,
    messages: &[ModelConversationMessage],
) -> usize {
    messages
        .iter()
        .filter_map(|message| match message {
            ModelConversationMessage::AssistantToolUse {
                producing_call,
                request,
                ..
            } if request.turn() == turn => Some(*producing_call),
            _ => None,
        })
        .collect::<BTreeSet<_>>()
        .len()
}
