//! Complete accepted-input scheduling projection and pure eligibility.
//!
//! ADR-0004, ADR-0027, ADR-0030, ADR-0035, ADR-0036, and ADR-0041 are
//! normative. This purpose-specific projection reconstructs every fact that
//! can change accepted-input eligibility or slot ownership in the initial
//! closed semantic-entry slice. It supports an ancestry-free session whose
//! durable total order consists of a failed-terminal prefix, at most one active
//! slot, and a queued suffix.
//!
//! Active records carry one exact checked phase and a validated,
//! session-scoped acceptance tail. This slice admits only evidence-free
//! prepared and running attempts; ADR-0041 requires later StopRequested and
//! durable-wait storage to supply complete owning-turn evidence rather than a
//! preassembled proof or wait subject.

use std::collections::{BTreeMap, BTreeSet};

use crate::{
    AcceptedInputDisposition, AcceptedInputId, AcceptedInputLifecycle, AcceptedInputQueueOrder,
    AcceptedInputQueueOrderError, AcceptedInputQueueWork, AcceptedInputStartingLineage,
    AcceptedInputTurnStart, ActiveTurnPhase, ContextFrontierId, CurrentTurnAttempt,
    CurrentTurnAttemptState, DeliveryRequest, InitialSemanticTranscriptEntryPayload,
    OriginConfiguration, ResolvedContextFrontierReconstitutionInput,
    ResolvedContextFrontierSnapshot, SemanticTranscriptEntry, SemanticTranscriptEntryId,
    SemanticTranscriptEntryReconstitutionInput, SemanticTranscriptEntryRef, Session, SessionId,
    SessionInputPosition, TranscriptAncestry, TurnAttemptId, TurnId,
    derive_accepted_input_total_order,
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
    /// The turn owns the session's progressing slot.
    Active {
        /// The stored lineage selected at eligibility.
        starting_lineage: AcceptedInputStartingLineage,
        /// The stored starting snapshot identity.
        starting_frontier: ContextFrontierId,
        /// The exact phase and its asserted owning turn.
        phase: ActiveTurnSchedulingReconstitutionInput,
    },
    /// The turn reached the only terminal disposition whose complete semantic
    /// frontier is constructible in this closed slice.
    TerminalFailed {
        /// The stored lineage selected at eligibility.
        starting_lineage: AcceptedInputStartingLineage,
        /// The stored starting snapshot identity.
        starting_frontier: ContextFrontierId,
        /// The complete frontier through the appended failed marker.
        terminal_frontier: ContextFrontierId,
    },
}

/// Evidence-free stored facts for one active scheduling phase.
///
/// ADR-0041 prohibits reconstructing `StopRequested` or a durable wait from a
/// preassembled proof or bare subject. Accordingly this type exposes only the
/// two current-attempt states whose construction needs no proof-bearing owner
/// evidence. Later storage for evidence-bearing phases must extend the owning
/// scheduling seam with its complete correlated fact projection first.
///
/// A bare wait subject is intentionally not a production constructor:
///
/// ```compile_fail
/// use signalbox_domain::{
///     ActiveTurnSchedulingReconstitutionInput, ToolRequestId, TurnId,
/// };
///
/// let turn = TurnId::from_uuid(uuid::Uuid::nil());
/// let request = ToolRequestId::from_uuid(uuid::Uuid::nil());
/// let _ = ActiveTurnSchedulingReconstitutionInput::awaiting_approval(turn, request);
/// ```
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActiveTurnSchedulingReconstitutionInput {
    owning_turn: TurnId,
    phase: ActiveTurnPhase,
}

impl ActiveTurnSchedulingReconstitutionInput {
    /// Supplies the exact prepared current attempt.
    pub const fn prepared(owning_turn: TurnId, current_attempt: TurnAttemptId) -> Self {
        Self {
            owning_turn,
            phase: ActiveTurnPhase::Running {
                current_attempt: CurrentTurnAttempt::prepared(current_attempt),
            },
        }
    }

    /// Supplies the exact running current attempt.
    pub const fn running(owning_turn: TurnId, current_attempt: TurnAttemptId) -> Self {
        Self {
            owning_turn,
            phase: ActiveTurnPhase::Running {
                current_attempt: CurrentTurnAttempt::reconstituted(
                    current_attempt,
                    CurrentTurnAttemptState::Running,
                ),
            },
        }
    }

    /// Returns the turn named as owner by the active-phase record.
    pub const fn owning_turn(&self) -> TurnId {
        self.owning_turn
    }

    /// Borrows the exact canonical active phase.
    pub const fn phase(&self) -> &ActiveTurnPhase {
        &self.phase
    }
}

/// One accepted input inside an active turn's claimed session tail.
///
/// The repeated session, immutable delivery request, acceptance position, and
/// current disposition are inert facts. They become a canonical tail entry
/// only after the scheduling seam validates the complete interval and every
/// disposition correlation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionAcceptanceTailEntryReconstitutionInput {
    session: SessionId,
    accepted_input: AcceptedInputLifecycle,
    position: SessionInputPosition,
    delivery: DeliveryRequest,
}

impl SessionAcceptanceTailEntryReconstitutionInput {
    /// Supplies the exact stored facts for one accepted input.
    pub const fn new(
        session: SessionId,
        accepted_input: AcceptedInputLifecycle,
        position: SessionInputPosition,
        delivery: DeliveryRequest,
    ) -> Self {
        Self {
            session,
            accepted_input,
            position,
            delivery,
        }
    }

    /// Returns the stored owning session.
    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// Borrows the accepted input and its current disposition.
    pub const fn accepted_input(&self) -> &AcceptedInputLifecycle {
        &self.accepted_input
    }

    /// Returns the immutable session acceptance position.
    pub const fn position(&self) -> SessionInputPosition {
        self.position
    }

    /// Returns the immutable delivery request.
    pub const fn delivery(&self) -> DeliveryRequest {
        self.delivery
    }
}

/// Claimed complete accepted-input interval for one active turn.
///
/// The interval begins at the owning turn's exact origin and ends at the
/// authoritative last session position observed by the same read. A filtered
/// pending-steering list or a bare maximum position cannot substitute for
/// these ordered facts.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionAcceptanceTailReconstitutionInput {
    session: SessionId,
    anchor: AcceptedInputId,
    observed_last_position: SessionInputPosition,
    entries: Vec<SessionAcceptanceTailEntryReconstitutionInput>,
}

impl SessionAcceptanceTailReconstitutionInput {
    /// Supplies one claimed complete session-scoped interval.
    pub fn new(
        session: SessionId,
        anchor: AcceptedInputId,
        observed_last_position: SessionInputPosition,
        entries: Vec<SessionAcceptanceTailEntryReconstitutionInput>,
    ) -> Self {
        Self {
            session,
            anchor,
            observed_last_position,
            entries,
        }
    }

    /// Returns the session whose observation supplied the interval.
    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// Returns the accepted-input identity anchoring the interval.
    pub const fn anchor(&self) -> AcceptedInputId {
        self.anchor
    }

    /// Returns the authoritative last position claimed by the observation.
    pub const fn observed_last_position(&self) -> SessionInputPosition {
        self.observed_last_position
    }

    /// Returns every ordered entry supplied for validation.
    pub fn entries(&self) -> &[SessionAcceptanceTailEntryReconstitutionInput] {
        &self.entries
    }
}

/// One validated accepted input in an active turn's session tail.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SessionAcceptanceTailEntry {
    accepted_input: AcceptedInputLifecycle,
    position: SessionInputPosition,
    delivery: DeliveryRequest,
}

/// Canonical complete accepted-input interval for one active turn.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SessionAcceptanceTail {
    session: SessionId,
    anchor: AcceptedInputId,
    observed_last_position: SessionInputPosition,
    entries: Box<[SessionAcceptanceTailEntry]>,
}

impl SessionAcceptanceTail {
    pub(crate) const fn observed_last_position(&self) -> SessionInputPosition {
        self.observed_last_position
    }
}

/// Complete checked values supplied for one accepted-input scheduling record.
///
/// Repeated session and turn correlations retain independently stored facts so
/// reconstitution rejects cross-wired accepted-input and queue records.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AcceptedInputTurnSchedulingRecord {
    stored_session: SessionId,
    turn: TurnId,
    accepted_input_session: SessionId,
    accepted_input: AcceptedInputLifecycle,
    queue_session: SessionId,
    queue_turn: TurnId,
    order: AcceptedInputQueueOrder,
    origin_configuration: OriginConfiguration,
    state: AcceptedInputTurnSchedulingRecordState,
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
            origin_configuration,
            state,
        }
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

    /// Borrows the complete frozen origin configuration.
    pub const fn origin_configuration(&self) -> &OriginConfiguration {
        &self.origin_configuration
    }

    /// Returns the stored lifecycle projection.
    pub const fn state(&self) -> &AcceptedInputTurnSchedulingRecordState {
        &self.state
    }
}

/// Complete purpose-specific stored facts for one session's scheduling read.
///
/// The input owns the already-checked current [`Session`], every currently
/// known accepted-input turn record, and complete semantic-entry and snapshot
/// collections needed by any stored start or failed-terminal frontier.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AcceptedInputSchedulingReconstitutionInput {
    session: Session,
    turns: Vec<AcceptedInputTurnSchedulingRecord>,
    semantic_entries: Vec<SemanticTranscriptEntryReconstitutionInput>,
    snapshots: Vec<ResolvedContextFrontierReconstitutionInput>,
    active_acceptance_tail: Option<SessionAcceptanceTailReconstitutionInput>,
}

impl AcceptedInputSchedulingReconstitutionInput {
    /// Supplies one complete typed scheduling projection.
    pub fn new(
        session: Session,
        turns: Vec<AcceptedInputTurnSchedulingRecord>,
        semantic_entries: Vec<SemanticTranscriptEntryReconstitutionInput>,
        snapshots: Vec<ResolvedContextFrontierReconstitutionInput>,
        active_acceptance_tail: Option<SessionAcceptanceTailReconstitutionInput>,
    ) -> Self {
        Self {
            session,
            turns,
            semantic_entries,
            snapshots,
            active_acceptance_tail,
        }
    }

