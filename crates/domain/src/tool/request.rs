//! Tool request for `docs/spec/tool-loop.md`.

use super::approval::InitialToolApproval;
use super::arguments::NormalizedToolArguments;
use super::name::ToolName;
use super::policy::ToolApprovalPosture;
use super::proposal::{ToolCallProposal, ToolRequestOrdinal};
use crate::{ModelCallId, SessionId, ToolRequestId, TurnId};

/// One immutable content-authoritative logical tool request.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ToolRequest {
    pub(super) id: ToolRequestId,
    session: SessionId,
    turn: TurnId,
    producing_call: ModelCallId,
    ordinal: ToolRequestOrdinal,
    name: ToolName,
    arguments: NormalizedToolArguments,
    approval_posture: ToolApprovalPosture,
}

impl ToolRequest {
    pub(crate) fn from_model_proposal(
        id: ToolRequestId,
        session: SessionId,
        turn: TurnId,
        producing_call: ModelCallId,
        ordinal: ToolRequestOrdinal,
        proposal: ToolCallProposal,
        approval: InitialToolApproval,
    ) -> Self {
        Self {
            id,
            session,
            turn,
            producing_call,
            ordinal,
            name: proposal.name,
            arguments: proposal.arguments,
            approval_posture: approval.posture(),
        }
    }

    /// Returns the logical request identity.
    pub const fn id(&self) -> ToolRequestId {
        self.id
    }

    /// Returns the owning session.
    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// Returns the owning logical turn.
    pub const fn turn(&self) -> TurnId {
        self.turn
    }

    /// Returns the definitive model call that proposed this request.
    pub const fn producing_call(&self) -> ModelCallId {
        self.producing_call
    }

    /// Returns proposal order among tool calls from the producing call.
    pub const fn ordinal(&self) -> ToolRequestOrdinal {
        self.ordinal
    }

    /// Borrows the checked request name.
    pub const fn name(&self) -> &ToolName {
        &self.name
    }

    /// Borrows the normalized request arguments.
    pub const fn arguments(&self) -> &NormalizedToolArguments {
        &self.arguments
    }

    /// Returns the exact per-request posture frozen when the proposal landed.
    pub const fn approval_posture(&self) -> ToolApprovalPosture {
        self.approval_posture
    }
}

/// Complete independently stored facts for one logical request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolRequestReconstitutionInput {
    request: ToolRequest,
}

impl ToolRequestReconstitutionInput {
    /// Supplies all typed stored facts without claiming batch correlation.
    #[allow(clippy::too_many_arguments)]
    pub const fn new(
        id: ToolRequestId,
        session: SessionId,
        turn: TurnId,
        producing_call: ModelCallId,
        ordinal: ToolRequestOrdinal,
        name: ToolName,
        arguments: NormalizedToolArguments,
    ) -> Self {
        Self {
            request: ToolRequest {
                id,
                session,
                turn,
                producing_call,
                ordinal,
                name,
                arguments,
                approval_posture: ToolApprovalPosture::Human,
            },
        }
    }

    /// Supplies the exact stored posture selected when this request landed.
    pub const fn with_approval_posture(mut self, posture: ToolApprovalPosture) -> Self {
        self.request.approval_posture = posture;
        self
    }

    /// Returns the inert typed request for complete aggregate validation.
    pub fn into_request(self) -> ToolRequest {
        self.request
    }
}
