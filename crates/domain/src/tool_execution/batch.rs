//! Logical tool-batch state, phase views, and transition outcomes for `docs/spec/tool-loop.md`.

mod approval;
mod attempt;
mod projection;
mod reconstitution;

pub use reconstitution::{
    ToolBatchPhaseReconstitutionInput, ToolBatchReconstitutionError,
    ToolBatchReconstitutionFailure, ToolBatchReconstitutionInput,
};

use crate::{
    ActiveTurnPhase, ApprovedToolRequest, AuthorizedToolAttempt, CurrentToolAttempt,
    DecideToolRequest, DelegateToolApproval, EndedToolAttempt, PreparedDecideToolRequest,
    ReconstitutedToolAttempt, ResolvedContextFrontierSnapshot, SemanticTranscriptEntry, SessionId,
    ToolApprovalResolution, ToolAttemptEnd, ToolAttemptId, ToolRequest, ToolRequestId,
    TurnAttemptId, TurnId, tool_attempt::RUNNER_ISSUANCE_AVAILABLE,
};
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::AtomicU8;
use std::sync::atomic::Ordering;

/// Canonical active phase derived from a complete tool-batch inventory.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolBatchPhase {
    /// No physical attempt exists and one exact decision is next.
    AwaitingApproval {
        /// Earliest undecided request.
        request: ToolRequestId,
    },
    /// All decisions exist and one turn attempt owns serial execution.
    Executing {
        /// Current turn-attempt tenure.
        turn_attempt: TurnAttemptId,
    },
    /// Exact external-effect ambiguity blocks progress.
    AwaitingRecovery {
        /// Terminal ambiguous tool attempt.
        attempt: ToolAttemptId,
    },
    /// One exact foreground delegation result remains pending.
    AwaitingChild {
        /// Await request receiving the result.
        request: ToolRequestId,
        /// Spawn request naming the child relationship.
        spawning_request: ToolRequestId,
        /// Exact child whose result is pending.
        child: SessionId,
    },
}

/// One completely validated active logical tool batch.
#[derive(Clone, Debug)]
pub struct ToolBatch {
    session: SessionId,
    turn: TurnId,
    producing_call: crate::ModelCallId,
    yielded_snapshot: ResolvedContextFrontierSnapshot,
    requests: Box<[ToolRequest]>,
    approvals: BTreeMap<ToolRequestId, ToolApprovalResolution>,
    attempts: BTreeMap<ToolRequestId, ReconstitutedToolAttempt>,
    retired_attempts: BTreeSet<ToolAttemptId>,
    runner_issuance: BTreeMap<ToolAttemptId, Arc<AtomicU8>>,
    phase: ToolBatchPhase,
}

// Runner issuance state is durable identity; atomics are shared by in-memory clones.
impl PartialEq for ToolBatch {
    fn eq(&self, other: &Self) -> bool {
        self.session == other.session
            && self.turn == other.turn
            && self.producing_call == other.producing_call
            && self.yielded_snapshot == other.yielded_snapshot
            && self.requests == other.requests
            && self.approvals == other.approvals
            && self.attempts == other.attempts
            && self.retired_attempts == other.retired_attempts
            && self
                .runner_authorized_attempts()
                .eq(other.runner_authorized_attempts())
            && self.phase == other.phase
    }
}

impl Eq for ToolBatch {}

impl ToolBatch {
    /// Returns the owning session.
    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// Returns the continuing logical turn.
    pub const fn turn(&self) -> TurnId {
        self.turn
    }

    /// Returns the definitive producing call.
    pub const fn producing_call(&self) -> crate::ModelCallId {
        self.producing_call
    }

    /// Borrows the yielded assistant-content snapshot.
    pub const fn yielded_snapshot(&self) -> &ResolvedContextFrontierSnapshot {
        &self.yielded_snapshot
    }

    /// Returns requests in proposal order.
    pub fn requests(&self) -> &[ToolRequest] {
        &self.requests
    }

    /// Returns the decision for one request, if resolved.
    pub fn approval(&self, request: ToolRequestId) -> Option<&ToolApprovalResolution> {
        self.approvals.get(&request)
    }

    /// Returns the physical attempt for one request, if created.
    pub fn attempt(&self, request: ToolRequestId) -> Option<&ReconstitutedToolAttempt> {
        self.attempts.get(&request)
    }

