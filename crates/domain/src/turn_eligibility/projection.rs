//! Turn scheduling projection for `docs/spec/turn-lifecycle-and-scheduling.md`.

use super::activated_turn::{
    AcceptedInputTurnActivationIdentities, ActivatedAcceptedInputTurn, ActivatedTurn,
};
use super::failure::{AcceptedInputEligibilityError, AcceptedInputTurnFailureError};
use super::prepared_activation::PreparedAcceptedInputTurnActivation;
use super::turn_failure::{AcceptedInputTurnFailureIdentities, PreparedAcceptedInputTurnFailure};
use crate::{
    AcceptedInputDisposition, AcceptedInputId, AcceptedInputLifecycle, AcceptedInputQueueOrder,
    AcceptedInputTurnStart, ActiveTurnPhase, AppliedInterruptCommandResult, ContextFrontierId,
    ContextFrontierProjection, ContextFrontierProjectionFailure, DeliveryRequest,
    DirectModelSelection, EndedTurnAttempt, OriginConfiguration, ResolvedContextFrontierSnapshot,
    SemanticTranscriptEntry, SemanticTranscriptEntryPayload, SemanticTranscriptEntryRef, Session,
    SessionId, SessionInputPosition, TurnAttemptId, TurnConfigurationProvenance, TurnId,
};
use std::{collections::BTreeMap, collections::BTreeSet};

use super::prepare::{prepare_active_turn_lost_failure, prepare_earliest_queued_activation};

/// One validated accepted input in an active turn's session tail.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SessionAcceptanceTailEntry {
    pub(super) accepted_input: AcceptedInputLifecycle,
    pub(super) position: SessionInputPosition,
    pub(super) delivery: DeliveryRequest,
}

/// One pending steering input proven by the complete active-session tail.
///
/// Construction stays inside checked scheduling reconstitution so an input's
/// identity, source-turn binding, and immutable acceptance position cannot be
/// cross-wired at an execution or terminalization boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PendingSteeringInput {
    pub(super) accepted_input: AcceptedInputLifecycle,
    pub(super) acceptance_position: SessionInputPosition,
}

/// One consumed steering input proven by the complete active-session tail.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConsumedSteeringInput {
    pub(super) accepted_input: AcceptedInputLifecycle,
    pub(super) acceptance_position: SessionInputPosition,
    pub(super) source_turn: TurnId,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct ActiveModelCallRecoveryWait {
    pub(super) call: crate::EndedModelCall,
    pub(super) attempt: EndedTurnAttempt,
    pub(super) source_snapshot: ResolvedContextFrontierSnapshot,
}

impl ConsumedSteeringInput {
    /// Returns the accepted input already consumed by a prepared call.
    pub const fn accepted_input(&self) -> AcceptedInputId {
        self.accepted_input.id()
    }

    /// Borrows the exact checked consumed lifecycle.
    pub const fn lifecycle(&self) -> &AcceptedInputLifecycle {
        &self.accepted_input
    }

    /// Returns the immutable session acceptance position.
    pub const fn acceptance_position(&self) -> SessionInputPosition {
        self.acceptance_position
    }

    /// Returns the exact active turn this input was accepted to steer.
    pub const fn source_turn(&self) -> TurnId {
        self.source_turn
    }
}

impl PendingSteeringInput {
    /// Reconstitutes one pending tail member bound to its exact active turn.
    pub fn reconstitute(
        accepted_input: AcceptedInputLifecycle,
        acceptance_position: SessionInputPosition,
        source_turn: TurnId,
    ) -> Option<Self> {
        matches!(
            accepted_input.disposition(),
            AcceptedInputDisposition::PendingSteering { binding }
                if binding.source_turn() == source_turn
        )
        .then_some(Self {
            accepted_input,
            acceptance_position,
        })
    }

    /// Returns the accepted input awaiting disposition.
    pub const fn accepted_input(&self) -> AcceptedInputId {
        self.accepted_input.id()
    }

    /// Borrows the exact checked pending lifecycle.
    pub const fn lifecycle(&self) -> &AcceptedInputLifecycle {
        &self.accepted_input
    }

    /// Returns the immutable session acceptance position.
    pub const fn acceptance_position(&self) -> SessionInputPosition {
        self.acceptance_position
    }
}

/// Canonical complete accepted-input interval for one active turn.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SessionAcceptanceTail {
    pub(super) session: SessionId,
    pub(super) anchor: AcceptedInputId,
    pub(super) observed_last_position: SessionInputPosition,
    pub(super) entries: Box<[SessionAcceptanceTailEntry]>,
}

