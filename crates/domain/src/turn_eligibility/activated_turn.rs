//! Turn scheduling activated turn for `docs/spec/turn-lifecycle-and-scheduling.md`.

#[cfg(test)]
use crate::{AcceptedInputId, SessionInputPosition};

use super::projection::{ConsumedSteeringInput, PendingSteeringInput};
use super::reconstitution_input::ConsumedSteeringReconstitutionInput;
use crate::{
    AcceptedInputDisposition, AcceptedInputLifecycle, AcceptedInputQueueOrder,
    AcceptedInputTurnStart, ActiveTurnPhase, AppliedInterruptCommandResult, ContextFrontierId,
    DelegationContent, EndedTurnAttempt, OriginConfiguration, ResolvedContextFrontierSnapshot,
    SemanticTranscriptEntry, SemanticTranscriptEntryId, SemanticTranscriptEntryReconstitutionInput,
    SessionId, ToolRequestId, TurnAttemptId, TurnConfigurationProvenance, TurnId,
};
use std::num::NonZeroU64;

/// Fresh identities supplied for one eligibility-time activation candidate.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AcceptedInputTurnActivationIdentities {
    pub(super) model_identity_entry: SemanticTranscriptEntryId,
    pub(super) origin_entry: SemanticTranscriptEntryId,
    pub(super) starting_frontier: ContextFrontierId,
    pub(super) initial_attempt: TurnAttemptId,
}

impl AcceptedInputTurnActivationIdentities {
    /// Supplies all four candidates, including the optional injected entry.
    pub const fn new(
        model_identity_entry: SemanticTranscriptEntryId,
        origin_entry: SemanticTranscriptEntryId,
        starting_frontier: ContextFrontierId,
        initial_attempt: TurnAttemptId,
    ) -> Self {
        Self {
            model_identity_entry,
            origin_entry,
            starting_frontier,
            initial_attempt,
        }
    }

    /// Returns the proposed injected model-identity entry.
    pub const fn model_identity_entry(&self) -> SemanticTranscriptEntryId {
        self.model_identity_entry
    }

    /// Returns the proposed origin semantic-entry identity.
    pub const fn origin_entry(&self) -> SemanticTranscriptEntryId {
        self.origin_entry
    }

    /// Returns the proposed starting snapshot identity.
    pub const fn starting_frontier(&self) -> ContextFrontierId {
        self.starting_frontier
    }

    /// Returns the proposed initial attempt identity.
    pub const fn initial_attempt(&self) -> TurnAttemptId {
        self.initial_attempt
    }
}

/// Exact checked active turn state prepared or reconstituted by eligibility.
///
/// Raw aggregate facts cannot construct this state:
///
/// ```compile_fail
/// use signalbox_domain::{
///     AcceptedInputLifecycle, AcceptedInputQueueOrder, AcceptedInputTurnStart,
///     ActivatedAcceptedInputTurn, ActiveTurnPhase, OriginConfiguration, SessionId,
///     TurnConfigurationProvenance, TurnId,
/// };
///
/// fn raw_facts_are_not_an_activation(
///     session: SessionId,
///     turn: TurnId,
///     accepted_input: AcceptedInputLifecycle,
///     order: AcceptedInputQueueOrder,
///     configuration: OriginConfiguration,
///     configuration_provenance: TurnConfigurationProvenance,
///     start: AcceptedInputTurnStart,
///     phase: ActiveTurnPhase,
///     pending_steering: Box<[PendingSteeringInput]>,
///     consumed_steering: Box<[ConsumedSteeringInput]>,
/// ) {
///     let _ = ActivatedAcceptedInputTurn {
///         session,
///         turn,
///         accepted_input,
///         order,
///         configuration,
///         configuration_provenance,
///         start,
///         phase,
///         pending_steering,
///         consumed_steering,
///     };
/// }
/// ```
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActivatedAcceptedInputTurn {
    pub(super) session: SessionId,
    pub(super) turn: TurnId,
    pub(super) accepted_input: AcceptedInputLifecycle,
    pub(super) order: AcceptedInputQueueOrder,
    pub(super) configuration: OriginConfiguration,
    pub(super) configuration_provenance: TurnConfigurationProvenance,
    pub(super) start: AcceptedInputTurnStart,
    pub(super) phase: ActiveTurnPhase,
    pub(super) pending_steering: Box<[PendingSteeringInput]>,
    pub(super) consumed_steering: Box<[ConsumedSteeringInput]>,
}

