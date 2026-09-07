//! Tool override denial for `docs/spec/tool-loop.md`.

#[cfg(doc)]
use super::decide::DecideToolRequest;

use super::approval::{ToolApprovalDecision, ToolApprovalResolution};
use super::arguments::NormalizedToolArguments;
use super::name::ToolName;
use super::policy::ToolApprovalDecider;
use super::proposal::ToolCallProposal;
use super::request::ToolRequest;
use super::result::ToolRequestResolution;
use crate::{DurableCommandId, ModelCallId, SessionId, ToolRequestId};

/// One recorded, not-yet-consumed user override of a delegate denial.
///
/// The override pre-approves exactly one future proposal in the owning
/// session: the first one whose tool name and normalized arguments equal the
/// denied request's. It links the denied request, the judge call that denied
/// it, and the user command that recorded the override, so the full audit chain
/// stays queryable from any of the three.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct RecordedUserOverride {
    command: DurableCommandId,
    session: SessionId,
    denied_request: ToolRequestId,
    judge_call: ModelCallId,
    tool: ToolName,
    arguments: NormalizedToolArguments,
}

impl RecordedUserOverride {
    /// Supplies all typed stored facts of one recorded override.
    pub const fn new(
        command: DurableCommandId,
        session: SessionId,
        denied_request: ToolRequestId,
        judge_call: ModelCallId,
        tool: ToolName,
        arguments: NormalizedToolArguments,
    ) -> Self {
        Self {
            command,
            session,
            denied_request,
            judge_call,
            tool,
            arguments,
        }
    }

    /// Returns the applied durable override command.
    pub const fn command(&self) -> DurableCommandId {
        self.command
    }

    /// Returns the session whose future proposal may consume this override.
    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// Returns the delegate-denied request the override names.
    pub const fn denied_request(&self) -> ToolRequestId {
        self.denied_request
    }

    /// Returns the completed judge call that denied the request.
    pub const fn judge_call(&self) -> ModelCallId {
        self.judge_call
    }

    /// Borrows the denied request's checked tool name.
    pub const fn tool(&self) -> &ToolName {
        &self.tool
    }

    /// Borrows the denied request's normalized arguments.
    pub const fn arguments(&self) -> &NormalizedToolArguments {
        &self.arguments
    }

    /// Returns whether the proposal re-proposes the exact denied command:
    /// equal tool name and equal normalized arguments.
    pub fn matches_proposal(&self, proposal: &ToolCallProposal) -> bool {
        self.tool == *proposal.name() && self.arguments == *proposal.arguments()
    }
}

/// The canonical user command overriding one delegate-denied tool request.
///
/// Applying the command records one one-shot pre-approval in the named session:
/// the next proposal of the exact denied command is approved under
/// user-override provenance instead of parking for the judge again. Unlike
/// [`DecideToolRequest`], the session is part of the canonical payload,
/// because the recorded override is a session-scoped standing fact consumed by a
/// later proposal rather than a decision on an already-parked request.
#[derive(Clone, Debug)]
pub struct OverrideDeniedToolRequest {
    command_id: DurableCommandId,
    session: SessionId,
    denied_request: ToolRequestId,
}

impl OverrideDeniedToolRequest {
    /// Constructs the complete canonical caller payload after rejecting the
    /// user-global nil and max command sentinels.
    pub fn try_new(
        command_id: DurableCommandId,
        session: SessionId,
        denied_request: ToolRequestId,
    ) -> Result<Self, OverrideDeniedToolRequestConstructionError> {
        if command_id.as_uuid().is_nil() || command_id.as_uuid().is_max() {
            return Err(OverrideDeniedToolRequestConstructionError { command_id });
        }
        Ok(Self {
            command_id,
            session,
            denied_request,
        })
    }

    /// Returns the user-global command identity.
    pub const fn command_id(&self) -> DurableCommandId {
        self.command_id
    }

    /// Returns the session the override covers.
    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// Returns the exact delegate-denied request named by the command.
    pub const fn denied_request(&self) -> ToolRequestId {
        self.denied_request
    }

