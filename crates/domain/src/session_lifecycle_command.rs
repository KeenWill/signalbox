//! The session-lifecycle command surface (docs/spec/session-lifecycle.md):
//! the lifecycle operations one durable command family carries, the finish
//! condition an owned session owes, the verdict a finish check returns, and
//! the authenticated principal every command envelope records.

use crate::{
    Actor, DescendantTerminationScope, DispatchingModule, DurableCommandId,
    FinishConditionStatement, LifecycleActor, SessionConfigurationDefaultsVersion,
    SessionFailureCause, SessionId, SessionLifecycleState, SessionTerminalOutcome, StopStickiness,
    TurnId,
};

/// The principal a command envelope carries, authenticated at the boundary
/// that admitted the command.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum CommandPrincipal {
    /// Daemon machinery issued the command.
    Core,
    /// The single user's authority, however connected.
    Operator,
    /// One exact module.
    Module {
        /// The module that issued the command.
        module: DispatchingModule,
    },
    /// The recovery scan or liveness watchdog.
    Watchdog,
}

impl CommandPrincipal {
    /// Returns the principal a command carrying this domain actor is issued
    /// under when no module composed it.
    pub const fn for_actor(actor: Actor) -> Self {
        match actor {
            Actor::User => Self::Operator,
            Actor::Core => Self::Core,
            Actor::Recovery => Self::Watchdog,
            Actor::Model { .. } | Actor::Tool { .. } => Self::Core,
        }
    }

    /// Classifies a command's agency from its envelope principal and the
    /// domain actor it carries, when it carries one.
    ///
    /// A module principal is the classification; otherwise the domain actor
    /// decides, and a command with no actor field reads as its principal.
    pub const fn classify(self, actor: Option<Actor>) -> LifecycleActor {
        match (self, actor) {
            (Self::Module { module }, _) => LifecycleActor::Module { module },
            (_, Some(actor)) => LifecycleActor::classify(actor),
            (Self::Core, None) => LifecycleActor::Core {
                agency: crate::CoreAgency::Daemon,
            },
            (Self::Operator, None) => LifecycleActor::Operator,
            (Self::Watchdog, None) => LifecycleActor::Watchdog,
        }
    }
}

/// Whether a creation holds its start gate.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum StartGate {
    /// The session may dispatch as soon as it has input.
    Open,
    /// The session stays `created` until `release_start` or gate expiry.
    Held,
}

/// The finish check a session's achievement is gated on.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum FinishCondition {
    /// The external gate the dispatch serves, re-checked on the exact head.
    ExternalGate,
    /// A condition declared at creation or adoption.
    Declared(FinishConditionStatement),
}

/// What a finish check decided about one achievement declaration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FinishCheckVerdict {
    /// The declared finish condition holds.
    Passed,
    /// The declared finish condition does not hold.
    Failed {
        /// The check result, surfaced as need text.
        detail: String,
    },
    /// The session carries no finish condition, or no verifier exists for
    /// the one it carries: the achievement is declared, never verified.
    Unverified,
}

/// One of the lifecycle operations.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum SessionLifecycleOperation {
    /// Open a held start gate so queued admission work may dispatch.
    ReleaseStart,
    /// Close the session `stopped{sticky}` from any non-terminal state.
    Stop {
        /// Whether re-dispatch stays suppressed.
        sticky: StopStickiness,
        /// Whether stopping also terminates delegated descendants.
        descendant_scope: DescendantTerminationScope,
    },
    /// Close the session `superseded{by}` in favour of its successor.
    Supersede {
        /// The session that takes the work.
        successor: SessionId,
    },
    /// Write off a parked session as `abandoned`.
    Abandon,
    /// Close a parked session as failed with its standing cause.
    CloseFailed {
        /// The cause to record; `None` closes with the park's standing cause,
        /// or `failed_unknown` when it holds none.
        cause: Option<SessionFailureCause>,
    },
    /// Return a parked session whose goal is not blocked to its mapped state.
    Resume,
    /// Take the liveness obligation, declaring the finish condition the
    /// session owes when it carries none.
    Adopt {
        /// The finish condition to declare.
        finish_condition: Option<FinishCondition>,
    },
    /// Drop the liveness obligation.
    Release,
}

/// One durable session-lifecycle command.
#[derive(Clone, Debug)]
pub struct SessionLifecycleCommand {
    command_id: DurableCommandId,
    session: SessionId,
    operation: SessionLifecycleOperation,
}

impl SessionLifecycleCommand {
    /// Creates the complete command payload.
    pub const fn new(
        command_id: DurableCommandId,
        session: SessionId,
        operation: SessionLifecycleOperation,
    ) -> Self {
        Self {
            command_id,
            session,
            operation,
        }
    }

