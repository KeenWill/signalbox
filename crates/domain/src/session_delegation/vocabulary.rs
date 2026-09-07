//! Session delegation vocabulary for `docs/spec/sessions-and-transcript.md`.

use crate::{DurableCommandId, GoalGeneration, SessionId, TurnId};

/// Action applied to a bound child when its parent terminalizes.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum BoundChildAction {
    KeepRunning,
    Stop,
    Cancel,
}

/// Durable relationship policy fixed when the child is spawned.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ChildRelationshipPolicy {
    Background,
    Bound {
        on_parent_stopped: BoundChildAction,
        on_parent_cancelled: BoundChildAction,
    },
}

/// Whether awaiting a child retains the parent turn slot.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum DelegationWaitMode {
    Foreground,
    Background,
}

/// Explicit scope of a parent stop or cancellation command.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum DescendantTerminationScope {
    ParentAlone,
    ParentAndDescendants,
}

/// Closed terminal action proven by an applied parent command.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ParentTerminationKind {
    Stopped,
    Cancelled,
}

/// Exact domain command source that applied a parent termination.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ParentTerminationCommandSource {
    /// An interrupt command applied to one exact live turn.
    Turn { turn: TurnId },
    /// A goal-stop command applied to one exact goal generation.
    Goal { generation: GoalGeneration },
    /// A lifecycle stop applied directly to a session with no live turn.
    Lifecycle,
}

/// Exact applied parent termination authority.
///
/// Raw identities cannot construct this proof. The scheduling slice supplies
/// it only from the exact applied stop or cancellation command result; this
/// foundation slice keeps that producer sealed.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ParentTerminationAuthority {
    pub(super) parent: SessionId,
    pub(super) source: ParentTerminationCommandSource,
    pub(super) command: DurableCommandId,
    pub(super) kind: ParentTerminationKind,
    pub(super) scope: DescendantTerminationScope,
}

impl ParentTerminationAuthority {
    pub const fn parent(self) -> SessionId {
        self.parent
    }

    pub const fn source(self) -> ParentTerminationCommandSource {
        self.source
    }

    pub const fn turn(self) -> Option<TurnId> {
        match self.source {
            ParentTerminationCommandSource::Turn { turn } => Some(turn),
            ParentTerminationCommandSource::Goal { .. }
            | ParentTerminationCommandSource::Lifecycle => None,
        }
    }

    pub const fn goal_generation(self) -> Option<GoalGeneration> {
        match self.source {
            ParentTerminationCommandSource::Goal { generation } => Some(generation),
            ParentTerminationCommandSource::Turn { .. }
            | ParentTerminationCommandSource::Lifecycle => None,
        }
    }

    pub const fn command(self) -> DurableCommandId {
        self.command
    }

    pub const fn scope(self) -> DescendantTerminationScope {
        self.scope
    }

    pub const fn kind(self) -> ParentTerminationKind {
        self.kind
    }
}