impl ActivatedAcceptedInputTurn {
    /// Returns the owning session.
    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// Returns the activated logical turn.
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

    /// Borrows the complete frozen origin configuration.
    pub const fn configuration(&self) -> &OriginConfiguration {
        &self.configuration
    }

    /// Borrows the explicit or inherited configuration provenance.
    pub const fn configuration_provenance(&self) -> &TurnConfigurationProvenance {
        &self.configuration_provenance
    }

    /// Returns the exact eligibility-fixed lineage and frontier.
    pub const fn start(&self) -> AcceptedInputTurnStart {
        self.start
    }

    /// Borrows the exact initial active phase.
    pub const fn phase(&self) -> &ActiveTurnPhase {
        &self.phase
    }

    /// Returns the complete accepted inputs that still await this turn's next
    /// model-call safe point or terminal reclassification.
    pub fn pending_steering(&self) -> &[PendingSteeringInput] {
        &self.pending_steering
    }

    /// Returns consumed steering in immutable acceptance order.
    pub fn consumed_steering(&self) -> &[ConsumedSteeringInput] {
        &self.consumed_steering
    }

    #[cfg(test)]
    pub(crate) fn with_phase_for_test(&self, phase: ActiveTurnPhase) -> Self {
        Self {
            session: self.session,
            turn: self.turn,
            accepted_input: self.accepted_input.clone(),
            order: self.order,
            configuration: self.configuration.clone(),
            configuration_provenance: self.configuration_provenance.clone(),
            start: self.start,
            phase,
            pending_steering: self.pending_steering.clone(),
            consumed_steering: self.consumed_steering.clone(),
        }
    }

    #[cfg(test)]
    pub(crate) fn with_start_for_test(&self, start: AcceptedInputTurnStart) -> Self {
        Self {
            session: self.session,
            turn: self.turn,
            accepted_input: self.accepted_input.clone(),
            order: self.order,
            configuration: self.configuration.clone(),
            configuration_provenance: self.configuration_provenance.clone(),
            start,
            phase: self.phase.clone(),
            pending_steering: self.pending_steering.clone(),
            consumed_steering: self.consumed_steering.clone(),
        }
    }

    #[cfg(test)]
    pub(crate) fn with_pending_steering_for_test(
        &self,
        pending_steering: Box<[(AcceptedInputId, SessionInputPosition)]>,
    ) -> Self {
        let pending_steering = pending_steering
            .into_vec()
            .into_iter()
            .map(
                |(accepted_input, acceptance_position)| PendingSteeringInput {
                    accepted_input: AcceptedInputLifecycle::new(
                        accepted_input,
                        AcceptedInputDisposition::PendingSteering {
                            binding: crate::SteeringBinding::new(self.turn),
                        },
                    ),
                    acceptance_position,
                },
            )
            .collect::<Vec<_>>()
            .into_boxed_slice();
        Self {
            session: self.session,
            turn: self.turn,
            accepted_input: self.accepted_input.clone(),
            order: self.order,
            configuration: self.configuration.clone(),
            configuration_provenance: self.configuration_provenance.clone(),
            start: self.start,
            phase: self.phase.clone(),
            pending_steering,
            consumed_steering: self.consumed_steering.clone(),
        }
    }

    #[cfg(test)]
    pub(crate) fn with_consumed_steering_for_test(
        &self,
        consumed_steering: Box<[(AcceptedInputId, SessionInputPosition, crate::ModelCallId)]>,
    ) -> Self {
        let consumed_steering = consumed_steering
            .into_vec()
            .into_iter()
            .map(
                |(accepted_input, acceptance_position, call)| ConsumedSteeringInput {
                    accepted_input: AcceptedInputLifecycle::new(
                        accepted_input,
                        AcceptedInputDisposition::ConsumedAsSteering { call },
                    ),
                    acceptance_position,
                    source_turn: self.turn,
                },
            )
            .collect::<Vec<_>>()
            .into_boxed_slice();
        Self {
            session: self.session,
            turn: self.turn,
            accepted_input: self.accepted_input.clone(),
            order: self.order,
            configuration: self.configuration.clone(),
            configuration_provenance: self.configuration_provenance.clone(),
            start: self.start,
            phase: self.phase.clone(),
            pending_steering: Box::new([]),
            consumed_steering,
        }
    }
}

