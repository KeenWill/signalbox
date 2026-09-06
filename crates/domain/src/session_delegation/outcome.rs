//! Session delegation outcome for `docs/spec/sessions-and-transcript.md`.

use super::content::{DelegationContent, delegation_content_digest};
use super::provenance::DelegationProvenance;
use super::terminal_turn::{
    TerminalChildTurn, TerminalChildTurnKind, delegation_content_from_live_completed,
    terminal_from_completed_content,
};
use super::vocabulary::{
    BoundChildAction, DescendantTerminationScope, ParentTerminationAuthority,
    ParentTerminationCommandSource, ParentTerminationKind,
};
use crate::{DurableCommandId, GoalGeneration, SessionId, TurnId};

/// Closed reason vocabulary for relation outcomes and no-change evaluations.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum DelegationOutcomeReason {
    ChildCompleted,
    ChildExecutionFailed,
    ChildResultUnavailable,
    ChildCancelled,
    ParentStopped { scope: DescendantTerminationScope },
    ParentCancelled { scope: DescendantTerminationScope },
}

/// Closed child disposition kind carried by a validated outcome.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum DelegationOutcomeKind {
    ResultReturned,
    ChildFailed,
    ChildStopped,
    ChildCancelled,
    AlreadyTerminal,
    ContinueRunning,
}

/// Validated child disposition with typed reason and sealed provenance.
///
/// Parent-policy construction remains crate-private so an external caller
/// cannot bypass the relationship policy chosen at spawn time:
///
/// ```compile_fail
/// use signalbox_domain::DelegationOutcome;
///
/// let _ = DelegationOutcome::from_parent_policy;
/// ```
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct DelegationOutcome {
    kind: DelegationOutcomeKind,
    content: Option<DelegationContent>,
    reason: DelegationOutcomeReason,
    provenance: DelegationOutcomeProvenance,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum DelegationOutcomeProvenance {
    ChildTurn(TerminalChildTurn),
    ParentCommand(ParentTerminationAuthority),
}

/// Stored proof source supplied to checked delegation-outcome reconstitution.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum DelegationProvenanceReconstitutionInput {
    ChildTurn {
        session: SessionId,
        turn: TurnId,
    },
    ParentTurnCommand {
        session: SessionId,
        turn: TurnId,
        command: DurableCommandId,
    },
    ParentGoalCommand {
        session: SessionId,
        generation: GoalGeneration,
        command: DurableCommandId,
    },
    ParentLifecycleCommand {
        session: SessionId,
        command: DurableCommandId,
    },
}

impl DelegationOutcome {
    /// Derives the complete delivered outcome from a completed child turn's
    /// proof-bearing assistant entries.
    pub fn from_completed_child(value: &crate::CompletedModelCallTurn) -> Self {
        let content = delegation_content_from_live_completed(value);
        let terminal =
            terminal_from_completed_content(value.session(), value.turn(), content.clone());
        Self {
            kind: if content.is_some() {
                DelegationOutcomeKind::ResultReturned
            } else {
                DelegationOutcomeKind::ChildFailed
            },
            content,
            reason: terminal.reason,
            provenance: DelegationOutcomeProvenance::ChildTurn(terminal),
        }
    }

    /// Derives a failed delivered outcome from a failed child turn.
    pub fn from_failed_child(value: &crate::FailedModelCallTurn) -> Self {
        Self::failed_child(TerminalChildTurn::from_failed(value))
    }

    /// Derives an unavailable failed result from a child whose ambiguous model
    /// call required terminal reconciliation.
    pub fn from_reconciliation_required_child(
        value: &crate::ReconciliationRequiredModelCallTurn,
    ) -> Self {
        Self::failed_child(TerminalChildTurn::from_reconciliation_required(value))
    }

    /// Derives a failed delivered outcome from a refused child turn.
    pub fn from_refused_child(value: &crate::RefusedModelCallTurn) -> Self {
        Self::failed_child(TerminalChildTurn::from_refused(value))
    }