    /// Returns every retired physical-attempt identity in stable order.
    pub fn retired_attempts(&self) -> impl Iterator<Item = ToolAttemptId> + '_ {
        self.retired_attempts.iter().copied()
    }

    /// Returns every physical attempt whose runner authority was durably issued.
    pub fn runner_authorized_attempts(&self) -> impl Iterator<Item = ToolAttemptId> + '_ {
        self.runner_issuance.iter().filter_map(|(attempt, issued)| {
            (issued.load(Ordering::Acquire) != RUNNER_ISSUANCE_AVAILABLE).then_some(*attempt)
        })
    }

    /// Returns the evidence-derived active phase.
    pub const fn phase(&self) -> ToolBatchPhase {
        self.phase
    }

    /// Produces opaque approval-wait evidence only from a matching batch.
    pub fn awaiting_approval(&self) -> Option<AwaitingToolApproval> {
        match self.phase {
            ToolBatchPhase::AwaitingApproval { request } => Some(AwaitingToolApproval {
                session: self.session,
                turn: self.turn,
                request,
            }),
            ToolBatchPhase::Executing { .. }
            | ToolBatchPhase::AwaitingRecovery { .. }
            | ToolBatchPhase::AwaitingChild { .. } => None,
        }
    }

    /// Produces opaque recovery-wait evidence only from a complete matching
    /// batch with one exact ambiguous physical attempt.
    pub fn awaiting_recovery(&self) -> Option<AwaitingToolRecovery> {
        match self.phase {
            ToolBatchPhase::AwaitingRecovery { attempt } => {
                self.attempts
                    .values()
                    .find_map(|candidate| match candidate {
                        ReconstitutedToolAttempt::Ended(ended)
                            if ended.attempt() == attempt
                                && ended.end() == &ToolAttemptEnd::Ambiguous =>
                        {
                            Some(AwaitingToolRecovery {
                                session: self.session,
                                turn: self.turn,
                                producing_call: self.producing_call,
                                yielded_frontier: self.yielded_snapshot.frontier().snapshot(),
                                issuing_attempt: ended.issuing_attempt(),
                                attempt,
                            })
                        }
                        ReconstitutedToolAttempt::Current(_)
                        | ReconstitutedToolAttempt::Ended(_) => None,
                    })
            }
            ToolBatchPhase::AwaitingApproval { .. }
            | ToolBatchPhase::Executing { .. }
            | ToolBatchPhase::AwaitingChild { .. } => None,
        }
    }
}

/// Opaque evidence for one exact approval wait.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AwaitingToolApproval {
    session: SessionId,
    turn: TurnId,
    request: ToolRequestId,
}

impl AwaitingToolApproval {
    /// Returns the owning session.
    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// Returns the owning logical turn.
    pub const fn turn(&self) -> TurnId {
        self.turn
    }

    /// Returns the exact earliest undecided request.
    pub const fn request(&self) -> ToolRequestId {
        self.request
    }
}

/// Opaque evidence for one exact tool-attempt recovery wait.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AwaitingToolRecovery {
    session: SessionId,
    turn: TurnId,
    producing_call: crate::ModelCallId,
    yielded_frontier: crate::ContextFrontierId,
    issuing_attempt: TurnAttemptId,
    attempt: ToolAttemptId,
}

impl AwaitingToolRecovery {
    /// Returns the owning session.
    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// Returns the owning logical turn.
    pub const fn turn(&self) -> TurnId {
        self.turn
    }

    /// Returns the model call that produced the ambiguous tool batch.
    pub const fn producing_call(&self) -> crate::ModelCallId {
        self.producing_call
    }

    /// Returns the batch frontier retained while recovery is unresolved.
    pub const fn yielded_frontier(&self) -> crate::ContextFrontierId {
        self.yielded_frontier
    }

    /// Returns the turn attempt that authorized the ambiguous tool attempt.
    pub const fn issuing_attempt(&self) -> TurnAttemptId {
        self.issuing_attempt
    }

    /// Returns the exact ambiguous physical attempt.
    pub const fn attempt(&self) -> ToolAttemptId {
        self.attempt
    }
}

/// One approval-command candidate plus the exact successor active phase.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreparedToolBatchDecision {
    batch: ToolBatch,
    prepared_command: PreparedDecideToolRequest,
    active_phase: ActiveTurnPhase,
}

/// One checked delegate result plus its exact successor active phase.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreparedDelegateToolApproval {
    batch: ToolBatch,
    approval: DelegateToolApproval,
    resolution: Option<crate::ToolApprovalResolution>,
    active_phase: ActiveTurnPhase,
}

impl PreparedDelegateToolApproval {
    /// Borrows the updated or unchanged canonical batch.
    pub const fn batch(&self) -> &ToolBatch {
        &self.batch
    }

