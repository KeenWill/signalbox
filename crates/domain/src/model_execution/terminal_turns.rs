//! Model-call terminal turns for `docs/spec/model-call-execution.md`.

use super::IssuedModelCallCorrelation;
use crate::{
    AcceptedInputLifecycle, AcceptedInputQueueOrder, ActiveTurnPhase, AppliedInterruptProof,
    CurrentModelCall, CurrentTurnAttempt, EffectiveConfiguration, EndedModelCall, EndedToolAttempt,
    EndedTurnAttempt, ModelCallId, NonEmptyIssuedOperationRefs, ResolvedContextFrontierSnapshot,
    SemanticTranscriptEntry, SessionId, SteeringBinding, ToolApprovalResolution, ToolRequest,
    TurnDisposition, TurnId,
};

/// One pending steering input atomically reclassified when its source turn
/// terminalizes before another model-call safe point.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReclassifiedPendingSteeringTurn {
    pub(super) session: SessionId,
    pub(super) source_turn: TurnId,
    pub(super) accepted_input: AcceptedInputLifecycle,
    pub(super) turn: TurnId,
    pub(super) order: AcceptedInputQueueOrder,
    pub(super) binding: SteeringBinding,
    pub(super) effective_configuration: EffectiveConfiguration,
}

impl ReclassifiedPendingSteeringTurn {
    /// Returns the owning session.
    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// Returns the terminal source turn.
    pub const fn source_turn(&self) -> TurnId {
        self.source_turn
    }

    /// Borrows the accepted input with its reclassified disposition.
    pub const fn accepted_input(&self) -> &AcceptedInputLifecycle {
        &self.accepted_input
    }

    /// Returns the fresh queued turn originated by the input.
    pub const fn turn(&self) -> TurnId {
        self.turn
    }

    /// Returns ordinary queue order at the input's original position.
    pub const fn order(&self) -> AcceptedInputQueueOrder {
        self.order
    }

    /// Returns inherited provenance binding the new origin to its source.
    pub const fn binding(&self) -> SteeringBinding {
        self.binding
    }

    /// Borrows the source turn's exact inherited effective configuration.
    pub const fn effective_configuration(&self) -> &EffectiveConfiguration {
        &self.effective_configuration
    }
}

/// One successful completed-turn commit candidate.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompletedModelCallTurn {
    pub(super) session: SessionId,
    pub(super) turn: TurnId,
    pub(super) call: EndedModelCall,
    pub(super) attempt: EndedTurnAttempt,
    pub(super) disposition: TurnDisposition,
    pub(super) assistant_entries: Box<[SemanticTranscriptEntry]>,
    pub(super) completion_entry: SemanticTranscriptEntry,
    pub(super) terminal_snapshot: ResolvedContextFrontierSnapshot,
    pub(super) reclassified_pending_steering: Box<[ReclassifiedPendingSteeringTurn]>,
}

/// One nonterminal commit candidate from a tool-using completed model call.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolRoundModelCallTurn {
    pub(super) session: SessionId,
    pub(super) turn: TurnId,
    pub(super) call: EndedModelCall,
    pub(super) attempt: EndedTurnAttempt,
    pub(super) assistant_entries: Box<[SemanticTranscriptEntry]>,
    pub(super) requests: Box<[ToolRequest]>,
    pub(super) automatic_approvals: Box<[ToolApprovalResolution]>,
    pub(super) yielded_snapshot: ResolvedContextFrontierSnapshot,
    pub(super) next_phase: ActiveTurnPhase,
}

/// One availability-failed call and the distinct prepared attempt succeeding it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AvailabilitySuccessorModelCallTurn {
    pub(super) session: SessionId,
    pub(super) turn: TurnId,
    pub(super) predecessor_call: EndedModelCall,
    pub(super) predecessor_attempt: EndedTurnAttempt,
    pub(super) successor_attempt: CurrentTurnAttempt,
}

impl AvailabilitySuccessorModelCallTurn {
    /// Returns the owning session.
    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// Returns the continuing logical turn.
    pub const fn turn(&self) -> TurnId {
        self.turn
    }