    /// Derives a cancelled delivered outcome from a cancelled child turn.
    pub fn from_cancelled_child(value: &crate::CancelledModelCallTurn) -> Self {
        Self::cancelled_child(TerminalChildTurn::from_cancelled(value))
    }

    /// Derives a cancelled delivered outcome from a cancelled tool-round child.
    pub fn from_cancelled_tool_round_child(value: &crate::CancelledToolRoundModelCallTurn) -> Self {
        Self::cancelled_child(TerminalChildTurn::from_cancelled_tool_round(value))
    }

    fn failed_child(terminal: TerminalChildTurn) -> Self {
        Self {
            kind: DelegationOutcomeKind::ChildFailed,
            content: None,
            reason: terminal.reason,
            provenance: DelegationOutcomeProvenance::ChildTurn(terminal),
        }
    }

    fn cancelled_child(terminal: TerminalChildTurn) -> Self {
        Self {
            kind: DelegationOutcomeKind::ChildCancelled,
            content: None,
            reason: terminal.reason,
            provenance: DelegationOutcomeProvenance::ChildTurn(terminal),
        }
    }

    /// Derives a returned, failed, or child-originated cancelled outcome from
    /// exact terminal child evidence. Stopped is not selectable here.
    pub fn from_terminal_child(
        terminal: TerminalChildTurn,
        content: Option<DelegationContent>,
    ) -> Option<Self> {
        let kind = match terminal.kind {
            TerminalChildTurnKind::Returned => match terminal.reason {
                DelegationOutcomeReason::ChildCompleted => match content.as_ref() {
                    Some(content)
                        if terminal.result_digest == Some(delegation_content_digest(content)) =>
                    {
                        DelegationOutcomeKind::ResultReturned
                    }
                    Some(_) | None => return None,
                },
                DelegationOutcomeReason::ChildExecutionFailed
                | DelegationOutcomeReason::ChildResultUnavailable
                | DelegationOutcomeReason::ChildCancelled
                | DelegationOutcomeReason::ParentStopped { .. }
                | DelegationOutcomeReason::ParentCancelled { .. } => return None,
            },
            TerminalChildTurnKind::Failed => match terminal.reason {
                DelegationOutcomeReason::ChildExecutionFailed
                | DelegationOutcomeReason::ChildResultUnavailable => match content.as_ref() {
                    None => DelegationOutcomeKind::ChildFailed,
                    Some(_) => return None,
                },
                DelegationOutcomeReason::ChildCompleted
                | DelegationOutcomeReason::ChildCancelled
                | DelegationOutcomeReason::ParentStopped { .. }
                | DelegationOutcomeReason::ParentCancelled { .. } => return None,
            },
            TerminalChildTurnKind::Cancelled => match terminal.reason {
                DelegationOutcomeReason::ChildCancelled => match content.as_ref() {
                    None => DelegationOutcomeKind::ChildCancelled,
                    Some(_) => return None,
                },
                DelegationOutcomeReason::ChildCompleted
                | DelegationOutcomeReason::ChildExecutionFailed
                | DelegationOutcomeReason::ChildResultUnavailable
                | DelegationOutcomeReason::ParentStopped { .. }
                | DelegationOutcomeReason::ParentCancelled { .. } => return None,
            },
        };
        Some(Self {
            kind,
            content,
            reason: terminal.reason,
            provenance: DelegationOutcomeProvenance::ChildTurn(terminal),
        })
    }