    /// Borrows the complete current-session snapshot.
    pub const fn session(&self) -> &Session {
        &self.session
    }

    /// Returns every stored turn record supplied as complete.
    pub fn turns(&self) -> &[AcceptedInputTurnSchedulingRecord] {
        &self.turns
    }

    /// Returns every stored initial semantic entry supplied as complete.
    pub fn semantic_entries(&self) -> &[SemanticTranscriptEntryReconstitutionInput] {
        &self.semantic_entries
    }

    /// Returns every stored context snapshot supplied as complete.
    pub fn snapshots(&self) -> &[ResolvedContextFrontierReconstitutionInput] {
        &self.snapshots
    }

    /// Borrows the claimed complete tail required by an active turn.
    pub const fn active_acceptance_tail(
        &self,
    ) -> Option<&SessionAcceptanceTailReconstitutionInput> {
        self.active_acceptance_tail.as_ref()
    }

    /// Reconstructs the canonical scheduling projection without effects.
    pub fn reconstitute(
        self,
    ) -> Result<AcceptedInputSchedulingProjection, AcceptedInputSchedulingReconstitutionError> {
        reconstitute(self)
    }
}

/// Why complete stored facts cannot reconstruct the closed scheduling model.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AcceptedInputSchedulingReconstitutionFailure {
    /// This slice cannot resolve a first frontier from session ancestry.
    UnsupportedSessionAncestry,
    /// One turn record belongs to a different session.
    TurnSessionMismatch {
        /// The cross-wired turn.
        turn: TurnId,
    },
    /// One accepted-input record belongs to a different session.
    AcceptedInputSessionMismatch {
        /// The affected turn.
        turn: TurnId,
    },
    /// One queue record belongs to a different session.
    QueueSessionMismatch {
        /// The affected turn.
        turn: TurnId,
    },
    /// One queue record names a different turn.
    QueueTurnMismatch {
        /// The affected turn.
        turn: TurnId,
    },
    /// The accepted input is not the exact typed origin of its turn.
    AcceptedInputOriginMismatch {
        /// The affected turn.
        turn: TurnId,
    },
    /// Two turn records referenced the same accepted input.
    DuplicateAcceptedInput {
        /// The duplicated accepted input.
        accepted_input: crate::AcceptedInputId,
    },
    /// The immutable queue facts cannot form one durable total order.
    InvalidQueueOrder {
        /// The complete queue-order rejection.
        error: AcceptedInputQueueOrderError,
    },
    /// A semantic entry belongs to a different source session.
    SemanticEntrySourceSessionMismatch {
        /// The affected entry.
        entry: SemanticTranscriptEntryId,
    },
    /// The same source-qualified semantic entry appeared more than once.
    DuplicateSemanticEntry {
        /// The duplicated exact reference.
        entry: SemanticTranscriptEntryRef,
    },
    /// A semantic payload names no accepted input or turn in the projection.
    SemanticEntrySubjectMissing {
        /// The affected entry.
        entry: SemanticTranscriptEntryId,
    },
    /// A semantic payload disagrees with its subject's lifecycle state.
    SemanticEntryStateMismatch {
        /// The affected entry.
        entry: SemanticTranscriptEntryId,
    },
    /// More than one origin or failure entry names the same subject.
    DuplicateSemanticEntryForSubject {
        /// The later duplicate entry.
        entry: SemanticTranscriptEntryId,
    },
    /// A started turn has no exact origin entry.
    MissingOriginEntry {
        /// The affected turn.
        turn: TurnId,
    },
    /// A failed turn has no exact failure marker.
    MissingFailureEntry {
        /// The affected turn.
        turn: TurnId,
    },
    /// The current attempt record names a different owning turn.
    CurrentAttemptOwnershipMismatch {
        /// The active turn whose attempt is cross-wired.
        turn: TurnId,
        /// The affected attempt.
        attempt: TurnAttemptId,
    },
    /// The same current-attempt identity appeared on multiple active records.
    DuplicateCurrentAttempt {
        /// The duplicated attempt.
        attempt: TurnAttemptId,
    },
    /// An active turn was supplied without its complete session acceptance
    /// tail.
    MissingActiveAcceptanceTail {
        /// The active turn requiring the tail.
        turn: TurnId,
    },
    /// A tail was supplied even though the session has no active turn.
    UnexpectedActiveAcceptanceTail,
    /// The claimed tail belongs to a different session.
    AcceptanceTailSessionMismatch {
        /// The current scheduling session.
        expected: SessionId,
        /// The session asserted by the tail.
        actual: SessionId,
    },
    /// The claimed tail does not begin with the active turn's exact origin.
    AcceptanceTailAnchorMismatch {
        /// The active turn whose origin anchors the tail.
        turn: TurnId,
        /// The active turn's exact origin accepted input.
        expected: AcceptedInputId,
        /// The accepted input asserted as the anchor.
        actual: AcceptedInputId,
    },
    /// One tail entry belongs to a different session.
    AcceptanceTailEntrySessionMismatch {
        /// The cross-wired accepted input.
        accepted_input: AcceptedInputId,
    },
    /// The same accepted-input identity appeared more than once in the tail.
    DuplicateAcceptanceTailEntry {
        /// The duplicated accepted input.
        accepted_input: AcceptedInputId,
    },
    /// A tail entry is not at the exact next claimed session position.
    AcceptanceTailPositionMismatch {
        /// The affected accepted input.
        accepted_input: AcceptedInputId,
        /// The exact position required by the interval.
        expected: SessionInputPosition,
        /// The inconsistent supplied position.
        actual: SessionInputPosition,
    },
    /// The ordered entries do not end at the claimed session observation.
    AcceptanceTailLastPositionMismatch {
        /// The authoritative last position claimed by the input.
        expected: SessionInputPosition,
        /// The last position actually represented, if any.
        actual: Option<SessionInputPosition>,
    },
    /// One immutable delivery request and current disposition do not form an
    /// accepted lifecycle correlation.
    AcceptanceTailDispositionMismatch {
        /// The affected accepted input.
        accepted_input: AcceptedInputId,
    },
    /// A stored snapshot belongs to a different consuming session.
    SnapshotOwningSessionMismatch {
        /// The affected snapshot.
        snapshot: ContextFrontierId,
    },
    /// The same session-scoped snapshot identity appeared more than once.
    DuplicateSnapshot {
        /// The duplicated snapshot.
        snapshot: ContextFrontierId,
    },
    /// A snapshot's complete membership contains a duplicate entry.
    InvalidSnapshotMembership {
        /// The affected snapshot.
        snapshot: ContextFrontierId,
    },
    /// A snapshot references an entry absent from the complete entry set.
    SnapshotEntryMissing {
        /// The affected snapshot.
        snapshot: ContextFrontierId,
        /// The absent exact semantic entry.
        entry: SemanticTranscriptEntryRef,
    },
    /// A started turn names a snapshot absent from the complete snapshot set.
    StartingSnapshotMissing {
        /// The affected turn.
        turn: TurnId,
    },
    /// A failed turn names a terminal snapshot absent from the complete set.
    TerminalSnapshotMissing {
        /// The affected turn.
        turn: TurnId,
    },
    /// Lifecycle states do not form terminal prefix, optional active slot, and
    /// queued suffix in durable total order.
    InvalidLifecycleOrder {
        /// The first affected turn.
        turn: TurnId,
    },
    /// The stored start does not name the derived exact lineage.
    StartingLineageMismatch {
        /// The affected turn.
        turn: TurnId,
        /// The exact lineage required by total order.
        expected: AcceptedInputStartingLineage,
        /// The inconsistent stored lineage.
        actual: AcceptedInputStartingLineage,
    },
    /// The stored start snapshot is not the predecessor prefix plus the exact
    /// origin entry.
    StartingFrontierMismatch {
        /// The affected turn.
        turn: TurnId,
    },
    /// The failed terminal frontier is not the start prefix plus its exact
    /// failed marker.
    TerminalFrontierMismatch {
        /// The affected turn.
        turn: TurnId,
    },
    /// A complete snapshot was supplied but no lifecycle fact references it.
    UnreferencedSnapshot {
        /// The unreferenced snapshot.
        snapshot: ContextFrontierId,
    },
}

/// Failed scheduling reconstitution retaining every supplied fact unchanged.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AcceptedInputSchedulingReconstitutionError {
    input: Box<AcceptedInputSchedulingReconstitutionInput>,
    failure: AcceptedInputSchedulingReconstitutionFailure,
}

impl AcceptedInputSchedulingReconstitutionError {
    /// Borrows every unchanged reconstitution input.
    pub const fn input(&self) -> &AcceptedInputSchedulingReconstitutionInput {
        &self.input
    }

    /// Borrows the exact integrity failure.
    pub const fn failure(&self) -> &AcceptedInputSchedulingReconstitutionFailure {
        &self.failure
    }

    /// Returns every unchanged input and the exact integrity failure.
    pub fn into_parts(
        self,
    ) -> (
        AcceptedInputSchedulingReconstitutionInput,
        AcceptedInputSchedulingReconstitutionFailure,
    ) {
        (*self.input, self.failure)
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
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ReconstitutedSchedulingState {
    Queued,
    Active {
        start: AcceptedInputTurnStart,
        phase: ActiveTurnPhase,
    },
    TerminalFailed {
        start: AcceptedInputTurnStart,
        terminal_frontier: ResolvedContextFrontierSnapshot,
    },
}

/// One canonical turn inside the complete scheduling projection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AcceptedInputTurnSchedulingProjection {
    session: SessionId,
    turn: TurnId,
    accepted_input: AcceptedInputLifecycle,
    order: AcceptedInputQueueOrder,
    origin_configuration: OriginConfiguration,
    state: ReconstitutedSchedulingState,
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
        }
    }

    /// Returns the opaque validated start for started work.
    pub const fn start(&self) -> Option<AcceptedInputTurnStart> {
        match &self.state {
            ReconstitutedSchedulingState::Queued => None,
            ReconstitutedSchedulingState::Active { start, .. }
            | ReconstitutedSchedulingState::TerminalFailed { start, .. } => Some(*start),
        }
    }

    /// Borrows the exact current active phase, when this turn owns the slot.
    pub const fn active_phase(&self) -> Option<&ActiveTurnPhase> {
        match &self.state {
            ReconstitutedSchedulingState::Active { phase, .. } => Some(phase),
            ReconstitutedSchedulingState::Queued
            | ReconstitutedSchedulingState::TerminalFailed { .. } => None,
        }
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
        }
    }
}