    /// Verifies the named request admits a user override and prepares the
    /// terminal typed result.
    ///
    /// This is the override verification predicate. Recording requires every
    /// conjunct, each with its own typed rejection:
    ///
    /// - the recorded approval is a delegate denial, so a user denial, any approval, or an
    ///   undecided request cannot be overridden;
    /// - the denial is terminal — its denied-result entry is materialized — so a denial whose round
    ///   is still resolving cannot be overridden;
    /// - the request belongs to the command's session, so an override can never pre-approve a
    ///   proposal in another session; and
    /// - no override is already recorded for the request, so each denial admits at most one
    ///   override ever.
    pub fn prepare(
        self,
        request: &ToolRequest,
        approval: Option<&ToolApprovalResolution>,
        terminal_resolution: Option<ToolRequestResolution>,
        existing_override_command: Option<DurableCommandId>,
    ) -> Result<PreparedOverrideDeniedToolRequest, OverrideDeniedToolRequestPreparationError> {
        if request.id() != self.denied_request {
            return Err(OverrideDeniedToolRequestPreparationError {
                command: self,
                provided_request: request.id(),
            });
        }
        if let Some(approval) = approval
            && approval.request() != self.denied_request
        {
            let provided_request = approval.request();
            return Err(OverrideDeniedToolRequestPreparationError {
                command: self,
                provided_request,
            });
        }
        if request.session() != self.session {
            return Ok(self.prepare_request_not_in_session());
        }
        // Delegate-denied: the decision is a denial and its decider is the
        // judge. The sealed resolution producers make a delegate decider
        // equivalent to the delegate source, so the decider is the checked
        // fact and also supplies the judge call the recorded override links.
        let judge_call = match approval {
            Some(approval) if matches!(approval.decision(), ToolApprovalDecision::Deny { .. }) => {
                match approval.decider() {
                    Some(ToolApprovalDecider::Delegate { call, .. }) => Some(*call),
                    Some(
                        ToolApprovalDecider::User { .. } | ToolApprovalDecider::UserOverride { .. },
                    )
                    | None => None,
                }
            }
            Some(_) | None => None,
        };
        let Some(judge_call) = judge_call else {
            return Ok(self.prepare_not_delegate_denied());
        };
        let terminally_denied = matches!(
            terminal_resolution,
            Some(ToolRequestResolution::Denied { request }) if request == self.denied_request
        );
        if !terminally_denied {
            return Ok(self.prepare_not_terminally_denied());
        }
        if existing_override_command.is_some() {
            return Ok(self.prepare_already_overridden());
        }
        let recorded = RecordedUserOverride::new(
            self.command_id,
            self.session,
            self.denied_request,
            judge_call,
            request.name().clone(),
            request.arguments().clone(),
        );
        Ok(PreparedOverrideDeniedToolRequest {
            command: self,
            result: OverrideDeniedToolRequestResult::Applied(
                OverrideDeniedToolRequestAppliedResult { recorded },
            ),
        })
    }

    /// Restores the exact recorded applied receipt from its durable recorded
    /// row, rejecting a row that does not correlate with this command.
    pub fn reconstitute_applied(
        self,
        recorded: RecordedUserOverride,
    ) -> Result<PreparedOverrideDeniedToolRequest, OverrideDeniedToolRequestPreparationError> {
        if recorded.command() != self.command_id
            || recorded.session() != self.session
            || recorded.denied_request() != self.denied_request
        {
            let provided_request = recorded.denied_request();
            return Err(OverrideDeniedToolRequestPreparationError {
                command: self,
                provided_request,
            });
        }
        Ok(PreparedOverrideDeniedToolRequest {
            command: self,
            result: OverrideDeniedToolRequestResult::Applied(
                OverrideDeniedToolRequestAppliedResult { recorded },
            ),
        })
    }

    /// Prepares an authoritative missing-request rejection.
    pub const fn prepare_request_not_found(self) -> PreparedOverrideDeniedToolRequest {
        let denied_request = self.denied_request;
        PreparedOverrideDeniedToolRequest {
            command: self,
            result: OverrideDeniedToolRequestResult::Rejected(
                OverrideDeniedToolRequestRejectedResult::RequestNotFound { denied_request },
            ),
        }
    }

    /// Prepares an authoritative other-session rejection.
    pub const fn prepare_request_not_in_session(self) -> PreparedOverrideDeniedToolRequest {
        let session = self.session;
        let denied_request = self.denied_request;
        PreparedOverrideDeniedToolRequest {
            command: self,
            result: OverrideDeniedToolRequestResult::Rejected(
                OverrideDeniedToolRequestRejectedResult::RequestNotInSession {
                    session,
                    denied_request,
                },
            ),
        }
    }

    /// Prepares an authoritative not-delegate-denied rejection.
    pub const fn prepare_not_delegate_denied(self) -> PreparedOverrideDeniedToolRequest {
        let denied_request = self.denied_request;
        PreparedOverrideDeniedToolRequest {
            command: self,
            result: OverrideDeniedToolRequestResult::Rejected(
                OverrideDeniedToolRequestRejectedResult::NotDelegateDenied { denied_request },
            ),
        }
    }