    /// Returns the user-global durable command identity.
    pub const fn command_id(&self) -> DurableCommandId {
        self.command_id
    }

    /// Returns the target session.
    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// Borrows the operation.
    pub const fn operation(&self) -> &SessionLifecycleOperation {
        &self.operation
    }
}

/// Replay equality covers every caller-supplied semantic field except the
/// identifier itself.
impl PartialEq for SessionLifecycleCommand {
    fn eq(&self, other: &Self) -> bool {
        self.session == other.session && self.operation == other.operation
    }
}

impl Eq for SessionLifecycleCommand {}

impl std::hash::Hash for SessionLifecycleCommand {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.session.hash(state);
        self.operation.hash(state);
    }
}

/// The closed rejection vocabulary a claimed lifecycle command records.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum SessionLifecycleCommandRejection {
    /// The target session does not exist.
    SessionNotFound,
    /// The lifecycle algebra does not admit the transition from the held state.
    TransitionNotAdmitted,
    /// The operation closes or resumes a park, and the session is not parked.
    RequiresParked,
    /// `release` on a parked session.
    ReleaseWhileParked,
    /// The session already holds the ownership the flip would install.
    OwnershipUnchanged,
    /// An adopt declares a finish condition the session already carries.
    FinishConditionAlreadyDeclared,
    /// A failed closure names a cause the park does not hold.
    StandingCauseMismatch,
    /// A supersession names a successor that does not exist.
    SuccessorNotFound,
    /// A supersession names the session itself.
    SuccessorIsSelf,
    /// A parked session with a blocked goal resumes through the goal command.
    GoalResumeRequired,
    /// The outcome contradicts the terminal state the goal already recorded.
    GoalOutcomeMismatch,
    /// A different terminal outcome is already committed to the settlement.
    PendingTerminalConflict,
}

/// What an applied lifecycle command did.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionLifecycleApplication {
    /// The held start gate opened.
    StartReleased,
    /// The session recorded terminal.
    Closed {
        /// The recorded outcome.
        outcome: SessionTerminalOutcome,
    },
    /// The outcome is committed to the handoff; the live turn settles first.
    ClosurePending {
        /// The committed outcome.
        outcome: SessionTerminalOutcome,
        /// The turn the committed interrupt machinery settles.
        live_turn: TurnId,
        /// The defaults epoch current when this handling produced its receipt.
        /// A replacement between attempts costs one attempt a recorded mismatch.
        defaults_version: SessionConfigurationDefaultsVersion,
    },
    /// The park lifted.
    Resumed {
        /// The state the suspended turn's phase maps to.
        state: SessionLifecycleState,
    },
    /// The ownership bit flipped.
    OwnershipChanged,
}

/// The recorded result of one claimed lifecycle command.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionLifecycleCommandResult {
    /// The command applied.
    Applied(SessionLifecycleApplication),
    /// The command was refused with a closed reason.
    Rejected(SessionLifecycleCommandRejection),
}

#[cfg(test)]
mod tests {
    use super::{
        CommandPrincipal, SessionLifecycleCommand, SessionLifecycleOperation, StopStickiness,
    };
    use crate::{
        Actor, CoreAgency, DescendantTerminationScope, DispatchingModule, LifecycleActor,
        test_support::{command_id, session_id, turn_id},
    };

    #[test]
    fn a_module_principal_outranks_the_domain_actor() {
        assert_eq!(
            CommandPrincipal::Module {
                module: DispatchingModule::RepositoryWatch,
            }
            .classify(Some(Actor::User)),
            LifecycleActor::Module {
                module: DispatchingModule::RepositoryWatch,
            }
        );
    }

    #[test]
    fn the_domain_actor_classifies_under_a_core_principal() {
        assert_eq!(
            CommandPrincipal::Core.classify(Some(Actor::Model { turn: turn_id(3) })),
            LifecycleActor::Core {
                agency: CoreAgency::Model { turn: turn_id(3) },
            }
        );
        assert_eq!(
            CommandPrincipal::Operator.classify(None),
            LifecycleActor::Operator
        );
        assert_eq!(
            CommandPrincipal::Watchdog.classify(None),
            LifecycleActor::Watchdog
        );
    }

    #[test]
    fn replay_equality_excludes_the_identifier_and_binds_the_operation() {
        let stop = |command: u128, sticky| {
            SessionLifecycleCommand::new(
                command_id(command),
                session_id(1),
                SessionLifecycleOperation::Stop {
                    sticky,
                    descendant_scope: DescendantTerminationScope::ParentAlone,
                },
            )
        };
        assert_eq!(
            stop(1, StopStickiness::Sticky),
            stop(2, StopStickiness::Sticky)
        );
        assert_ne!(
            stop(1, StopStickiness::Sticky),
            stop(1, StopStickiness::Redispatchable)
        );
    }
}
