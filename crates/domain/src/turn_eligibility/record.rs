//! Turn scheduling record for `docs/spec/turn-lifecycle-and-scheduling.md`.

#[cfg(doc)]
use crate::AcceptedInputTurnStart;

use super::reconstitution_input::{
    ActiveTurnSchedulingReconstitutionInput, CancelledTurnExecutionReconstitutionInput,
    FailedTurnExecutionReconstitutionInput, TerminalAttemptEndReconstitutionInput,
};
use crate::{
    AcceptedInputLifecycle, AcceptedInputQueueOrder, AcceptedInputStartingLineage,
    AppliedInterruptCommandResult, ContextFrontierId, DeliveryRequest, DirectModelSelection,
    OriginConfiguration, SessionId, TurnAttemptId, TurnConfigurationProvenance, TurnId,
};

/// The lifecycle fact stored for one accepted-input scheduling record.
///
/// Started variants name raw lineage and snapshot identities only as
/// reconstitution candidates. They become opaque [`AcceptedInputTurnStart`]
/// values solely after collection-wide validation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AcceptedInputTurnSchedulingRecordState {
    /// No start, semantic origin entry, snapshot, or attempt exists.
    Queued,
    /// The unstarted turn was retired; its immutable origin still proves an interrupt.
    Retired,
    /// The turn owns the session's progressing slot.
    Active {
        /// The stored lineage selected at eligibility.
        starting_lineage: AcceptedInputStartingLineage,
        /// The stored starting snapshot identity.
        starting_frontier: ContextFrontierId,
        /// The exact phase and its asserted owning turn.
        phase: ActiveTurnSchedulingReconstitutionInput,
    },
    /// The turn reached a known-failure disposition.
    TerminalFailed {
        /// The stored lineage selected at eligibility.
        starting_lineage: AcceptedInputStartingLineage,
        /// The stored starting snapshot identity.
        starting_frontier: ContextFrontierId,
        /// The complete terminal execution provenance, when the failure
        /// followed a physical attempt.
        terminal_execution: Option<FailedTurnExecutionReconstitutionInput>,
        /// The complete frontier through the appended failed marker.
        terminal_frontier: ContextFrontierId,
    },
    /// The turn committed a complete assistant response and completion marker.
    TerminalCompleted {
        /// The stored lineage selected at eligibility.
        starting_lineage: AcceptedInputStartingLineage,
        /// The stored starting snapshot identity.
        starting_frontier: ContextFrontierId,
        /// The ended physical attempt that supplied the completed call.
        completing_attempt: TurnAttemptId,
        /// The complete stored end classification for that attempt.
        completing_attempt_end: TerminalAttemptEndReconstitutionInput,
        /// The outcome-authoritative call that completed the turn.
        completing_call: crate::ModelCallId,
        /// The complete frontier through the final completion marker.
        terminal_frontier: ContextFrontierId,
    },
    /// The turn committed an explicit refusal without semantic response content.
    TerminalRefused {
        /// The stored lineage selected at eligibility.
        starting_lineage: AcceptedInputStartingLineage,
        /// The stored starting snapshot identity.
        starting_frontier: ContextFrontierId,
        /// The ended physical attempt that supplied the refusal.
        refusing_attempt: TurnAttemptId,
        /// The complete stored end classification for that attempt.
        refusing_attempt_end: TerminalAttemptEndReconstitutionInput,
        /// The outcome-authoritative call that refused the request.
        refusing_call: crate::ModelCallId,
        /// The equal-content terminal frontier identifying the turn boundary.
        terminal_frontier: ContextFrontierId,
    },
    /// The turn ended from one exactly applied and confirmed interrupt.
    TerminalCancelled {
        /// The stored lineage selected at eligibility.
        starting_lineage: AcceptedInputStartingLineage,
        /// The stored starting snapshot identity.
        starting_frontier: ContextFrontierId,
        /// The complete proof-bearing terminal execution provenance.
        terminal_execution: CancelledTurnExecutionReconstitutionInput,
        /// The complete frontier through the cancellation marker.
        terminal_frontier: ContextFrontierId,
    },
    /// The turn released its slot while one interrupted call remains
    /// durably ambiguous.
    TerminalReconciliationRequired {
        /// The stored lineage selected at eligibility.
        starting_lineage: AcceptedInputStartingLineage,
        /// The stored starting snapshot identity.
        starting_frontier: ContextFrontierId,
        /// The ended attempt that owns the ambiguous call.
        reconciling_attempt: TurnAttemptId,
        /// The preserved stored end classification for that attempt.
        reconciling_attempt_end: TerminalAttemptEndReconstitutionInput,
        /// The exact ambiguous physical call.
        ambiguous_call: crate::ModelCallId,
        /// The exact durable authority that requires reconciliation.
        authority: AutomaticReconciliationAuthority,
        /// The equal-content terminal frontier identifying the turn boundary.
        terminal_frontier: ContextFrontierId,
    },
    /// The turn released its slot while one interrupted tool attempt remains
    /// durably ambiguous.
    TerminalToolReconciliationRequired {
        /// The stored lineage selected at eligibility.
        starting_lineage: AcceptedInputStartingLineage,
        /// The stored starting snapshot identity.
        starting_frontier: ContextFrontierId,
        /// The ended turn attempt that owns the ambiguous tool attempt.
        reconciling_attempt: TurnAttemptId,
        /// The preserved stored end classification for that attempt.
        reconciling_attempt_end: TerminalAttemptEndReconstitutionInput,
        /// Complete checked batch carrying the exact ambiguous tool attempt.
        tool_batch: crate::ToolBatch,
        /// The exact durable authority that requires reconciliation.
        authority: AutomaticReconciliationAuthority,
        /// The exact proposal-ordered result-suffix terminal frontier.
        terminal_frontier: ContextFrontierId,
    },
}

