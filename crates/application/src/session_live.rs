//! Bounded current-session projection used to start or replace a live view.

use std::future::Future;

use signalbox_domain::{ModelCallId, RunnerId, SessionId, ToolAttemptId, ToolRequestId, TurnId};

/// Maximum queued turn identities retained in one current snapshot.
#[must_use]
pub const fn max_session_live_queued_turns() -> u8 {
    32
}

/// Durable state of the one active turn, when the session has one.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionLiveActiveState {
    /// The turn is executing or ready to execute.
    Running {
        /// Current provider call, when one is prepared or in flight.
        model_call: Option<ModelCallId>,
    },
    /// One ambiguous provider call needs an explicit recovery decision.
    AwaitingModelCallRecovery { call: ModelCallId },
    /// One tool request needs an explicit approval decision.
    AwaitingToolApproval { request: ToolRequestId },
    /// The foreground turn is waiting for one child session.
    AwaitingChild {
        request: ToolRequestId,
        child: SessionId,
    },
    /// One ambiguous external tool effect needs a recovery decision.
    AwaitingToolRecovery { attempt: ToolAttemptId },
    /// The active turn is parked until a lost runner placement is replaced.
    AwaitingRunnerRecovery {
        runner: RunnerId,
        placement_revision: u64,
    },
}

/// One active turn and its current durable state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SessionLiveActiveTurn {
    pub turn: TurnId,
    pub state: SessionLiveActiveState,
}

/// Exact ambiguous operation retained by a terminal reconciliation park.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionLiveReconciliation {
    ModelCall {
        turn: TurnId,
        call: ModelCallId,
    },
    ToolAttempt {
        turn: TurnId,
        attempt: ToolAttemptId,
    },
}

/// Current durable runner placement state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionLiveRunnerState {
    Unpinned,
    Pinned,
    RunnerLostBeforePin,
    RunnerLost,
    RunnerAbandoned,
}

/// Current runner connection health, when a pinned runner has one.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionLiveRunnerConnectionHealth {
    Connected,
    Suspect,
    Shutdown,
    Lost,
}

/// Lightweight runner facts from the same repeatable-read snapshot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SessionLiveRunner {
    pub runner: Option<RunnerId>,
    pub placement_revision: u64,
    pub state: SessionLiveRunnerState,
    pub connection_health: Option<SessionLiveRunnerConnectionHealth>,
}

/// Bounded current projection that replaces transient client presentation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionLiveSnapshot {
    pub session: SessionId,
    pub observed_through: u64,
    pub active: Option<SessionLiveActiveTurn>,
    pub queued_turn_count: u64,
    /// Earliest queued identities, in acceptance order, capped at 32.
    pub queued_turns: Vec<TurnId>,
    pub reconciliation: Option<SessionLiveReconciliation>,
    pub runner: Option<SessionLiveRunner>,
}

/// Application-owned read port for a coherent current session projection.
pub trait SessionLiveReader {
    type Error;

    fn read_live_snapshot(
        &self,
        session: SessionId,
    ) -> impl Future<Output = Result<Option<SessionLiveSnapshot>, Self::Error>> + Send;
}

/// Coordinates current snapshot reads without exporting adapter types.
#[derive(Debug)]
pub struct ReadSessionLiveService<Reader> {
    reader: Reader,
}

impl<Reader> ReadSessionLiveService<Reader> {
    #[must_use]
    pub const fn new(reader: Reader) -> Self {
        Self { reader }
    }
}

impl<Reader: SessionLiveReader> ReadSessionLiveService<Reader> {
    pub async fn snapshot(
        &self,
        session: SessionId,
    ) -> Result<Option<SessionLiveSnapshot>, Reader::Error> {
        self.reader.read_live_snapshot(session).await
    }
}