/// Checked active turn whose immutable origin is a delegated task rather than
/// an accepted input.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActivatedDelegatedTurn {
    pub(super) session: SessionId,
    pub(super) turn: TurnId,
    pub(super) origin: ActivatedDelegatedTurnOrigin,
    pub(super) configuration: OriginConfiguration,
    pub(super) start: AcceptedInputTurnStart,
    pub(super) phase: ActiveTurnPhase,
    pub(super) pending_steering: Box<[PendingSteeringInput]>,
    pub(super) consumed_steering: Box<[ConsumedSteeringInput]>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum ActivatedDelegatedTurnOrigin {
    InitialTask {
        spawning_request: ToolRequestId,
        task: DelegationContent,
    },
    PendingDeliveries {
        first: NonZeroU64,
        through: NonZeroU64,
    },
}

impl ActivatedDelegatedTurn {
    pub const fn session(&self) -> SessionId {
        self.session
    }

    pub const fn turn(&self) -> TurnId {
        self.turn
    }

    pub const fn spawning_request(&self) -> Option<ToolRequestId> {
        match &self.origin {
            ActivatedDelegatedTurnOrigin::InitialTask {
                spawning_request, ..
            } => Some(*spawning_request),
            ActivatedDelegatedTurnOrigin::PendingDeliveries { .. } => None,
        }
    }

    pub const fn task(&self) -> Option<&DelegationContent> {
        match &self.origin {
            ActivatedDelegatedTurnOrigin::InitialTask { task, .. } => Some(task),
            ActivatedDelegatedTurnOrigin::PendingDeliveries { .. } => None,
        }
    }

    pub const fn delivery_range(&self) -> Option<(NonZeroU64, NonZeroU64)> {
        match &self.origin {
            ActivatedDelegatedTurnOrigin::InitialTask { .. } => None,
            ActivatedDelegatedTurnOrigin::PendingDeliveries { first, through } => {
                Some((*first, *through))
            }
        }
    }

    pub const fn configuration(&self) -> &OriginConfiguration {
        &self.configuration
    }

    pub const fn start(&self) -> AcceptedInputTurnStart {
        self.start
    }

    pub const fn phase(&self) -> &ActiveTurnPhase {
        &self.phase
    }

    /// Attaches the complete accepted-input steering tail targeting this turn.
    pub fn with_pending_steering(
        mut self,
        pending_steering: Vec<PendingSteeringInput>,
    ) -> Option<Self> {
        if pending_steering.iter().any(|pending| {
            !matches!(
                pending.lifecycle().disposition(),
                AcceptedInputDisposition::PendingSteering { binding }
                    if binding.source_turn() == self.turn
            )
        }) {
            return None;
        }
        self.pending_steering = pending_steering.into_boxed_slice();
        Some(self)
    }

    /// Attaches every stored steering input consumed by this delegated turn.
    pub fn with_consumed_steering(
        mut self,
        consumed_steering: Vec<ConsumedSteeringReconstitutionInput>,
    ) -> Option<Self> {
        self.consumed_steering = consumed_steering
            .into_iter()
            .map(|consumed| {
                (consumed.session() == self.session
                    && consumed.source_turn() == self.turn
                    && matches!(
                        consumed.accepted_input().disposition(),
                        AcceptedInputDisposition::ConsumedAsSteering { .. }
                    ))
                .then(|| ConsumedSteeringInput {
                    accepted_input: consumed.accepted_input().clone(),
                    acceptance_position: consumed.acceptance_position(),
                    source_turn: consumed.source_turn(),
                })
            })
            .collect::<Option<Vec<_>>>()?
            .into_boxed_slice();
        Some(self)
    }

    pub fn pending_steering(&self) -> &[PendingSteeringInput] {
        &self.pending_steering
    }

    /// Returns consumed steering in immutable acceptance order.
    pub fn consumed_steering(&self) -> &[ConsumedSteeringInput] {
        &self.consumed_steering
    }
}

/// Origin-agnostic active turn consumed by model execution.
#[derive(Clone, Debug, Eq, PartialEq)]
// Both variants remain inline so activation reconstitution preserves the
// established public value shape across accepted-input and delegation origins.
#[allow(clippy::large_enum_variant)]
pub enum ActivatedTurn {
    Accepted(ActivatedAcceptedInputTurn),
    Delegated(ActivatedDelegatedTurn),
}

impl From<ActivatedAcceptedInputTurn> for ActivatedTurn {
    fn from(value: ActivatedAcceptedInputTurn) -> Self {
        Self::Accepted(value)
    }
}