    /// Prepares an authoritative still-resolving rejection.
    pub const fn prepare_not_terminally_denied(self) -> PreparedOverrideDeniedToolRequest {
        let denied_request = self.denied_request;
        PreparedOverrideDeniedToolRequest {
            command: self,
            result: OverrideDeniedToolRequestResult::Rejected(
                OverrideDeniedToolRequestRejectedResult::NotTerminallyDenied { denied_request },
            ),
        }
    }

    /// Prepares an authoritative already-overridden rejection.
    pub const fn prepare_already_overridden(self) -> PreparedOverrideDeniedToolRequest {
        let denied_request = self.denied_request;
        PreparedOverrideDeniedToolRequest {
            command: self,
            result: OverrideDeniedToolRequestResult::Rejected(
                OverrideDeniedToolRequestRejectedResult::AlreadyOverridden { denied_request },
            ),
        }
    }
}

impl PartialEq for OverrideDeniedToolRequest {
    fn eq(&self, other: &Self) -> bool {
        self.session == other.session && self.denied_request == other.denied_request
    }
}

impl Eq for OverrideDeniedToolRequest {}

impl std::hash::Hash for OverrideDeniedToolRequest {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.session.hash(state);
        self.denied_request.hash(state);
    }
}

/// An override command used a reserved user-global identity.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OverrideDeniedToolRequestConstructionError {
    command_id: DurableCommandId,
}

impl OverrideDeniedToolRequestConstructionError {
    /// Returns the rejected command identity.
    pub const fn command_id(self) -> DurableCommandId {
        self.command_id
    }
}

/// Terminal typed result for one override command.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum OverrideDeniedToolRequestResult {
    /// The override was recorded.
    Applied(OverrideDeniedToolRequestAppliedResult),
    /// Authoritative current state rejected the command.
    Rejected(OverrideDeniedToolRequestRejectedResult),
}

/// The recorded override and its complete linked provenance.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct OverrideDeniedToolRequestAppliedResult {
    recorded: RecordedUserOverride,
}

impl OverrideDeniedToolRequestAppliedResult {
    /// Borrows the exact recorded override.
    pub const fn recorded(&self) -> &RecordedUserOverride {
        &self.recorded
    }
}

/// Closed authoritative rejection vocabulary for override commands.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum OverrideDeniedToolRequestRejectedResult {
    /// No logical request had this identity.
    RequestNotFound {
        /// The absent request.
        denied_request: ToolRequestId,
    },
    /// The named request belongs to another session.
    RequestNotInSession {
        /// The session the command named.
        session: SessionId,
        /// The request owned elsewhere.
        denied_request: ToolRequestId,
    },
    /// The request's recorded approval is not a delegate denial.
    NotDelegateDenied {
        /// The request without a delegate denial.
        denied_request: ToolRequestId,
    },
    /// The delegate denial has not reached its terminal denied result.
    NotTerminallyDenied {
        /// The request whose denial is still resolving.
        denied_request: ToolRequestId,
    },
    /// An override is already recorded for this denial.
    AlreadyOverridden {
        /// The already-overridden request.
        denied_request: ToolRequestId,
    },
}

/// A pre-commit override-command candidate.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreparedOverrideDeniedToolRequest {
    command: OverrideDeniedToolRequest,
    result: OverrideDeniedToolRequestResult,
}

impl PreparedOverrideDeniedToolRequest {
    /// Borrows the canonical command.
    pub const fn command(&self) -> &OverrideDeniedToolRequest {
        &self.command
    }

    /// Borrows the terminal typed result.
    pub const fn result(&self) -> &OverrideDeniedToolRequestResult {
        &self.result
    }

    /// Returns the command and result for one transaction.
    pub fn into_parts(self) -> (OverrideDeniedToolRequest, OverrideDeniedToolRequestResult) {
        (self.command, self.result)
    }
}

/// A command/request adapter correlation error, not a recorded rejection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OverrideDeniedToolRequestPreparationError {
    command: OverrideDeniedToolRequest,
    provided_request: ToolRequestId,
}

impl OverrideDeniedToolRequestPreparationError {
    /// Borrows the unchanged command.
    pub const fn command(&self) -> &OverrideDeniedToolRequest {
        &self.command
    }

    /// Returns the mismatched supplied-evidence request identity.
    pub const fn provided_request(&self) -> ToolRequestId {
        self.provided_request
    }

    /// Returns both unchanged values.
    pub fn into_parts(self) -> (OverrideDeniedToolRequest, ToolRequestId) {
        (self.command, self.provided_request)
    }
}