impl SessionAcceptanceTail {
    pub(crate) const fn observed_last_position(&self) -> SessionInputPosition {
        self.observed_last_position
    }
}

/// The scheduling-visible lifecycle classification.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum AcceptedInputTurnSchedulingStatus {
    /// No start or semantic projection exists.
    Queued,
    /// The turn owns the session's progressing slot.
    Active,
    /// The turn terminalized as failed and has a complete closed semantic
    /// frontier through its failed marker.
    TerminalFailed,
    /// The turn committed a complete assistant response and completion marker.
    TerminalCompleted,
    /// The turn committed an explicit refusal.
    TerminalRefused,
    /// The turn committed a proof-bearing cancellation marker.
    TerminalCancelled,
    /// The turn released its slot with proof-bearing ambiguous work.
    TerminalReconciliationRequired,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum ReconstitutedSchedulingState {
    Queued,
    Active {
        start: AcceptedInputTurnStart,
        phase: ActiveTurnPhase,
    },
    TerminalFailed {
        start: AcceptedInputTurnStart,
        terminal_frontier: ResolvedContextFrontierSnapshot,
    },
    TerminalCompleted {
        start: AcceptedInputTurnStart,
        terminal_frontier: ResolvedContextFrontierSnapshot,
    },
    TerminalRefused {
        start: AcceptedInputTurnStart,
        terminal_frontier: ResolvedContextFrontierSnapshot,
    },
    TerminalCancelled {
        start: AcceptedInputTurnStart,
        terminal_frontier: ResolvedContextFrontierSnapshot,
    },
    TerminalReconciliationRequired {
        start: AcceptedInputTurnStart,
        terminal_frontier: ResolvedContextFrontierSnapshot,
    },
}

/// One canonical turn inside the complete scheduling projection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AcceptedInputTurnSchedulingProjection {
    pub(super) session: SessionId,
    pub(super) turn: TurnId,
    pub(super) accepted_input: AcceptedInputLifecycle,
    pub(super) order: AcceptedInputQueueOrder,
    pub(super) origin_configuration: OriginConfiguration,
    pub(super) configuration_provenance: TurnConfigurationProvenance,
    pub(super) state: ReconstitutedSchedulingState,
}

impl AcceptedInputTurnSchedulingProjection {
    /// Returns the owning session.
    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// Returns the accepted-input-origin turn.
    pub const fn turn(&self) -> TurnId {
        self.turn
    }

    /// Borrows the exact accepted input whose disposition is `OriginOf(turn)`.
    pub const fn accepted_input(&self) -> &AcceptedInputLifecycle {
        &self.accepted_input
    }

    /// Returns the immutable durable queue-order facts.
    pub const fn order(&self) -> AcceptedInputQueueOrder {
        self.order
    }

    /// Borrows the complete frozen origin configuration.
    pub const fn origin_configuration(&self) -> &OriginConfiguration {
        &self.origin_configuration
    }

    /// Borrows the explicit or inherited configuration provenance.
    pub const fn configuration_provenance(&self) -> &TurnConfigurationProvenance {
        &self.configuration_provenance
    }

    /// Returns the scheduling-visible lifecycle classification.
    pub const fn status(&self) -> AcceptedInputTurnSchedulingStatus {
        match &self.state {
            ReconstitutedSchedulingState::Queued => AcceptedInputTurnSchedulingStatus::Queued,
            ReconstitutedSchedulingState::Active { .. } => {
                AcceptedInputTurnSchedulingStatus::Active
            }
            ReconstitutedSchedulingState::TerminalFailed { .. } => {
                AcceptedInputTurnSchedulingStatus::TerminalFailed
            }
            ReconstitutedSchedulingState::TerminalCompleted { .. } => {
                AcceptedInputTurnSchedulingStatus::TerminalCompleted
            }
            ReconstitutedSchedulingState::TerminalRefused { .. } => {
                AcceptedInputTurnSchedulingStatus::TerminalRefused
            }
            ReconstitutedSchedulingState::TerminalCancelled { .. } => {
                AcceptedInputTurnSchedulingStatus::TerminalCancelled
            }
            ReconstitutedSchedulingState::TerminalReconciliationRequired { .. } => {
                AcceptedInputTurnSchedulingStatus::TerminalReconciliationRequired
            }
        }
    }