/// Canonical complete scheduling state for one session.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AcceptedInputSchedulingProjection {
    session: Session,
    turns: Box<[AcceptedInputTurnSchedulingProjection]>,
    active_acceptance_tail: Option<SessionAcceptanceTail>,
    semantic_entries: BTreeMap<SemanticTranscriptEntryRef, SemanticTranscriptEntry>,
    snapshots: BTreeMap<ContextFrontierId, ResolvedContextFrontierSnapshot>,
    current_attempts: BTreeMap<TurnAttemptId, TurnId>,
}

impl AcceptedInputSchedulingProjection {
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

    /// Returns the earliest queued work in durable total order.
    pub fn earliest_queued_turn(&self) -> Option<&AcceptedInputTurnSchedulingProjection> {
        self.turns
            .iter()
            .find(|turn| turn.status() == AcceptedInputTurnSchedulingStatus::Queued)
    }

    /// Consumes this complete projection and prepares the earliest queued turn
    /// as one sealed commit candidate.
    pub fn prepare_earliest_queued_activation(
        self,
        identities: AcceptedInputTurnActivationIdentities,
    ) -> Result<PreparedAcceptedInputTurnActivation, AcceptedInputEligibilityError> {
        prepare_earliest_queued_activation(self, identities)
    }
}

/// Fresh identities supplied for one eligibility-time activation candidate.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AcceptedInputTurnActivationIdentities {
    origin_entry: SemanticTranscriptEntryId,
    starting_frontier: ContextFrontierId,
    initial_attempt: TurnAttemptId,
}

impl AcceptedInputTurnActivationIdentities {
    /// Supplies the three distinct identities created by the transaction.
    pub const fn new(
        origin_entry: SemanticTranscriptEntryId,
        starting_frontier: ContextFrontierId,
        initial_attempt: TurnAttemptId,
    ) -> Self {
        Self {
            origin_entry,
            starting_frontier,
            initial_attempt,
        }
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

/// Exact initial active turn state prepared by eligibility.
///
/// Raw aggregate facts cannot construct this state:
///
/// ```compile_fail
/// use signalbox_domain::{
///     AcceptedInputLifecycle, AcceptedInputQueueOrder, AcceptedInputTurnStart,
///     ActivatedAcceptedInputTurn, ActiveTurnPhase, OriginConfiguration, SessionId, TurnId,
/// };
///
/// fn raw_facts_are_not_an_activation(
///     session: SessionId,
///     turn: TurnId,
///     accepted_input: AcceptedInputLifecycle,
///     order: AcceptedInputQueueOrder,
///     configuration: OriginConfiguration,
///     start: AcceptedInputTurnStart,
///     phase: ActiveTurnPhase,
/// ) {
///     let _ = ActivatedAcceptedInputTurn {
///         session,
///         turn,
///         accepted_input,
///         order,
///         configuration,
///         start,
///         phase,
///     };
/// }
/// ```
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActivatedAcceptedInputTurn {
    session: SessionId,
    turn: TurnId,
    accepted_input: AcceptedInputLifecycle,
    order: AcceptedInputQueueOrder,
    configuration: OriginConfiguration,
    start: AcceptedInputTurnStart,
    phase: ActiveTurnPhase,
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

    /// Returns the exact eligibility-fixed lineage and frontier.
    pub const fn start(&self) -> AcceptedInputTurnStart {
        self.start
    }

    /// Borrows the exact initial active phase.
    pub const fn phase(&self) -> &ActiveTurnPhase {
        &self.phase
    }
}

/// One sealed candidate for the atomic eligibility commit.
///
/// The candidate contains the exact origin entry, prefix-preserving starting
/// snapshot, opaque start, and active turn with one prepared initial attempt.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreparedAcceptedInputTurnActivation {
    turn: ActivatedAcceptedInputTurn,
    origin_entry: SemanticTranscriptEntry,
    starting_snapshot: ResolvedContextFrontierSnapshot,
}

impl PreparedAcceptedInputTurnActivation {
    /// Borrows the exact initial active turn.
    pub const fn turn(&self) -> &ActivatedAcceptedInputTurn {
        &self.turn
    }

    /// Returns the newly created origin semantic entry.
    pub const fn origin_entry(&self) -> SemanticTranscriptEntry {
        self.origin_entry
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
        SemanticTranscriptEntry,
        ResolvedContextFrontierSnapshot,
    ) {
        (self.turn, self.origin_entry, self.starting_snapshot)
    }
}

/// Why the complete scheduling projection cannot prepare an activation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AcceptedInputEligibilityFailure {
    /// A turn already owns the session's progressing slot.
    ActiveTurnPresent {
        /// The exact active slot owner.
        turn: TurnId,
    },
    /// No queued accepted-input turn exists.
    NoQueuedTurn,
    /// The proposed origin entry identity is already present.
    OriginEntryIdentityAlreadyExists,
    /// The proposed session-scoped snapshot identity is already present.
    StartingFrontierIdentityAlreadyExists,
    /// The proposed initial-attempt identity is already current in the
    /// complete scheduling projection.
    InitialAttemptIdentityAlreadyExists,
}

/// Rejected eligibility preparation retaining the complete projection and
/// supplied identities unchanged.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AcceptedInputEligibilityError {
    projection: Box<AcceptedInputSchedulingProjection>,
    identities: AcceptedInputTurnActivationIdentities,
    failure: AcceptedInputEligibilityFailure,
}

impl AcceptedInputEligibilityError {
    /// Borrows the unchanged complete scheduling projection.
    pub const fn projection(&self) -> &AcceptedInputSchedulingProjection {
        &self.projection
    }

    /// Returns the unchanged supplied identities.
    pub const fn identities(&self) -> AcceptedInputTurnActivationIdentities {
        self.identities
    }

    /// Returns the exact eligibility failure.
    pub const fn failure(&self) -> AcceptedInputEligibilityFailure {
        self.failure
    }

    /// Returns every unchanged input and the exact failure.
    pub fn into_parts(
        self,
    ) -> (
        AcceptedInputSchedulingProjection,
        AcceptedInputTurnActivationIdentities,
        AcceptedInputEligibilityFailure,
    ) {
        (*self.projection, self.identities, self.failure)
    }
}

fn reconstitute(
    input: AcceptedInputSchedulingReconstitutionInput,
) -> Result<AcceptedInputSchedulingProjection, AcceptedInputSchedulingReconstitutionError> {
    match reconstitute_inner(&input) {
        Ok(projection) => Ok(projection),
        Err(failure) => Err(AcceptedInputSchedulingReconstitutionError {
            input: Box::new(input),
            failure,
        }),
    }
}