    /// Borrows the terminal failed predecessor call.
    pub const fn predecessor_call(&self) -> &EndedModelCall {
        &self.predecessor_call
    }

    /// Borrows the terminal failed predecessor attempt.
    pub const fn predecessor_attempt(&self) -> &EndedTurnAttempt {
        &self.predecessor_attempt
    }

    /// Borrows the fresh prepared successor attempt.
    pub const fn successor_attempt(&self) -> &CurrentTurnAttempt {
        &self.successor_attempt
    }
}

impl ToolRoundModelCallTurn {
    /// Returns the owning session.
    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// Returns the continuing logical turn.
    pub const fn turn(&self) -> TurnId {
        self.turn
    }

    /// Borrows the completed producing call.
    pub const fn call(&self) -> &EndedModelCall {
        &self.call
    }

    /// Borrows the yielded producing attempt.
    pub const fn attempt(&self) -> &EndedTurnAttempt {
        &self.attempt
    }

    /// Returns ordered text/tool-use semantic entries.
    pub fn assistant_entries(&self) -> &[SemanticTranscriptEntry] {
        &self.assistant_entries
    }

    /// Returns logical requests in proposal order.
    pub fn requests(&self) -> &[ToolRequest] {
        &self.requests
    }

    /// Returns only automatic decisions, in proposal order among those selected.
    pub fn automatic_approvals(&self) -> &[ToolApprovalResolution] {
        &self.automatic_approvals
    }

    /// Borrows the source-plus-assistant yielded snapshot.
    pub const fn yielded_snapshot(&self) -> &ResolvedContextFrontierSnapshot {
        &self.yielded_snapshot
    }

    /// Borrows the approval wait or prepared continuation attempt.
    pub const fn next_phase(&self) -> &ActiveTurnPhase {
        &self.next_phase
    }
}

/// One interrupt-cancelled turn whose racing response proposed tools.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CancelledToolRoundModelCallTurn {
    pub(super) session: SessionId,
    pub(super) turn: TurnId,
    pub(super) call: EndedModelCall,
    pub(super) attempt: EndedTurnAttempt,
    pub(super) disposition: TurnDisposition,
    pub(super) assistant_entries: Box<[SemanticTranscriptEntry]>,
    pub(super) requests: Box<[ToolRequest]>,
    pub(super) closed_result_entries: Box<[SemanticTranscriptEntry]>,
    pub(super) cancellation_entry: SemanticTranscriptEntry,
    pub(super) terminal_snapshot: ResolvedContextFrontierSnapshot,
    pub(super) reclassified_pending_steering: Box<[ReclassifiedPendingSteeringTurn]>,
}

impl CancelledToolRoundModelCallTurn {
    /// Returns the owning session.
    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// Returns the cancelled logical turn.
    pub const fn turn(&self) -> TurnId {
        self.turn
    }

    /// Borrows the completed producing call.
    pub const fn call(&self) -> &EndedModelCall {
        &self.call
    }

    /// Borrows the proof-bearing ended attempt.
    pub const fn attempt(&self) -> &EndedTurnAttempt {
        &self.attempt
    }

    /// Borrows the cancelled disposition.
    pub const fn disposition(&self) -> &TurnDisposition {
        &self.disposition
    }

    /// Returns ordered assistant response entries.
    pub fn assistant_entries(&self) -> &[SemanticTranscriptEntry] {
        &self.assistant_entries
    }

    /// Returns proposed logical requests in proposal order.
    pub fn requests(&self) -> &[ToolRequest] {
        &self.requests
    }

    /// Returns proposal-ordered closed-result entries.
    pub fn closed_result_entries(&self) -> &[SemanticTranscriptEntry] {
        &self.closed_result_entries
    }

    /// Borrows the final proof-bearing cancellation marker.
    pub const fn cancellation_entry(&self) -> &SemanticTranscriptEntry {
        &self.cancellation_entry
    }

    /// Borrows the complete terminal snapshot.
    pub const fn terminal_snapshot(&self) -> &ResolvedContextFrontierSnapshot {
        &self.terminal_snapshot
    }

