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
    AcceptedInputQueueOrderError, AcceptedInputQueuePriority, AcceptedInputQueueWork,
    AcceptedInputStartingLineage, AcceptedInputTurnStart, ActiveTurnPhase, ContextFrontierId,
    CurrentTurnAttempt, DeliveryRequest, InitialSemanticTranscriptEntryPayload,
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
///
/// Raw stored facts cannot be used to obtain a canonical active phase before
/// the owning scheduling projection validates them:
///
/// ```compile_fail
/// use signalbox_domain::{ActiveTurnSchedulingReconstitutionInput, TurnAttemptId, TurnId};
///
/// let turn = TurnId::from_uuid(uuid::Uuid::nil());
/// let attempt = TurnAttemptId::from_uuid(uuid::Uuid::nil());
/// let input = ActiveTurnSchedulingReconstitutionInput::running(turn, attempt);
/// let _ = input.phase();
/// ```
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActiveTurnSchedulingReconstitutionInput {
    owning_turn: TurnId,
    current_attempt: TurnAttemptId,
    state: EvidenceFreeCurrentAttemptState,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EvidenceFreeCurrentAttemptState {
    Prepared,
    Running,
}

impl ActiveTurnSchedulingReconstitutionInput {
    /// Supplies inert facts for a stored prepared current attempt.
    pub const fn prepared(owning_turn: TurnId, current_attempt: TurnAttemptId) -> Self {
        Self {
            owning_turn,
            current_attempt,
            state: EvidenceFreeCurrentAttemptState::Prepared,
        }
    }

    /// Supplies inert facts for a stored running current attempt.
    pub const fn running(owning_turn: TurnId, current_attempt: TurnAttemptId) -> Self {
        Self {
            owning_turn,
            current_attempt,
            state: EvidenceFreeCurrentAttemptState::Running,
        }
    }

    /// Returns the turn named as owner by the active-phase record.
    pub const fn owning_turn(&self) -> TurnId {
        self.owning_turn
    }

    fn canonical_phase(&self) -> ActiveTurnPhase {
        let current_attempt = CurrentTurnAttempt::prepared(self.current_attempt);
        let current_attempt = match self.state {
            EvidenceFreeCurrentAttemptState::Prepared => current_attempt,
            EvidenceFreeCurrentAttemptState::Running => current_attempt
                .begin_running()
                .expect("a stored running attempt starts from the validated prepared value"),
        };
        ActiveTurnPhase::Running { current_attempt }
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
    origin_delivery: DeliveryRequest,
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

    /// Returns the immutable accepted delivery that created this origin.
    pub const fn origin_delivery(&self) -> DeliveryRequest {
        self.origin_delivery
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
    /// One origin's accepted delivery contradicts its durable queue facts or
    /// historical target.
    OriginDeliveryMismatch {
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
    /// The complete acceptance tail contains applied interrupt evidence that
    /// requires a proof-bearing phase outside this evidence-free seam.
    ActivePhaseEvidenceMismatch {
        /// The active turn whose phase cannot remain evidence-free.
        turn: TurnId,
        /// The accepted interrupt that requires a different phase.
        accepted_input: AcceptedInputId,
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
    for record in records_by_turn.values() {
        if !origin_delivery_matches_record(record.origin_delivery, record, &records_by_turn) {
            return Err(
                AcceptedInputSchedulingReconstitutionFailure::OriginDeliveryMismatch {
                    turn: record.turn,
                },
            );
        }
    }

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
                    return Err(
                        AcceptedInputSchedulingReconstitutionFailure::CurrentAttemptOwnershipMismatch {
                            turn,
                            attempt: phase.current_attempt,
                        },
                    );
                }
                if current_attempts
                    .insert(phase.current_attempt, turn)
                    .is_some()
                {
                    return Err(
                        AcceptedInputSchedulingReconstitutionFailure::DuplicateCurrentAttempt {
                            attempt: phase.current_attempt,
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
                    phase: phase.canonical_phase(),
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

    let latest_known_origin_position = records_by_turn
        .values()
        .map(|record| record.order.acceptance_position())
        .max()
        .expect("the active turn is present in the scheduling inventory");
    if latest_known_origin_position > candidate.observed_last_position {
        return Err(
            AcceptedInputSchedulingReconstitutionFailure::AcceptanceTailLastPositionMismatch {
                expected: candidate.observed_last_position,
                actual: Some(latest_known_origin_position),
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
    let origin_by_position = records_by_turn
        .values()
        .map(|record| {
            (
                record.order.acceptance_position(),
                record.accepted_input.id(),
            )
        })
        .collect::<BTreeMap<_, _>>();
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
                        && entry.delivery == record.origin_delivery
                        && origin_delivery_matches_record(
                            record.origin_delivery,
                            record,
                            records_by_turn,
                        )
                })
            }
            AcceptedInputDisposition::PendingSteering { binding } => {
                !accepted_input_turns.contains_key(&accepted_input)
                    && !origin_by_position.contains_key(&entry.position)
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

        if index > 0
            && matches!(
                entry.delivery,
                DeliveryRequest::Interrupt {
                    expected_active_turn,
                    ..
                } if expected_active_turn == active
            )
        {
            return Err(
                AcceptedInputSchedulingReconstitutionFailure::ActivePhaseEvidenceMismatch {
                    turn: active,
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

fn origin_delivery_matches_record(
    delivery: DeliveryRequest,
    record: &AcceptedInputTurnSchedulingRecord,
    records_by_turn: &BTreeMap<TurnId, &AcceptedInputTurnSchedulingRecord>,
) -> bool {
    if !origin_configuration_matches_delivery(delivery, &record.origin_configuration) {
        return false;
    }

    match (delivery, record.order.priority()) {
        (DeliveryRequest::StartWhenNoActiveTurn { .. }, AcceptedInputQueuePriority::Ordinary) => {
            true
        }
        (
            DeliveryRequest::AfterCurrentTurn {
                expected_active_turn,
                ..
            },
            AcceptedInputQueuePriority::Ordinary,
        ) => historical_target_precedes_origin(expected_active_turn, record, records_by_turn),
        (
            DeliveryRequest::Interrupt {
                expected_active_turn,
                ..
            },
            AcceptedInputQueuePriority::InterruptImmediatelyAfter { predecessor },
        ) => {
            expected_active_turn == predecessor
                && historical_target_precedes_origin(expected_active_turn, record, records_by_turn)
        }
        (
            DeliveryRequest::StartWhenNoActiveTurn { .. }
            | DeliveryRequest::AfterCurrentTurn { .. },
            AcceptedInputQueuePriority::InterruptImmediatelyAfter { .. },
        )
        | (
            DeliveryRequest::Interrupt { .. } | DeliveryRequest::NextSafePoint { .. },
            AcceptedInputQueuePriority::Ordinary,
        )
        | (
            DeliveryRequest::NextSafePoint { .. },
            AcceptedInputQueuePriority::InterruptImmediatelyAfter { .. },
        ) => false,
    }
}

fn origin_configuration_matches_delivery(
    delivery: DeliveryRequest,
    origin_configuration: &OriginConfiguration,
) -> bool {
    let configuration = match delivery {
        DeliveryRequest::StartWhenNoActiveTurn { configuration }
        | DeliveryRequest::Interrupt { configuration, .. }
        | DeliveryRequest::AfterCurrentTurn { configuration, .. } => configuration,
        DeliveryRequest::NextSafePoint { .. } => return false,
    };

    configuration.expected_session_defaults_version()
        == origin_configuration.session_defaults_version()
        && match configuration.model() {
            crate::ModelSelectionOverride::UseSessionDefault => true,
            crate::ModelSelectionOverride::ReplaceWith(requested) => {
                origin_configuration.requested().model() == requested
            }
        }
}

fn historical_target_precedes_origin(
    expected_active_turn: TurnId,
    origin: &AcceptedInputTurnSchedulingRecord,
    records_by_turn: &BTreeMap<TurnId, &AcceptedInputTurnSchedulingRecord>,
) -> bool {
    records_by_turn
        .get(&expected_active_turn)
        .is_some_and(|target| {
            target.order.acceptance_position() < origin.order.acceptance_position()
                && !matches!(target.state, AcceptedInputTurnSchedulingRecordState::Queued)
        })
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
    use expect_test::expect;
    use signalbox_expect_table::table;

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

    fn default_origin_delivery() -> DeliveryRequest {
        DeliveryRequest::StartWhenNoActiveTurn {
            configuration: PerInputConfigurationChoices::new(
                SessionConfigurationDefaultsVersion::first(),
                ModelSelectionOverride::UseSessionDefault,
            ),
        }
    }

    /// One accepted turn origin whose sole identity/order knob is its
    /// acceptance ordinal. Turn and accepted-input identities descend as the
    /// ordinal ascends, so identity order cannot accidentally stand in for
    /// durable acceptance order (`docs/testing-style.md`, rule 4).
    #[derive(Clone, Copy)]
    struct OriginFixture {
        acceptance: u64,
    }

    fn accepted_origin(acceptance: u64) -> OriginFixture {
        OriginFixture { acceptance }
    }

    impl OriginFixture {
        fn turn(self) -> TurnId {
            turn_id(u128::from(u64::MAX - self.acceptance))
        }

        fn accepted_input(self) -> AcceptedInputId {
            accepted_input_id(u128::from(u64::MAX / 2 - self.acceptance))
        }

        fn position(self) -> SessionInputPosition {
            SessionInputPosition::try_from_u64(self.acceptance)
                .expect("test acceptance ordinals are positive")
        }

        fn ordinary_order(self) -> AcceptedInputQueueOrder {
            AcceptedInputQueueOrder::ordinary(self.position())
        }

        fn record(
            self,
            session: &Session,
            state: AcceptedInputTurnSchedulingRecordState,
        ) -> AcceptedInputTurnSchedulingRecord {
            self.record_with(
                session,
                OriginRecordFacts {
                    order: self.ordinary_order(),
                    delivery: default_origin_delivery(),
                    state,
                },
            )
        }

        fn record_with(
            self,
            session: &Session,
            facts: OriginRecordFacts,
        ) -> AcceptedInputTurnSchedulingRecord {
            let turn = self.turn();
            AcceptedInputTurnSchedulingRecord::new(
                session.id(),
                turn,
                session.id(),
                AcceptedInputLifecycle::new(
                    self.accepted_input(),
                    AcceptedInputDisposition::OriginOf(turn),
                ),
                session.id(),
                turn,
                facts.order,
                facts.delivery,
                configuration(session),
                facts.state,
            )
        }

        fn entry(
            self,
            session: &Session,
            entry: SemanticEntryFixture,
        ) -> SemanticTranscriptEntryReconstitutionInput {
            SemanticTranscriptEntryReconstitutionInput::new(
                entry.id(),
                session.id(),
                InitialSemanticTranscriptEntryPayload::OriginAcceptedInput {
                    accepted_input: self.accepted_input(),
                },
            )
        }

        fn active_tail(self, session: &Session) -> SessionAcceptanceTailReconstitutionInput {
            SessionAcceptanceTailReconstitutionInput::new(
                session.id(),
                self.accepted_input(),
                self.position(),
                vec![SessionAcceptanceTailEntryReconstitutionInput::new(
                    session.id(),
                    AcceptedInputLifecycle::new(
                        self.accepted_input(),
                        AcceptedInputDisposition::OriginOf(self.turn()),
                    ),
                    self.position(),
                    default_origin_delivery(),
                )],
            )
        }
    }

    struct OriginRecordFacts {
        order: AcceptedInputQueueOrder,
        delivery: DeliveryRequest,
        state: AcceptedInputTurnSchedulingRecordState,
    }

    #[derive(Clone, Copy)]
    struct SemanticEntryFixture {
        seed: u128,
    }

    fn semantic_entry(seed: u128) -> SemanticEntryFixture {
        SemanticEntryFixture { seed }
    }

    impl SemanticEntryFixture {
        fn id(self) -> SemanticTranscriptEntryId {
            semantic_transcript_entry_id(self.seed)
        }

        fn reference(self, session: &Session) -> SemanticTranscriptEntryRef {
            SemanticTranscriptEntryRef::from_source(session.id(), self.id())
        }

        fn failed_turn(
            self,
            session: &Session,
            turn: OriginFixture,
        ) -> SemanticTranscriptEntryReconstitutionInput {
            SemanticTranscriptEntryReconstitutionInput::new(
                self.id(),
                session.id(),
                InitialSemanticTranscriptEntryPayload::TurnFailed { turn: turn.turn() },
            )
        }
    }

    #[derive(Clone, Copy)]
    struct FrontierFixture {
        seed: u128,
    }

    fn frontier(seed: u128) -> FrontierFixture {
        FrontierFixture { seed }
    }

    impl FrontierFixture {
        fn id(self) -> ContextFrontierId {
            context_frontier_id(self.seed)
        }

        fn snapshot(
            self,
            session: &Session,
            entries: &[SemanticEntryFixture],
        ) -> ResolvedContextFrontierReconstitutionInput {
            ResolvedContextFrontierReconstitutionInput::new(
                session.id(),
                self.id(),
                entries
                    .iter()
                    .map(|entry| entry.reference(session))
                    .collect(),
            )
        }
    }

    #[derive(Clone, Copy)]
    struct ActivationFixture {
        seed: u128,
    }

    fn activation(seed: u128) -> ActivationFixture {
        ActivationFixture { seed }
    }

    fn matching_active_attempt() -> TurnAttemptId {
        turn_attempt_id(50)
    }

    impl ActivationFixture {
        fn origin_entry(self) -> SemanticEntryFixture {
            semantic_entry(100 + self.seed)
        }

        fn starting_frontier(self) -> FrontierFixture {
            frontier(200 + self.seed)
        }

        fn initial_attempt(self) -> TurnAttemptId {
            turn_attempt_id(300 + self.seed)
        }

        fn identities(self) -> AcceptedInputTurnActivationIdentities {
            AcceptedInputTurnActivationIdentities::new(
                self.origin_entry().id(),
                self.starting_frontier().id(),
                self.initial_attempt(),
            )
        }

        fn identities_with_attempt(
            self,
            initial_attempt: TurnAttemptId,
        ) -> AcceptedInputTurnActivationIdentities {
            AcceptedInputTurnActivationIdentities::new(
                self.origin_entry().id(),
                self.starting_frontier().id(),
                initial_attempt,
            )
        }
    }

    #[derive(Clone)]
    struct ActiveReconstitutionFacts {
        session: Session,
        turns: Vec<AcceptedInputTurnSchedulingRecord>,
        semantic_entries: Vec<SemanticTranscriptEntryReconstitutionInput>,
        snapshots: Vec<ResolvedContextFrontierReconstitutionInput>,
        acceptance_tail: Option<SessionAcceptanceTailReconstitutionInput>,
    }

    impl ActiveReconstitutionFacts {
        fn matching(session: &Session, active: OriginFixture) -> Self {
            let origin_entry = semantic_entry(30);
            let starting_frontier = frontier(40);
            Self {
                session: session.clone(),
                turns: vec![active.record(
                    session,
                    AcceptedInputTurnSchedulingRecordState::Active {
                        starting_lineage: AcceptedInputStartingLineage::FirstInSession,
                        starting_frontier: starting_frontier.id(),
                        phase: ActiveTurnSchedulingReconstitutionInput::prepared(
                            active.turn(),
                            matching_active_attempt(),
                        ),
                    },
                )],
                semantic_entries: vec![active.entry(session, origin_entry)],
                snapshots: vec![starting_frontier.snapshot(session, &[origin_entry])],
                acceptance_tail: Some(active.active_tail(session)),
            }
        }

        /// Replaces only the behavior-relevant stored active phase while
        /// retaining every matching identity, lineage, frontier, origin,
        /// configuration, and acceptance-tail fact.
        fn replace_active_phase(&mut self, replacement: ActiveTurnSchedulingReconstitutionInput) {
            let AcceptedInputTurnSchedulingRecordState::Active { phase, .. } =
                &mut self.turns[0].state
            else {
                panic!("matching active facts retain an active scheduling record");
            };
            *phase = replacement;
        }

        fn input(self) -> AcceptedInputSchedulingReconstitutionInput {
            AcceptedInputSchedulingReconstitutionInput::new(
                self.session,
                self.turns,
                self.semantic_entries,
                self.snapshots,
                self.acceptance_tail,
            )
        }
    }

    fn active_input(
        session: &Session,
        active: OriginFixture,
        acceptance_tail: Option<SessionAcceptanceTailReconstitutionInput>,
    ) -> AcceptedInputSchedulingReconstitutionInput {
        ActiveReconstitutionFacts {
            acceptance_tail,
            ..ActiveReconstitutionFacts::matching(session, active)
        }
        .input()
    }

    #[derive(Clone, Copy)]
    struct PostAnchorOrigins {
        active: OriginFixture,
        queued: OriginFixture,
    }

    fn active_input_with_post_anchor_origin(
        session: &Session,
        origins: PostAnchorOrigins,
        delivery: DeliveryRequest,
    ) -> AcceptedInputSchedulingReconstitutionInput {
        let mut facts = ActiveReconstitutionFacts::matching(session, origins.active);
        let tail = facts
            .acceptance_tail
            .as_mut()
            .expect("matching active facts include the acceptance tail");
        tail.observed_last_position = origins.queued.position();
        tail.entries
            .push(SessionAcceptanceTailEntryReconstitutionInput::new(
                session.id(),
                AcceptedInputLifecycle::new(
                    origins.queued.accepted_input(),
                    AcceptedInputDisposition::OriginOf(origins.queued.turn()),
                ),
                origins.queued.position(),
                delivery,
            ));
        facts.turns.push(origins.queued.record_with(
            session,
            OriginRecordFacts {
                order: origins.queued.ordinary_order(),
                delivery,
                state: AcceptedInputTurnSchedulingRecordState::Queued,
            },
        ));
        facts.input()
    }

    #[derive(Clone, Copy)]
    struct FailedPredecessorPostAnchorOrigins {
        predecessor: OriginFixture,
        active: OriginFixture,
        queued: OriginFixture,
    }

    fn active_input_after_failed_predecessor_with_post_anchor_origin(
        session: &Session,
        origins: FailedPredecessorPostAnchorOrigins,
        delivery: DeliveryRequest,
    ) -> AcceptedInputSchedulingReconstitutionInput {
        let predecessor_origin_entry = semantic_entry(29);
        let predecessor_failure_entry = semantic_entry(30);
        let active_origin_entry = semantic_entry(31);
        let predecessor_starting_frontier = frontier(39);
        let predecessor_terminal_frontier = frontier(40);
        let active_starting_frontier = frontier(41);
        let predecessor_record = origins.predecessor.record(
            session,
            AcceptedInputTurnSchedulingRecordState::TerminalFailed {
                starting_lineage: AcceptedInputStartingLineage::FirstInSession,
                starting_frontier: predecessor_starting_frontier.id(),
                terminal_frontier: predecessor_terminal_frontier.id(),
            },
        );
        let active_delivery = DeliveryRequest::AfterCurrentTurn {
            expected_active_turn: origins.predecessor.turn(),
            configuration: PerInputConfigurationChoices::new(
                SessionConfigurationDefaultsVersion::first(),
                ModelSelectionOverride::UseSessionDefault,
            ),
        };
        let active_record = origins.active.record_with(
            session,
            OriginRecordFacts {
                order: origins.active.ordinary_order(),
                delivery: active_delivery,
                state: AcceptedInputTurnSchedulingRecordState::Active {
                    starting_lineage: AcceptedInputStartingLineage::After {
                        immediate_predecessor: origins.predecessor.turn(),
                    },
                    starting_frontier: active_starting_frontier.id(),
                    phase: ActiveTurnSchedulingReconstitutionInput::prepared(
                        origins.active.turn(),
                        turn_attempt_id(50),
                    ),
                },
            },
        );
        let queued_record = origins.queued.record_with(
            session,
            OriginRecordFacts {
                order: origins.queued.ordinary_order(),
                delivery,
                state: AcceptedInputTurnSchedulingRecordState::Queued,
            },
        );
        let tail = SessionAcceptanceTailReconstitutionInput::new(
            session.id(),
            origins.active.accepted_input(),
            origins.queued.position(),
            vec![
                SessionAcceptanceTailEntryReconstitutionInput::new(
                    session.id(),
                    AcceptedInputLifecycle::new(
                        origins.active.accepted_input(),
                        AcceptedInputDisposition::OriginOf(origins.active.turn()),
                    ),
                    origins.active.position(),
                    active_delivery,
                ),
                SessionAcceptanceTailEntryReconstitutionInput::new(
                    session.id(),
                    AcceptedInputLifecycle::new(
                        origins.queued.accepted_input(),
                        AcceptedInputDisposition::OriginOf(origins.queued.turn()),
                    ),
                    origins.queued.position(),
                    delivery,
                ),
            ],
        );
        AcceptedInputSchedulingReconstitutionInput::new(
            session.clone(),
            vec![predecessor_record, active_record, queued_record],
            vec![
                origins.predecessor.entry(session, predecessor_origin_entry),
                predecessor_failure_entry.failed_turn(session, origins.predecessor),
                origins.active.entry(session, active_origin_entry),
            ],
            vec![
                predecessor_starting_frontier.snapshot(session, &[predecessor_origin_entry]),
                predecessor_terminal_frontier.snapshot(
                    session,
                    &[predecessor_origin_entry, predecessor_failure_entry],
                ),
                active_starting_frontier.snapshot(
                    session,
                    &[
                        predecessor_origin_entry,
                        predecessor_failure_entry,
                        active_origin_entry,
                    ],
                ),
            ],
            Some(tail),
        )
    }

    #[derive(Debug)]
    #[allow(
        dead_code,
        reason = "the table renderer reads every field through the Debug derive"
    )]
    struct ReconstitutionFailureRow {
        perturbed_stored_fact: &'static str,
        failure: String,
    }

    /// Asserts one named perturbation rejects while retaining the complete
    /// unchanged input, then returns its precise failure.
    #[track_caller]
    fn assert_reconstitution_rejects_unchanged(
        facts: ActiveReconstitutionFacts,
    ) -> AcceptedInputSchedulingReconstitutionFailure {
        let input = facts.input();
        let error = input
            .clone()
            .reconstitute()
            .expect_err("perturbed scheduling facts must fail closed");
        let failure = error.failure().clone();
        assert_eq!(error.input(), &input);
        let (returned, returned_failure) = error.into_parts();
        assert_eq!(returned, input);
        assert_eq!(returned_failure, failure);
        failure
    }

    /// S01 / INV-009 / INV-015: ancestry-free first eligibility fixes the
    /// origin-only frontier and enters Running with one Prepared attempt in
    /// the same sealed candidate.
    #[test]
    fn s01_first_eligibility_prepares_one_atomic_activation_candidate() {
        let session = current_session();
        let queued = accepted_origin(1);
        let activation = activation(1);
        let no_semantic_entries = Vec::new();
        let no_snapshots = Vec::new();
        let no_active_acceptance_tail = None;
        let input = AcceptedInputSchedulingReconstitutionInput::new(
            session.clone(),
            vec![queued.record(&session, AcceptedInputTurnSchedulingRecordState::Queued)],
            no_semantic_entries,
            no_snapshots,
            no_active_acceptance_tail,
        );

        let candidate = input
            .reconstitute()
            .expect("a complete queued projection is valid")
            .prepare_earliest_queued_activation(activation.identities())
            .expect("the sole queued turn is eligible with no active slot");

        assert_eq!(candidate.turn().turn(), queued.turn());
        assert_eq!(
            candidate.turn().accepted_input().id(),
            queued.accepted_input()
        );
        assert_eq!(
            candidate.origin_entry().payload(),
            InitialSemanticTranscriptEntryPayload::OriginAcceptedInput {
                accepted_input: queued.accepted_input(),
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
            vec![activation.origin_entry().reference(&session)]
        );
        assert!(matches!(
            candidate.turn().phase(),
            ActiveTurnPhase::Running { current_attempt }
                if current_attempt.id() == activation.initial_attempt()
                    && current_attempt.state() == &crate::CurrentTurnAttemptState::Prepared
        ));
    }

    /// S03 / INV-009: restart returns a queued scheduling projection with no
    /// manufactured start, and a cross-wired OriginOf fact fails closed.
    #[test]
    fn s03_checked_reconstitution_preserves_queued_state_and_exact_origin() {
        let session = current_session();
        let origin = accepted_origin(1);
        let queued = origin.record(&session, AcceptedInputTurnSchedulingRecordState::Queued);
        let no_semantic_entries = Vec::new();
        let no_snapshots = Vec::new();
        let no_active_acceptance_tail = None;
        let projection = AcceptedInputSchedulingReconstitutionInput::new(
            session.clone(),
            vec![queued.clone()],
            no_semantic_entries,
            no_snapshots,
            no_active_acceptance_tail,
        )
        .reconstitute()
        .expect("the complete queued record is valid");
        let reconstituted = projection
            .turn(origin.turn())
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
            queued.origin_delivery(),
            queued.origin_configuration().clone(),
            queued.state().clone(),
        );
        let no_semantic_entries = Vec::new();
        let no_snapshots = Vec::new();
        let no_active_acceptance_tail = None;
        let error = AcceptedInputSchedulingReconstitutionInput::new(
            session,
            vec![cross_wired],
            no_semantic_entries,
            no_snapshots,
            no_active_acceptance_tail,
        )
        .reconstitute()
        .expect_err("the exact OriginOf(turn) correlation is required");
        assert_eq!(
            error.failure(),
            &AcceptedInputSchedulingReconstitutionFailure::AcceptedInputOriginMismatch {
                turn: origin.turn(),
            }
        );
    }

    /// S03 / INV-009: an admitted active restart record owns its exact
    /// Prepared attempt, reconstructs Running, and makes that identity
    /// unavailable to a second activation candidate.
    #[test]
    fn s03_active_reconstitution_requires_and_exposes_exact_prepared_attempt() {
        let session = current_session();
        let active_origin = accepted_origin(1);
        let stored_attempt = matching_active_attempt();
        let facts = ActiveReconstitutionFacts::matching(&session, active_origin);
        let projection = facts
            .input()
            .reconstitute()
            .expect("the active turn has its exact prepared attempt");
        let active = projection
            .active_turn()
            .expect("the reconstructed turn owns the active slot");
        assert!(matches!(
            active.active_phase(),
            Some(ActiveTurnPhase::Running { current_attempt })
                if current_attempt.id() == stored_attempt
                    && current_attempt.state() == &CurrentTurnAttemptState::Prepared
        ));

        let colliding_activation = activation(1);
        let collision = projection
            .clone()
            .prepare_earliest_queued_activation(
                colliding_activation.identities_with_attempt(stored_attempt),
            )
            .expect_err("a current attempt identity cannot be proposed again");
        assert_eq!(
            collision.failure(),
            AcceptedInputEligibilityFailure::InitialAttemptIdentityAlreadyExists
        );
        let occupied_activation = activation(2);
        let occupied = projection
            .prepare_earliest_queued_activation(occupied_activation.identities())
            .expect_err("an active slot blocks every queued activation");
        assert_eq!(
            occupied.failure(),
            AcceptedInputEligibilityFailure::ActiveTurnPresent {
                turn: active_origin.turn(),
            }
        );
    }

    /// S03 / INV-009 / ADR-0041: inert prepared facts become a canonical
    /// attempt only inside the validated owner projection.
    #[test]
    fn active_reconstitution_derives_prepared_attempt_after_validation() {
        let session = current_session();
        let active = accepted_origin(1);
        let expected_attempt = matching_active_attempt();
        let facts = ActiveReconstitutionFacts::matching(&session, active);
        let projection = facts
            .input()
            .reconstitute()
            .expect("the complete owner projection derives the prepared attempt");
        let phase = projection
            .active_turn()
            .expect("the turn owns the active slot")
            .active_phase();
        assert!(matches!(
            phase,
            Some(ActiveTurnPhase::Running { current_attempt })
                if current_attempt.id() == expected_attempt
                    && current_attempt.state() == &CurrentTurnAttemptState::Prepared
        ));
    }

    /// S03 / INV-009 / ADR-0041: inert running facts traverse the sealed
    /// prepared-to-running transition only inside the validated owner
    /// projection.
    #[test]
    fn active_reconstitution_derives_running_attempt_after_validation() {
        let session = current_session();
        let active = accepted_origin(1);
        let expected_attempt = turn_attempt_id(51);
        let mut facts = ActiveReconstitutionFacts::matching(&session, active);
        facts.replace_active_phase(ActiveTurnSchedulingReconstitutionInput::running(
            active.turn(),
            expected_attempt,
        ));
        let projection = facts
            .input()
            .reconstitute()
            .expect("the complete owner projection derives the running attempt");
        let phase = projection
            .active_turn()
            .expect("the turn owns the active slot")
            .active_phase();
        assert!(matches!(
            phase,
            Some(ActiveTurnPhase::Running { current_attempt })
                if current_attempt.id() == expected_attempt
                    && current_attempt.state() == &CurrentTurnAttemptState::Running
        ));
    }

    /// S03 / S08 / INV-009 / INV-016 / ADR-0041: an active
    /// scheduling projection requires the exact session-scoped interval
    /// anchored at its origin; a missing, cross-session, or cross-wired
    /// interval fails closed.
    #[test]
    fn active_reconstitution_requires_exact_session_acceptance_tail_identity() {
        let session = current_session();
        let active = accepted_origin(1);

        let missing = assert_reconstitution_rejects_unchanged(ActiveReconstitutionFacts {
            acceptance_tail: None,
            ..ActiveReconstitutionFacts::matching(&session, active)
        });
        assert_eq!(
            missing,
            AcceptedInputSchedulingReconstitutionFailure::MissingActiveAcceptanceTail {
                turn: active.turn(),
            }
        );

        let other_session = session_id(2);
        let mut wrong_session_facts = ActiveReconstitutionFacts::matching(&session, active);
        wrong_session_facts
            .acceptance_tail
            .as_mut()
            .expect("matching facts include the acceptance tail")
            .session = other_session;
        let wrong_session = assert_reconstitution_rejects_unchanged(wrong_session_facts);
        assert_eq!(
            wrong_session,
            AcceptedInputSchedulingReconstitutionFailure::AcceptanceTailSessionMismatch {
                expected: session.id(),
                actual: other_session,
            }
        );

        let other_anchor = accepted_input_id(99);
        let mut wrong_anchor_facts = ActiveReconstitutionFacts::matching(&session, active);
        wrong_anchor_facts
            .acceptance_tail
            .as_mut()
            .expect("matching facts include the acceptance tail")
            .anchor = other_anchor;
        let wrong_anchor = assert_reconstitution_rejects_unchanged(wrong_anchor_facts);
        assert_eq!(
            wrong_anchor,
            AcceptedInputSchedulingReconstitutionFailure::AcceptanceTailAnchorMismatch {
                turn: active.turn(),
                expected: active.accepted_input(),
                actual: other_anchor,
            }
        );

        expect![[r#"
            ┌──────────────────────────┬─────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┐
            │ perturbed_stored_fact    │ failure                                                                                                                                                                                                             │
            ├──────────────────────────┼─────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┤
            │ active tail omitted      │ MissingActiveAcceptanceTail { turn: TurnId(00000000-0000-0000-ffff-fffffffffffe) }                                                                                                                                  │
            │ tail session cross-wired │ AcceptanceTailSessionMismatch { expected: SessionId(00000000-0000-0000-0000-000000000001), actual: SessionId(00000000-0000-0000-0000-000000000002) }                                                                │
            │ tail anchor cross-wired  │ AcceptanceTailAnchorMismatch { turn: TurnId(00000000-0000-0000-ffff-fffffffffffe), expected: AcceptedInputId(00000000-0000-0000-7fff-fffffffffffe), actual: AcceptedInputId(00000000-0000-0000-0000-000000000063) } │
            └──────────────────────────┴─────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┘
        "#]]
        .assert_eq(&table([
            ReconstitutionFailureRow {
                perturbed_stored_fact: "active tail omitted",
                failure: format!("{missing:?}"),
            },
            ReconstitutionFailureRow {
                perturbed_stored_fact: "tail session cross-wired",
                failure: format!("{wrong_session:?}"),
            },
            ReconstitutionFailureRow {
                perturbed_stored_fact: "tail anchor cross-wired",
                failure: format!("{wrong_anchor:?}"),
            },
        ]));
    }

    /// S03 / S08 / INV-016 / ADR-0041: every position
    /// from the active origin through the observed session tail is present
    /// exactly once and every pending-steering disposition remains bound to
    /// that active turn.
    #[test]
    fn active_reconstitution_rejects_gapped_or_misbound_acceptance_tail() {
        let session = current_session();
        let active = accepted_origin(1);
        let second = accepted_origin(2);
        let third = accepted_origin(3);

        let mut gapped_facts = ActiveReconstitutionFacts::matching(&session, active);
        let gapped_tail = gapped_facts
            .acceptance_tail
            .as_mut()
            .expect("matching facts include the acceptance tail");
        gapped_tail.observed_last_position = third.position();
        gapped_tail
            .entries
            .push(SessionAcceptanceTailEntryReconstitutionInput::new(
                session.id(),
                AcceptedInputLifecycle::new(
                    second.accepted_input(),
                    AcceptedInputDisposition::PendingSteering {
                        binding: crate::SteeringBinding::new(active.turn()),
                    },
                ),
                third.position(),
                DeliveryRequest::NextSafePoint {
                    expected_active_turn: active.turn(),
                },
            ));
        let gapped = assert_reconstitution_rejects_unchanged(gapped_facts);
        assert_eq!(
            gapped,
            AcceptedInputSchedulingReconstitutionFailure::AcceptanceTailPositionMismatch {
                accepted_input: second.accepted_input(),
                expected: second.position(),
                actual: third.position(),
            }
        );

        let other_turn = turn_id(99);
        let mut misbound_facts = ActiveReconstitutionFacts::matching(&session, active);
        let misbound_tail = misbound_facts
            .acceptance_tail
            .as_mut()
            .expect("matching facts include the acceptance tail");
        misbound_tail.observed_last_position = second.position();
        misbound_tail
            .entries
            .push(SessionAcceptanceTailEntryReconstitutionInput::new(
                session.id(),
                AcceptedInputLifecycle::new(
                    second.accepted_input(),
                    AcceptedInputDisposition::PendingSteering {
                        binding: crate::SteeringBinding::new(other_turn),
                    },
                ),
                second.position(),
                DeliveryRequest::NextSafePoint {
                    expected_active_turn: other_turn,
                },
            ));
        let misbound = assert_reconstitution_rejects_unchanged(misbound_facts);
        assert_eq!(
            misbound,
            AcceptedInputSchedulingReconstitutionFailure::AcceptanceTailDispositionMismatch {
                accepted_input: second.accepted_input(),
            }
        );

        let after_active_delivery = DeliveryRequest::AfterCurrentTurn {
            expected_active_turn: active.turn(),
            configuration: PerInputConfigurationChoices::new(
                SessionConfigurationDefaultsVersion::first(),
                ModelSelectionOverride::UseSessionDefault,
            ),
        };
        let mut cross_wired_facts = ActiveReconstitutionFacts::matching(&session, active);
        let cross_wired_tail = cross_wired_facts
            .acceptance_tail
            .as_mut()
            .expect("matching facts include the acceptance tail");
        cross_wired_tail.observed_last_position = third.position();
        cross_wired_tail
            .entries
            .push(SessionAcceptanceTailEntryReconstitutionInput::new(
                session.id(),
                AcceptedInputLifecycle::new(
                    second.accepted_input(),
                    AcceptedInputDisposition::OriginOf(second.turn()),
                ),
                second.position(),
                after_active_delivery,
            ));
        cross_wired_tail
            .entries
            .push(SessionAcceptanceTailEntryReconstitutionInput::new(
                session.id(),
                AcceptedInputLifecycle::new(
                    third.accepted_input(),
                    AcceptedInputDisposition::PendingSteering {
                        binding: crate::SteeringBinding::new(active.turn()),
                    },
                ),
                third.position(),
                DeliveryRequest::NextSafePoint {
                    expected_active_turn: active.turn(),
                },
            ));
        cross_wired_facts.turns.push(second.record_with(
            &session,
            OriginRecordFacts {
                order: AcceptedInputQueueOrder::ordinary(third.position()),
                delivery: after_active_delivery,
                state: AcceptedInputTurnSchedulingRecordState::Queued,
            },
        ));
        let cross_wired = assert_reconstitution_rejects_unchanged(cross_wired_facts);
        assert_eq!(
            cross_wired,
            AcceptedInputSchedulingReconstitutionFailure::AcceptanceTailDispositionMismatch {
                accepted_input: second.accepted_input(),
            }
        );

        expect![[r#"
            ┌────────────────────────────────────┬──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┐
            │ perturbed_stored_fact              │ failure                                                                                                                                                                      │
            ├────────────────────────────────────┼──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┤
            │ interior position omitted          │ AcceptanceTailPositionMismatch { accepted_input: AcceptedInputId(00000000-0000-0000-7fff-fffffffffffd), expected: SessionInputPosition(2), actual: SessionInputPosition(3) } │
            │ pending steering owner cross-wired │ AcceptanceTailDispositionMismatch { accepted_input: AcceptedInputId(00000000-0000-0000-7fff-fffffffffffd) }                                                                  │
            │ origin position cross-wired        │ AcceptanceTailDispositionMismatch { accepted_input: AcceptedInputId(00000000-0000-0000-7fff-fffffffffffd) }                                                                  │
            └────────────────────────────────────┴──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┘
        "#]]
        .assert_eq(&table([
            ReconstitutionFailureRow {
                perturbed_stored_fact: "interior position omitted",
                failure: format!("{gapped:?}"),
            },
            ReconstitutionFailureRow {
                perturbed_stored_fact: "pending steering owner cross-wired",
                failure: format!("{misbound:?}"),
            },
            ReconstitutionFailureRow {
                perturbed_stored_fact: "origin position cross-wired",
                failure: format!("{cross_wired:?}"),
            },
        ]));
    }

    /// S03 / S09 / INV-009 / INV-016 / ADR-0041: a scheduler-gap start remains
    /// a valid ordinary origin after an earlier queued turn becomes active.
    #[test]
    fn active_reconstitution_preserves_post_anchor_scheduler_gap_start() {
        let session = current_session();
        let origins = FailedPredecessorPostAnchorOrigins {
            predecessor: accepted_origin(1),
            active: accepted_origin(2),
            queued: accepted_origin(3),
        };
        active_input_after_failed_predecessor_with_post_anchor_origin(
            &session,
            origins,
            DeliveryRequest::StartWhenNoActiveTurn {
                configuration: PerInputConfigurationChoices::new(
                    SessionConfigurationDefaultsVersion::first(),
                    ModelSelectionOverride::UseSessionDefault,
                ),
            },
        )
        .reconstitute()
        .expect("the later origin was accepted during a valid scheduler gap");
    }

    /// S03 / S09 / INV-009 / INV-016 / ADR-0041: an ordinary queued origin
    /// retains the historical active target named at acceptance.
    #[test]
    fn active_reconstitution_preserves_post_anchor_historical_target() {
        let session = current_session();
        let origins = FailedPredecessorPostAnchorOrigins {
            predecessor: accepted_origin(1),
            active: accepted_origin(2),
            queued: accepted_origin(3),
        };
        active_input_after_failed_predecessor_with_post_anchor_origin(
            &session,
            origins,
            DeliveryRequest::AfterCurrentTurn {
                expected_active_turn: origins.predecessor.turn(),
                configuration: PerInputConfigurationChoices::new(
                    SessionConfigurationDefaultsVersion::first(),
                    ModelSelectionOverride::UseSessionDefault,
                ),
            },
        )
        .reconstitute()
        .expect("the later origin retains its exact previously active target");
    }

    /// S03 / S09 / INV-009 / INV-016 / ADR-0041: after-current delivery must
    /// name an earlier nonqueued target in the complete turn inventory.
    #[test]
    fn active_reconstitution_rejects_missing_historical_delivery_target() {
        let session = current_session();
        let origins = PostAnchorOrigins {
            active: accepted_origin(1),
            queued: accepted_origin(2),
        };
        let missing_target_turn = turn_id(99);
        let missing_target = active_input_with_post_anchor_origin(
            &session,
            origins,
            DeliveryRequest::AfterCurrentTurn {
                expected_active_turn: missing_target_turn,
                configuration: PerInputConfigurationChoices::new(
                    SessionConfigurationDefaultsVersion::first(),
                    ModelSelectionOverride::UseSessionDefault,
                ),
            },
        )
        .reconstitute()
        .expect_err("after-current delivery requires its historical target record");
        assert_eq!(
            missing_target.failure(),
            &AcceptedInputSchedulingReconstitutionFailure::OriginDeliveryMismatch {
                turn: origins.queued.turn(),
            }
        );
    }

    /// S03 / S07 / INV-009 / INV-016 / ADR-0041: an interrupt delivery must
    /// agree with the origin record's durable interrupt-priority relation.
    #[test]
    fn active_reconstitution_rejects_delivery_priority_mismatch() {
        let session = current_session();
        let origins = PostAnchorOrigins {
            active: accepted_origin(1),
            queued: accepted_origin(2),
        };
        let wrong_priority = active_input_with_post_anchor_origin(
            &session,
            origins,
            DeliveryRequest::Interrupt {
                expected_active_turn: origins.active.turn(),
                configuration: PerInputConfigurationChoices::new(
                    SessionConfigurationDefaultsVersion::first(),
                    ModelSelectionOverride::UseSessionDefault,
                ),
            },
        )
        .reconstitute()
        .expect_err("interrupt delivery cannot carry ordinary queue priority");
        assert_eq!(
            wrong_priority.failure(),
            &AcceptedInputSchedulingReconstitutionFailure::OriginDeliveryMismatch {
                turn: origins.queued.turn(),
            }
        );
    }

    /// S01 / INV-009 / INV-016 / ADR-0041: origin delivery and queue facts
    /// are validated even when no active turn requires an acceptance tail.
    #[test]
    fn s01_inv009_inv016_queued_reconstitution_rejects_delivery_order_mismatch() {
        let session = current_session();
        let queued = accepted_origin(1);
        let no_semantic_entries = Vec::new();
        let no_snapshots = Vec::new();
        let no_active_acceptance_tail = None;
        let input = AcceptedInputSchedulingReconstitutionInput::new(
            session.clone(),
            vec![queued.record_with(
                &session,
                OriginRecordFacts {
                    order: queued.ordinary_order(),
                    delivery: DeliveryRequest::NextSafePoint {
                        expected_active_turn: turn_id(99),
                    },
                    state: AcceptedInputTurnSchedulingRecordState::Queued,
                },
            )],
            no_semantic_entries,
            no_snapshots,
            no_active_acceptance_tail,
        );

        let error = input
            .reconstitute()
            .expect_err("steering-only delivery cannot reconstruct queued turn work");
        assert_eq!(
            error.failure(),
            &AcceptedInputSchedulingReconstitutionFailure::OriginDeliveryMismatch {
                turn: queued.turn(),
            }
        );
    }

    /// S01 / INV-008 / INV-009 / INV-016 / ADR-0041: a configured origin's
    /// accepted defaults version must equal its frozen provenance version.
    #[test]
    fn s01_inv008_inv009_inv016_queued_origin_rejects_defaults_version_mismatch() {
        let session = current_session();
        let queued = accepted_origin(1);
        let mismatched_version = SessionConfigurationDefaultsVersion::try_from_u64(2)
            .expect("the mismatched test version is positive");
        let no_semantic_entries = Vec::new();
        let no_snapshots = Vec::new();
        let no_active_acceptance_tail = None;
        let input = AcceptedInputSchedulingReconstitutionInput::new(
            session.clone(),
            vec![queued.record_with(
                &session,
                OriginRecordFacts {
                    order: queued.ordinary_order(),
                    delivery: DeliveryRequest::StartWhenNoActiveTurn {
                        configuration: PerInputConfigurationChoices::new(
                            mismatched_version,
                            ModelSelectionOverride::UseSessionDefault,
                        ),
                    },
                    state: AcceptedInputTurnSchedulingRecordState::Queued,
                },
            )],
            no_semantic_entries,
            no_snapshots,
            no_active_acceptance_tail,
        );

        let error = input
            .reconstitute()
            .expect_err("accepted delivery and frozen provenance versions must agree");
        assert_eq!(
            error.failure(),
            &AcceptedInputSchedulingReconstitutionFailure::OriginDeliveryMismatch {
                turn: queued.turn(),
            }
        );
    }

    /// S01 / INV-008 / INV-009 / INV-016 / ADR-0041: an explicit accepted
    /// model request must equal the request retained by frozen provenance.
    #[test]
    fn s01_inv008_inv009_inv016_queued_origin_rejects_explicit_request_mismatch() {
        let session = current_session();
        let queued = accepted_origin(1);
        let requested = ModelSelectionRequest::Direct(direct(99));
        let no_semantic_entries = Vec::new();
        let no_snapshots = Vec::new();
        let no_active_acceptance_tail = None;
        let input = AcceptedInputSchedulingReconstitutionInput::new(
            session.clone(),
            vec![queued.record_with(
                &session,
                OriginRecordFacts {
                    order: queued.ordinary_order(),
                    delivery: DeliveryRequest::StartWhenNoActiveTurn {
                        configuration: PerInputConfigurationChoices::new(
                            SessionConfigurationDefaultsVersion::first(),
                            ModelSelectionOverride::ReplaceWith(requested),
                        ),
                    },
                    state: AcceptedInputTurnSchedulingRecordState::Queued,
                },
            )],
            no_semantic_entries,
            no_snapshots,
            no_active_acceptance_tail,
        );

        let error = input
            .reconstitute()
            .expect_err("explicit delivery request and frozen provenance must agree");
        assert_eq!(
            error.failure(),
            &AcceptedInputSchedulingReconstitutionFailure::OriginDeliveryMismatch {
                turn: queued.turn(),
            }
        );
    }

    /// S03 / INV-008 / INV-016 / ADR-0041: the tail repeats the exact
    /// immutable versioned delivery stored for its origin rather than
    /// supplying an independently plausible configuration choice.
    #[test]
    fn active_reconstitution_rejects_origin_delivery_configuration_mismatch() {
        let session = current_session();
        let active = accepted_origin(1);
        let mut facts = ActiveReconstitutionFacts::matching(&session, active);
        facts
            .acceptance_tail
            .as_mut()
            .expect("matching facts include the active tail")
            .entries[0]
            .delivery = DeliveryRequest::StartWhenNoActiveTurn {
            configuration: PerInputConfigurationChoices::new(
                SessionConfigurationDefaultsVersion::try_from_u64(2)
                    .expect("the mismatched test version is positive"),
                ModelSelectionOverride::UseSessionDefault,
            ),
        };

        let error = assert_reconstitution_rejects_unchanged(facts);
        assert_eq!(
            error,
            AcceptedInputSchedulingReconstitutionFailure::AcceptanceTailDispositionMismatch {
                accepted_input: active.accepted_input(),
            }
        );
    }

    /// S03 / S07 / INV-001 / INV-009 / ADR-0041: an accepted interrupt
    /// against the current owner prevents evidence-free phase reconstruction.
    #[test]
    fn active_reconstitution_rejects_interrupt_evidence_for_evidence_free_phase() {
        let session = current_session();
        let origins = PostAnchorOrigins {
            active: accepted_origin(1),
            queued: accepted_origin(2),
        };
        let delivery = DeliveryRequest::Interrupt {
            expected_active_turn: origins.active.turn(),
            configuration: PerInputConfigurationChoices::new(
                SessionConfigurationDefaultsVersion::first(),
                ModelSelectionOverride::UseSessionDefault,
            ),
        };
        let mut input = active_input_with_post_anchor_origin(&session, origins, delivery);
        input.turns[1] = origins.queued.record_with(
            &session,
            OriginRecordFacts {
                order: AcceptedInputQueueOrder::interrupt_immediately_after(
                    origins.queued.position(),
                    origins.active.turn(),
                ),
                delivery,
                state: AcceptedInputTurnSchedulingRecordState::Queued,
            },
        );

        let error = input
            .reconstitute()
            .expect_err("applied interrupt evidence requires a proof-bearing phase projection");
        assert_eq!(
            error.failure(),
            &AcceptedInputSchedulingReconstitutionFailure::ActivePhaseEvidenceMismatch {
                turn: origins.active.turn(),
                accepted_input: origins.queued.accepted_input(),
            }
        );
    }

    /// S03 / S08 / INV-009 / INV-016 / ADR-0041: one accepted input cannot
    /// be both pending steering and a turn origin in the scheduling inventory.
    #[test]
    fn active_reconstitution_rejects_pending_identity_that_is_also_an_origin() {
        let session = current_session();
        let active = accepted_origin(1);
        let pending = accepted_origin(2);
        let mut tail = active.active_tail(&session);
        tail.observed_last_position = pending.position();
        tail.entries
            .push(SessionAcceptanceTailEntryReconstitutionInput::new(
                session.id(),
                AcceptedInputLifecycle::new(
                    pending.accepted_input(),
                    AcceptedInputDisposition::PendingSteering {
                        binding: crate::SteeringBinding::new(active.turn()),
                    },
                ),
                pending.position(),
                DeliveryRequest::NextSafePoint {
                    expected_active_turn: active.turn(),
                },
            ));

        active_input(&session, active, Some(tail.clone()))
            .reconstitute()
            .expect("pending steering remains distinct from every origin");

        let mut aliased = active_input(&session, active, Some(tail));
        aliased
            .turns
            .push(pending.record(&session, AcceptedInputTurnSchedulingRecordState::Queued));
        let aliased = aliased
            .reconstitute()
            .expect_err("pending steering cannot reuse a turn-origin identity");
        assert_eq!(
            aliased.failure(),
            &AcceptedInputSchedulingReconstitutionFailure::AcceptanceTailDispositionMismatch {
                accepted_input: pending.accepted_input(),
            }
        );
    }

    /// S03 / S08 / INV-007 / INV-016 / ADR-0041: a pending tail entry cannot
    /// replace a different origin that owns the same acceptance position.
    #[test]
    fn active_reconstitution_rejects_pending_position_owned_by_an_origin() {
        let session = current_session();
        let active = accepted_origin(1);
        let origin = accepted_origin(2);
        let pending = accepted_origin(3);
        let mut tail = active.active_tail(&session);
        tail.observed_last_position = origin.position();
        tail.entries
            .push(SessionAcceptanceTailEntryReconstitutionInput::new(
                session.id(),
                AcceptedInputLifecycle::new(
                    pending.accepted_input(),
                    AcceptedInputDisposition::PendingSteering {
                        binding: crate::SteeringBinding::new(active.turn()),
                    },
                ),
                origin.position(),
                DeliveryRequest::NextSafePoint {
                    expected_active_turn: active.turn(),
                },
            ));
        let mut input = active_input(&session, active, Some(tail));
        input
            .turns
            .push(origin.record(&session, AcceptedInputTurnSchedulingRecordState::Queued));

        let error = input
            .reconstitute()
            .expect_err("the complete tail cannot replace an origin at the same position");
        assert_eq!(
            error.failure(),
            &AcceptedInputSchedulingReconstitutionFailure::AcceptanceTailDispositionMismatch {
                accepted_input: pending.accepted_input(),
            }
        );
    }

    /// S03 / INV-016 / ADR-0041: the last represented position must equal
    /// the authoritative session tail observed by the same read.
    #[test]
    fn active_reconstitution_rejects_incomplete_claimed_acceptance_tail() {
        let session = current_session();
        let active = accepted_origin(1);
        let next = accepted_origin(2);
        let mut incomplete = active.active_tail(&session);
        incomplete.observed_last_position = next.position();
        let incomplete = active_input(&session, active, Some(incomplete))
            .reconstitute()
            .expect_err("the represented interval must reach the claimed session tail");
        assert_eq!(
            incomplete.failure(),
            &AcceptedInputSchedulingReconstitutionFailure::AcceptanceTailLastPositionMismatch {
                expected: next.position(),
                actual: Some(active.position()),
            }
        );
    }

    /// S03 / INV-009 / INV-016 / ADR-0041: the claimed session observation
    /// cannot end before a later origin supplied by the same scheduling read.
    #[test]
    fn s03_inv009_inv016_active_tail_reaches_every_known_origin() {
        let session = current_session();
        let origins = PostAnchorOrigins {
            active: accepted_origin(1),
            queued: accepted_origin(2),
        };
        let mut input = active_input_with_post_anchor_origin(
            &session,
            origins,
            DeliveryRequest::StartWhenNoActiveTurn {
                configuration: PerInputConfigurationChoices::new(
                    SessionConfigurationDefaultsVersion::first(),
                    ModelSelectionOverride::UseSessionDefault,
                ),
            },
        );
        let tail = input
            .active_acceptance_tail
            .as_mut()
            .expect("the helper supplies an active tail");
        tail.observed_last_position = origins.active.position();
        tail.entries.truncate(1);

        let error = input
            .reconstitute()
            .expect_err("a known later origin disproves the claimed tail observation");
        assert_eq!(
            error.failure(),
            &AcceptedInputSchedulingReconstitutionFailure::AcceptanceTailLastPositionMismatch {
                expected: origins.active.position(),
                actual: Some(origins.queued.position()),
            }
        );
    }

    /// S03 / INV-009: a current attempt owned by another turn cannot
    /// reconstruct an active aggregate.
    #[test]
    fn s03_active_reconstitution_rejects_cross_wired_attempt_owner() {
        let session = current_session();
        let active = accepted_origin(1);
        let other_turn = turn_id(99);
        let attempt = matching_active_attempt();
        let mut facts = ActiveReconstitutionFacts::matching(&session, active);
        facts.replace_active_phase(ActiveTurnSchedulingReconstitutionInput::prepared(
            other_turn, attempt,
        ));
        let error = assert_reconstitution_rejects_unchanged(facts);
        assert_eq!(
            error,
            AcceptedInputSchedulingReconstitutionFailure::CurrentAttemptOwnershipMismatch {
                turn: active.turn(),
                attempt,
            }
        );
    }

    /// S03 / INV-009: eligibility derives the target from complete durable
    /// order and cannot be directed to skip earlier queued work.
    #[test]
    fn s03_eligibility_consumes_the_earliest_queued_origin() {
        let session = current_session();
        let later = accepted_origin(2);
        let earlier = accepted_origin(1);
        let later_record = later.record(&session, AcceptedInputTurnSchedulingRecordState::Queued);
        let earlier_record =
            earlier.record(&session, AcceptedInputTurnSchedulingRecordState::Queued);
        let no_semantic_entries = Vec::new();
        let no_snapshots = Vec::new();
        let activation = activation(1);
        let no_active_acceptance_tail = None;
        let candidate = AcceptedInputSchedulingReconstitutionInput::new(
            session,
            vec![later_record, earlier_record],
            no_semantic_entries,
            no_snapshots,
            no_active_acceptance_tail,
        )
        .reconstitute()
        .expect("the complete queue order is valid")
        .prepare_earliest_queued_activation(activation.identities())
        .expect("no active slot blocks the earliest queued work");

        assert_eq!(candidate.turn().turn(), earlier.turn());
        assert_eq!(
            candidate.turn().accepted_input().id(),
            earlier.accepted_input()
        );
    }

    /// S09 / INV-009 / INV-015: the earliest queued successor starts only
    /// after the exact immediately preceding failed turn and retains its
    /// complete origin-then-failure terminal prefix before appending its own
    /// origin.
    #[test]
    fn s09_successor_uses_exact_failed_predecessor_terminal_frontier() {
        let session = current_session();
        let predecessor = accepted_origin(1);
        let successor = accepted_origin(2);
        let predecessor_origin_entry = semantic_entry(30);
        let predecessor_failure_entry = semantic_entry(31);
        let predecessor_starting_frontier = frontier(40);
        let predecessor_terminal_frontier = frontier(41);
        let activation = activation(1);
        let no_active_acceptance_tail = None;
        let predecessor_record = predecessor.record(
            &session,
            AcceptedInputTurnSchedulingRecordState::TerminalFailed {
                starting_lineage: AcceptedInputStartingLineage::FirstInSession,
                starting_frontier: predecessor_starting_frontier.id(),
                terminal_frontier: predecessor_terminal_frontier.id(),
            },
        );
        let successor_record =
            successor.record(&session, AcceptedInputTurnSchedulingRecordState::Queued);
        let projection = AcceptedInputSchedulingReconstitutionInput::new(
            session.clone(),
            vec![successor_record, predecessor_record],
            vec![
                predecessor_failure_entry.failed_turn(&session, predecessor),
                predecessor.entry(&session, predecessor_origin_entry),
            ],
            vec![
                predecessor_terminal_frontier.snapshot(
                    &session,
                    &[predecessor_origin_entry, predecessor_failure_entry],
                ),
                predecessor_starting_frontier.snapshot(&session, &[predecessor_origin_entry]),
            ],
            no_active_acceptance_tail,
        )
        .reconstitute()
        .expect("the failed predecessor has a complete validated frontier");

        let candidate = projection
            .prepare_earliest_queued_activation(activation.identities())
            .expect("the successor is the earliest queued turn with no active slot");

        assert_eq!(candidate.turn().turn(), successor.turn());
        assert_eq!(
            candidate.start().lineage(),
            AcceptedInputStartingLineage::After {
                immediate_predecessor: predecessor.turn(),
            }
        );
        assert_eq!(
            candidate
                .starting_snapshot()
                .ordered_entries()
                .collect::<Vec<_>>(),
            vec![
                predecessor_origin_entry.reference(&session),
                predecessor_failure_entry.reference(&session),
                activation.origin_entry().reference(&session),
            ]
        );
    }

    /// S09 / INV-015: a predecessor snapshot that omits its required failed
    /// marker is not a terminal frontier and cannot authorize a successor.
    #[test]
    fn s09_incomplete_failed_terminal_frontier_fails_closed() {
        let session = current_session();
        let predecessor = accepted_origin(1);
        let origin_entry = semantic_entry(30);
        let failure_entry = semantic_entry(31);
        let starting_frontier = frontier(40);
        let terminal_frontier = frontier(41);
        let no_active_acceptance_tail = None;
        let error = AcceptedInputSchedulingReconstitutionInput::new(
            session.clone(),
            vec![predecessor.record(
                &session,
                AcceptedInputTurnSchedulingRecordState::TerminalFailed {
                    starting_lineage: AcceptedInputStartingLineage::FirstInSession,
                    starting_frontier: starting_frontier.id(),
                    terminal_frontier: terminal_frontier.id(),
                },
            )],
            vec![
                predecessor.entry(&session, origin_entry),
                failure_entry.failed_turn(&session, predecessor),
            ],
            vec![
                starting_frontier.snapshot(&session, &[origin_entry]),
                terminal_frontier.snapshot(&session, &[origin_entry]),
            ],
            no_active_acceptance_tail,
        )
        .reconstitute()
        .expect_err("the failed marker must follow the exact starting prefix");

        assert_eq!(
            error.failure(),
            &AcceptedInputSchedulingReconstitutionFailure::TerminalFrontierMismatch {
                turn: predecessor.turn(),
            }
        );
    }
}