fn reconstitute_inner(
    input: &AcceptedInputSchedulingReconstitutionInput,
) -> Result<AcceptedInputSchedulingProjection, AcceptedInputSchedulingReconstitutionFailure> {
    if input.session.creation_provenance().ancestry() != TranscriptAncestry::None {
        return Err(AcceptedInputSchedulingReconstitutionFailure::UnsupportedSessionAncestry);
    }

    let session = input.session.id();
    let mut accepted_input_turns = BTreeMap::new();
    for record in &input.turns {
        validate_record_correlations(session, record)?;
        if accepted_input_turns
            .insert(record.accepted_input.id(), record.turn)
            .is_some()
        {
            return Err(
                AcceptedInputSchedulingReconstitutionFailure::DuplicateAcceptedInput {
                    accepted_input: record.accepted_input.id(),
                },
            );
        }
    }

    let queue_work = input.turns.iter().map(|record| {
        AcceptedInputQueueWork::new(record.queue_session, record.queue_turn, record.order)
    });
    let total_order = derive_accepted_input_total_order(queue_work).map_err(|error| {
        AcceptedInputSchedulingReconstitutionFailure::InvalidQueueOrder { error }
    })?;
    let records_by_turn = input
        .turns
        .iter()
        .map(|record| (record.turn, record))
        .collect::<BTreeMap<_, _>>();

    let mut semantic_entries = BTreeMap::new();
    let mut origin_by_turn = BTreeMap::new();
    let mut failure_by_turn = BTreeMap::new();
    for candidate in &input.semantic_entries {
        if candidate.source_session() != session {
            return Err(
                AcceptedInputSchedulingReconstitutionFailure::SemanticEntrySourceSessionMismatch {
                    entry: candidate.identity(),
                },
            );
        }

        let entry = SemanticTranscriptEntry::from_validated_parts(
            candidate.identity(),
            candidate.source_session(),
            candidate.payload(),
        );
        if semantic_entries.insert(entry.reference(), entry).is_some() {
            return Err(
                AcceptedInputSchedulingReconstitutionFailure::DuplicateSemanticEntry {
                    entry: entry.reference(),
                },
            );
        }

        match candidate.payload() {
            InitialSemanticTranscriptEntryPayload::OriginAcceptedInput { accepted_input } => {
                let Some(turn) = accepted_input_turns.get(&accepted_input).copied() else {
                    return Err(
                        AcceptedInputSchedulingReconstitutionFailure::SemanticEntrySubjectMissing {
                            entry: candidate.identity(),
                        },
                    );
                };
                let record = records_by_turn[&turn];
                if matches!(
                    &record.state,
                    AcceptedInputTurnSchedulingRecordState::Queued
                ) {
                    return Err(
                        AcceptedInputSchedulingReconstitutionFailure::SemanticEntryStateMismatch {
                            entry: candidate.identity(),
                        },
                    );
                }
                if origin_by_turn.insert(turn, entry.reference()).is_some() {
                    return Err(
                        AcceptedInputSchedulingReconstitutionFailure::DuplicateSemanticEntryForSubject {
                            entry: candidate.identity(),
                        },
                    );
                }
            }
            InitialSemanticTranscriptEntryPayload::TurnFailed { turn } => {
                let Some(record) = records_by_turn.get(&turn) else {
                    return Err(
                        AcceptedInputSchedulingReconstitutionFailure::SemanticEntrySubjectMissing {
                            entry: candidate.identity(),
                        },
                    );
                };
                if !matches!(
                    &record.state,
                    AcceptedInputTurnSchedulingRecordState::TerminalFailed { .. }
                ) {
                    return Err(
                        AcceptedInputSchedulingReconstitutionFailure::SemanticEntryStateMismatch {
                            entry: candidate.identity(),
                        },
                    );
                }
                if failure_by_turn.insert(turn, entry.reference()).is_some() {
                    return Err(
                        AcceptedInputSchedulingReconstitutionFailure::DuplicateSemanticEntryForSubject {
                            entry: candidate.identity(),
                        },
                    );
                }
            }
        }
    }

    let mut snapshots = BTreeMap::new();
    for candidate in &input.snapshots {
        if candidate.owning_session() != session {
            return Err(
                AcceptedInputSchedulingReconstitutionFailure::SnapshotOwningSessionMismatch {
                    snapshot: candidate.snapshot(),
                },
            );
        }
        for entry in candidate.ordered_entries() {
            if !semantic_entries.contains_key(entry) {
                return Err(
                    AcceptedInputSchedulingReconstitutionFailure::SnapshotEntryMissing {
                        snapshot: candidate.snapshot(),
                        entry: *entry,
                    },
                );
            }
        }
        let (owning_session, snapshot, ordered_entries) = candidate.clone().into_parts();
        let resolved = ResolvedContextFrontierSnapshot::try_from_candidate(
            owning_session,
            snapshot,
            ordered_entries,
        )
        .map_err(|_| {
            AcceptedInputSchedulingReconstitutionFailure::InvalidSnapshotMembership { snapshot }
        })?;
        if snapshots.insert(snapshot, resolved).is_some() {
            return Err(
                AcceptedInputSchedulingReconstitutionFailure::DuplicateSnapshot { snapshot },
            );
        }
    }

    let mut turns = Vec::with_capacity(total_order.len());
    let mut previous_terminal: Option<(TurnId, ResolvedContextFrontierSnapshot)> = None;
    let mut active = None;
    let mut queued_seen = false;
    let mut referenced_snapshots = BTreeSet::new();
    let mut current_attempts = BTreeMap::new();

    for (index, turn) in total_order.into_iter().enumerate() {
        let record = records_by_turn[&turn];
        let state = match &record.state {
            AcceptedInputTurnSchedulingRecordState::Queued => {
                queued_seen = true;
                ReconstitutedSchedulingState::Queued
            }
            AcceptedInputTurnSchedulingRecordState::Active {
                starting_lineage,
                starting_frontier,
                phase,
            } => {
                if active.is_some() || queued_seen {
                    return Err(
                        AcceptedInputSchedulingReconstitutionFailure::InvalidLifecycleOrder {
                            turn,
                        },
                    );
                }
                active = Some(turn);
                if phase.owning_turn != turn {
                    let ActiveTurnPhase::Running { current_attempt } = &phase.phase else {
                        unreachable!("public reconstitution inputs admit only running phases");
                    };
                    return Err(
                        AcceptedInputSchedulingReconstitutionFailure::CurrentAttemptOwnershipMismatch {
                            turn,
                            attempt: current_attempt.id(),
                        },
                    );
                }
                if let ActiveTurnPhase::Running { current_attempt } = &phase.phase
                    && current_attempts
                        .insert(current_attempt.id(), turn)
                        .is_some()
                {
                    return Err(
                        AcceptedInputSchedulingReconstitutionFailure::DuplicateCurrentAttempt {
                            attempt: current_attempt.id(),
                        },
                    );
                }
                let start = validate_start(
                    index,
                    turn,
                    *starting_lineage,
                    *starting_frontier,
                    previous_terminal.as_ref(),
                    &origin_by_turn,
                    &snapshots,
                    &mut referenced_snapshots,
                )?;
                ReconstitutedSchedulingState::Active {
                    start,
                    phase: phase.phase.clone(),
                }
            }
            AcceptedInputTurnSchedulingRecordState::TerminalFailed {
                starting_lineage,
                starting_frontier,
                terminal_frontier,
            } => {
                if active.is_some() || queued_seen {
                    return Err(
                        AcceptedInputSchedulingReconstitutionFailure::InvalidLifecycleOrder {
                            turn,
                        },
                    );
                }
                let start = validate_start(
                    index,
                    turn,
                    *starting_lineage,
                    *starting_frontier,
                    previous_terminal.as_ref(),
                    &origin_by_turn,
                    &snapshots,
                    &mut referenced_snapshots,
                )?;
                let terminal = snapshots.get(terminal_frontier).cloned().ok_or(
                    AcceptedInputSchedulingReconstitutionFailure::TerminalSnapshotMissing { turn },
                )?;
                if !referenced_snapshots.insert(*terminal_frontier) {
                    return Err(
                        AcceptedInputSchedulingReconstitutionFailure::TerminalFrontierMismatch {
                            turn,
                        },
                    );
                }
                let failed_entry = failure_by_turn.get(&turn).copied().ok_or(
                    AcceptedInputSchedulingReconstitutionFailure::MissingFailureEntry { turn },
                )?;
                let mut expected = start_snapshot_entries(&snapshots, start);
                expected.push(failed_entry);
                if terminal.ordered_entries().ne(expected.iter().copied()) {
                    return Err(
                        AcceptedInputSchedulingReconstitutionFailure::TerminalFrontierMismatch {
                            turn,
                        },
                    );
                }
                previous_terminal = Some((turn, terminal.clone()));
                ReconstitutedSchedulingState::TerminalFailed {
                    start,
                    terminal_frontier: terminal,
                }
            }
        };

        if !matches!(state, ReconstitutedSchedulingState::Queued)
            && !origin_by_turn.contains_key(&turn)
        {
            return Err(AcceptedInputSchedulingReconstitutionFailure::MissingOriginEntry { turn });
        }
        if matches!(state, ReconstitutedSchedulingState::Queued) {
            previous_terminal = None;
        }

        turns.push(AcceptedInputTurnSchedulingProjection {
            session,
            turn,
            accepted_input: record.accepted_input.clone(),
            order: record.order,
            origin_configuration: record.origin_configuration.clone(),
            state,
        });
    }

    if let Some(snapshot) = snapshots
        .keys()
        .copied()
        .find(|snapshot| !referenced_snapshots.contains(snapshot))
    {
        return Err(
            AcceptedInputSchedulingReconstitutionFailure::UnreferencedSnapshot { snapshot },
        );
    }

    let active_acceptance_tail = reconstitute_active_acceptance_tail(
        session,
        active,
        input.active_acceptance_tail.as_ref(),
        &records_by_turn,
        &accepted_input_turns,
    )?;

    Ok(AcceptedInputSchedulingProjection {
        session: input.session.clone(),
        turns: turns.into_boxed_slice(),
        active_acceptance_tail,
        semantic_entries,
        snapshots,
        current_attempts,
    })
}