    /// Checks complete stored outcome facts before restoring their sealed proof.
    pub fn reconstitute(
        kind: DelegationOutcomeKind,
        content: Option<DelegationContent>,
        reason: DelegationOutcomeReason,
        provenance: DelegationProvenanceReconstitutionInput,
    ) -> Option<Self> {
        match provenance {
            DelegationProvenanceReconstitutionInput::ChildTurn { session, turn } => {
                let (terminal_kind, result_digest) = match (kind, reason, content.as_ref()) {
                    (
                        DelegationOutcomeKind::ResultReturned,
                        DelegationOutcomeReason::ChildCompleted,
                        Some(content),
                    ) => (
                        TerminalChildTurnKind::Returned,
                        Some(delegation_content_digest(content)),
                    ),
                    (
                        DelegationOutcomeKind::ChildFailed,
                        DelegationOutcomeReason::ChildExecutionFailed
                        | DelegationOutcomeReason::ChildResultUnavailable,
                        None,
                    ) => (TerminalChildTurnKind::Failed, None),
                    (
                        DelegationOutcomeKind::ChildCancelled,
                        DelegationOutcomeReason::ChildCancelled,
                        None,
                    ) => (TerminalChildTurnKind::Cancelled, None),
                    _ => return None,
                };
                Self::from_terminal_child(
                    TerminalChildTurn {
                        session,
                        turn,
                        kind: terminal_kind,
                        reason,
                        result_digest,
                    },
                    content,
                )
            }
            DelegationProvenanceReconstitutionInput::ParentTurnCommand {
                session,
                turn,
                command,
            } => Self::reconstitute_parent_outcome(
                kind,
                content,
                reason,
                session,
                ParentTerminationCommandSource::Turn { turn },
                command,
            ),
            DelegationProvenanceReconstitutionInput::ParentGoalCommand {
                session,
                generation,
                command,
            } => Self::reconstitute_parent_outcome(
                kind,
                content,
                reason,
                session,
                ParentTerminationCommandSource::Goal { generation },
                command,
            ),
            DelegationProvenanceReconstitutionInput::ParentLifecycleCommand {
                session,
                command,
            } => Self::reconstitute_parent_outcome(
                kind,
                content,
                reason,
                session,
                ParentTerminationCommandSource::Lifecycle,
                command,
            ),
        }
    }

    fn reconstitute_parent_outcome(
        kind: DelegationOutcomeKind,
        content: Option<DelegationContent>,
        reason: DelegationOutcomeReason,
        parent: SessionId,
        source: ParentTerminationCommandSource,
        command: DurableCommandId,
    ) -> Option<Self> {
        if content.is_some() {
            return None;
        }
        let termination_kind = match reason {
            DelegationOutcomeReason::ParentStopped {
                scope: DescendantTerminationScope::ParentAndDescendants,
            } => ParentTerminationKind::Stopped,
            DelegationOutcomeReason::ParentCancelled {
                scope: DescendantTerminationScope::ParentAndDescendants,
            } => ParentTerminationKind::Cancelled,
            DelegationOutcomeReason::ChildCompleted
            | DelegationOutcomeReason::ChildExecutionFailed
            | DelegationOutcomeReason::ChildResultUnavailable
            | DelegationOutcomeReason::ChildCancelled
            | DelegationOutcomeReason::ParentStopped {
                scope: DescendantTerminationScope::ParentAlone,
            }
            | DelegationOutcomeReason::ParentCancelled {
                scope: DescendantTerminationScope::ParentAlone,
            } => return None,
        };
        let authority = ParentTerminationAuthority {
            parent,
            source,
            command,
            kind: termination_kind,
            scope: DescendantTerminationScope::ParentAndDescendants,
        };
        match kind {
            DelegationOutcomeKind::AlreadyTerminal => Self::from_parent_already_terminal(authority),
            DelegationOutcomeKind::ContinueRunning => {
                Self::from_parent_policy(authority, BoundChildAction::KeepRunning)
            }
            DelegationOutcomeKind::ChildStopped => {
                Self::from_parent_policy(authority, BoundChildAction::Stop)
            }
            DelegationOutcomeKind::ChildCancelled => {
                Self::from_parent_policy(authority, BoundChildAction::Cancel)
            }
            DelegationOutcomeKind::ResultReturned | DelegationOutcomeKind::ChildFailed => None,
        }
    }

