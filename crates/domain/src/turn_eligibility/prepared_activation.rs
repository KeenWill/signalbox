//! Turn scheduling prepared activation for `docs/spec/turn-lifecycle-and-scheduling.md`.

use super::activated_turn::{
    ActivatedAcceptedInputTurn, ActivatedDelegatedTurn, ActivatedDelegatedTurnOrigin, ActivatedTurn,
};
use super::reconstitution_input::{
    ActiveTurnSchedulingReconstitutionInput, DelegatedModelCallRecoveryReconstitutionInput,
    StoredActiveTurnPhase,
};
use crate::{
    AcceptedInputStartingLineage, AcceptedInputTurnStart, ActiveTurnPhase, AttemptEnd,
    CancellationStopDisposition, ContextFrontierId, CurrentTurnAttempt, DelegationContent,
    DelegationWaitMode, EndedTurnAttempt, ModelCallDisposition, NonEmptyIssuedOperationRefs,
    OriginConfiguration, ResolvedContextFrontierSnapshot, SemanticTranscriptEntry,
    SemanticTranscriptEntryPayload, SemanticTranscriptEntryReconstitutionInput, SessionId,
    ToolRequestId, TurnAttemptId, TurnId, UnstoppedAttemptDisposition,
};
use std::num::NonZeroU64;

/// Complete durable facts for preparing one delegated initial-task activation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DelegatedTurnActivationInput {
    pub session: SessionId,
    pub turn: TurnId,
    pub spawning_request: ToolRequestId,
    pub task: DelegationContent,
    pub task_entry: SemanticTranscriptEntryReconstitutionInput,
    pub configuration: OriginConfiguration,
    pub starting_frontier: ContextFrontierId,
    pub initial_attempt: TurnAttemptId,
}

/// Complete durable facts for preparing one idle delegation-delivery wake.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DelegatedWakeTurnActivationInput {
    pub session: SessionId,
    pub turn: TurnId,
    pub first_delivery_sequence: NonZeroU64,
    pub through_delivery_sequence: NonZeroU64,
    pub deliveries: Vec<SemanticTranscriptEntryReconstitutionInput>,
    pub predecessor: TurnId,
    pub predecessor_snapshot: ResolvedContextFrontierSnapshot,
    pub configuration: OriginConfiguration,
    pub starting_frontier: ContextFrontierId,
    pub initial_attempt: TurnAttemptId,
}

/// Sealed delegated activation candidate.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreparedDelegatedTurnActivation {
    turn: ActivatedDelegatedTurn,
    starting_entries: Vec<SemanticTranscriptEntry>,
    starting_snapshot: ResolvedContextFrontierSnapshot,
}

impl PreparedDelegatedTurnActivation {
    pub fn prepare(input: DelegatedTurnActivationInput) -> Option<Self> {
        if input.task_entry.source_session() != input.session
            || !matches!(
                input.task_entry.payload(),
                SemanticTranscriptEntryPayload::DelegatedTask {
                    spawning_request,
                    content,
                    ..
                } if *spawning_request == input.spawning_request && content == &input.task
            )
        {
            return None;
        }
        let task_entry = SemanticTranscriptEntry::from_validated_parts(
            input.task_entry.identity(),
            input.task_entry.source_session(),
            input.task_entry.payload().clone(),
        );
        let starting_snapshot = ResolvedContextFrontierSnapshot::try_from_candidate(
            input.session,
            input.starting_frontier,
            vec![task_entry.reference()],
        )
        .ok()?;
        let start = AcceptedInputTurnStart::from_validated_eligibility(
            AcceptedInputStartingLineage::FirstInSession,
            starting_snapshot.frontier(),
        );
        let phase = ActiveTurnPhase::Running {
            current_attempt: CurrentTurnAttempt::prepared(input.initial_attempt),
        };
        Some(Self {
            turn: ActivatedDelegatedTurn {
                session: input.session,
                turn: input.turn,
                origin: ActivatedDelegatedTurnOrigin::InitialTask {
                    spawning_request: input.spawning_request,
                    task: input.task,
                },
                configuration: input.configuration,
                start,
                phase,
                pending_steering: Box::new([]),
                consumed_steering: Box::new([]),
            },
            starting_entries: vec![task_entry],
            starting_snapshot,
        })
    }