/// Durable authority for one automatic reconciliation terminal boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AutomaticReconciliationAuthority {
    /// A later or already-applied interrupt left the operation ambiguous.
    AppliedInterrupt(AppliedInterruptCommandResult),
    /// The daemon spent one recorded automatic recovery attempt.
    AutomaticRecovery {
        /// The one-based durable recovery attempt that terminalized the turn.
        attempt: std::num::NonZeroU32,
    },
}

/// Stored lifecycle classification for one delegation-origin turn retained by
/// an accepted-input scheduling projection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DelegatedTurnSchedulingState {
    /// The delegated turn still owns its physical runtime slot.
    Active,
    /// Parent-command authority made the delegated turn logically terminal
    /// without rewriting its retained physical lifecycle state.
    RuntimeTerminal,
    /// The delegated turn completed with delivered assistant content.
    TerminalCompleted,
    /// The delegated turn completed with an explicit refusal.
    TerminalRefused,
    /// The delegated turn ended with a known failure.
    TerminalFailed,
    /// The delegated turn ended from applied cancellation authority.
    TerminalCancelled,
    /// The delegated turn ended with unresolved physical ambiguity.
    TerminalReconciliationRequired,
}

/// Complete configuration and lifecycle facts for one delegation-origin turn
/// referenced outside the accepted-input turn collection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DelegatedTurnSchedulingFact {
    turn: TurnId,
    defaults_version: crate::SessionConfigurationDefaultsVersion,
    selected: DirectModelSelection,
    state: DelegatedTurnSchedulingState,
}

impl DelegatedTurnSchedulingFact {
    /// Records the exact stored configuration and lifecycle projection.
    pub const fn new(
        turn: TurnId,
        defaults_version: crate::SessionConfigurationDefaultsVersion,
        selected: DirectModelSelection,
        state: DelegatedTurnSchedulingState,
    ) -> Self {
        Self {
            turn,
            defaults_version,
            selected,
            state,
        }
    }

    /// Returns the delegation-origin turn identity.
    pub const fn turn(&self) -> TurnId {
        self.turn
    }

    /// Returns the defaults epoch frozen by the delegated origin.
    pub const fn defaults_version(&self) -> crate::SessionConfigurationDefaultsVersion {
        self.defaults_version
    }

    /// Returns the exact selected direct model frozen by the delegated origin.
    pub const fn selected(&self) -> DirectModelSelection {
        self.selected
    }

    /// Returns the stored lifecycle classification.
    pub const fn state(&self) -> DelegatedTurnSchedulingState {
        self.state
    }
}