    /// Derives a policy disposition from exact applied parent authority.
    #[allow(dead_code, reason = "consumed by the stacked delegation aggregate")]
    pub(crate) const fn from_parent_policy(
        authority: ParentTerminationAuthority,
        action: BoundChildAction,
    ) -> Option<Self> {
        match authority.scope {
            DescendantTerminationScope::ParentAlone => None,
            DescendantTerminationScope::ParentAndDescendants => {
                let reason = match authority.kind {
                    ParentTerminationKind::Stopped => DelegationOutcomeReason::ParentStopped {
                        scope: DescendantTerminationScope::ParentAndDescendants,
                    },
                    ParentTerminationKind::Cancelled => DelegationOutcomeReason::ParentCancelled {
                        scope: DescendantTerminationScope::ParentAndDescendants,
                    },
                };
                let kind = match action {
                    BoundChildAction::KeepRunning => DelegationOutcomeKind::ContinueRunning,
                    BoundChildAction::Stop => DelegationOutcomeKind::ChildStopped,
                    BoundChildAction::Cancel => DelegationOutcomeKind::ChildCancelled,
                };
                Some(Self {
                    kind,
                    content: None,
                    reason,
                    provenance: DelegationOutcomeProvenance::ParentCommand(authority),
                })
            }
        }
    }

    /// Records that an evaluated descendant edge was already terminal.
    ///
    /// The relationship aggregate calls this only after resolving the
    /// relationship's unique immutable child result. That result remains the
    /// authority for the prior terminal state; this disposition records the
    /// exact parent command that evaluated the edge without fabricating a
    /// second child result.
    #[allow(dead_code, reason = "consumed by the stacked delegation aggregate")]
    pub(crate) const fn from_parent_already_terminal(
        authority: ParentTerminationAuthority,
    ) -> Option<Self> {
        match authority.scope {
            DescendantTerminationScope::ParentAlone => None,
            DescendantTerminationScope::ParentAndDescendants => {
                let reason = match authority.kind {
                    ParentTerminationKind::Stopped => DelegationOutcomeReason::ParentStopped {
                        scope: DescendantTerminationScope::ParentAndDescendants,
                    },
                    ParentTerminationKind::Cancelled => DelegationOutcomeReason::ParentCancelled {
                        scope: DescendantTerminationScope::ParentAndDescendants,
                    },
                };
                Some(Self {
                    kind: DelegationOutcomeKind::AlreadyTerminal,
                    content: None,
                    reason,
                    provenance: DelegationOutcomeProvenance::ParentCommand(authority),
                })
            }
        }
    }

    pub const fn kind(&self) -> DelegationOutcomeKind {
        self.kind
    }

    pub const fn content(&self) -> Option<&DelegationContent> {
        self.content.as_ref()
    }

    pub const fn reason(&self) -> DelegationOutcomeReason {
        self.reason
    }

    pub const fn provenance(&self) -> DelegationProvenance {
        match self.provenance {
            DelegationOutcomeProvenance::ChildTurn(terminal) => {
                DelegationProvenance::from_terminal_child(terminal)
            }
            DelegationOutcomeProvenance::ParentCommand(authority) => {
                DelegationProvenance::from_parent_termination(authority)
            }
        }
    }

    pub const fn reconstitution_provenance(&self) -> DelegationProvenanceReconstitutionInput {
        match self.provenance {
            DelegationOutcomeProvenance::ChildTurn(terminal) => {
                DelegationProvenanceReconstitutionInput::ChildTurn {
                    session: terminal.session(),
                    turn: terminal.turn(),
                }
            }
            DelegationOutcomeProvenance::ParentCommand(authority) => match authority.source() {
                ParentTerminationCommandSource::Turn { turn } => {
                    DelegationProvenanceReconstitutionInput::ParentTurnCommand {
                        session: authority.parent(),
                        turn,
                        command: authority.command(),
                    }
                }
                ParentTerminationCommandSource::Goal { generation } => {
                    DelegationProvenanceReconstitutionInput::ParentGoalCommand {
                        session: authority.parent(),
                        generation,
                        command: authority.command(),
                    }
                }
                ParentTerminationCommandSource::Lifecycle => {
                    DelegationProvenanceReconstitutionInput::ParentLifecycleCommand {
                        session: authority.parent(),
                        command: authority.command(),
                    }
                }
            },
        }
    }
}