    pub fn prepare_wake(input: DelegatedWakeTurnActivationInput) -> Option<Self> {
        let expected_count = input
            .through_delivery_sequence
            .get()
            .checked_sub(input.first_delivery_sequence.get())?
            .checked_add(1)?;
        if usize::try_from(expected_count).ok()? != input.deliveries.len()
            || input.predecessor_snapshot.frontier().owning_session() != input.session
        {
            return None;
        }
        let mut entries = Vec::with_capacity(input.deliveries.len());
        for (offset, delivery) in input.deliveries.into_iter().enumerate() {
            let expected_sequence = input
                .first_delivery_sequence
                .get()
                .checked_add(u64::try_from(offset).ok()?)?;
            if delivery.source_session() != input.session
                || delegation_delivery_sequence(delivery.payload())?.get() != expected_sequence
            {
                return None;
            }
            entries.push(SemanticTranscriptEntry::from_validated_parts(
                delivery.identity(),
                delivery.source_session(),
                delivery.payload().clone(),
            ));
        }
        let starting_snapshot = input
            .predecessor_snapshot
            .derive_appending_candidate(
                input.starting_frontier,
                entries
                    .iter()
                    .map(SemanticTranscriptEntry::reference)
                    .collect(),
            )
            .ok()?;
        let start = AcceptedInputTurnStart::from_validated_eligibility(
            AcceptedInputStartingLineage::After {
                immediate_predecessor: input.predecessor,
            },
            starting_snapshot.frontier(),
        );
        Some(Self {
            turn: ActivatedDelegatedTurn {
                session: input.session,
                turn: input.turn,
                origin: ActivatedDelegatedTurnOrigin::PendingDeliveries {
                    first: input.first_delivery_sequence,
                    through: input.through_delivery_sequence,
                },
                configuration: input.configuration,
                start,
                phase: ActiveTurnPhase::Running {
                    current_attempt: CurrentTurnAttempt::prepared(input.initial_attempt),
                },
                pending_steering: Box::new([]),
                consumed_steering: Box::new([]),
            },
            starting_entries: entries,
            starting_snapshot,
        })
    }

    pub fn into_parts(
        self,
    ) -> (
        ActivatedDelegatedTurn,
        Vec<SemanticTranscriptEntry>,
        ResolvedContextFrontierSnapshot,
    ) {
        (self.turn, self.starting_entries, self.starting_snapshot)
    }

    /// Reconstitutes the same delegated origin under its exact stored live
    /// phase after the initial activation transaction.
    pub fn with_reconstituted_phase(
        mut self,
        phase: ActiveTurnSchedulingReconstitutionInput,
    ) -> Option<(
        ActivatedDelegatedTurn,
        Vec<SemanticTranscriptEntry>,
        ResolvedContextFrontierSnapshot,
    )> {
        if phase.owning_turn() != self.turn.turn {
            return None;
        }
        self.turn.phase = phase.canonical_evidence_free_phase()?;
        Some((self.turn, self.starting_entries, self.starting_snapshot))
    }

    /// Reconstitutes a delegated tool wait with its exact ended attempt.
    pub fn with_reconstituted_tool_recovery(
        mut self,
        phase: ActiveTurnSchedulingReconstitutionInput,
        pending: Vec<crate::PendingSteeringInput>,
        consumed: Vec<crate::ConsumedSteeringReconstitutionInput>,
    ) -> Option<(
        ActivatedTurn,
        EndedTurnAttempt,
        ResolvedContextFrontierSnapshot,
    )> {
        let ActiveTurnSchedulingReconstitutionInput {
            owning_turn,
            current_attempt: Some(current_attempt),
            state: StoredActiveTurnPhase::AwaitingToolRecovery { wait, attempt_end },
            executing_tool_batch: None,
        } = phase
        else {
            return None;
        };
        if owning_turn != self.turn.turn
            || wait.turn() != owning_turn
            || wait.session() != self.turn.session
            || wait.issuing_attempt() != current_attempt
        {
            return None;
        }
        let attempt = reconstitute_recovery_attempt(
            self.turn.session,
            owning_turn,
            current_attempt,
            &attempt_end,
        )?;
        self.turn.phase = ActiveTurnPhase::AwaitingRecoveryDecision {
            ambiguous_operations: NonEmptyIssuedOperationRefs::singleton(
                crate::IssuedOperationRef::ToolAttempt(wait.attempt()),
            ),
            applied_interrupt: attempt_end.interrupt().map(|interrupt| interrupt.proof()),
        };
        self.turn = self
            .turn
            .with_pending_steering(pending)?
            .with_consumed_steering(consumed)?;
        Some((self.turn.into(), attempt, self.starting_snapshot))
    }

