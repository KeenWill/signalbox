//! Tool decide for `docs/spec/tool-loop.md`.

use super::approval::{ToolApprovalDecision, ToolApprovalResolution};
use super::request::ToolRequest;
use crate::{DurableCommandId, ToolRequestId};

/// The canonical user command for one pending tool request.
#[derive(Clone, Debug)]
pub struct DecideToolRequest {
    command_id: DurableCommandId,
    request: ToolRequestId,
    decision: ToolApprovalDecision,
}

impl DecideToolRequest {
    /// Constructs the complete canonical caller payload after rejecting the
    /// user-global nil and max command sentinels.
    pub fn try_new(
        command_id: DurableCommandId,
        request: ToolRequestId,
        decision: ToolApprovalDecision,
    ) -> Result<Self, DecideToolRequestConstructionError> {
        if command_id.as_uuid().is_nil() || command_id.as_uuid().is_max() {
            return Err(DecideToolRequestConstructionError { command_id });
        }
        Ok(Self {
            command_id,
            request,
            decision,
        })
    }

    /// Returns the user-global command identity.
    pub const fn command_id(&self) -> DurableCommandId {
        self.command_id
    }

    /// Returns the exact logical request.
    pub const fn request(&self) -> ToolRequestId {
        self.request
    }

    /// Borrows the requested approval decision.
    pub const fn decision(&self) -> &ToolApprovalDecision {
        &self.decision
    }

    /// Prepares user-sourced resolution against the exact request record.
    pub fn prepare_applied(
        self,
        request: &ToolRequest,
    ) -> Result<PreparedDecideToolRequest, DecideToolRequestPreparationError> {
        if request.id != self.request {
            return Err(DecideToolRequestPreparationError {
                command: self,
                provided_request: request.id,
            });
        }
        let resolution =
            ToolApprovalResolution::user(self.command_id, self.request, self.decision.clone());
        Ok(PreparedDecideToolRequest {
            command: self,
            result: DecideToolRequestResult::Applied(DecideToolRequestAppliedResult { resolution }),
        })
    }

    /// Prepares a closure-sourced denial against the exact request record.
    pub fn prepare_lifecycle_closure_applied(
        self,
        request: &ToolRequest,
    ) -> Result<PreparedDecideToolRequest, DecideToolRequestPreparationError> {
        if request.id != self.request
            || self.decision != (ToolApprovalDecision::Deny { reason: None })
        {
            return Err(DecideToolRequestPreparationError {
                command: self,
                provided_request: request.id,
            });
        }
        let resolution = ToolApprovalResolution::lifecycle_closure(self.request);
        Ok(PreparedDecideToolRequest {
            command: self,
            result: DecideToolRequestResult::Applied(DecideToolRequestAppliedResult { resolution }),
        })
    }

    /// Prepares a runtime denial for an expired human approval wait.
    pub fn prepare_approval_timeout_applied(
        self,
        request: &ToolRequest,
    ) -> Result<PreparedDecideToolRequest, DecideToolRequestPreparationError> {
        let resolution = ToolApprovalResolution::approval_timeout(self.request);
        if request.id != self.request || &self.decision != resolution.decision() {
            return Err(DecideToolRequestPreparationError {
                command: self,
                provided_request: request.id,
            });
        }
        Ok(PreparedDecideToolRequest {
            command: self,
            result: DecideToolRequestResult::Applied(DecideToolRequestAppliedResult { resolution }),
        })
    }

    /// Prepares an authoritative missing-request rejection.
    pub const fn prepare_request_not_found(self) -> PreparedDecideToolRequest {
        let request = self.request;
        PreparedDecideToolRequest {
            command: self,
            result: DecideToolRequestResult::Rejected(
                DecideToolRequestRejectedResult::RequestNotFound { request },
            ),
        }
    }

    /// Prepares an authoritative already-resolved rejection.
    pub const fn prepare_already_resolved(self) -> PreparedDecideToolRequest {
        let request = self.request;
        PreparedDecideToolRequest {
            command: self,
            result: DecideToolRequestResult::Rejected(
                DecideToolRequestRejectedResult::AlreadyResolved { request },
            ),
        }
    }