fn reconstitute_active_acceptance_tail(
    session: SessionId,
    active: Option<TurnId>,
    candidate: Option<&SessionAcceptanceTailReconstitutionInput>,
    records_by_turn: &BTreeMap<TurnId, &AcceptedInputTurnSchedulingRecord>,
    accepted_input_turns: &BTreeMap<AcceptedInputId, TurnId>,
) -> Result<Option<SessionAcceptanceTail>, AcceptedInputSchedulingReconstitutionFailure> {
    let (active, candidate) = match (active, candidate) {
        (None, None) => return Ok(None),
        (None, Some(_)) => {
            return Err(
                AcceptedInputSchedulingReconstitutionFailure::UnexpectedActiveAcceptanceTail,
            );
        }
        (Some(active), None) => {
            return Err(
                AcceptedInputSchedulingReconstitutionFailure::MissingActiveAcceptanceTail {
                    turn: active,
                },
            );
        }
        (Some(active), Some(candidate)) => (active, candidate),
    };

    if candidate.session != session {
        return Err(
            AcceptedInputSchedulingReconstitutionFailure::AcceptanceTailSessionMismatch {
                expected: session,
                actual: candidate.session,
            },
        );
    }

    let active_record = records_by_turn[&active];
    let expected_anchor = active_record.accepted_input.id();
    if candidate.anchor != expected_anchor {
        return Err(
            AcceptedInputSchedulingReconstitutionFailure::AcceptanceTailAnchorMismatch {
                turn: active,
                expected: expected_anchor,
                actual: candidate.anchor,
            },
        );
    }

    if let Some(first) = candidate.entries.first()
        && first.accepted_input.id() != expected_anchor
    {
        return Err(
            AcceptedInputSchedulingReconstitutionFailure::AcceptanceTailAnchorMismatch {
                turn: active,
                expected: expected_anchor,
                actual: first.accepted_input.id(),
            },
        );
    }

    let mut expected_position = active_record.order.acceptance_position();
    let mut seen = BTreeSet::new();
    let mut entries = Vec::with_capacity(candidate.entries.len());
    for (index, entry) in candidate.entries.iter().enumerate() {
        let accepted_input = entry.accepted_input.id();
        if entry.session != session {
            return Err(
                AcceptedInputSchedulingReconstitutionFailure::AcceptanceTailEntrySessionMismatch {
                    accepted_input,
                },
            );
        }
        if !seen.insert(accepted_input) {
            return Err(
                AcceptedInputSchedulingReconstitutionFailure::DuplicateAcceptanceTailEntry {
                    accepted_input,
                },
            );
        }
        if entry.position != expected_position {
            return Err(
                AcceptedInputSchedulingReconstitutionFailure::AcceptanceTailPositionMismatch {
                    accepted_input,
                    expected: expected_position,
                    actual: entry.position,
                },
            );
        }

        let disposition_valid = match entry.accepted_input.disposition() {
            AcceptedInputDisposition::OriginOf(origin) => {
                records_by_turn.get(origin).is_some_and(|record| {
                    record.accepted_input.id() == accepted_input
                        && record.order.acceptance_position() == entry.position
                        && if index == 0 {
                            !matches!(entry.delivery, DeliveryRequest::NextSafePoint { .. })
                        } else {
                            matches!(
                                entry.delivery,
                                DeliveryRequest::AfterCurrentTurn {
                                    expected_active_turn,
                                    ..
                                } if expected_active_turn == active
                            )
                        }
                })
            }
            AcceptedInputDisposition::PendingSteering { binding } => {
                !accepted_input_turns.contains_key(&accepted_input)
                    && matches!(
                        entry.delivery,
                        DeliveryRequest::NextSafePoint {
                            expected_active_turn,
                        } if expected_active_turn == binding.source_turn()
                            && expected_active_turn == active
                    )
            }
            AcceptedInputDisposition::ConsumedAsSteering { .. }
            | AcceptedInputDisposition::ReclassifiedAsTurnOrigin { .. } => false,
        };
        if !disposition_valid
            || (index == 0
                && entry.accepted_input.disposition()
                    != &AcceptedInputDisposition::OriginOf(active))
        {
            return Err(
                AcceptedInputSchedulingReconstitutionFailure::AcceptanceTailDispositionMismatch {
                    accepted_input,
                },
            );
        }

        entries.push(SessionAcceptanceTailEntry {
            accepted_input: entry.accepted_input.clone(),
            position: entry.position,
            delivery: entry.delivery,
        });
        if index + 1 < candidate.entries.len() {
            expected_position = expected_position.checked_next().ok_or(
                AcceptedInputSchedulingReconstitutionFailure::AcceptanceTailLastPositionMismatch {
                    expected: candidate.observed_last_position,
                    actual: Some(entry.position),
                },
            )?;
        }
    }

    let actual_last = entries.last().map(|entry| entry.position);
    if actual_last != Some(candidate.observed_last_position) {
        return Err(
            AcceptedInputSchedulingReconstitutionFailure::AcceptanceTailLastPositionMismatch {
                expected: candidate.observed_last_position,
                actual: actual_last,
            },
        );
    }

    Ok(Some(SessionAcceptanceTail {
        session,
        anchor: expected_anchor,
        observed_last_position: candidate.observed_last_position,
        entries: entries.into_boxed_slice(),
    }))
}