    /// Returns successor turns for pending steering.
    pub fn reclassified_pending_steering(&self) -> &[ReclassifiedPendingSteeringTurn] {
        &self.reclassified_pending_steering
    }
}

impl CompletedModelCallTurn {
    /// Returns the owning session.
    pub const fn session(&self) -> SessionId {
        self.session
    }
    /// Returns the completed turn.
    pub const fn turn(&self) -> TurnId {
        self.turn
    }
    /// Borrows the completed physical call.
    pub const fn call(&self) -> &EndedModelCall {
        &self.call
    }
    /// Borrows the ended attempt.
    pub const fn attempt(&self) -> &EndedTurnAttempt {
        &self.attempt
    }
    /// Borrows the completed turn disposition.
    pub const fn disposition(&self) -> &TurnDisposition {
        &self.disposition
    }
    /// Returns ordered assistant text entries.
    pub fn assistant_entries(&self) -> &[SemanticTranscriptEntry] {
        &self.assistant_entries
    }
    /// Borrows the final completion marker.
    pub const fn completion_entry(&self) -> &SemanticTranscriptEntry {
        &self.completion_entry
    }
    /// Borrows the complete terminal frontier.
    pub const fn terminal_snapshot(&self) -> &ResolvedContextFrontierSnapshot {
        &self.terminal_snapshot
    }
    /// Returns queued turns created from every pending steering input.
    pub fn reclassified_pending_steering(&self) -> &[ReclassifiedPendingSteeringTurn] {
        &self.reclassified_pending_steering
    }
}

/// One failed-turn commit candidate, with an optional physical call.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FailedModelCallTurn {
    pub(super) session: SessionId,
    pub(super) turn: TurnId,
    pub(super) call: Option<EndedModelCall>,
    pub(super) attempt: EndedTurnAttempt,
    pub(super) disposition: TurnDisposition,
    pub(super) failure_entry: SemanticTranscriptEntry,
    pub(super) terminal_snapshot: ResolvedContextFrontierSnapshot,
    pub(super) reclassified_pending_steering: Box<[ReclassifiedPendingSteeringTurn]>,
}

impl FailedModelCallTurn {
    /// Returns the owning session.
    pub const fn session(&self) -> SessionId {
        self.session
    }
    /// Returns the failed turn.
    pub const fn turn(&self) -> TurnId {
        self.turn
    }
    /// Borrows the physical call when one existed.
    pub const fn call(&self) -> Option<&EndedModelCall> {
        self.call.as_ref()
    }
    /// Borrows the ended attempt.
    pub const fn attempt(&self) -> &EndedTurnAttempt {
        &self.attempt
    }
    /// Borrows the failed turn disposition.
    pub const fn disposition(&self) -> &TurnDisposition {
        &self.disposition
    }
    /// Borrows the explicit failure marker.
    pub const fn failure_entry(&self) -> &SemanticTranscriptEntry {
        &self.failure_entry
    }
    /// Borrows the complete terminal frontier.
    pub const fn terminal_snapshot(&self) -> &ResolvedContextFrontierSnapshot {
        &self.terminal_snapshot
    }
    /// Returns queued turns created from every pending steering input.
    pub fn reclassified_pending_steering(&self) -> &[ReclassifiedPendingSteeringTurn] {
        &self.reclassified_pending_steering
    }
}

/// Typed terminal failure for a pool that admitted no credential member.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CredentialPoolExhaustedModelCallTurn {
    pub(super) pool_name: String,
    pub(super) failed: FailedModelCallTurn,
}

/// Typed terminal boundary that requires compaction before another model call.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContextHeadroomExhaustedModelCallTurn {
    pub(super) producing_call: ModelCallId,
    pub(super) failed: FailedModelCallTurn,
}

impl ContextHeadroomExhaustedModelCallTurn {
    /// Returns the completed tool-producing call whose usage exhausted headroom.
    pub const fn producing_call(&self) -> ModelCallId {
        self.producing_call
    }

    /// Borrows the ordinary failed-turn projection committed for clients.
    pub const fn failed(&self) -> &FailedModelCallTurn {
        &self.failed
    }