    /// Reconstitutes a delegated ambiguous model-call wait from its complete
    /// stored evidence. The returned call, attempt, and snapshots are checked
    /// against the delegated origin and its pinned configuration.
    pub fn with_reconstituted_model_call_recovery(
        mut self,
        input: DelegatedModelCallRecoveryReconstitutionInput,
    ) -> Option<(
        ActivatedTurn,
        crate::EndedModelCall,
        EndedTurnAttempt,
        ResolvedContextFrontierSnapshot,
        ResolvedContextFrontierSnapshot,
    )> {
        let ActiveTurnSchedulingReconstitutionInput {
            owning_turn,
            current_attempt: Some(current_attempt),
            state:
                StoredActiveTurnPhase::AwaitingModelCallRecovery {
                    call: recovery_call,
                    attempt_end,
                },
            executing_tool_batch: None,
        } = input.phase
        else {
            return None;
        };
        if owning_turn != self.turn.turn
            || input.call.id() != recovery_call
            || input.call.turn() != owning_turn
            || input.call.attempt() != current_attempt
            || input.call.selection() != *self.turn.configuration.effective().model()
            || input.call.state()
                != crate::ModelCallReconstitutionState::Terminal(ModelCallDisposition::Ambiguous)
        {
            return None;
        }
        let pinned = input.pinned_target.reconstitute_for_turn(owning_turn)?;
        let source_snapshot = input.source_snapshot.reconstitute()?;
        if !self
            .starting_snapshot
            .is_semantic_prefix_of(&source_snapshot)
        {
            return None;
        }
        let crate::ReconstitutedModelCall::Ended(call) =
            input.call.reconstitute(&source_snapshot, pinned).ok()?
        else {
            return None;
        };
        let attempt = reconstitute_recovery_attempt(
            self.turn.session,
            owning_turn,
            current_attempt,
            &attempt_end,
        )?;
        self.turn.phase = ActiveTurnPhase::AwaitingRecoveryDecision {
            ambiguous_operations: NonEmptyIssuedOperationRefs::singleton(
                crate::IssuedOperationRef::ModelCall(recovery_call),
            ),
            applied_interrupt: attempt_end.interrupt().map(|interrupt| interrupt.proof()),
        };
        self.turn = self
            .turn
            .with_pending_steering(input.pending_steering)?
            .with_consumed_steering(input.consumed_steering)?;
        Some((
            self.turn.into(),
            call,
            attempt,
            source_snapshot,
            self.starting_snapshot,
        ))
    }
}

fn reconstitute_recovery_attempt(
    session: SessionId,
    turn: TurnId,
    current_attempt: TurnAttemptId,
    attempt_end: &crate::TerminalAttemptEndReconstitutionInput,
) -> Option<EndedTurnAttempt> {
    let running_attempt = CurrentTurnAttempt::prepared(current_attempt)
        .begin_running()
        .ok()?;
    let attempt = match attempt_end.end() {
        AttemptEnd::WithoutStop {
            disposition:
                disposition @ (UnstoppedAttemptDisposition::Ambiguous
                | UnstoppedAttemptDisposition::Lost),
        } => running_attempt.end_without_stop(*disposition).ok()?,
        AttemptEnd::AfterCancellation {
            cause,
            disposition:
                disposition @ (CancellationStopDisposition::Ambiguous
                | CancellationStopDisposition::Lost),
        } => {
            let interrupt = attempt_end.interrupt()?;
            if interrupt.session() != session
                || interrupt.proof() != *cause
                || cause.predecessor() != turn
            {
                return None;
            }
            running_attempt
                .request_cancellation(*cause)
                .and_then(|attempt| attempt.end_after_cancellation(*cause, *disposition))
                .ok()?
        }
        _ => return None,
    };
    Some(attempt)
}

fn delegation_delivery_sequence(payload: &SemanticTranscriptEntryPayload) -> Option<NonZeroU64> {
    match payload {
        SemanticTranscriptEntryPayload::DelegationMessage {
            delivery_sequence, ..
        } => Some(*delivery_sequence),
        SemanticTranscriptEntryPayload::DelegationResult {
            mode: DelegationWaitMode::Background,
            delivery_sequence: Some(delivery_sequence),
            ..
        } => Some(*delivery_sequence),
        _ => None,
    }
}