    /// Returns the opaque validated start for started work.
    pub const fn start(&self) -> Option<AcceptedInputTurnStart> {
        match &self.state {
            ReconstitutedSchedulingState::Queued => None,
            ReconstitutedSchedulingState::Active { start, .. }
            | ReconstitutedSchedulingState::TerminalFailed { start, .. }
            | ReconstitutedSchedulingState::TerminalCompleted { start, .. }
            | ReconstitutedSchedulingState::TerminalRefused { start, .. }
            | ReconstitutedSchedulingState::TerminalCancelled { start, .. }
            | ReconstitutedSchedulingState::TerminalReconciliationRequired { start, .. } => {
                Some(*start)
            }
        }
    }

    /// Borrows the exact current active phase, when this turn owns the slot.
    pub const fn active_phase(&self) -> Option<&ActiveTurnPhase> {
        match &self.state {
            ReconstitutedSchedulingState::Active { phase, .. } => Some(phase),
            ReconstitutedSchedulingState::Queued
            | ReconstitutedSchedulingState::TerminalFailed { .. }
            | ReconstitutedSchedulingState::TerminalCompleted { .. }
            | ReconstitutedSchedulingState::TerminalRefused { .. }
            | ReconstitutedSchedulingState::TerminalCancelled { .. }
            | ReconstitutedSchedulingState::TerminalReconciliationRequired { .. } => None,
        }
    }

    fn active_turn_execution_with_pending(
        &self,
        pending_steering: Box<[PendingSteeringInput]>,
        consumed_steering: Box<[ConsumedSteeringInput]>,
    ) -> Option<ActivatedAcceptedInputTurn> {
        let ReconstitutedSchedulingState::Active { start, phase } = &self.state else {
            return None;
        };
        Some(ActivatedAcceptedInputTurn {
            session: self.session,
            turn: self.turn,
            accepted_input: self.accepted_input.clone(),
            order: self.order,
            configuration: self.origin_configuration.clone(),
            configuration_provenance: self.configuration_provenance.clone(),
            start: *start,
            phase: phase.clone(),
            pending_steering,
            consumed_steering,
        })
    }

    /// Borrows the complete semantic frontier through a failed marker.
    pub const fn failed_terminal_frontier(&self) -> Option<&ResolvedContextFrontierSnapshot> {
        match &self.state {
            ReconstitutedSchedulingState::TerminalFailed {
                terminal_frontier, ..
            } => Some(terminal_frontier),
            ReconstitutedSchedulingState::Queued | ReconstitutedSchedulingState::Active { .. } => {
                None
            }
            ReconstitutedSchedulingState::TerminalCompleted { .. }
            | ReconstitutedSchedulingState::TerminalRefused { .. }
            | ReconstitutedSchedulingState::TerminalCancelled { .. }
            | ReconstitutedSchedulingState::TerminalReconciliationRequired { .. } => None,
        }
    }

    /// Borrows the complete semantic frontier of any terminal turn.
    pub const fn terminal_frontier(&self) -> Option<&ResolvedContextFrontierSnapshot> {
        match &self.state {
            ReconstitutedSchedulingState::TerminalFailed {
                terminal_frontier, ..
            }
            | ReconstitutedSchedulingState::TerminalCompleted {
                terminal_frontier, ..
            }
            | ReconstitutedSchedulingState::TerminalRefused {
                terminal_frontier, ..
            }
            | ReconstitutedSchedulingState::TerminalCancelled {
                terminal_frontier, ..
            }
            | ReconstitutedSchedulingState::TerminalReconciliationRequired {
                terminal_frontier,
                ..
            } => Some(terminal_frontier),
            ReconstitutedSchedulingState::Queued | ReconstitutedSchedulingState::Active { .. } => {
                None
            }
        }
    }
}

/// Canonical complete scheduling state for one session.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AcceptedInputSchedulingProjection {
    pub(super) runner_placement_frontier: Option<ContextFrontierId>,
    pub(super) session: Session,
    pub(super) initial_seed_frontier: Option<ContextFrontierId>,
    pub(super) latest_compaction_result: Option<ContextFrontierId>,
    pub(super) active_compaction_call: Option<crate::ModelCallId>,
    pub(super) turns: Box<[AcceptedInputTurnSchedulingProjection]>,
    pub(super) active_acceptance_tail: Option<SessionAcceptanceTail>,
    pub(super) semantic_entries: BTreeMap<SemanticTranscriptEntryRef, SemanticTranscriptEntry>,
    pub(super) snapshots: BTreeMap<ContextFrontierId, ResolvedContextFrontierSnapshot>,
    pub(super) attempt_owners: BTreeMap<TurnAttemptId, TurnId>,
    pub(super) active_model_call_recovery: Option<ActiveModelCallRecoveryWait>,
    pub(super) active_stop_requested_frontier: Option<ContextFrontierId>,
    pub(super) active_tool_recovery_attempt: Option<EndedTurnAttempt>,
    pub(super) active_tool_recovery_frontier: Option<ContextFrontierId>,
    pub(super) active_executing_tool_batch: Option<ActiveExecutingToolBatchCorrelation>,
    pub(super) preceding_non_accepted_successors: BTreeMap<TurnId, TurnId>,
    pub(super) preceding_non_accepted_terminals:
        BTreeMap<TurnId, (ResolvedContextFrontierSnapshot, DirectModelSelection)>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct ActiveExecutingToolBatchCorrelation {
    pub(super) session: SessionId,
    pub(super) turn: TurnId,
    pub(super) producing_call: crate::ModelCallId,
    pub(super) yielded_frontier: ContextFrontierId,
    pub(super) turn_attempt: Option<TurnAttemptId>,
}

