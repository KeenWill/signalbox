//! Tool approval for `docs/spec/tool-loop.md`.

use super::decide::{DecideToolRequestResult, PreparedDecideToolRequest};
use super::policy::{
    DangerousToolAutoApproval, DelegateApprovalRecommendation, DelegateToolApproval,
    ToolApprovalDecider, ToolApprovalPosture, ToolDecisionRationale, ToolDecisionSource,
    ToolDenialReason,
};
use crate::{DurableCommandId, ToolRequestId};

pub(super) const SUPPRESSED_TOOL_DENIAL_REASON: &str =
    "Tool arguments were suppressed by the credential boundary";

/// One durable logical approval decision.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum ToolApprovalDecision {
    /// Execution is permitted subject to current aggregate guards.
    Approve,
    /// Execution is permanently prohibited for this request.
    Deny {
        /// Optional bounded denial explanation rendered to the model; its
        /// author — user or judge — follows from the decision source.
        reason: Option<ToolDenialReason>,
    },
}

/// One request-bound approval resolution with explicit provenance.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ToolApprovalResolution {
    request: ToolRequestId,
    decision: ToolApprovalDecision,
    source: ToolDecisionSource,
    decider: Option<ToolApprovalDecider>,
    rationale: Option<ToolDecisionRationale>,
}

impl ToolApprovalResolution {
    pub(crate) const fn policy_auto(request: ToolRequestId) -> Self {
        Self {
            request,
            decision: ToolApprovalDecision::Approve,
            source: ToolDecisionSource::PolicyAuto,
            decider: None,
            rationale: None,
        }
    }

    pub(crate) const fn session_blanket(request: ToolRequestId) -> Self {
        Self {
            request,
            decision: ToolApprovalDecision::Approve,
            source: ToolDecisionSource::SessionBlanket,
            decider: None,
            rationale: None,
        }
    }

    pub(crate) fn runtime_safety(request: ToolRequestId) -> Self {
        Self {
            request,
            decision: ToolApprovalDecision::Deny {
                reason: Some(ToolDenialReason(String::from(
                    SUPPRESSED_TOOL_DENIAL_REASON,
                ))),
            },
            source: ToolDecisionSource::RuntimeSafety,
            decider: None,
            rationale: None,
        }
    }

    /// Constructs the fixed denial recorded when a human wait expires.
    pub fn approval_timeout(request: ToolRequestId) -> Self {
        Self {
            request,
            decision: ToolApprovalDecision::Deny {
                reason: Some(ToolDenialReason(String::from("approval_wait_timeout"))),
            },
            source: ToolDecisionSource::RuntimeSafety,
            decider: None,
            rationale: None,
        }
    }

    pub(crate) const fn lifecycle_closure(request: ToolRequestId) -> Self {
        Self {
            request,
            decision: ToolApprovalDecision::Deny { reason: None },
            source: ToolDecisionSource::LifecycleClosure,
            decider: None,
            rationale: None,
        }
    }

    pub(super) fn user(
        command: DurableCommandId,
        request: ToolRequestId,
        decision: ToolApprovalDecision,
    ) -> Self {
        Self {
            request,
            decision,
            source: ToolDecisionSource::UserCommand,
            decider: Some(ToolApprovalDecider::User { command }),
            rationale: None,
        }
    }

    pub(crate) const fn user_override(
        request: ToolRequestId,
        command: DurableCommandId,
        denied_request: ToolRequestId,
    ) -> Self {
        Self {
            request,
            decision: ToolApprovalDecision::Approve,
            source: ToolDecisionSource::UserOverride,
            decider: Some(ToolApprovalDecider::UserOverride {
                command,
                denied_request,
            }),
            rationale: None,
        }
    }

    pub(crate) fn delegate(approval: &DelegateToolApproval) -> Option<Self> {
        let decision = match approval.recommendation {
            DelegateApprovalRecommendation::Approve => ToolApprovalDecision::Approve,
            DelegateApprovalRecommendation::Deny => ToolApprovalDecision::Deny {
                reason: ToolDenialReason::from_rationale(&approval.rationale),
            },
            DelegateApprovalRecommendation::EscalateToHuman => return None,
        };
        Some(Self {
            request: approval.request,
            decision,
            source: ToolDecisionSource::Delegate,
            decider: Some(ToolApprovalDecider::Delegate {
                model: approval.model,
                call: approval.call,
            }),
            rationale: Some(approval.rationale.clone()),
        })
    }

    /// Returns the resolved request.
    pub const fn request(&self) -> ToolRequestId {
        self.request
    }

    /// Borrows the exact decision.
    pub const fn decision(&self) -> &ToolApprovalDecision {
        &self.decision
    }

    /// Returns the provenance that made the decision.
    pub const fn source(&self) -> ToolDecisionSource {
        self.source
    }

    /// Returns the explicit decider, absent only for automatic policy.
    pub const fn decider(&self) -> Option<&ToolApprovalDecider> {
        self.decider.as_ref()
    }