/// Complete checked values supplied for one accepted-input scheduling record.
///
/// Repeated session and turn correlations retain independently stored facts so
/// reconstitution rejects cross-wired accepted-input and queue records.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AcceptedInputTurnSchedulingRecord {
    pub(super) stored_session: SessionId,
    pub(super) turn: TurnId,
    pub(super) accepted_input_session: SessionId,
    pub(super) accepted_input: AcceptedInputLifecycle,
    pub(super) queue_session: SessionId,
    pub(super) queue_turn: TurnId,
    pub(super) order: AcceptedInputQueueOrder,
    pub(super) origin_delivery: DeliveryRequest,
    pub(super) origin_configuration: OriginConfiguration,
    pub(super) configuration_provenance: TurnConfigurationProvenance,
    pub(super) model_identity_boundary_required: bool,
    pub(super) state: AcceptedInputTurnSchedulingRecordState,
}

impl AcceptedInputTurnSchedulingRecord {
    /// Supplies all typed stored facts for one scheduling record.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        stored_session: SessionId,
        turn: TurnId,
        accepted_input_session: SessionId,
        accepted_input: AcceptedInputLifecycle,
        queue_session: SessionId,
        queue_turn: TurnId,
        order: AcceptedInputQueueOrder,
        origin_delivery: DeliveryRequest,
        origin_configuration: OriginConfiguration,
        state: AcceptedInputTurnSchedulingRecordState,
    ) -> Self {
        Self {
            stored_session,
            turn,
            accepted_input_session,
            accepted_input,
            queue_session,
            queue_turn,
            order,
            origin_delivery,
            configuration_provenance: TurnConfigurationProvenance::ExplicitOrigin(
                origin_configuration.clone(),
            ),
            origin_configuration,
            model_identity_boundary_required: true,
            state,
        }
    }

    /// Supplies a reclassified steering origin using its immutable receipt,
    /// original position, source binding, and source-derived configuration.
    #[allow(clippy::too_many_arguments)]
    pub fn reclassified(
        stored_session: SessionId,
        turn: TurnId,
        accepted_input_session: SessionId,
        accepted_input: AcceptedInputLifecycle,
        queue_session: SessionId,
        queue_turn: TurnId,
        order: AcceptedInputQueueOrder,
        origin_delivery: DeliveryRequest,
        binding: crate::SteeringBinding,
        source_configuration: OriginConfiguration,
        state: AcceptedInputTurnSchedulingRecordState,
    ) -> Self {
        Self {
            stored_session,
            turn,
            accepted_input_session,
            accepted_input,
            queue_session,
            queue_turn,
            order,
            origin_delivery,
            origin_configuration: source_configuration,
            configuration_provenance: TurnConfigurationProvenance::InheritedForReclassifiedSteering(
                binding,
            ),
            model_identity_boundary_required: true,
            state,
        }
    }

    /// Marks a started record as predating durable model-identity boundaries.
    ///
    /// This is only for reconstituting frontiers committed before the boundary
    /// law existed. Newly accepted queued work remains subject to the law.
    pub fn without_legacy_model_identity_boundary(mut self) -> Self {
        self.model_identity_boundary_required = false;
        self
    }

    /// Returns the session identity on the stored turn record.
    pub const fn stored_session(&self) -> SessionId {
        self.stored_session
    }

    /// Returns the stored turn identity.
    pub const fn turn(&self) -> TurnId {
        self.turn
    }

    /// Returns the session identity on the accepted-input record.
    pub const fn accepted_input_session(&self) -> SessionId {
        self.accepted_input_session
    }

    /// Borrows the accepted input and its exact stored disposition.
    pub const fn accepted_input(&self) -> &AcceptedInputLifecycle {
        &self.accepted_input
    }

    /// Returns the session identity on the queue record.
    pub const fn queue_session(&self) -> SessionId {
        self.queue_session
    }

    /// Returns the turn identity on the queue record.
    pub const fn queue_turn(&self) -> TurnId {
        self.queue_turn
    }

    /// Returns the immutable queue-order facts.
    pub const fn order(&self) -> AcceptedInputQueueOrder {
        self.order
    }

    /// Returns the immutable accepted delivery that created this origin.
    pub const fn origin_delivery(&self) -> DeliveryRequest {
        self.origin_delivery
    }

    /// Borrows the complete canonical configuration, whether explicit or
    /// inherited from reclassified steering's source turn.
    pub const fn origin_configuration(&self) -> &OriginConfiguration {
        &self.origin_configuration
    }

    /// Borrows the checked explicit or inherited configuration provenance.
    pub const fn configuration_provenance(&self) -> &TurnConfigurationProvenance {
        &self.configuration_provenance
    }

    /// Returns the stored lifecycle projection.
    pub const fn state(&self) -> &AcceptedInputTurnSchedulingRecordState {
        &self.state
    }
}
