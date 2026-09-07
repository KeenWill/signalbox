//! Turn scheduling turn failure for `docs/spec/turn-lifecycle-and-scheduling.md`.

use crate::{
    AcceptedInputLifecycle, AcceptedInputQueueOrder, AcceptedInputTurnStart, ContextFrontierId,
    EndedTurnAttempt, PendingSteeringReclassificationIdentity, ReclassifiedPendingSteeringTurn,
    ResolvedContextFrontierSnapshot, SemanticTranscriptEntry, SemanticTranscriptEntryId, SessionId,
    TurnDisposition, TurnId,
};

/// Fresh identities supplied for one failed-terminal startup candidate.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AcceptedInputTurnFailureIdentities {
    pub(super) failure_entry: SemanticTranscriptEntryId,
    pub(super) terminal_frontier: ContextFrontierId,
    pending_steering_reclassifications: Vec<PendingSteeringReclassificationIdentity>,
}

impl AcceptedInputTurnFailureIdentities {
    /// Supplies the semantic failure-entry and terminal-frontier identities.
    pub const fn new(
        failure_entry: SemanticTranscriptEntryId,
        terminal_frontier: ContextFrontierId,
    ) -> Self {
        Self {
            failure_entry,
            terminal_frontier,
            pending_steering_reclassifications: Vec::new(),
        }
    }

    /// Supplies one fresh successor identity per pending steering input, in
    /// session acceptance order.
    pub fn with_pending_steering_reclassifications(
        mut self,
        identities: Vec<PendingSteeringReclassificationIdentity>,
    ) -> Self {
        self.pending_steering_reclassifications = identities;
        self
    }

    /// Returns the proposed failed-marker identity.
    pub const fn failure_entry(&self) -> SemanticTranscriptEntryId {
        self.failure_entry
    }

    /// Returns the proposed terminal-frontier identity.
    pub const fn terminal_frontier(&self) -> ContextFrontierId {
        self.terminal_frontier
    }

    /// Borrows the proposed successor identities for pending steering.
    pub fn pending_steering_reclassifications(&self) -> &[PendingSteeringReclassificationIdentity] {
        &self.pending_steering_reclassifications
    }
}

/// Exact failed turn state prepared by the startup scan.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FailedAcceptedInputTurn {
    pub(super) session: SessionId,
    pub(super) turn: TurnId,
    pub(super) accepted_input: AcceptedInputLifecycle,
    pub(super) order: AcceptedInputQueueOrder,
    pub(super) start: AcceptedInputTurnStart,
    pub(super) ended_attempt: EndedTurnAttempt,
    pub(super) disposition: TurnDisposition,
    pub(super) terminal_frontier: ContextFrontierId,
}

impl FailedAcceptedInputTurn {
    /// Returns the owning session.
    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// Returns the failed logical turn.
    pub const fn turn(&self) -> TurnId {
        self.turn
    }

    /// Borrows the exact accepted origin input.
    pub const fn accepted_input(&self) -> &AcceptedInputLifecycle {
        &self.accepted_input
    }

    /// Returns the immutable accepted-input queue order.
    pub const fn order(&self) -> AcceptedInputQueueOrder {
        self.order
    }

    /// Returns the eligibility-fixed lineage and starting frontier.
    pub const fn start(&self) -> AcceptedInputTurnStart {
        self.start
    }

    /// Borrows the exact Lost physical-attempt history.
    pub const fn ended_attempt(&self) -> &EndedTurnAttempt {
        &self.ended_attempt
    }

    /// Borrows the failed logical-turn disposition.
    pub const fn disposition(&self) -> &TurnDisposition {
        &self.disposition
    }

    /// Returns the complete terminal-frontier identity.
    pub const fn terminal_frontier(&self) -> ContextFrontierId {
        self.terminal_frontier
    }
}

/// One sealed atomic failed-terminal startup-recovery candidate.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreparedAcceptedInputTurnFailure {
    pub(super) turn: FailedAcceptedInputTurn,
    pub(super) failure_entry: SemanticTranscriptEntry,
    pub(super) terminal_snapshot: ResolvedContextFrontierSnapshot,
    pub(super) reclassified_pending_steering: Box<[ReclassifiedPendingSteeringTurn]>,
}

impl PreparedAcceptedInputTurnFailure {
    /// Borrows the exact failed logical turn and ended physical attempt.
    pub const fn turn(&self) -> &FailedAcceptedInputTurn {
        &self.turn
    }

    /// Returns the newly created `TurnFailed` semantic entry.
    pub fn failure_entry(&self) -> SemanticTranscriptEntry {
        self.failure_entry.clone()
    }

    /// Borrows the start-prefix-preserving terminal snapshot.
    pub const fn terminal_snapshot(&self) -> &ResolvedContextFrontierSnapshot {
        &self.terminal_snapshot
    }

    /// Borrows the queued successors reclassified from pending steering.
    pub fn reclassified_pending_steering(&self) -> &[ReclassifiedPendingSteeringTurn] {
        &self.reclassified_pending_steering
    }

    /// Returns all atomic commit values.
    pub fn into_parts(
        self,
    ) -> (
        FailedAcceptedInputTurn,
        SemanticTranscriptEntry,
        ResolvedContextFrontierSnapshot,
        Box<[ReclassifiedPendingSteeringTurn]>,
    ) {
        (
            self.turn,
            self.failure_entry,
            self.terminal_snapshot,
            self.reclassified_pending_steering,
        )
    }
}