    /// Returns the delegate rationale, when a delegate decided.
    pub const fn rationale(&self) -> Option<&ToolDecisionRationale> {
        self.rationale.as_ref()
    }

    /// Returns whether this resolution permits an attempt.
    pub const fn is_approved(&self) -> bool {
        matches!(self.decision, ToolApprovalDecision::Approve)
    }
}

/// Independently stored approval evidence supplied for checked
/// reconstitution.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolApprovalResolutionReconstitutionInput {
    evidence: StoredToolApprovalEvidence,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum StoredToolApprovalEvidence {
    UserCommand(PreparedDecideToolRequest),
    Delegate {
        approval: Box<DelegateToolApproval>,
        stored_denial_reason: Option<ToolDenialReason>,
    },
    PolicyAuto(ToolRequestId),
    SessionBlanket {
        request: ToolRequestId,
        frozen_posture: DangerousToolAutoApproval,
    },
    RuntimeSafety(ToolRequestId),
    LifecycleClosure(ToolRequestId),
    UserOverride {
        request: ToolRequestId,
        command: DurableCommandId,
        denied_request: ToolRequestId,
        frozen_posture: ToolApprovalPosture,
    },
}

impl ToolApprovalResolutionReconstitutionInput {
    /// Supplies the exact applied user command that owns one stored decision.
    pub const fn user_command(command: PreparedDecideToolRequest) -> Self {
        Self {
            evidence: StoredToolApprovalEvidence::UserCommand(command),
        }
    }

    /// Supplies one authority-checked delegate decision, its recorded call,
    /// and the denial reason exactly as stored beside the decision.
    ///
    /// Reconstitution requires the stored reason to equal the derivation
    /// from the recorded rationale — null exactly when the rationale
    /// derives nothing — so a row missing its current evidence fails closed
    /// instead of restoring as an unexplained denial.
    pub fn delegate(
        approval: DelegateToolApproval,
        stored_denial_reason: Option<ToolDenialReason>,
    ) -> Self {
        Self {
            evidence: StoredToolApprovalEvidence::Delegate {
                approval: Box::new(approval),
                stored_denial_reason,
            },
        }
    }

    /// Supplies one request-bound registry-policy approval.
    pub const fn policy_auto(request: ToolRequestId) -> Self {
        Self {
            evidence: StoredToolApprovalEvidence::PolicyAuto(request),
        }
    }

    /// Supplies one request-bound session-blanket approval and the exact
    /// dangerous posture frozen for its turn.
    pub const fn session_blanket(
        request: ToolRequestId,
        frozen_posture: DangerousToolAutoApproval,
    ) -> Self {
        Self {
            evidence: StoredToolApprovalEvidence::SessionBlanket {
                request,
                frozen_posture,
            },
        }
    }

    /// Supplies one credential-boundary safety denial.
    pub const fn runtime_safety(request: ToolRequestId) -> Self {
        Self {
            evidence: StoredToolApprovalEvidence::RuntimeSafety(request),
        }
    }

    /// Supplies one denial caused by a committed session closure.
    pub const fn lifecycle_closure(request: ToolRequestId) -> Self {
        Self {
            evidence: StoredToolApprovalEvidence::LifecycleClosure(request),
        }
    }

    /// Supplies one request-bound consumed user override, its recorded
    /// command and overridden request, and the exact approval posture frozen
    /// on the approved request.
    pub const fn user_override(
        request: ToolRequestId,
        command: DurableCommandId,
        denied_request: ToolRequestId,
        frozen_posture: ToolApprovalPosture,
    ) -> Self {
        Self {
            evidence: StoredToolApprovalEvidence::UserOverride {
                request,
                command,
                denied_request,
                frozen_posture,
            },
        }
    }