    /// Consumes the typed boundary into its failed-turn persistence payload.
    pub fn into_failed(self) -> FailedModelCallTurn {
        self.failed
    }
}

impl CredentialPoolExhaustedModelCallTurn {
    /// Borrows the deployment-owned pool name whose members were unavailable.
    pub fn pool_name(&self) -> &str {
        &self.pool_name
    }

    /// Borrows the ordinary failed-turn projection committed for the client.
    pub const fn failed(&self) -> &FailedModelCallTurn {
        &self.failed
    }

    /// Consumes the typed cause into its failed-turn persistence payload.
    pub fn into_failed(self) -> FailedModelCallTurn {
        self.failed
    }
}

/// One interrupt-cancelled turn commit candidate.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CancelledModelCallTurn {
    pub(super) session: SessionId,
    pub(super) turn: TurnId,
    pub(super) call: Option<EndedModelCall>,
    pub(super) attempt: Option<EndedTurnAttempt>,
    pub(super) disposition: TurnDisposition,
    pub(super) tool_result_entries: Box<[SemanticTranscriptEntry]>,
    pub(super) cancellation_entry: SemanticTranscriptEntry,
    pub(super) terminal_snapshot: ResolvedContextFrontierSnapshot,
    pub(super) reclassified_pending_steering: Box<[ReclassifiedPendingSteeringTurn]>,
}

impl CancelledModelCallTurn {
    /// Returns the owning session.
    pub const fn session(&self) -> SessionId {
        self.session
    }
    /// Returns the cancelled turn.
    pub const fn turn(&self) -> TurnId {
        self.turn
    }
    /// Borrows the physical call when one existed.
    pub const fn call(&self) -> Option<&EndedModelCall> {
        self.call.as_ref()
    }
    /// Borrows the ended attempt when cancellation closed live execution.
    pub const fn attempt(&self) -> Option<&EndedTurnAttempt> {
        self.attempt.as_ref()
    }
    /// Borrows the proof-bearing cancelled disposition.
    pub const fn disposition(&self) -> &TurnDisposition {
        &self.disposition
    }
    /// Borrows proposal-ordered tool results materialized before cancellation.
    pub fn tool_result_entries(&self) -> &[SemanticTranscriptEntry] {
        &self.tool_result_entries
    }
    /// Borrows the explicit cancellation marker.
    pub const fn cancellation_entry(&self) -> &SemanticTranscriptEntry {
        &self.cancellation_entry
    }
    /// Borrows the complete terminal frontier.
    pub const fn terminal_snapshot(&self) -> &ResolvedContextFrontierSnapshot {
        &self.terminal_snapshot
    }
    /// Returns queued turns created from every pending steering input.
    pub fn reclassified_pending_steering(&self) -> &[ReclassifiedPendingSteeringTurn] {
        &self.reclassified_pending_steering
    }
}

/// One durable cancellation request retaining the active turn slot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StopRequestedModelCallTurn {
    pub(super) session: SessionId,
    pub(super) turn: TurnId,
    pub(super) call: CurrentModelCall,
    pub(super) attempt: CurrentTurnAttempt,
    pub(super) interrupt: AppliedInterruptProof,
}

impl StopRequestedModelCallTurn {
    /// Returns the owning session.
    pub const fn session(&self) -> SessionId {
        self.session
    }
    /// Returns the stopped active turn.
    pub const fn turn(&self) -> TurnId {
        self.turn
    }
    /// Borrows the exact cancellation-requested call.
    pub const fn call(&self) -> &CurrentModelCall {
        &self.call
    }
    /// Borrows the proof-bearing stop-requested attempt.
    pub const fn attempt(&self) -> &CurrentTurnAttempt {
        &self.attempt
    }
    /// Returns the applied interrupt authorizing cancellation.
    pub const fn interrupt(&self) -> AppliedInterruptProof {
        self.interrupt
    }

    /// Returns the issued facts binding a provider-neutral cancellation
    /// observation to this exact stopped authorization.
    pub const fn observation_correlation(&self) -> IssuedModelCallCorrelation {
        IssuedModelCallCorrelation {
            session: self.session,
            turn: self.turn,
            attempt: self.attempt.id(),
            call: self.call.id(),
            target: self.call.target(),
            frontier: self.call.frontier().snapshot(),
        }
    }
}