impl AcceptedInputSchedulingProjection {
    pub(super) fn base_with_runner_placement<'a>(
        &'a self,
        base: Option<&'a ResolvedContextFrontierSnapshot>,
    ) -> Result<Option<&'a ResolvedContextFrontierSnapshot>, ()> {
        let placement = self
            .runner_placement_frontier
            .and_then(|frontier| self.snapshots.get(&frontier));
        match (base, placement) {
            (None, placement) => Ok(placement),
            (base, None) => Ok(base),
            (Some(base), Some(placement)) if base.is_semantic_prefix_of(placement) => {
                Ok(Some(placement))
            }
            (Some(base), Some(placement)) if placement.is_semantic_prefix_of(base) => {
                Ok(Some(base))
            }
            _ => Err(()),
        }
    }
    /// Borrows the complete current-session snapshot.
    pub const fn session(&self) -> &Session {
        &self.session
    }

    pub(crate) const fn active_acceptance_tail(&self) -> Option<&SessionAcceptanceTail> {
        self.active_acceptance_tail.as_ref()
    }

    /// Iterates over every turn in derived durable total order.
    pub fn turns(&self) -> impl ExactSizeIterator<Item = &AcceptedInputTurnSchedulingProjection> {
        self.turns.iter()
    }

    /// Looks up one turn in the complete scheduling projection.
    pub fn turn(&self, turn: TurnId) -> Option<&AcceptedInputTurnSchedulingProjection> {
        self.turns.iter().find(|candidate| candidate.turn == turn)
    }

    /// Returns the sole active slot owner, when present.
    pub fn active_turn(&self) -> Option<&AcceptedInputTurnSchedulingProjection> {
        self.turns
            .iter()
            .find(|turn| turn.status() == AcceptedInputTurnSchedulingStatus::Active)
    }

    /// Reconstructs the sealed active-turn facts and complete pending-steering
    /// inventory for an execution aggregate.
    pub fn active_turn_execution(&self) -> Option<ActivatedAcceptedInputTurn> {
        let active = self.active_turn()?;
        let tail = self.active_acceptance_tail.as_ref()?;
        let (pending_steering, consumed_steering) =
            active_execution_steering_inputs(active.turn, tail);
        active.active_turn_execution_with_pending(pending_steering, consumed_steering)
    }

    /// Returns accepted-input origins retained by the active turn's exact
    /// model-visible frontier.
    pub fn active_rendered_frontier_origins(&self) -> Option<Vec<AcceptedInputId>> {
        let active = self.active_turn()?;
        if matches!(
            active.active_phase(),
            Some(ActiveTurnPhase::AwaitingRunnerRecovery { .. })
        ) {
            return None;
        }
        let snapshot = self
            .active_model_call_recovery
            .as_ref()
            .map(|recovery| recovery.source_snapshot.frontier().snapshot())
            .or(self.active_stop_requested_frontier)
            .or_else(|| {
                self.active_executing_tool_batch
                    .map(|batch| batch.yielded_frontier)
            })
            .or(self.active_tool_recovery_frontier)
            .or_else(|| active.start().map(|start| start.frontier().snapshot()))
            .and_then(|frontier| self.snapshots.get(&frontier));
        Self::rendered_frontier_origins(snapshot, &self.semantic_entries)
    }

    /// Returns the earliest queued work in durable total order.
    pub fn earliest_queued_turn(&self) -> Option<&AcceptedInputTurnSchedulingProjection> {
        self.turns
            .iter()
            .find(|turn| turn.status() == AcceptedInputTurnSchedulingStatus::Queued)
    }

    /// Returns accepted-input origins retained by the exact base from which
    /// the earliest queued turn would be rendered.
    ///
    /// The base is reported as the model would see it: when it carries a
    /// context summary, the entries that summary hides are not retained
    /// origins, exactly as the live-execution path projects its own frontier
    /// before collecting origins. Counting hidden origins here would sum
    /// attachments no render ever clones, and a submission whose visible
    /// frontier fits the byte bound would be durably rejected because a
    /// summarized-away one did not.
    ///
    /// The outer absence means a turn is active or no queued turn exists. The
    /// inner failure means the base's own summary range is unprojectable, a
    /// durable corruption the caller must surface rather than read as an empty
    /// base. It excludes the queued turn's own origin; callers can append
    /// queued origins in [`Self::turns`] order to project each eventual
    /// frontier.
    pub fn earliest_queued_rendered_base_origins(
        &self,
    ) -> Option<Result<Vec<AcceptedInputId>, ContextFrontierProjectionFailure>> {
        if self.active_turn().is_some() {
            return None;
        }
        let index = self
            .turns
            .iter()
            .position(|turn| turn.status() == AcceptedInputTurnSchedulingStatus::Queued)?;
        let queued = &self.turns[index];
        let preceding_non_accepted_terminal = self
            .preceding_non_accepted_successors
            .get(&queued.turn())
            .and_then(|predecessor| self.preceding_non_accepted_terminals.get(predecessor))
            .map(|(snapshot, _)| snapshot);
        let base = if index == 0 && preceding_non_accepted_terminal.is_none() {
            let seed = self
                .initial_seed_frontier
                .and_then(|frontier| self.snapshots.get(&frontier));
            self.latest_compaction_result
                .and_then(|frontier| self.snapshots.get(&frontier))
                .filter(|latest| seed.is_some_and(|seed| seed.is_semantic_prefix_of(latest)))
                .or(seed)
        } else {
            let terminal = preceding_non_accepted_terminal.or_else(|| {
                index
                    .checked_sub(1)
                    .and_then(|predecessor| self.turns[predecessor].terminal_frontier())
            })?;
            self.latest_compaction_result
                .and_then(|frontier| self.snapshots.get(&frontier))
                .filter(|latest| terminal.is_semantic_prefix_of(latest))
                .or(Some(terminal))
        };
        Self::projected_rendered_frontier_origins(base, &self.semantic_entries)
    }

    /// Returns the rendered base origins for a queued turn rooted directly at
    /// a terminal non-accepted predecessor.
    ///
    /// Absence means this turn continues the accepted-input chain and does not
    /// reset prospective frontier accounting.
    pub fn external_predecessor_rendered_base_origins(
        &self,
        turn: TurnId,
    ) -> Option<Vec<AcceptedInputId>> {
        let predecessor = self.preceding_non_accepted_successors.get(&turn)?;
        let terminal = &self.preceding_non_accepted_terminals.get(predecessor)?.0;
        let base = self
            .latest_compaction_result
            .and_then(|frontier| self.snapshots.get(&frontier))
            .filter(|latest| terminal.is_semantic_prefix_of(latest))
            .unwrap_or(terminal);
        Self::rendered_frontier_origins(Some(base), &self.semantic_entries)
    }

    pub(super) fn rendered_frontier_origins(
        snapshot: Option<&ResolvedContextFrontierSnapshot>,
        semantic_entries: &BTreeMap<SemanticTranscriptEntryRef, SemanticTranscriptEntry>,
    ) -> Option<Vec<AcceptedInputId>> {
        Self::projected_rendered_frontier_origins(snapshot, semantic_entries)?.ok()
    }

    fn projected_rendered_frontier_origins(
        snapshot: Option<&ResolvedContextFrontierSnapshot>,
        semantic_entries: &BTreeMap<SemanticTranscriptEntryRef, SemanticTranscriptEntry>,
    ) -> Option<Result<Vec<AcceptedInputId>, ContextFrontierProjectionFailure>> {
        let complete_entries = snapshot
            .into_iter()
            .flat_map(ResolvedContextFrontierSnapshot::ordered_entries)
            .map(|reference| semantic_entries.get(&reference).cloned())
            .collect::<Option<Vec<_>>>()?;
        let projection = match ContextFrontierProjection::from_complete_entries(&complete_entries) {
            Ok(projection) => projection,
            Err(failure) => return Some(Err(failure)),
        };
        let entries_by_reference = complete_entries
            .iter()
            .map(|entry| (entry.reference(), entry))
            .collect::<BTreeMap<_, _>>();
        let mut origins = Vec::new();
        let mut distinct = BTreeSet::new();
        for reference in projection.ordered_entries() {
            let accepted_input = match entries_by_reference.get(&reference)?.payload() {
                SemanticTranscriptEntryPayload::OriginAcceptedInput { accepted_input }
                | SemanticTranscriptEntryPayload::SteeringAcceptedInput {
                    accepted_input, ..
                } => Some(*accepted_input),
                SemanticTranscriptEntryPayload::TurnFailed { .. }
                | SemanticTranscriptEntryPayload::DelegatedTask { .. }
                | SemanticTranscriptEntryPayload::DelegationMessage { .. }
                | SemanticTranscriptEntryPayload::DelegationResult { .. }
                | SemanticTranscriptEntryPayload::ModelIdentityChanged { .. }
                | SemanticTranscriptEntryPayload::ContextSummary { .. }
                | SemanticTranscriptEntryPayload::RunnerPlacementChanged { .. }
                | SemanticTranscriptEntryPayload::TurnCancelled { .. }
                | SemanticTranscriptEntryPayload::AssistantText { .. }
                | SemanticTranscriptEntryPayload::ProviderCompaction { .. }
                | SemanticTranscriptEntryPayload::ProviderReasoning { .. }
                | SemanticTranscriptEntryPayload::AssistantToolUse { .. }
                | SemanticTranscriptEntryPayload::ToolExecutionResult { .. }
                | SemanticTranscriptEntryPayload::ToolDenied { .. }
                | SemanticTranscriptEntryPayload::ToolClosed { .. }
                | SemanticTranscriptEntryPayload::TurnCompleted { .. }
                | SemanticTranscriptEntryPayload::Imported { .. } => None,
            };
            if let Some(accepted_input) = accepted_input.filter(|value| distinct.insert(*value)) {
                origins.push(accepted_input);
            }
        }
        Some(Ok(origins))
    }

    /// Borrows one complete resolved snapshot from this checked projection.
    pub fn resolved_snapshot(
        &self,
        snapshot: ContextFrontierId,
    ) -> Option<&ResolvedContextFrontierSnapshot> {
        self.snapshots.get(&snapshot)
    }

    /// Borrows one canonical semantic entry from this checked projection.
    pub fn semantic_entry(
        &self,
        entry: SemanticTranscriptEntryRef,
    ) -> Option<&SemanticTranscriptEntry> {
        self.semantic_entries.get(&entry)
    }

    /// Closes the active model-call recovery wait under one newly applied
    /// interrupt while preserving its exact ambiguity set.
    pub fn apply_interrupt_to_model_call_recovery(
        self,
        interrupt: AppliedInterruptCommandResult,
        identities: crate::AmbiguousModelCallTurnIdentities,
    ) -> Result<crate::ReconciliationRequiredModelCallTurn, crate::ModelCallClosureError> {
        let active_turn = self
            .active_turn_execution()
            .ok_or(crate::ModelCallClosureError::AttemptStateMismatch)?;
        let recovery = self
            .active_model_call_recovery
            .ok_or(crate::ModelCallClosureError::AttemptStateMismatch)?;
        crate::model_execution::apply_interrupt_to_recovery_wait(
            active_turn.into(),
            recovery.call,
            recovery.attempt,
            recovery.source_snapshot,
            interrupt,
            identities,
        )
    }

    /// Closes the active model-call recovery wait under a daemon-owned durable
    /// attempt while preserving its exact ambiguity set.
    pub fn apply_automatic_reconciliation(
        self,
        attempt: std::num::NonZeroU32,
        identities: crate::AmbiguousModelCallTurnIdentities,
    ) -> Result<crate::ReconciliationRequiredModelCallTurn, crate::ModelCallClosureError> {
        let active_turn = self
            .active_turn_execution()
            .ok_or(crate::ModelCallClosureError::AttemptStateMismatch)?;
        let recovery = self
            .active_model_call_recovery
            .ok_or(crate::ModelCallClosureError::AttemptStateMismatch)?;
        crate::model_execution::apply_automatic_reconciliation(
            active_turn.into(),
            recovery.call,
            recovery.attempt,
            recovery.source_snapshot,
            attempt,
            identities,
        )
    }

    /// Cancels a turn parked on runner loss without claiming that any retained
    /// runner effect failed. The supplied source is the latest already-durable
    /// semantic boundary; runner-loss evidence remains on the placement.
    pub fn apply_interrupt_to_runner_recovery(
        self,
        source_snapshot: ResolvedContextFrontierSnapshot,
        result_projection: Option<crate::PreparedToolResultProjection>,
        interrupt: AppliedInterruptCommandResult,
        identities: crate::CancelledModelCallTurnIdentities,
    ) -> Result<crate::CancelledModelCallTurn, crate::ModelCallClosureError> {
        let active_turn = self
            .active_turn_execution()
            .ok_or(crate::ModelCallClosureError::AttemptStateMismatch)?;
        let starting_snapshot = self
            .snapshots
            .get(&active_turn.start().frontier().snapshot())
            .cloned()
            .ok_or(crate::ModelCallClosureError::FrontierDerivationFailed)?;
        ActivatedTurn::from(active_turn).apply_interrupt_to_runner_recovery(
            starting_snapshot,
            source_snapshot,
            result_projection,
            interrupt,
            identities,
        )
    }

    /// Closes a runner-loss wait that retained one ambiguous physical tool
    /// attempt, preserving that ambiguity as reconciliation-required.
    pub fn apply_interrupt_to_runner_tool_recovery(
        self,
        wait: crate::AwaitingToolRecovery,
        tool_attempt: crate::EndedToolAttempt,
        yielded_attempt: TurnAttemptId,
        result_projection: crate::PreparedToolResultProjection,
        interrupt: AppliedInterruptCommandResult,
        identities: crate::AmbiguousModelCallTurnIdentities,
    ) -> Result<crate::ReconciliationRequiredToolTurn, crate::ModelCallClosureError> {
        let active_turn = self
            .active_turn_execution()
            .ok_or(crate::ModelCallClosureError::AttemptStateMismatch)?;
        crate::model_execution::apply_interrupt_to_runner_tool_recovery_wait(
            active_turn.into(),
            wait,
            tool_attempt,
            yielded_attempt,
            result_projection,
            interrupt,
            identities,
        )
    }

    /// Cancels a runner-loss wait after its retryable physical attempt has
    /// been retired as a known crash loss.
    pub fn apply_interrupt_to_retryable_runner_tool_recovery(
        self,
        batch: crate::ToolBatch,
        result_projection: crate::PreparedToolResultProjection,
        interrupt: AppliedInterruptCommandResult,
        identities: crate::CancelledModelCallTurnIdentities,
    ) -> Result<crate::CancelledModelCallTurn, crate::ModelCallClosureError> {
        let active_turn = self
            .active_turn_execution()
            .ok_or(crate::ModelCallClosureError::AttemptStateMismatch)?;
        let starting_snapshot = self
            .snapshots
            .get(&active_turn.start().frontier().snapshot())
            .cloned()
            .ok_or(crate::ModelCallClosureError::FrontierDerivationFailed)?;
        crate::model_execution::apply_interrupt_to_retryable_runner_tool_recovery_wait(
            active_turn.into(),
            starting_snapshot,
            batch,
            result_projection,
            interrupt,
            identities,
        )
    }

    /// Closes one executing tool batch under a newly applied interrupt.
    ///
    /// The checked scheduling projection supplies the current active phase;
    /// the batch supplies its exact yielded frontier and complete physical
    /// attempt inventory, while the result projection supplies the already
    /// checked logical closures. Result identities are consumed only after
    /// all three projections agree.
    pub fn apply_interrupt_to_tool_batch(
        self,
        batch: crate::ToolBatch,
        result_projection: crate::PreparedToolResultProjection,
        interrupt: AppliedInterruptCommandResult,
        identities: crate::CancelledModelCallTurnIdentities,
    ) -> Result<crate::CancelledModelCallTurn, crate::ModelCallClosureError> {
        let Some(correlation) = self.active_executing_tool_batch else {
            return Err(crate::ModelCallClosureError::InterruptCorrelationMismatch);
        };
        let turn_attempt = match batch.phase() {
            crate::ToolBatchPhase::Executing { turn_attempt } => Some(turn_attempt),
            crate::ToolBatchPhase::AwaitingChild { .. } => None,
            crate::ToolBatchPhase::AwaitingApproval { .. }
            | crate::ToolBatchPhase::AwaitingRecovery { .. } => {
                return Err(crate::ModelCallClosureError::AttemptStateMismatch);
            }
        };
        if correlation.session != batch.session()
            || correlation.turn != batch.turn()
            || correlation.producing_call != batch.producing_call()
            || correlation.yielded_frontier != batch.yielded_snapshot().frontier().snapshot()
            || correlation.turn_attempt != turn_attempt
        {
            return Err(crate::ModelCallClosureError::InterruptCorrelationMismatch);
        }
        let active_turn = self
            .active_turn_execution()
            .ok_or(crate::ModelCallClosureError::AttemptStateMismatch)?;
        crate::model_execution::apply_interrupt_to_executing_tool_batch(
            active_turn.into(),
            batch,
            result_projection,
            interrupt,
            identities,
        )
    }

    /// Closes the active tool-attempt recovery wait under one newly applied
    /// interrupt while preserving its exact ambiguity.
    pub fn apply_interrupt_to_tool_recovery(
        self,
        wait: crate::AwaitingToolRecovery,
        tool_attempt: crate::EndedToolAttempt,
        result_projection: crate::PreparedToolResultProjection,
        interrupt: AppliedInterruptCommandResult,
        identities: crate::AmbiguousModelCallTurnIdentities,
    ) -> Result<crate::ReconciliationRequiredToolTurn, crate::ModelCallClosureError> {
        let active_turn = self
            .active_turn_execution()
            .ok_or(crate::ModelCallClosureError::AttemptStateMismatch)?;
        let attempt = self
            .active_tool_recovery_attempt
            .ok_or(crate::ModelCallClosureError::AttemptStateMismatch)?;
        crate::model_execution::apply_interrupt_to_tool_recovery_wait(
            active_turn.into(),
            wait,
            tool_attempt,
            attempt,
            result_projection,
            interrupt,
            identities,
        )
    }

    /// Closes the active tool-attempt recovery wait under one daemon-owned
    /// durable attempt while preserving its exact physical ambiguity.
    pub fn apply_automatic_tool_reconciliation(
        self,
        wait: crate::AwaitingToolRecovery,
        tool_attempt: crate::EndedToolAttempt,
        result_projection: crate::PreparedToolResultProjection,
        recovery_attempt: std::num::NonZeroU32,
        identities: crate::AmbiguousModelCallTurnIdentities,
    ) -> Result<crate::ReconciliationRequiredToolTurn, crate::ModelCallClosureError> {
        let active_turn = self
            .active_turn_execution()
            .ok_or(crate::ModelCallClosureError::AttemptStateMismatch)?;
        let attempt = self
            .active_tool_recovery_attempt
            .ok_or(crate::ModelCallClosureError::AttemptStateMismatch)?;
        crate::model_execution::apply_automatic_tool_reconciliation(
            active_turn.into(),
            wait,
            tool_attempt,
            attempt,
            result_projection,
            recovery_attempt,
            identities,
        )
    }

    /// Consumes this complete projection and prepares the earliest queued turn
    /// as one sealed commit candidate.
    pub fn prepare_earliest_queued_activation(
        self,
        identities: AcceptedInputTurnActivationIdentities,
    ) -> Result<PreparedAcceptedInputTurnActivation, AcceptedInputEligibilityError> {
        prepare_earliest_queued_activation(self, identities)
    }

    /// Consumes this complete projection and prepares the active prior-process
    /// attempt as one failed-terminal startup-recovery candidate.
    pub fn prepare_active_turn_lost_failure(
        self,
        identities: AcceptedInputTurnFailureIdentities,
    ) -> Result<PreparedAcceptedInputTurnFailure, AcceptedInputTurnFailureError> {
        prepare_active_turn_lost_failure(self, identities)
    }
}

