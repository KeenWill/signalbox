//! Durable user commands over a session's commissioned-goal lineage.

use crate::{
    DescendantTerminationScope, DurableCommandId, GoalEvent, GoalGuidance, GoalStatement, SessionId,
};

/// One durable user operation over a session's goal lineage.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GoalUserAction {
    /// Attach an immutable statement when no goal is active.
    Attach(GoalStatement),
    /// Resume a blocked goal with optional next-turn guidance.
    Resume(Option<GoalGuidance>),
    /// End the current goal without achievement.
    Stop {
        /// Whether stopping the parent also terminates its descendants.
        descendant_scope: DescendantTerminationScope,
    },
    /// Replace an open statement with a new immutable generation.
    Supersede(GoalStatement),
}

impl GoalUserAction {
    /// Whether this action can begin or resume goal pursuit.
    pub const fn starts_pursuit(&self) -> bool {
        match self {
            Self::Attach(_) | Self::Resume(_) | Self::Supersede(_) => true,
            Self::Stop { .. } => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{GoalUserAction, GoalUserCommand};
    use crate::{DescendantTerminationScope, DurableCommandId, SessionId};

    /// stop-command replay comparison binds descendant scope.
    #[test]
    fn stop_command_identity_includes_descendant_scope() {
        let command_id = DurableCommandId::from_uuid(uuid::Uuid::from_u128(1));
        let session = SessionId::from_uuid(uuid::Uuid::from_u128(2));
        let parent_alone = GoalUserCommand::new(
            command_id,
            session,
            GoalUserAction::Stop {
                descendant_scope: DescendantTerminationScope::ParentAlone,
            },
        );
        let equal_replay = GoalUserCommand::new(
            command_id,
            session,
            GoalUserAction::Stop {
                descendant_scope: DescendantTerminationScope::ParentAlone,
            },
        );
        let conflicting_replay = GoalUserCommand::new(
            command_id,
            session,
            GoalUserAction::Stop {
                descendant_scope: DescendantTerminationScope::ParentAndDescendants,
            },
        );

        assert_eq!(parent_alone, equal_replay);
        assert_ne!(parent_alone, conflicting_replay);
    }

    #[test]
    fn pursuit_starting_classification_names_every_action() {
        let statement = crate::GoalStatement::try_new(String::from("ship the change"))
            .expect("fixture statement is valid");

        assert!(GoalUserAction::Attach(statement.clone()).starts_pursuit());
        assert!(GoalUserAction::Resume(None).starts_pursuit());
        assert!(GoalUserAction::Supersede(statement).starts_pursuit());
        assert!(
            !GoalUserAction::Stop {
                descendant_scope: DescendantTerminationScope::ParentAlone,
            }
            .starts_pursuit()
        );
    }
}

/// A user-global durable command for one goal operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GoalUserCommand {
    command_id: DurableCommandId,
    session: SessionId,
    action: GoalUserAction,
}

impl GoalUserCommand {
    /// Retains the durable identity, target session, and exact operation.
    pub const fn new(
        command_id: DurableCommandId,
        session: SessionId,
        action: GoalUserAction,
    ) -> Self {
        Self {
            command_id,
            session,
            action,
        }
    }

    /// Returns the user-global command identity.
    pub const fn command_id(&self) -> DurableCommandId {
        self.command_id
    }

    /// Returns the target session.
    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// Borrows the exact requested action.
    pub const fn action(&self) -> &GoalUserAction {
        &self.action
    }
}

/// Why a durable user goal command had no event effect.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GoalCommandRejection {
    /// The target session does not exist.
    SessionNotFound,
    /// The session's closure is pending; the closure settles the goal.
    SessionClosing,
    /// A goal is already pursuing or blocked.
    GoalAlreadyAttached,
    /// The session has no goal lineage.
    GoalNotAttached,
    /// The session's selected model alias was unknown at turn acceptance.
    UnknownModelAlias,
    /// The session's accepted-input position cannot advance beyond `u64::MAX`.
    AcceptancePositionExhausted,
    /// Resume requires the current generation to be blocked.
    RequiresBlocked,
    /// Stop and supersede require a pursuing or blocked generation.
    RequiresPursuingOrBlocked,
    /// No successor generation can be represented.
    GenerationExhausted,
    /// No successor event position can be represented.
    EventOrdinalExhausted,
}

/// The exact recorded result of one durable user goal command.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GoalCommandResult {
    /// The command appended this event.
    Applied(GoalEvent),
    /// The command was durably rejected without appending an event.
    Rejected(GoalCommandRejection),
}

/// A checked durable command and its recorded result.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReconstitutedGoalCommand {
    command: GoalUserCommand,
    result: GoalCommandResult,
}

impl ReconstitutedGoalCommand {
    /// Retains a checked command and its exact recorded result.
    pub const fn new(command: GoalUserCommand, result: GoalCommandResult) -> Self {
        Self { command, result }
    }

    /// Borrows the exact original command.
    pub const fn command(&self) -> &GoalUserCommand {
        &self.command
    }

    /// Borrows the exact recorded result.
    pub const fn result(&self) -> &GoalCommandResult {
        &self.result
    }
}