impl From<ActivatedDelegatedTurn> for ActivatedTurn {
    fn from(value: ActivatedDelegatedTurn) -> Self {
        Self::Delegated(value)
    }
}

impl ActivatedTurn {
    /// Borrows the accepted-input origin when this is an accepted-input turn.
    pub const fn accepted_input(&self) -> Option<&AcceptedInputLifecycle> {
        match self {
            Self::Accepted(turn) => Some(turn.accepted_input()),
            Self::Delegated(_) => None,
        }
    }

    /// Borrows the delegated origin when this is a delegated turn.
    pub const fn delegated(&self) -> Option<&ActivatedDelegatedTurn> {
        match self {
            Self::Accepted(_) => None,
            Self::Delegated(turn) => Some(turn),
        }
    }

    /// Seals stored semantic entries for this active turn's model frontier.
    pub fn reconstitute_frontier_entries(
        &self,
        entries: Vec<SemanticTranscriptEntryReconstitutionInput>,
    ) -> Option<Vec<SemanticTranscriptEntry>> {
        entries
            .into_iter()
            .map(|entry| {
                (entry.source_session() == self.session()).then(|| {
                    SemanticTranscriptEntry::from_validated_parts(
                        entry.identity(),
                        entry.source_session(),
                        entry.payload().clone(),
                    )
                })
            })
            .collect()
    }

    pub const fn session(&self) -> SessionId {
        match self {
            Self::Accepted(turn) => turn.session(),
            Self::Delegated(turn) => turn.session(),
        }
    }

    pub const fn turn(&self) -> TurnId {
        match self {
            Self::Accepted(turn) => turn.turn(),
            Self::Delegated(turn) => turn.turn(),
        }
    }

    pub const fn configuration(&self) -> &OriginConfiguration {
        match self {
            Self::Accepted(turn) => turn.configuration(),
            Self::Delegated(turn) => turn.configuration(),
        }
    }

    pub fn configuration_provenance(&self) -> TurnConfigurationProvenance {
        match self {
            Self::Accepted(turn) => turn.configuration_provenance().clone(),
            Self::Delegated(turn) => {
                TurnConfigurationProvenance::ExplicitOrigin(turn.configuration().clone())
            }
        }
    }

    pub const fn start(&self) -> AcceptedInputTurnStart {
        match self {
            Self::Accepted(turn) => turn.start(),
            Self::Delegated(turn) => turn.start(),
        }
    }

    pub const fn phase(&self) -> &ActiveTurnPhase {
        match self {
            Self::Accepted(turn) => turn.phase(),
            Self::Delegated(turn) => turn.phase(),
        }
    }

    pub fn pending_steering(&self) -> &[PendingSteeringInput] {
        match self {
            Self::Accepted(turn) => turn.pending_steering(),
            Self::Delegated(turn) => turn.pending_steering(),
        }
    }

    pub fn consumed_steering(&self) -> &[ConsumedSteeringInput] {
        match self {
            Self::Accepted(turn) => turn.consumed_steering(),
            Self::Delegated(turn) => turn.consumed_steering(),
        }
    }

    /// Applies one daemon-owned reconciliation attempt to a checked
    /// origin-agnostic model-call recovery wait.
    pub fn apply_automatic_model_call_reconciliation(
        self,
        call: crate::EndedModelCall,
        attempt: EndedTurnAttempt,
        source_snapshot: ResolvedContextFrontierSnapshot,
        recovery_attempt: std::num::NonZeroU32,
        identities: crate::AmbiguousModelCallTurnIdentities,
    ) -> Result<crate::ReconciliationRequiredModelCallTurn, crate::ModelCallClosureError> {
        crate::model_execution::apply_automatic_reconciliation(
            self,
            call,
            attempt,
            source_snapshot,
            recovery_attempt,
            identities,
        )
    }

    /// Cancels this turn while it is parked on exact runner-loss evidence.
    pub fn apply_interrupt_to_runner_recovery(
        self,
        starting_snapshot: ResolvedContextFrontierSnapshot,
        source_snapshot: ResolvedContextFrontierSnapshot,
        result_projection: Option<crate::PreparedToolResultProjection>,
        interrupt: AppliedInterruptCommandResult,
        identities: crate::CancelledModelCallTurnIdentities,
    ) -> Result<crate::CancelledModelCallTurn, crate::ModelCallClosureError> {
        crate::model_execution::apply_interrupt_to_runner_recovery_wait(
            self,
            starting_snapshot,
            source_snapshot,
            result_projection,
            interrupt,
            identities,
        )
    }