    /// Checks source-specific evidence before restoring execution authority.
    pub fn reconstitute(
        self,
    ) -> Result<ToolApprovalResolution, ToolApprovalResolutionReconstitutionError> {
        let resolution = match &self.evidence {
            StoredToolApprovalEvidence::UserCommand(command) => match command.result() {
                DecideToolRequestResult::Applied(applied)
                    if command.command().request() == applied.resolution().request()
                        && applied.resolution().source() == ToolDecisionSource::UserCommand =>
                {
                    Some(applied.resolution().clone())
                }
                DecideToolRequestResult::Applied(_) | DecideToolRequestResult::Rejected(_) => None,
            },
            StoredToolApprovalEvidence::Delegate {
                approval,
                stored_denial_reason,
            } => ToolApprovalResolution::delegate(approval).and_then(|resolution| {
                // The stored reason must equal the derivation exactly — a
                // null admitted only when the rationale derives nothing — so
                // a row missing its current evidence is corruption, never an
                // unexplained denial.
                match &resolution.decision {
                    ToolApprovalDecision::Approve => {
                        stored_denial_reason.is_none().then_some(resolution)
                    }
                    ToolApprovalDecision::Deny { reason } => {
                        (reason == stored_denial_reason).then_some(resolution)
                    }
                }
            }),
            StoredToolApprovalEvidence::PolicyAuto(request) => {
                Some(ToolApprovalResolution::policy_auto(*request))
            }
            StoredToolApprovalEvidence::SessionBlanket {
                request,
                frozen_posture: DangerousToolAutoApproval::ApproveAll,
            } => Some(ToolApprovalResolution::session_blanket(*request)),
            StoredToolApprovalEvidence::SessionBlanket {
                frozen_posture: DangerousToolAutoApproval::Disabled,
                ..
            } => None,
            StoredToolApprovalEvidence::RuntimeSafety(request) => {
                Some(ToolApprovalResolution::runtime_safety(*request))
            }
            StoredToolApprovalEvidence::LifecycleClosure(request) => {
                Some(ToolApprovalResolution::lifecycle_closure(*request))
            }
            StoredToolApprovalEvidence::UserOverride {
                request,
                command,
                denied_request,
                frozen_posture: ToolApprovalPosture::Delegated,
            } => Some(ToolApprovalResolution::user_override(
                *request,
                *command,
                *denied_request,
            )),
            StoredToolApprovalEvidence::UserOverride {
                frozen_posture: ToolApprovalPosture::Auto | ToolApprovalPosture::Human,
                ..
            } => None,
        };
        match resolution {
            Some(resolution) => Ok(resolution),
            None => Err(ToolApprovalResolutionReconstitutionError {
                input: Box::new(self),
            }),
        }
    }
}

/// Stored approval facts outside the implemented producer vocabulary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolApprovalResolutionReconstitutionError {
    input: Box<ToolApprovalResolutionReconstitutionInput>,
}

impl ToolApprovalResolutionReconstitutionError {
    /// Borrows the unchanged stored facts.
    pub const fn input(&self) -> &ToolApprovalResolutionReconstitutionInput {
        &self.input
    }

    /// Returns the unchanged stored facts.
    pub fn into_input(self) -> ToolApprovalResolutionReconstitutionInput {
        *self.input
    }
}

impl std::fmt::Display for ToolApprovalResolutionReconstitutionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("stored tool approval evidence cannot be reconstituted")
    }
}

impl std::error::Error for ToolApprovalResolutionReconstitutionError {}

/// One initial policy outcome for a newly proposed request.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum InitialToolApproval {
    /// Leave the request undecided and fail closed.
    Confirm,
    /// Leave an `AlwaysConfirm` request undecided despite blanket posture.
    AlwaysConfirm,
    /// Leave the request parked for an explicitly human-only decision.
    Human,
    /// Leave the request parked for a delegate judge.
    Delegated,
    /// Record automatic approval from registry policy.
    PolicyAuto,
    /// Record automatic approval from the frozen dangerous blanket.
    SessionBlanket,
    /// Record an automatic denial for credential-suppressed arguments.
    RuntimeSafetyDeny,
    /// Record approval consumed from a user-recorded override of one exact
    /// delegate denial instead of parking for the judge again.
    UserOverride {
        /// The applied durable override command.
        command: DurableCommandId,
        /// The delegate-denied request whose recorded override this proposal
        /// consumes.
        denied_request: ToolRequestId,
    },
}

impl InitialToolApproval {
    pub(crate) fn resolution(self, request: ToolRequestId) -> Option<ToolApprovalResolution> {
        match self {
            Self::Confirm | Self::AlwaysConfirm | Self::Human | Self::Delegated => None,
            Self::PolicyAuto => Some(ToolApprovalResolution::policy_auto(request)),
            Self::SessionBlanket => Some(ToolApprovalResolution::session_blanket(request)),
            Self::RuntimeSafetyDeny => Some(ToolApprovalResolution::runtime_safety(request)),
            Self::UserOverride {
                command,
                denied_request,
            } => Some(ToolApprovalResolution::user_override(
                request,
                command,
                denied_request,
            )),
        }
    }

    pub(crate) const fn posture(self) -> ToolApprovalPosture {
        match self {
            Self::Confirm | Self::AlwaysConfirm | Self::Human => ToolApprovalPosture::Human,
            Self::Delegated | Self::UserOverride { .. } => ToolApprovalPosture::Delegated,
            Self::PolicyAuto | Self::SessionBlanket | Self::RuntimeSafetyDeny => {
                ToolApprovalPosture::Auto
            }
        }
    }

    /// Returns whether this outcome leaves an explicit decision outstanding.
    pub const fn requires_decision(self) -> bool {
        match self {
            Self::Confirm | Self::AlwaysConfirm | Self::Human | Self::Delegated => true,
            Self::PolicyAuto
            | Self::SessionBlanket
            | Self::RuntimeSafetyDeny
            | Self::UserOverride { .. } => false,
        }
    }
}