/// One refused-turn commit candidate.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RefusedModelCallTurn {
    pub(super) session: SessionId,
    pub(super) turn: TurnId,
    pub(super) call: EndedModelCall,
    pub(super) attempt: EndedTurnAttempt,
    pub(super) disposition: TurnDisposition,
    pub(super) provider_compaction_entries: Box<[SemanticTranscriptEntry]>,
    pub(super) terminal_snapshot: ResolvedContextFrontierSnapshot,
    pub(super) reclassified_pending_steering: Box<[ReclassifiedPendingSteeringTurn]>,
}

impl RefusedModelCallTurn {
    /// Returns the owning session.
    pub const fn session(&self) -> SessionId {
        self.session
    }
    /// Returns the refused turn.
    pub const fn turn(&self) -> TurnId {
        self.turn
    }
    /// Borrows the refused physical call.
    pub const fn call(&self) -> &EndedModelCall {
        &self.call
    }
    /// Borrows the ended attempt.
    pub const fn attempt(&self) -> &EndedTurnAttempt {
        &self.attempt
    }
    /// Borrows the refused turn disposition.
    pub const fn disposition(&self) -> &TurnDisposition {
        &self.disposition
    }
    /// Returns provider compaction entries retained before refusal.
    pub fn provider_compaction_entries(&self) -> &[SemanticTranscriptEntry] {
        &self.provider_compaction_entries
    }
    /// Borrows the terminal frontier.
    pub const fn terminal_snapshot(&self) -> &ResolvedContextFrontierSnapshot {
        &self.terminal_snapshot
    }
    /// Returns queued turns created from every pending steering input.
    pub fn reclassified_pending_steering(&self) -> &[ReclassifiedPendingSteeringTurn] {
        &self.reclassified_pending_steering
    }
}

/// One proof-bearing reconciliation-required commit candidate.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReconciliationRequiredModelCallTurn {
    pub(super) session: SessionId,
    pub(super) turn: TurnId,
    pub(super) call: EndedModelCall,
    pub(super) attempt: EndedTurnAttempt,
    pub(super) disposition: TurnDisposition,
    pub(super) terminal_snapshot: ResolvedContextFrontierSnapshot,
    pub(super) reclassified_pending_steering: Box<[ReclassifiedPendingSteeringTurn]>,
}

impl ReconciliationRequiredModelCallTurn {
    /// Returns the owning session.
    pub const fn session(&self) -> SessionId {
        self.session
    }
    /// Returns the turn whose ambiguity requires reconciliation.
    pub const fn turn(&self) -> TurnId {
        self.turn
    }
    /// Borrows the exact ambiguous physical call.
    pub const fn call(&self) -> &EndedModelCall {
        &self.call
    }
    /// Borrows the proof-bearing ended attempt.
    pub const fn attempt(&self) -> &EndedTurnAttempt {
        &self.attempt
    }
    /// Borrows the exact reconciliation disposition and marker.
    pub const fn disposition(&self) -> &TurnDisposition {
        &self.disposition
    }
    /// Borrows the terminal frontier.
    pub const fn terminal_snapshot(&self) -> &ResolvedContextFrontierSnapshot {
        &self.terminal_snapshot
    }
    /// Returns queued turns created from every pending steering input.
    pub fn reclassified_pending_steering(&self) -> &[ReclassifiedPendingSteeringTurn] {
        &self.reclassified_pending_steering
    }
}

/// One proof-bearing tool-attempt reconciliation commit candidate.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReconciliationRequiredToolTurn {
    pub(super) session: SessionId,
    pub(super) turn: TurnId,
    pub(super) tool_attempt: EndedToolAttempt,
    pub(super) attempt: EndedTurnAttempt,
    pub(super) disposition: TurnDisposition,
    pub(super) tool_result_entries: Box<[SemanticTranscriptEntry]>,
    pub(super) terminal_snapshot: ResolvedContextFrontierSnapshot,
    pub(super) reclassified_pending_steering: Box<[ReclassifiedPendingSteeringTurn]>,
}