    /// Borrows the checked delegate result.
    pub const fn approval(&self) -> &DelegateToolApproval {
        &self.approval
    }

    /// Borrows the resulting approve-or-deny resolution, absent on escalation.
    pub const fn resolution(&self) -> Option<&crate::ToolApprovalResolution> {
        self.resolution.as_ref()
    }

    /// Borrows the exact active phase to store atomically.
    pub const fn active_phase(&self) -> &ActiveTurnPhase {
        &self.active_phase
    }
}

/// Why a checked delegate result could not advance this exact batch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DelegateToolApprovalTransitionFailure {
    /// No request remains undecided.
    NoUndecidedRequest,
    /// The result does not name the exact earliest request and posture.
    RequestMismatch,
    /// The next phase and supplied continuation identity disagreed.
    ContinuationAttemptMismatch,
}

/// Failed delegate transition retaining every unchanged input.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DelegateToolApprovalTransitionError {
    batch: Box<ToolBatch>,
    approval: DelegateToolApproval,
    failure: DelegateToolApprovalTransitionFailure,
}

impl DelegateToolApprovalTransitionError {
    fn new(
        batch: ToolBatch,
        approval: DelegateToolApproval,
        failure: DelegateToolApprovalTransitionFailure,
    ) -> Self {
        Self {
            batch: Box::new(batch),
            approval,
            failure,
        }
    }

    /// Borrows the unchanged batch.
    pub const fn batch(&self) -> &ToolBatch {
        &self.batch
    }

    /// Borrows the unchanged delegate result.
    pub const fn approval(&self) -> &DelegateToolApproval {
        &self.approval
    }

    /// Returns the exact failure.
    pub const fn failure(&self) -> DelegateToolApprovalTransitionFailure {
        self.failure
    }
}

impl std::fmt::Display for DelegateToolApprovalTransitionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "delegate approval transition failed: {:?}",
            self.failure
        )
    }
}

impl std::error::Error for DelegateToolApprovalTransitionError {}

impl PreparedToolBatchDecision {
    fn rejected(
        batch: ToolBatch,
        prepared_command: PreparedDecideToolRequest,
        waiting_on: ToolRequestId,
    ) -> Self {
        Self {
            batch,
            prepared_command,
            active_phase: ActiveTurnPhase::AwaitingApproval {
                request: waiting_on,
            },
        }
    }

    /// Borrows the updated or unchanged canonical batch.
    pub const fn batch(&self) -> &ToolBatch {
        &self.batch
    }

    /// Borrows the command and terminal result candidate.
    pub const fn prepared_command(&self) -> &PreparedDecideToolRequest {
        &self.prepared_command
    }

    /// Borrows the exact active phase to store atomically with the decision.
    pub const fn active_phase(&self) -> &ActiveTurnPhase {
        &self.active_phase
    }

    /// Returns every transaction value.
    pub fn into_parts(self) -> (ToolBatch, PreparedDecideToolRequest, ActiveTurnPhase) {
        (self.batch, self.prepared_command, self.active_phase)
    }
}

/// Why decision preparation found inconsistent adapter input.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolBatchDecisionFailure {
    /// No request remains undecided.
    NoUndecidedRequest,
    /// The command and located request did not correlate.
    CommandCorrelationMismatch,
    /// The next phase and supplied continuation identity disagreed.
    ContinuationAttemptMismatch,
}

/// Nonterminal decision-preparation error retaining the batch and command.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolBatchDecisionError {
    batch: Box<ToolBatch>,
    command: DecideToolRequest,
    failure: ToolBatchDecisionFailure,
}

impl ToolBatchDecisionError {
    /// Borrows the unchanged batch.
    pub const fn batch(&self) -> &ToolBatch {
        &self.batch
    }

    /// Borrows the unchanged command.
    pub const fn command(&self) -> &DecideToolRequest {
        &self.command
    }

    /// Returns the exact preparation failure.
    pub const fn failure(&self) -> ToolBatchDecisionFailure {
        self.failure
    }
}

/// One pre-commit first-generation physical-attempt candidate.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreparedToolAttempt {
    attempt: CurrentToolAttempt,
}

impl PreparedToolAttempt {
    /// Borrows the prepared attempt.
    pub const fn attempt(&self) -> &CurrentToolAttempt {
        &self.attempt
    }

    /// Returns the prepared attempt.
    pub fn into_attempt(self) -> CurrentToolAttempt {
        self.attempt
    }
}