    /// Closes a delegated runner-loss wait that retained one ambiguous
    /// physical tool attempt without erasing that ambiguity.
    pub fn apply_interrupt_to_runner_tool_recovery(
        self,
        wait: crate::AwaitingToolRecovery,
        tool_attempt: crate::EndedToolAttempt,
        yielded_attempt: TurnAttemptId,
        result_projection: crate::PreparedToolResultProjection,
        interrupt: AppliedInterruptCommandResult,
        identities: crate::AmbiguousModelCallTurnIdentities,
    ) -> Result<crate::ReconciliationRequiredToolTurn, crate::ModelCallClosureError> {
        crate::model_execution::apply_interrupt_to_runner_tool_recovery_wait(
            self,
            wait,
            tool_attempt,
            yielded_attempt,
            result_projection,
            interrupt,
            identities,
        )
    }

    /// Cancels a delegated runner-loss wait after its retryable physical
    /// attempt has been retired as a known crash loss.
    pub fn apply_interrupt_to_retryable_runner_tool_recovery(
        self,
        starting_snapshot: ResolvedContextFrontierSnapshot,
        batch: crate::ToolBatch,
        result_projection: crate::PreparedToolResultProjection,
        interrupt: AppliedInterruptCommandResult,
        identities: crate::CancelledModelCallTurnIdentities,
    ) -> Result<crate::CancelledModelCallTurn, crate::ModelCallClosureError> {
        crate::model_execution::apply_interrupt_to_retryable_runner_tool_recovery_wait(
            self,
            starting_snapshot,
            batch,
            result_projection,
            interrupt,
            identities,
        )
    }

    #[cfg(test)]
    pub(crate) fn with_phase_for_test(&self, phase: ActiveTurnPhase) -> Self {
        match self {
            Self::Accepted(turn) => Self::Accepted(turn.with_phase_for_test(phase)),
            Self::Delegated(turn) => {
                let mut delegated = turn.clone();
                delegated.phase = phase;
                Self::Delegated(delegated)
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn with_start_for_test(&self, start: AcceptedInputTurnStart) -> Self {
        match self {
            Self::Accepted(turn) => Self::Accepted(turn.with_start_for_test(start)),
            Self::Delegated(turn) => {
                let mut delegated = turn.clone();
                delegated.start = start;
                Self::Delegated(delegated)
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn with_pending_steering_for_test(
        &self,
        pending: Box<[(AcceptedInputId, SessionInputPosition)]>,
    ) -> Self {
        match self {
            Self::Accepted(turn) => Self::Accepted(turn.with_pending_steering_for_test(pending)),
            Self::Delegated(turn) => {
                let pending = pending
                    .into_vec()
                    .into_iter()
                    .map(
                        |(accepted_input, acceptance_position)| PendingSteeringInput {
                            accepted_input: AcceptedInputLifecycle::new(
                                accepted_input,
                                AcceptedInputDisposition::PendingSteering {
                                    binding: crate::SteeringBinding::new(turn.turn),
                                },
                            ),
                            acceptance_position,
                        },
                    )
                    .collect::<Vec<_>>();
                Self::Delegated(
                    turn.clone()
                        .with_pending_steering(pending)
                        .expect("the test steering targets the delegated turn"),
                )
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn with_consumed_steering_for_test(
        &self,
        consumed: Box<[(AcceptedInputId, SessionInputPosition, crate::ModelCallId)]>,
    ) -> Self {
        match self {
            Self::Accepted(turn) => Self::Accepted(turn.with_consumed_steering_for_test(consumed)),
            Self::Delegated(turn) => {
                let consumed = consumed
                    .into_vec()
                    .into_iter()
                    .map(|(accepted_input, acceptance_position, call)| {
                        ConsumedSteeringReconstitutionInput::new(
                            turn.session,
                            AcceptedInputLifecycle::new(
                                accepted_input,
                                AcceptedInputDisposition::ConsumedAsSteering { call },
                            ),
                            acceptance_position,
                            turn.turn,
                        )
                    })
                    .collect();
                Self::Delegated(
                    turn.clone()
                        .with_consumed_steering(consumed)
                        .expect("the test steering targets the delegated turn"),
                )
            }
        }
    }
}