pub(super) fn active_execution_steering_inputs(
    active_turn: TurnId,
    tail: &SessionAcceptanceTail,
) -> (Box<[PendingSteeringInput]>, Box<[ConsumedSteeringInput]>) {
    let pending = tail
        .entries
        .iter()
        .filter(|entry| {
            matches!(
                entry.accepted_input.disposition(),
                AcceptedInputDisposition::PendingSteering { .. }
            )
        })
        .map(|entry| PendingSteeringInput {
            accepted_input: entry.accepted_input.clone(),
            acceptance_position: entry.position,
        })
        .collect::<Vec<_>>()
        .into_boxed_slice();
    let consumed = tail
        .entries
        .iter()
        .filter_map(|entry| {
            let AcceptedInputDisposition::ConsumedAsSteering { .. } =
                entry.accepted_input.disposition()
            else {
                return None;
            };
            let DeliveryRequest::NextSafePoint {
                expected_active_turn,
            } = entry.delivery
            else {
                return None;
            };
            (expected_active_turn == active_turn).then(|| ConsumedSteeringInput {
                accepted_input: entry.accepted_input.clone(),
                acceptance_position: entry.position,
                source_turn: expected_active_turn,
            })
        })
        .collect::<Vec<_>>()
        .into_boxed_slice();
    (pending, consumed)
}