impl ReconciliationRequiredToolTurn {
    /// Returns the owning session.
    pub const fn session(&self) -> SessionId {
        self.session
    }
    /// Returns the turn whose ambiguity requires reconciliation.
    pub const fn turn(&self) -> TurnId {
        self.turn
    }
    /// Borrows the exact ambiguous physical tool attempt.
    pub const fn tool_attempt(&self) -> &EndedToolAttempt {
        &self.tool_attempt
    }
    /// Borrows the proof-bearing ended turn attempt.
    pub const fn attempt(&self) -> &EndedTurnAttempt {
        &self.attempt
    }
    /// Borrows the exact reconciliation disposition and marker.
    pub const fn disposition(&self) -> &TurnDisposition {
        &self.disposition
    }
    /// Returns proposal-ordered logical results closing the terminal batch.
    pub fn tool_result_entries(&self) -> &[SemanticTranscriptEntry] {
        &self.tool_result_entries
    }
    /// Borrows the prefix-extending terminal frontier.
    pub const fn terminal_snapshot(&self) -> &ResolvedContextFrontierSnapshot {
        &self.terminal_snapshot
    }
    /// Returns queued turns created from every pending steering input.
    pub fn reclassified_pending_steering(&self) -> &[ReclassifiedPendingSteeringTurn] {
        &self.reclassified_pending_steering
    }
}

/// One ambiguity wait candidate retaining immutable physical history.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AmbiguousModelCallTurn {
    pub(super) session: SessionId,
    pub(super) turn: TurnId,
    pub(super) call: EndedModelCall,
    pub(super) attempt: EndedTurnAttempt,
    pub(super) ambiguous_operations: NonEmptyIssuedOperationRefs,
}

impl AmbiguousModelCallTurn {
    /// Returns the owning session.
    pub const fn session(&self) -> SessionId {
        self.session
    }
    /// Returns the active turn retaining the slot.
    pub const fn turn(&self) -> TurnId {
        self.turn
    }
    /// Borrows the ambiguous physical call.
    pub const fn call(&self) -> &EndedModelCall {
        &self.call
    }
    /// Borrows the ended attempt.
    pub const fn attempt(&self) -> &EndedTurnAttempt {
        &self.attempt
    }
    /// Borrows the exact recovery wait set.
    pub const fn ambiguous_operations(&self) -> &NonEmptyIssuedOperationRefs {
        &self.ambiguous_operations
    }
}

/// Why a guarded terminal candidate could not be constructed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModelCallClosureError {
    /// Candidate identities do not match the observed disposition.
    IdentityShapeMismatch,
    /// The call cannot take the requested terminal transition.
    CallStateMismatch,
    /// The observation names different issued authority than fresh state.
    ObservationCorrelationMismatch,
    /// The applied interrupt does not name this exact session, predecessor,
    /// and immediate successor relation.
    InterruptCorrelationMismatch,
    /// The attempt cannot take the required terminal transition.
    AttemptStateMismatch,
    /// Claimed target-resolution failure does not match this execution's
    /// immutable catalog and frozen selection.
    TargetResolutionMismatch,
    /// Assistant text and entry identity counts differ.
    AssistantIdentityCountMismatch,
    /// Tool response parts and their identity shapes differ.
    ToolResponseIdentityMismatch,
    /// The request ordinal cannot fit the durable zero-based space.
    ToolRequestOrdinalOverflow,
    /// Initial approval provenance contradicts the frozen blanket posture.
    InitialToolApprovalMismatch,
    /// A continuation attempt was missing, unexpected, or reused the yielded attempt.
    ContinuationAttemptIdentityMismatch,
    /// Pending steering and proposed successor identities are not exact,
    /// ordered, distinct, and source-turn-safe.
    PendingSteeringReclassificationMismatch,
    /// The exact terminal frontier could not preserve its source prefix.
    FrontierDerivationFailed,
    /// The exact nonempty ambiguity set could not be constructed.
    AmbiguityConstructionFailed,
}