    /// Prepares a rejection while the delegated approval judge is outstanding.
    pub const fn prepare_awaiting_approval_judge(self) -> PreparedDecideToolRequest {
        let request = self.request;
        PreparedDecideToolRequest {
            command: self,
            result: DecideToolRequestResult::Rejected(
                DecideToolRequestRejectedResult::AwaitingApprovalJudge { request },
            ),
        }
    }

    /// Prepares an authoritative proposal-order rejection.
    pub const fn prepare_not_earliest(self, earliest: ToolRequestId) -> PreparedDecideToolRequest {
        let request = self.request;
        PreparedDecideToolRequest {
            command: self,
            result: DecideToolRequestResult::Rejected(
                DecideToolRequestRejectedResult::NotEarliestUndecided { request, earliest },
            ),
        }
    }
}

/// A tool-decision command used a reserved user-global identity.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DecideToolRequestConstructionError {
    command_id: DurableCommandId,
}

impl DecideToolRequestConstructionError {
    /// Returns the rejected command identity.
    pub const fn command_id(self) -> DurableCommandId {
        self.command_id
    }
}

impl PartialEq for DecideToolRequest {
    fn eq(&self, other: &Self) -> bool {
        self.request == other.request && self.decision == other.decision
    }
}

impl Eq for DecideToolRequest {}

impl std::hash::Hash for DecideToolRequest {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.request.hash(state);
        self.decision.hash(state);
    }
}

/// Terminal typed result for one tool-decision command.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum DecideToolRequestResult {
    /// The user decision was recorded.
    Applied(DecideToolRequestAppliedResult),
    /// Authoritative current state rejected the command.
    Rejected(DecideToolRequestRejectedResult),
}

/// The applied user decision and its non-forgeable source tag.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct DecideToolRequestAppliedResult {
    pub(super) resolution: ToolApprovalResolution,
}

impl DecideToolRequestAppliedResult {
    /// Borrows the exact user-sourced resolution.
    pub const fn resolution(&self) -> &ToolApprovalResolution {
        &self.resolution
    }
}

/// Closed authoritative rejection vocabulary for tool decisions.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum DecideToolRequestRejectedResult {
    /// The delegated request has no terminal approval-judge evidence yet.
    AwaitingApprovalJudge {
        /// The request awaiting its judge.
        request: ToolRequestId,
    },
    /// No logical request had this identity.
    RequestNotFound {
        /// The absent request.
        request: ToolRequestId,
    },
    /// The request already had a terminal approval resolution.
    AlreadyResolved {
        /// The already-resolved request.
        request: ToolRequestId,
    },
    /// An earlier request in the same batch still awaited decision.
    NotEarliestUndecided {
        /// The out-of-order requested subject.
        request: ToolRequestId,
        /// The exact request that must be decided first.
        earliest: ToolRequestId,
    },
}

/// A pre-commit tool-decision candidate.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreparedDecideToolRequest {
    pub(super) command: DecideToolRequest,
    pub(super) result: DecideToolRequestResult,
}

impl PreparedDecideToolRequest {
    /// Borrows the canonical command.
    pub const fn command(&self) -> &DecideToolRequest {
        &self.command
    }

    /// Borrows the terminal typed result.
    pub const fn result(&self) -> &DecideToolRequestResult {
        &self.result
    }

    /// Returns the command and result for one transaction.
    pub fn into_parts(self) -> (DecideToolRequest, DecideToolRequestResult) {
        (self.command, self.result)
    }
}

/// A command/request adapter correlation error, not a recorded rejection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DecideToolRequestPreparationError {
    command: DecideToolRequest,
    provided_request: ToolRequestId,
}

impl DecideToolRequestPreparationError {
    /// Borrows the unchanged command.
    pub const fn command(&self) -> &DecideToolRequest {
        &self.command
    }

    /// Returns the mismatched request record identity.
    pub const fn provided_request(&self) -> ToolRequestId {
        self.provided_request
    }

    /// Returns both unchanged values.
    pub fn into_parts(self) -> (DecideToolRequest, ToolRequestId) {
        (self.command, self.provided_request)
    }
}