/// Origin-agnostic sealed candidate for an atomic turn activation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PreparedTurnActivation {
    Accepted(Box<PreparedAcceptedInputTurnActivation>),
    Delegated(Box<PreparedDelegatedTurnActivation>),
}

impl From<PreparedAcceptedInputTurnActivation> for PreparedTurnActivation {
    fn from(value: PreparedAcceptedInputTurnActivation) -> Self {
        Self::Accepted(Box::new(value))
    }
}

impl From<PreparedDelegatedTurnActivation> for PreparedTurnActivation {
    fn from(value: PreparedDelegatedTurnActivation) -> Self {
        Self::Delegated(Box::new(value))
    }
}

impl PreparedTurnActivation {
    pub fn turn(&self) -> ActivatedTurn {
        match self {
            Self::Accepted(prepared) => prepared.turn().clone().into(),
            Self::Delegated(prepared) => prepared.turn.clone().into(),
        }
    }

    pub fn starting_entries(&self) -> &[SemanticTranscriptEntry] {
        match self {
            Self::Accepted(prepared) => prepared.starting_entries(),
            Self::Delegated(prepared) => &prepared.starting_entries,
        }
    }

    pub const fn starting_snapshot(&self) -> &ResolvedContextFrontierSnapshot {
        match self {
            Self::Accepted(prepared) => prepared.starting_snapshot(),
            Self::Delegated(prepared) => &prepared.starting_snapshot,
        }
    }
}

/// One sealed candidate for the atomic eligibility commit.
///
/// The candidate contains the exact origin entry, prefix-preserving starting
/// snapshot, opaque start, and active turn with one prepared initial attempt.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreparedAcceptedInputTurnActivation {
    pub(super) turn: ActivatedAcceptedInputTurn,
    pub(super) starting_entries: AcceptedInputTurnStartingEntries,
    pub(super) starting_snapshot: ResolvedContextFrontierSnapshot,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum AcceptedInputTurnStartingEntries {
    Origin([SemanticTranscriptEntry; 1]),
    ModelIdentityThenOrigin([SemanticTranscriptEntry; 2]),
}

impl AcceptedInputTurnStartingEntries {
    pub(super) fn as_slice(&self) -> &[SemanticTranscriptEntry] {
        match self {
            Self::Origin(entries) => entries,
            Self::ModelIdentityThenOrigin(entries) => entries,
        }
    }

    fn origin(&self) -> &SemanticTranscriptEntry {
        match self {
            Self::Origin([origin]) | Self::ModelIdentityThenOrigin([_, origin]) => origin,
        }
    }

    fn into_boxed_slice(self) -> Box<[SemanticTranscriptEntry]> {
        match self {
            Self::Origin(entries) => Box::new(entries),
            Self::ModelIdentityThenOrigin(entries) => Box::new(entries),
        }
    }
}

impl PreparedAcceptedInputTurnActivation {
    /// Borrows the exact initial active turn.
    pub const fn turn(&self) -> &ActivatedAcceptedInputTurn {
        &self.turn
    }

    /// Returns the newly created origin semantic entry.
    pub fn origin_entry(&self) -> SemanticTranscriptEntry {
        self.starting_entries.origin().clone()
    }

    /// Borrows the ordered entries appended at the turn-start boundary.
    pub fn starting_entries(&self) -> &[SemanticTranscriptEntry] {
        self.starting_entries.as_slice()
    }

    /// Borrows the new immutable starting snapshot.
    pub const fn starting_snapshot(&self) -> &ResolvedContextFrontierSnapshot {
        &self.starting_snapshot
    }

    /// Returns the opaque eligibility-fixed start.
    pub const fn start(&self) -> AcceptedInputTurnStart {
        self.turn.start
    }

    /// Returns all atomic commit values.
    pub fn into_parts(
        self,
    ) -> (
        ActivatedAcceptedInputTurn,
        Box<[SemanticTranscriptEntry]>,
        ResolvedContextFrontierSnapshot,
    ) {
        (
            self.turn,
            self.starting_entries.into_boxed_slice(),
            self.starting_snapshot,
        )
    }
}
