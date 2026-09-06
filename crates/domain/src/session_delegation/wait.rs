//! Session delegation wait for `docs/spec/sessions-and-transcript.md`.

use super::relation::SessionDelegation;
use super::request::DelegationAwaitRequest;
use super::vocabulary::DelegationWaitMode;
use crate::{SessionId, ToolRequestId};

/// Exact subject retained by a foreground parent turn wait.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ChildWait {
    awaiting_request: ToolRequestId,
    spawning_request: ToolRequestId,
    child: SessionId,
}

impl ChildWait {
    pub(crate) const fn from_checked_parts(
        awaiting_request: ToolRequestId,
        spawning_request: ToolRequestId,
        child: SessionId,
    ) -> Self {
        Self {
            awaiting_request,
            spawning_request,
            child,
        }
    }

    pub const fn awaiting_request(self) -> ToolRequestId {
        self.awaiting_request
    }
    pub const fn spawning_request(self) -> ToolRequestId {
        self.spawning_request
    }
    pub const fn child(self) -> SessionId {
        self.child
    }
}

/// One exact parent delivery registration made by an await tool request.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct DelegationWait {
    pub(super) awaiting_request: ToolRequestId,
    pub(super) spawning_request: ToolRequestId,
    pub(super) parent: SessionId,
    pub(super) child: SessionId,
    pub(super) mode: DelegationWaitMode,
}

impl DelegationWait {
    /// Reconstitutes one stored wait from the checked relationship and exact
    /// canonical await request. No live dispatch authority is minted.
    pub fn reconstitute(
        relation: &SessionDelegation,
        awaiting_request: &DelegationAwaitRequest,
    ) -> Option<Self> {
        Self::reconstitute_stored(
            awaiting_request,
            relation.spawning_request,
            relation.parent,
            relation.child,
            awaiting_request.mode(),
        )
    }

    /// Reconstitutes one stored wait from its immutable relationship endpoints
    /// and mode without loading relationship event history.
    pub fn reconstitute_stored(
        awaiting_request: &DelegationAwaitRequest,
        spawning_request: ToolRequestId,
        parent: SessionId,
        child: SessionId,
        mode: DelegationWaitMode,
    ) -> Option<Self> {
        (awaiting_request.request().session() == parent
            && parent != child
            && awaiting_request.request().id() != spawning_request
            && awaiting_request.child() == child
            && awaiting_request.mode() == mode)
            .then_some(Self {
                awaiting_request: awaiting_request.request().id(),
                spawning_request,
                parent,
                child,
                mode,
            })
    }

    pub const fn awaiting_request(self) -> ToolRequestId {
        self.awaiting_request
    }
    pub const fn spawning_request(self) -> ToolRequestId {
        self.spawning_request
    }
    pub const fn parent(self) -> SessionId {
        self.parent
    }
    pub const fn child(self) -> SessionId {
        self.child
    }
    pub const fn mode(self) -> DelegationWaitMode {
        self.mode
    }
    pub const fn foreground_subject(self) -> Option<ChildWait> {
        match self.mode {
            DelegationWaitMode::Foreground => Some(ChildWait {
                awaiting_request: self.awaiting_request,
                spawning_request: self.spawning_request,
                child: self.child,
            }),
            DelegationWaitMode::Background => None,
        }
    }
}