pub(crate) struct PreparedClaimedToolAttemptReplacement {
    pub(crate) batch: ToolBatch,
    pub(crate) retired: EndedToolAttempt,
    pub(crate) approved: ApprovedToolRequest,
    pub(crate) authorized: AuthorizedToolAttempt,
}

/// Why no next serialized attempt can be prepared.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolBatchExecutionFailure {
    /// The batch is parked on approval or recovery.
    NotExecuting,
    /// One attempt already remains prepared or in flight.
    LiveAttemptPresent,
    /// The requested current attempt is absent from the complete batch.
    AttemptMissing,
    /// The requested attempt is not in the required durable stage.
    AttemptStageMismatch,
    /// Every approved request has terminal attempt evidence.
    ReadyForContinuation,
    /// A prior crash-lost attempt requires turn-level failure.
    TurnLevelFailure,
    /// The proposed physical-attempt identity already belongs to the batch.
    AttemptIdentityReuse,
    /// Approval evidence did not authorize the selected request.
    ApprovalMismatch,
}

/// Rejected next-attempt preparation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ToolBatchExecutionError {
    failure: ToolBatchExecutionFailure,
}

impl ToolBatchExecutionError {
    /// Returns the exact preparation failure.
    pub const fn failure(&self) -> ToolBatchExecutionFailure {
        self.failure
    }
}

/// One proposal-ordered result projection and prefix-preserving snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreparedToolResultProjection {
    source_frontier: crate::ContextFrontierId,
    turn: TurnId,
    producing_call: crate::ModelCallId,
    entries: Box<[SemanticTranscriptEntry]>,
    snapshot: ResolvedContextFrontierSnapshot,
}

impl PreparedToolResultProjection {
    #[cfg(test)]
    pub(crate) fn from_validated_parts(
        source_frontier: crate::ContextFrontierId,
        turn: TurnId,
        producing_call: crate::ModelCallId,
        entries: Vec<SemanticTranscriptEntry>,
        snapshot: ResolvedContextFrontierSnapshot,
    ) -> Self {
        Self {
            source_frontier,
            turn,
            producing_call,
            entries: entries.into_boxed_slice(),
            snapshot,
        }
    }

    /// Returns the exact yielded frontier from which the results were derived.
    pub(crate) const fn source_frontier(&self) -> crate::ContextFrontierId {
        self.source_frontier
    }

    pub(crate) const fn turn(&self) -> TurnId {
        self.turn
    }

    pub(crate) const fn producing_call(&self) -> crate::ModelCallId {
        self.producing_call
    }

    /// Returns proposal-order results followed by any runner replacement boundary.
    pub fn entries(&self) -> &[SemanticTranscriptEntry] {
        &self.entries
    }

    /// Borrows the yielded-plus-results snapshot.
    pub const fn snapshot(&self) -> &ResolvedContextFrontierSnapshot {
        &self.snapshot
    }

    /// Extends the complete result frontier by its checked runner replacement boundary.
    pub fn with_runner_placement_boundary(
        mut self,
        boundary: &crate::RunnerPlacementBoundary,
    ) -> Result<Self, crate::RunnerDomainError> {
        if !self.snapshot.is_semantic_prefix_of(boundary.frontier())
            || boundary.frontier().entry_count() != self.snapshot.entry_count() + 1
        {
            return Err(crate::RunnerDomainError::CorrelationMismatch);
        }
        let mut entries = self.entries.into_vec();
        entries.push(boundary.entry().clone());
        self.entries = entries.into_boxed_slice();
        self.snapshot = boundary.frontier().clone();
        Ok(self)
    }

    /// Returns both atomic projection values.
    pub fn into_parts(
        self,
    ) -> (
        Box<[SemanticTranscriptEntry]>,
        ResolvedContextFrontierSnapshot,
    ) {
        (self.entries, self.snapshot)
    }
}

/// Why result projection cannot yet form a continuation boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolResultProjectionFailure {
    /// At least one request lacks a continuation-safe logical resolution.
    BatchNotResolved,
    /// Crash or ambiguity requires turn-level failure/recovery instead.
    TurnLevelFailure,
    /// A fresh semantic-entry identity was not distinct.
    EntryIdentityReuse,
    /// The new snapshot could not preserve the yielded prefix.
    FrontierDerivationFailed,
}

/// Rejected continuation result projection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ToolResultProjectionError {
    failure: ToolResultProjectionFailure,
}

impl ToolResultProjectionError {
    /// Returns the exact projection failure.
    pub const fn failure(&self) -> ToolResultProjectionFailure {
        self.failure
    }
}