fn validate_record_correlations(
    session: SessionId,
    record: &AcceptedInputTurnSchedulingRecord,
) -> Result<(), AcceptedInputSchedulingReconstitutionFailure> {
    if record.stored_session != session {
        return Err(
            AcceptedInputSchedulingReconstitutionFailure::TurnSessionMismatch { turn: record.turn },
        );
    }
    if record.accepted_input_session != session {
        return Err(
            AcceptedInputSchedulingReconstitutionFailure::AcceptedInputSessionMismatch {
                turn: record.turn,
            },
        );
    }
    if record.queue_session != session {
        return Err(
            AcceptedInputSchedulingReconstitutionFailure::QueueSessionMismatch {
                turn: record.turn,
            },
        );
    }
    if record.queue_turn != record.turn {
        return Err(
            AcceptedInputSchedulingReconstitutionFailure::QueueTurnMismatch { turn: record.turn },
        );
    }
    if record.accepted_input.disposition() != &AcceptedInputDisposition::OriginOf(record.turn) {
        return Err(
            AcceptedInputSchedulingReconstitutionFailure::AcceptedInputOriginMismatch {
                turn: record.turn,
            },
        );
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn validate_start(
    index: usize,
    turn: TurnId,
    actual_lineage: AcceptedInputStartingLineage,
    starting_frontier: ContextFrontierId,
    previous_terminal: Option<&(TurnId, ResolvedContextFrontierSnapshot)>,
    origin_by_turn: &BTreeMap<TurnId, SemanticTranscriptEntryRef>,
    snapshots: &BTreeMap<ContextFrontierId, ResolvedContextFrontierSnapshot>,
    referenced_snapshots: &mut BTreeSet<ContextFrontierId>,
) -> Result<AcceptedInputTurnStart, AcceptedInputSchedulingReconstitutionFailure> {
    let expected_lineage = match (index, previous_terminal) {
        (0, None) => AcceptedInputStartingLineage::FirstInSession,
        (_, Some((predecessor, _))) => AcceptedInputStartingLineage::After {
            immediate_predecessor: *predecessor,
        },
        _ => {
            return Err(
                AcceptedInputSchedulingReconstitutionFailure::InvalidLifecycleOrder { turn },
            );
        }
    };
    if actual_lineage != expected_lineage {
        return Err(
            AcceptedInputSchedulingReconstitutionFailure::StartingLineageMismatch {
                turn,
                expected: expected_lineage,
                actual: actual_lineage,
            },
        );
    }
    let snapshot = snapshots
        .get(&starting_frontier)
        .ok_or(AcceptedInputSchedulingReconstitutionFailure::StartingSnapshotMissing { turn })?;
    if !referenced_snapshots.insert(starting_frontier) {
        return Err(
            AcceptedInputSchedulingReconstitutionFailure::StartingFrontierMismatch { turn },
        );
    }
    let origin = origin_by_turn
        .get(&turn)
        .copied()
        .ok_or(AcceptedInputSchedulingReconstitutionFailure::MissingOriginEntry { turn })?;
    let mut expected = previous_terminal
        .map(|(_, frontier)| frontier.ordered_entries().collect::<Vec<_>>())
        .unwrap_or_default();
    expected.push(origin);
    if snapshot.ordered_entries().ne(expected.iter().copied()) {
        return Err(
            AcceptedInputSchedulingReconstitutionFailure::StartingFrontierMismatch { turn },
        );
    }
    Ok(AcceptedInputTurnStart::from_validated_eligibility(
        actual_lineage,
        snapshot.frontier(),
    ))
}

fn start_snapshot_entries(
    snapshots: &BTreeMap<ContextFrontierId, ResolvedContextFrontierSnapshot>,
    start: AcceptedInputTurnStart,
) -> Vec<SemanticTranscriptEntryRef> {
    snapshots[&start.frontier().snapshot()]
        .ordered_entries()
        .collect()
}

fn prepare_earliest_queued_activation(
    projection: AcceptedInputSchedulingProjection,
    identities: AcceptedInputTurnActivationIdentities,
) -> Result<PreparedAcceptedInputTurnActivation, AcceptedInputEligibilityError> {
    let fail = |projection, failure| AcceptedInputEligibilityError {
        projection: Box::new(projection),
        identities,
        failure,
    };

    if projection
        .current_attempts
        .contains_key(&identities.initial_attempt)
    {
        return Err(fail(
            projection,
            AcceptedInputEligibilityFailure::InitialAttemptIdentityAlreadyExists,
        ));
    }
    if let Some(active) = projection.active_turn() {
        let turn = active.turn();
        return Err(fail(
            projection,
            AcceptedInputEligibilityFailure::ActiveTurnPresent { turn },
        ));
    }
    let Some(index) = projection
        .turns
        .iter()
        .position(|turn| turn.status() == AcceptedInputTurnSchedulingStatus::Queued)
    else {
        return Err(fail(
            projection,
            AcceptedInputEligibilityFailure::NoQueuedTurn,
        ));
    };

    let source_session = projection.session.id();
    let origin_ref =
        SemanticTranscriptEntryRef::from_source(source_session, identities.origin_entry);
    if projection.semantic_entries.contains_key(&origin_ref) {
        return Err(fail(
            projection,
            AcceptedInputEligibilityFailure::OriginEntryIdentityAlreadyExists,
        ));
    }
    if projection
        .snapshots
        .contains_key(&identities.starting_frontier)
    {
        return Err(fail(
            projection,
            AcceptedInputEligibilityFailure::StartingFrontierIdentityAlreadyExists,
        ));
    }

    let queued = &projection.turns[index];
    let origin_entry = SemanticTranscriptEntry::from_validated_parts(
        identities.origin_entry,
        source_session,
        InitialSemanticTranscriptEntryPayload::OriginAcceptedInput {
            accepted_input: queued.accepted_input.id(),
        },
    );
    let (lineage, starting_snapshot) = if index == 0 {
        let snapshot = ResolvedContextFrontierSnapshot::try_from_candidate(
            source_session,
            identities.starting_frontier,
            vec![origin_entry.reference()],
        )
        .expect("one fresh exact origin reference is ordered and distinct");
        (AcceptedInputStartingLineage::FirstInSession, snapshot)
    } else {
        let predecessor = &projection.turns[index - 1];
        let terminal_frontier = predecessor
            .failed_terminal_frontier()
            .expect("validated earliest queued work follows a failed-terminal prefix");
        let snapshot = terminal_frontier
            .derive_appending_candidate(
                identities.starting_frontier,
                vec![origin_entry.reference()],
            )
            .expect("fresh entry and snapshot identities preserve the validated prefix");
        (
            AcceptedInputStartingLineage::After {
                immediate_predecessor: predecessor.turn,
            },
            snapshot,
        )
    };
    let start =
        AcceptedInputTurnStart::from_validated_eligibility(lineage, starting_snapshot.frontier());
    let turn = ActivatedAcceptedInputTurn {
        session: source_session,
        turn: queued.turn,
        accepted_input: queued.accepted_input.clone(),
        order: queued.order,
        configuration: queued.origin_configuration.clone(),
        start,
        phase: ActiveTurnPhase::Running {
            current_attempt: CurrentTurnAttempt::prepared(identities.initial_attempt),
        },
    };

    Ok(PreparedAcceptedInputTurnActivation {
        turn,
        origin_entry,
        starting_snapshot,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        AcceptedInputDisposition, CurrentTurnAttemptState, ModelSelectionOverride,
        ModelSelectionRequest, PerInputConfigurationChoices, SessionConfigurationDefaults,
        SessionConfigurationDefaultsVersion, SessionCreationCause, SessionCreationProvenance,
        SessionReconstitutionInput,
        test_support::{
            accepted_input_id, context_frontier_id, direct, semantic_transcript_entry_id,
            session_id, turn_attempt_id, turn_id,
        },
    };

    fn current_session() -> Session {
        let session = session_id(1);
        let version = SessionConfigurationDefaultsVersion::first();
        let defaults = SessionConfigurationDefaults::new(ModelSelectionRequest::Direct(direct(1)));
        SessionReconstitutionInput::new(
            session,
            session,
            SessionCreationProvenance::new(
                SessionCreationCause::OwnerInitiated,
                TranscriptAncestry::None,
            ),
            session,
            version,
            session,
            version,
            defaults,
        )
        .reconstitute()
        .expect("test session facts are fully correlated")
    }

    fn configuration(session: &Session) -> OriginConfiguration {
        let checked = session
            .current_configuration_defaults()
            .derive_request(
                SessionConfigurationDefaultsVersion::first(),
                ModelSelectionOverride::UseSessionDefault,
            )
            .expect("the test request names the current defaults");
        OriginConfiguration::freeze(checked, |_| None)
            .expect("a direct model selection does not consult aliases")
    }

    fn record(
        session: &Session,
        turn_value: u128,
        accepted_input_value: u128,
        position: u64,
        state: AcceptedInputTurnSchedulingRecordState,
    ) -> AcceptedInputTurnSchedulingRecord {
        let turn = turn_id(turn_value);
        let accepted_input = AcceptedInputLifecycle::new(
            accepted_input_id(accepted_input_value),
            AcceptedInputDisposition::OriginOf(turn),
        );
        AcceptedInputTurnSchedulingRecord::new(
            session.id(),
            turn,
            session.id(),
            accepted_input,
            session.id(),
            turn,
            AcceptedInputQueueOrder::ordinary(
                crate::SessionInputPosition::try_from_u64(position)
                    .expect("test positions are positive"),
            ),
            configuration(session),
            state,
        )
    }

    fn origin_entry(
        session: &Session,
        entry: u128,
        accepted_input: u128,
    ) -> SemanticTranscriptEntryReconstitutionInput {
        SemanticTranscriptEntryReconstitutionInput::new(
            semantic_transcript_entry_id(entry),
            session.id(),
            InitialSemanticTranscriptEntryPayload::OriginAcceptedInput {
                accepted_input: accepted_input_id(accepted_input),
            },
        )
    }

    fn failed_entry(
        session: &Session,
        entry: u128,
        turn: u128,
    ) -> SemanticTranscriptEntryReconstitutionInput {
        SemanticTranscriptEntryReconstitutionInput::new(
            semantic_transcript_entry_id(entry),
            session.id(),
            InitialSemanticTranscriptEntryPayload::TurnFailed {
                turn: turn_id(turn),
            },
        )
    }

    fn entry_ref(session: &Session, entry: u128) -> SemanticTranscriptEntryRef {
        SemanticTranscriptEntryRef::from_source(session.id(), semantic_transcript_entry_id(entry))
    }

    fn snapshot(
        session: &Session,
        identity: u128,
        entries: &[u128],
    ) -> ResolvedContextFrontierReconstitutionInput {
        ResolvedContextFrontierReconstitutionInput::new(
            session.id(),
            context_frontier_id(identity),
            entries
                .iter()
                .map(|entry| entry_ref(session, *entry))
                .collect(),
        )
    }

    fn identities(
        entry: u128,
        snapshot: u128,
        attempt: u128,
    ) -> AcceptedInputTurnActivationIdentities {
        AcceptedInputTurnActivationIdentities::new(
            semantic_transcript_entry_id(entry),
            context_frontier_id(snapshot),
            turn_attempt_id(attempt),
        )
    }

    fn active_tail(
        session: &Session,
        turn_value: u128,
        accepted_input_value: u128,
        position: u64,
    ) -> Option<SessionAcceptanceTailReconstitutionInput> {
        let accepted_input = accepted_input_id(accepted_input_value);
        let position =
            SessionInputPosition::try_from_u64(position).expect("test positions are positive");
        Some(SessionAcceptanceTailReconstitutionInput::new(
            session.id(),
            accepted_input,
            position,
            vec![SessionAcceptanceTailEntryReconstitutionInput::new(
                session.id(),
                AcceptedInputLifecycle::new(
                    accepted_input,
                    AcceptedInputDisposition::OriginOf(turn_id(turn_value)),
                ),
                position,
                DeliveryRequest::StartWhenNoActiveTurn {
                    configuration: PerInputConfigurationChoices::new(
                        SessionConfigurationDefaultsVersion::first(),
                        ModelSelectionOverride::UseSessionDefault,
                    ),
                },
            )],
        ))
    }

    fn active_input(
        session: &Session,
        tail: Option<SessionAcceptanceTailReconstitutionInput>,
    ) -> AcceptedInputSchedulingReconstitutionInput {
        AcceptedInputSchedulingReconstitutionInput::new(
            session.clone(),
            vec![record(
                session,
                10,
                20,
                1,
                AcceptedInputTurnSchedulingRecordState::Active {
                    starting_lineage: AcceptedInputStartingLineage::FirstInSession,
                    starting_frontier: context_frontier_id(40),
                    phase: ActiveTurnSchedulingReconstitutionInput::prepared(
                        turn_id(10),
                        turn_attempt_id(50),
                    ),
                },
            )],
            vec![origin_entry(session, 30, 20)],
            vec![snapshot(session, 40, &[30])],
            tail,
        )
    }

    fn active_input_with_post_anchor_origin(
        session: &Session,
        delivery: DeliveryRequest,
    ) -> AcceptedInputSchedulingReconstitutionInput {
        let mut tail =
            active_tail(session, 10, 20, 1).expect("the helper constructs an active tail");
        tail.observed_last_position =
            SessionInputPosition::try_from_u64(2).expect("positive test position");
        tail.entries
            .push(SessionAcceptanceTailEntryReconstitutionInput::new(
                session.id(),
                AcceptedInputLifecycle::new(
                    accepted_input_id(21),
                    AcceptedInputDisposition::OriginOf(turn_id(11)),
                ),
                SessionInputPosition::try_from_u64(2).expect("positive test position"),
                delivery,
            ));
        let mut input = active_input(session, Some(tail));
        input.turns.push(record(
            session,
            11,
            21,
            2,
            AcceptedInputTurnSchedulingRecordState::Queued,
        ));
        input
    }

    /// S01 / INV-009 / INV-015: ancestry-free first eligibility fixes the
    /// origin-only frontier and enters Running with one Prepared attempt in
    /// the same sealed candidate.
    #[test]
    fn s01_first_eligibility_prepares_one_atomic_activation_candidate() {
        let session = current_session();
        let input = AcceptedInputSchedulingReconstitutionInput::new(
            session.clone(),
            vec![record(
                &session,
                10,
                20,
                1,
                AcceptedInputTurnSchedulingRecordState::Queued,
            )],
            vec![],
            vec![],
            None,
        );

        let candidate = input
            .reconstitute()
            .expect("a complete queued projection is valid")
            .prepare_earliest_queued_activation(identities(30, 40, 50))
            .expect("the sole queued turn is eligible with no active slot");

        assert_eq!(candidate.turn().turn(), turn_id(10));
        assert_eq!(
            candidate.turn().accepted_input().id(),
            accepted_input_id(20)
        );
        assert_eq!(
            candidate.origin_entry().payload(),
            InitialSemanticTranscriptEntryPayload::OriginAcceptedInput {
                accepted_input: accepted_input_id(20),
            }
        );
        assert_eq!(
            candidate.start().lineage(),
            AcceptedInputStartingLineage::FirstInSession
        );
        assert_eq!(
            candidate
                .starting_snapshot()
                .ordered_entries()
                .collect::<Vec<_>>(),
            vec![entry_ref(&session, 30)]
        );
        assert!(matches!(
            candidate.turn().phase(),
            ActiveTurnPhase::Running { current_attempt }
                if current_attempt.id() == turn_attempt_id(50)
                    && current_attempt.state() == &crate::CurrentTurnAttemptState::Prepared
        ));
    }

    /// S03 / INV-009: restart returns a queued scheduling projection with no
    /// manufactured start, and a cross-wired OriginOf fact fails closed.
    #[test]
    fn s03_checked_reconstitution_preserves_queued_state_and_exact_origin() {
        let session = current_session();
        let queued = record(
            &session,
            10,
            20,
            1,
            AcceptedInputTurnSchedulingRecordState::Queued,
        );
        let projection = AcceptedInputSchedulingReconstitutionInput::new(
            session.clone(),
            vec![queued.clone()],
            vec![],
            vec![],
            None,
        )
        .reconstitute()
        .expect("the complete queued record is valid");
        let reconstituted = projection
            .turn(turn_id(10))
            .expect("the stored queued turn remains present");
        assert_eq!(
            reconstituted.status(),
            AcceptedInputTurnSchedulingStatus::Queued
        );
        assert_eq!(reconstituted.start(), None);

        let wrong_turn = turn_id(99);
        let cross_wired = AcceptedInputTurnSchedulingRecord::new(
            queued.stored_session(),
            queued.turn(),
            queued.accepted_input_session(),
            AcceptedInputLifecycle::new(
                queued.accepted_input().id(),
                AcceptedInputDisposition::OriginOf(wrong_turn),
            ),
            queued.queue_session(),
            queued.queue_turn(),
            queued.order(),
            queued.origin_configuration().clone(),
            queued.state().clone(),
        );
        let error = AcceptedInputSchedulingReconstitutionInput::new(
            session,
            vec![cross_wired],
            vec![],
            vec![],
            None,
        )
        .reconstitute()
        .expect_err("the exact OriginOf(turn) correlation is required");
        assert_eq!(
            error.failure(),
            &AcceptedInputSchedulingReconstitutionFailure::AcceptedInputOriginMismatch {
                turn: turn_id(10),
            }
        );
    }

    /// S03 / INV-009: an admitted active restart record owns its exact
    /// Prepared attempt, reconstructs Running, and makes that identity
    /// unavailable to a second activation candidate.
    #[test]
    fn s03_active_reconstitution_requires_and_exposes_exact_prepared_attempt() {
        let session = current_session();
        let active = record(
            &session,
            10,
            20,
            1,
            AcceptedInputTurnSchedulingRecordState::Active {
                starting_lineage: AcceptedInputStartingLineage::FirstInSession,
                starting_frontier: context_frontier_id(40),
                phase: ActiveTurnSchedulingReconstitutionInput::prepared(
                    turn_id(10),
                    turn_attempt_id(50),
                ),
            },
        );
        let projection = AcceptedInputSchedulingReconstitutionInput::new(
            session.clone(),
            vec![active],
            vec![origin_entry(&session, 30, 20)],
            vec![snapshot(&session, 40, &[30])],
            active_tail(&session, 10, 20, 1),
        )
        .reconstitute()
        .expect("the active turn has its exact prepared attempt");
        let active = projection
            .active_turn()
            .expect("the reconstructed turn owns the active slot");
        assert!(matches!(
            active.active_phase(),
            Some(ActiveTurnPhase::Running { current_attempt })
                if current_attempt.id() == turn_attempt_id(50)
                    && current_attempt.state() == &CurrentTurnAttemptState::Prepared
        ));

        let collision = projection
            .clone()
            .prepare_earliest_queued_activation(identities(31, 41, 50))
            .expect_err("a current attempt identity cannot be proposed again");
        assert_eq!(
            collision.failure(),
            AcceptedInputEligibilityFailure::InitialAttemptIdentityAlreadyExists
        );
        let occupied = projection
            .prepare_earliest_queued_activation(identities(31, 41, 51))
            .expect_err("an active slot blocks every queued activation");
        assert_eq!(
            occupied.failure(),
            AcceptedInputEligibilityFailure::ActiveTurnPresent { turn: turn_id(10) }
        );
    }

    /// S03 / INV-009 / ADR-0041: evidence-free prepared and running phases
    /// reconstruct through explicit constructors. Proof-bearing stop and wait
    /// phases have no constructor until their complete owning facts exist.
    #[test]
    fn active_reconstitution_admits_only_evidence_free_attempt_phases() {
        let session = current_session();
        let phases = [
            (
                ActiveTurnSchedulingReconstitutionInput::prepared(turn_id(10), turn_attempt_id(50)),
                ActiveTurnPhase::Running {
                    current_attempt: CurrentTurnAttempt::prepared(turn_attempt_id(50)),
                },
            ),
            (
                ActiveTurnSchedulingReconstitutionInput::running(turn_id(10), turn_attempt_id(51)),
                ActiveTurnPhase::Running {
                    current_attempt: CurrentTurnAttempt::prepared(turn_attempt_id(51))
                        .begin_running()
                        .expect("the prepared attempt begins running"),
                },
            ),
        ];

        for (phase_input, phase) in phases {
            let projection = AcceptedInputSchedulingReconstitutionInput::new(
                session.clone(),
                vec![record(
                    &session,
                    10,
                    20,
                    1,
                    AcceptedInputTurnSchedulingRecordState::Active {
                        starting_lineage: AcceptedInputStartingLineage::FirstInSession,
                        starting_frontier: context_frontier_id(40),
                        phase: phase_input,
                    },
                )],
                vec![origin_entry(&session, 30, 20)],
                vec![snapshot(&session, 40, &[30])],
                active_tail(&session, 10, 20, 1),
            )
            .reconstitute()
            .expect("the canonical active phase reconstructs");
            assert_eq!(
                projection
                    .active_turn()
                    .expect("the turn owns the active slot")
                    .active_phase(),
                Some(&phase)
            );
        }
    }

    /// S03 / S08 / INV-009 / INV-016 / ADR-0041: an active
    /// scheduling projection requires the exact session-scoped interval
    /// anchored at its origin; a missing, cross-session, or cross-wired
    /// interval fails closed.
    #[test]
    fn active_reconstitution_requires_exact_session_acceptance_tail_identity() {
        let session = current_session();
        let missing = active_input(&session, None)
            .reconstitute()
            .expect_err("an active turn requires its complete acceptance tail");
        assert_eq!(
            missing.failure(),
            &AcceptedInputSchedulingReconstitutionFailure::MissingActiveAcceptanceTail {
                turn: turn_id(10),
            }
        );

        let mut wrong_session =
            active_tail(&session, 10, 20, 1).expect("the helper constructs an active tail");
        wrong_session.session = session_id(2);
        let wrong_session = active_input(&session, Some(wrong_session))
            .reconstitute()
            .expect_err("a tail from another session cannot authorize reconstitution");
        assert_eq!(
            wrong_session.failure(),
            &AcceptedInputSchedulingReconstitutionFailure::AcceptanceTailSessionMismatch {
                expected: session_id(1),
                actual: session_id(2),
            }
        );

        let mut wrong_anchor =
            active_tail(&session, 10, 20, 1).expect("the helper constructs an active tail");
        wrong_anchor.anchor = accepted_input_id(99);
        let wrong_anchor = active_input(&session, Some(wrong_anchor))
            .reconstitute()
            .expect_err("the interval must be anchored at the active origin");
        assert_eq!(
            wrong_anchor.failure(),
            &AcceptedInputSchedulingReconstitutionFailure::AcceptanceTailAnchorMismatch {
                turn: turn_id(10),
                expected: accepted_input_id(20),
                actual: accepted_input_id(99),
            }
        );
    }

    /// S03 / S08 / INV-016 / ADR-0041: every position
    /// from the active origin through the observed session tail is present
    /// exactly once and every pending-steering disposition remains bound to
    /// that active turn.
    #[test]
    fn active_reconstitution_rejects_gapped_or_misbound_acceptance_tail() {
        let session = current_session();
        let mut gapped =
            active_tail(&session, 10, 20, 1).expect("the helper constructs an active tail");
        gapped.observed_last_position =
            SessionInputPosition::try_from_u64(3).expect("positive test position");
        gapped
            .entries
            .push(SessionAcceptanceTailEntryReconstitutionInput::new(
                session.id(),
                AcceptedInputLifecycle::new(
                    accepted_input_id(21),
                    AcceptedInputDisposition::PendingSteering {
                        binding: crate::SteeringBinding::new(turn_id(10)),
                    },
                ),
                SessionInputPosition::try_from_u64(3).expect("positive test position"),
                DeliveryRequest::NextSafePoint {
                    expected_active_turn: turn_id(10),
                },
            ));
        let gapped = active_input(&session, Some(gapped))
            .reconstitute()
            .expect_err("the complete tail cannot omit an interior position");
        assert_eq!(
            gapped.failure(),
            &AcceptedInputSchedulingReconstitutionFailure::AcceptanceTailPositionMismatch {
                accepted_input: accepted_input_id(21),
                expected: SessionInputPosition::try_from_u64(2).expect("positive test position"),
                actual: SessionInputPosition::try_from_u64(3).expect("positive test position"),
            }
        );

        let mut misbound =
            active_tail(&session, 10, 20, 1).expect("the helper constructs an active tail");
        misbound.observed_last_position =
            SessionInputPosition::try_from_u64(2).expect("positive test position");
        misbound
            .entries
            .push(SessionAcceptanceTailEntryReconstitutionInput::new(
                session.id(),
                AcceptedInputLifecycle::new(
                    accepted_input_id(21),
                    AcceptedInputDisposition::PendingSteering {
                        binding: crate::SteeringBinding::new(turn_id(99)),
                    },
                ),
                SessionInputPosition::try_from_u64(2).expect("positive test position"),
                DeliveryRequest::NextSafePoint {
                    expected_active_turn: turn_id(99),
                },
            ));
        let misbound = active_input(&session, Some(misbound))
            .reconstitute()
            .expect_err("pending steering must bind to the exact active owner");
        assert_eq!(
            misbound.failure(),
            &AcceptedInputSchedulingReconstitutionFailure::AcceptanceTailDispositionMismatch {
                accepted_input: accepted_input_id(21),
            }
        );

        let mut cross_wired =
            active_tail(&session, 10, 20, 1).expect("the helper constructs an active tail");
        cross_wired.observed_last_position =
            SessionInputPosition::try_from_u64(2).expect("positive test position");
        cross_wired
            .entries
            .push(SessionAcceptanceTailEntryReconstitutionInput::new(
                session.id(),
                AcceptedInputLifecycle::new(
                    accepted_input_id(21),
                    AcceptedInputDisposition::OriginOf(turn_id(11)),
                ),
                SessionInputPosition::try_from_u64(2).expect("positive test position"),
                DeliveryRequest::AfterCurrentTurn {
                    expected_active_turn: turn_id(10),
                    configuration: PerInputConfigurationChoices::new(
                        SessionConfigurationDefaultsVersion::first(),
                        ModelSelectionOverride::UseSessionDefault,
                    ),
                },
            ));
        let mut cross_wired_input = active_input(&session, Some(cross_wired));
        cross_wired_input.turns.push(record(
            &session,
            11,
            21,
            3,
            AcceptedInputTurnSchedulingRecordState::Queued,
        ));
        let cross_wired = cross_wired_input
            .reconstitute()
            .expect_err("an origin disposition must retain its exact immutable position");
        assert_eq!(
            cross_wired.failure(),
            &AcceptedInputSchedulingReconstitutionFailure::AcceptanceTailDispositionMismatch {
                accepted_input: accepted_input_id(21),
            }
        );
    }

    /// S03 / S09 / INV-009 / INV-016 / ADR-0041: every post-anchor origin
    /// was accepted after the exact active owner rather than through a vacant
    /// start or against another predecessor.
    #[test]
    fn active_reconstitution_rejects_non_owning_post_anchor_origin_delivery() {
        let session = current_session();
        let vacant_start = active_input_with_post_anchor_origin(
            &session,
            DeliveryRequest::StartWhenNoActiveTurn {
                configuration: PerInputConfigurationChoices::new(
                    SessionConfigurationDefaultsVersion::first(),
                    ModelSelectionOverride::UseSessionDefault,
                ),
            },
        )
        .reconstitute()
        .expect_err("a post-anchor origin cannot claim a vacant-slot start");
        assert_eq!(
            vacant_start.failure(),
            &AcceptedInputSchedulingReconstitutionFailure::AcceptanceTailDispositionMismatch {
                accepted_input: accepted_input_id(21),
            }
        );

        let another_predecessor = active_input_with_post_anchor_origin(
            &session,
            DeliveryRequest::AfterCurrentTurn {
                expected_active_turn: turn_id(99),
                configuration: PerInputConfigurationChoices::new(
                    SessionConfigurationDefaultsVersion::first(),
                    ModelSelectionOverride::UseSessionDefault,
                ),
            },
        )
        .reconstitute()
        .expect_err("a post-anchor origin must name the exact active predecessor");
        assert_eq!(
            another_predecessor.failure(),
            &AcceptedInputSchedulingReconstitutionFailure::AcceptanceTailDispositionMismatch {
                accepted_input: accepted_input_id(21),
            }
        );

        active_input_with_post_anchor_origin(
            &session,
            DeliveryRequest::AfterCurrentTurn {
                expected_active_turn: turn_id(10),
                configuration: PerInputConfigurationChoices::new(
                    SessionConfigurationDefaultsVersion::first(),
                    ModelSelectionOverride::UseSessionDefault,
                ),
            },
        )
        .reconstitute()
        .expect("the exact active predecessor admits the queued origin");
    }

    /// S03 / S08 / INV-009 / INV-016 / ADR-0041: one accepted input cannot
    /// be both pending steering and a turn origin in the scheduling inventory.
    #[test]
    fn active_reconstitution_rejects_pending_identity_that_is_also_an_origin() {
        let session = current_session();
        let mut tail =
            active_tail(&session, 10, 20, 1).expect("the helper constructs an active tail");
        tail.observed_last_position =
            SessionInputPosition::try_from_u64(2).expect("positive test position");
        tail.entries
            .push(SessionAcceptanceTailEntryReconstitutionInput::new(
                session.id(),
                AcceptedInputLifecycle::new(
                    accepted_input_id(21),
                    AcceptedInputDisposition::PendingSteering {
                        binding: crate::SteeringBinding::new(turn_id(10)),
                    },
                ),
                SessionInputPosition::try_from_u64(2).expect("positive test position"),
                DeliveryRequest::NextSafePoint {
                    expected_active_turn: turn_id(10),
                },
            ));

        active_input(&session, Some(tail.clone()))
            .reconstitute()
            .expect("pending steering remains distinct from every origin");

        let mut aliased = active_input(&session, Some(tail));
        aliased.turns.push(record(
            &session,
            11,
            21,
            2,
            AcceptedInputTurnSchedulingRecordState::Queued,
        ));
        let aliased = aliased
            .reconstitute()
            .expect_err("pending steering cannot reuse a turn-origin identity");
        assert_eq!(
            aliased.failure(),
            &AcceptedInputSchedulingReconstitutionFailure::AcceptanceTailDispositionMismatch {
                accepted_input: accepted_input_id(21),
            }
        );
    }

    /// S03 / INV-016 / ADR-0041: the last represented position must equal
    /// the authoritative session tail observed by the same read.
    #[test]
    fn active_reconstitution_rejects_incomplete_claimed_acceptance_tail() {
        let session = current_session();
        let mut incomplete =
            active_tail(&session, 10, 20, 1).expect("the helper constructs an active tail");
        incomplete.observed_last_position =
            SessionInputPosition::try_from_u64(2).expect("positive test position");
        let incomplete = active_input(&session, Some(incomplete))
            .reconstitute()
            .expect_err("the represented interval must reach the claimed session tail");
        assert_eq!(
            incomplete.failure(),
            &AcceptedInputSchedulingReconstitutionFailure::AcceptanceTailLastPositionMismatch {
                expected: SessionInputPosition::try_from_u64(2).expect("positive test position"),
                actual: Some(SessionInputPosition::first()),
            }
        );
    }

    /// S03 / INV-009: a current attempt owned by another turn cannot
    /// reconstruct an active aggregate.
    #[test]
    fn s03_active_reconstitution_rejects_cross_wired_attempt_owner() {
        let session = current_session();
        let error = AcceptedInputSchedulingReconstitutionInput::new(
            session.clone(),
            vec![record(
                &session,
                10,
                20,
                1,
                AcceptedInputTurnSchedulingRecordState::Active {
                    starting_lineage: AcceptedInputStartingLineage::FirstInSession,
                    starting_frontier: context_frontier_id(40),
                    phase: ActiveTurnSchedulingReconstitutionInput::prepared(
                        turn_id(99),
                        turn_attempt_id(50),
                    ),
                },
            )],
            vec![origin_entry(&session, 30, 20)],
            vec![snapshot(&session, 40, &[30])],
            active_tail(&session, 10, 20, 1),
        )
        .reconstitute()
        .expect_err("attempt ownership must match the exact active turn");
        assert_eq!(
            error.failure(),
            &AcceptedInputSchedulingReconstitutionFailure::CurrentAttemptOwnershipMismatch {
                turn: turn_id(10),
                attempt: turn_attempt_id(50),
            }
        );
    }

    /// S03 / INV-009: eligibility derives the target from complete durable
    /// order and cannot be directed to skip earlier queued work.
    #[test]
    fn s03_eligibility_consumes_the_earliest_queued_origin() {
        let session = current_session();
        let later = record(
            &session,
            11,
            21,
            2,
            AcceptedInputTurnSchedulingRecordState::Queued,
        );
        let earlier = record(
            &session,
            10,
            20,
            1,
            AcceptedInputTurnSchedulingRecordState::Queued,
        );
        let candidate = AcceptedInputSchedulingReconstitutionInput::new(
            session,
            vec![later, earlier],
            vec![],
            vec![],
            None,
        )
        .reconstitute()
        .expect("the complete queue order is valid")
        .prepare_earliest_queued_activation(identities(30, 40, 50))
        .expect("no active slot blocks the earliest queued work");

        assert_eq!(candidate.turn().turn(), turn_id(10));
        assert_eq!(
            candidate.turn().accepted_input().id(),
            accepted_input_id(20)
        );
    }

    /// S09 / INV-009 / INV-015: the earliest queued successor starts only
    /// after the exact immediately preceding failed turn and retains its
    /// complete origin-then-failure terminal prefix before appending its own
    /// origin.
    #[test]
    fn s09_successor_uses_exact_failed_predecessor_terminal_frontier() {
        let session = current_session();
        let predecessor = record(
            &session,
            10,
            20,
            1,
            AcceptedInputTurnSchedulingRecordState::TerminalFailed {
                starting_lineage: AcceptedInputStartingLineage::FirstInSession,
                starting_frontier: context_frontier_id(40),
                terminal_frontier: context_frontier_id(41),
            },
        );
        let successor = record(
            &session,
            11,
            21,
            2,
            AcceptedInputTurnSchedulingRecordState::Queued,
        );
        let projection = AcceptedInputSchedulingReconstitutionInput::new(
            session.clone(),
            vec![successor, predecessor],
            vec![
                failed_entry(&session, 31, 10),
                origin_entry(&session, 30, 20),
            ],
            vec![
                snapshot(&session, 41, &[30, 31]),
                snapshot(&session, 40, &[30]),
            ],
            None,
        )
        .reconstitute()
        .expect("the failed predecessor has a complete validated frontier");

        let candidate = projection
            .prepare_earliest_queued_activation(identities(32, 42, 50))
            .expect("the successor is the earliest queued turn with no active slot");

        assert_eq!(candidate.turn().turn(), turn_id(11));
        assert_eq!(
            candidate.start().lineage(),
            AcceptedInputStartingLineage::After {
                immediate_predecessor: turn_id(10),
            }
        );
        assert_eq!(
            candidate
                .starting_snapshot()
                .ordered_entries()
                .collect::<Vec<_>>(),
            vec![
                entry_ref(&session, 30),
                entry_ref(&session, 31),
                entry_ref(&session, 32),
            ]
        );
    }

    /// S09 / INV-015: a predecessor snapshot that omits its required failed
    /// marker is not a terminal frontier and cannot authorize a successor.
    #[test]
    fn s09_incomplete_failed_terminal_frontier_fails_closed() {
        let session = current_session();
        let error = AcceptedInputSchedulingReconstitutionInput::new(
            session.clone(),
            vec![record(
                &session,
                10,
                20,
                1,
                AcceptedInputTurnSchedulingRecordState::TerminalFailed {
                    starting_lineage: AcceptedInputStartingLineage::FirstInSession,
                    starting_frontier: context_frontier_id(40),
                    terminal_frontier: context_frontier_id(41),
                },
            )],
            vec![
                origin_entry(&session, 30, 20),
                failed_entry(&session, 31, 10),
            ],
            vec![snapshot(&session, 40, &[30]), snapshot(&session, 41, &[30])],
            None,
        )
        .reconstitute()
        .expect_err("the failed marker must follow the exact starting prefix");

        assert_eq!(
            error.failure(),
            &AcceptedInputSchedulingReconstitutionFailure::TerminalFrontierMismatch {
                turn: turn_id(10),
            }
        );
    }
}
