//! Turn scheduling failure for `docs/spec/turn-lifecycle-and-scheduling.md`.

use super::activated_turn::AcceptedInputTurnActivationIdentities;
use super::projection::AcceptedInputSchedulingProjection;
use super::reconstitution_input::AcceptedInputSchedulingReconstitutionInput;
use super::turn_failure::AcceptedInputTurnFailureIdentities;
use crate::{
    AcceptedInputId, AcceptedInputQueueOrderError, AcceptedInputStartingLineage, ContextFrontierId,
    SemanticTranscriptEntryId, SemanticTranscriptEntryRef, SessionId, SessionInputPosition,
    TurnAttemptId, TurnId,
};

/// Why complete stored facts cannot reconstruct the closed scheduling model.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AcceptedInputSchedulingReconstitutionFailure {
    /// This slice cannot resolve a first frontier from native session
    /// ancestry.
    UnsupportedSessionAncestry,
    /// An imported session omitted its complete checked seed projection.
    MissingImportedSession,
    /// A non-imported session supplied an imported seed projection.
    UnexpectedImportedSession,
    /// The supplied imported projection is not the exact current session.
    ImportedSessionMismatch,
    /// A semantic entry was supplied outside its purpose-specific producer.
    UnsupportedSemanticEntry {
        /// The affected entry.
        entry: SemanticTranscriptEntryId,
    },
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
    /// A supplied delegation-origin turn duplicates another fact or belongs to
    /// the accepted-input projection.
    DelegatedTurnFactMismatch {
        /// The affected delegation-origin turn.
        turn: TurnId,
    },
    /// A consumed-steering subject fact belongs to another session.
    ConsumedSteeringSessionMismatch {
        /// The cross-wired accepted input.
        accepted_input: AcceptedInputId,
    },
    /// The same consumed accepted input appeared more than once.
    DuplicateConsumedSteering {
        /// The duplicated accepted input.
        accepted_input: AcceptedInputId,
    },
    /// A steering semantic entry has no exact consumed lifecycle/source fact.
    SteeringSemanticEntryMismatch {
        /// The affected semantic entry.
        entry: SemanticTranscriptEntryId,
    },
    /// A consumed lifecycle/source fact has no exact semantic entry or call.
    ConsumedSteeringMismatch {
        /// The affected accepted input.
        accepted_input: AcceptedInputId,
    },
    /// Continuation-round evidence duplicates a call or names a call that
    /// consumed no steering.
    SteeringContinuationRoundMismatch {
        /// The affected call.
        call: crate::ModelCallId,
    },
    /// Gate-named continuation-round evidence duplicates a call or names a
    /// call no terminal or recovery gate proved against it.
    ContinuationRoundMismatch {
        /// The affected call.
        call: crate::ModelCallId,
    },
    /// A semantic entry names a model call absent from the purpose-specific
    /// complete call facts.
    SemanticEntryCallMissing {
        /// The affected semantic entry.
        entry: SemanticTranscriptEntryId,
        /// The absent producing call.
        call: crate::ModelCallId,
    },
    /// Assistant content names a call that did not complete successfully.
    SemanticEntryCallMismatch {
        /// The affected semantic entry.
        entry: SemanticTranscriptEntryId,
        /// The non-completing producing call.
        call: crate::ModelCallId,
    },
    /// The same model-call identity appeared more than once.
    DuplicateModelCall {
        /// The duplicated call.
        call: crate::ModelCallId,
    },
    /// One global model-call identity appeared in both ordinary and compaction facts.
    DuplicateModelCallIdentityAcrossKinds {
        /// The cross-kind identity collision.
        call: crate::ModelCallId,
    },
    /// The same turn-level pinned-target fact appeared more than once.
    DuplicatePinnedTarget {
        /// The turn whose target was duplicated.
        turn: TurnId,
    },
    /// A call has no independently stored turn-level pinned target.
    PinnedTargetMissing {
        /// The affected call.
        call: crate::ModelCallId,
    },
    /// A turn-level pinned target is unrelated to every supplied call.
    UnreferencedPinnedTarget {
        /// The unrelated turn.
        turn: TurnId,
    },
    /// A model call references a snapshot absent from this complete read.
    ModelCallSnapshotMissing {
        /// The affected call.
        call: crate::ModelCallId,
    },
    /// Stored model-call facts cannot reconstruct canonical call history.
    InvalidModelCall {
        /// The affected call.
        call: crate::ModelCallId,
    },
    /// A dedicated compaction call references an absent source snapshot.
    CompactionCallSnapshotMissing { call: crate::ModelCallId },
    /// The same dedicated compaction-call identity appeared more than once.
    DuplicateCompactionCall { call: crate::ModelCallId },
    /// Stored dedicated compaction-call facts are inconsistent.
    InvalidCompactionCall { call: crate::ModelCallId },
    /// A compaction references an absent source or result snapshot.
    CompactionSnapshotMissing {
        compaction: crate::ContextCompactionId,
    },
    /// A compaction's completed call or summary entry is absent.
    CompactionEvidenceMissing {
        compaction: crate::ContextCompactionId,
    },
    /// Stored compaction facts fail exact provenance reconstruction.
    InvalidCompaction {
        compaction: crate::ContextCompactionId,
    },
    /// The same compaction identity appeared more than once.
    DuplicateCompaction {
        compaction: crate::ContextCompactionId,
    },
    /// A summary or dedicated call is unrelated to every compaction record.
    UnreferencedCompactionEvidence { call: crate::ModelCallId },
    /// A predecessor link is absent, duplicated as a root, or not a prefix.
    InvalidCompactionChain {
        compaction: crate::ContextCompactionId,
    },
    /// A supplied model call is not the terminal call named by its turn.
    UnreferencedModelCall {
        /// The unrelated call.
        call: crate::ModelCallId,
    },
    /// A completed or refused turn names a model call absent from the complete
    /// terminal-call facts.
    TerminalModelCallMissing {
        /// The affected turn.
        turn: TurnId,
        /// The absent terminal call.
        call: crate::ModelCallId,
    },
    /// The named terminal call disagrees with its turn, selection, frontier,
    /// or required physical disposition.
    TerminalModelCallMismatch {
        /// The affected turn.
        turn: TurnId,
    },
    /// A recovery wait names an ambiguous call absent from the complete call
    /// facts.
    RecoveryModelCallMissing {
        /// The affected active turn.
        turn: TurnId,
        /// The absent ambiguous call.
        call: crate::ModelCallId,
    },
    /// The recovery call disagrees with its turn, selection, frontier, or
    /// required ambiguous physical disposition.
    RecoveryModelCallMismatch {
        /// The affected active turn.
        turn: TurnId,
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
    /// A completed turn has no exact final completion marker.
    MissingCompletionEntry {
        /// The affected turn.
        turn: TurnId,
    },
    /// A cancelled turn has no exact final cancellation marker.
    MissingCancellationEntry {
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
    /// A failed terminal's ended attempt names a different owning turn.
    TerminalAttemptOwnershipMismatch {
        /// The failed turn being reconstructed.
        turn: TurnId,
        /// The cross-wired ended attempt.
        attempt: TurnAttemptId,
    },
    /// A failed terminal's ended attempt has an ineligible disposition.
    TerminalAttemptEndMismatch {
        /// The failed turn being reconstructed.
        turn: TurnId,
        /// The incorrectly ended attempt.
        attempt: TurnAttemptId,
    },
    /// The same attempt identity appeared on multiple active or terminal
    /// records represented by this projection.
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
    pub(super) input: Box<AcceptedInputSchedulingReconstitutionInput>,
    pub(super) failure: AcceptedInputSchedulingReconstitutionFailure,
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

/// Why the complete scheduling projection cannot prepare an activation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AcceptedInputEligibilityFailure {
    /// A turn already owns the session's progressing slot.
    ActiveTurnPresent {
        /// The exact active slot owner.
        turn: TurnId,
    },
    /// A dedicated compaction call owns the session-wide execution slot.
    ContextCompactionInProgress {
        /// The exact unfinished compaction call.
        call: crate::ModelCallId,
    },
    /// No queued accepted-input turn exists.
    NoQueuedTurn,
    /// The proposed origin entry identity is already present.
    OriginEntryIdentityAlreadyExists,
    /// The proposed injected model-identity entry is already present.
    ModelIdentityEntryIdentityAlreadyExists,
    /// The proposed session-scoped snapshot identity is already present.
    StartingFrontierIdentityAlreadyExists,
    /// The proposed initial-attempt identity already appears in the complete
    /// scheduling projection's represented attempt history.
    InitialAttemptIdentityAlreadyExists,
    /// Internal preparation could not construct the origin-only first
    /// frontier from the already-validated projection.
    InternalOriginFrontierConstructionFailed,
    /// Internal preparation found earliest queued work after a predecessor
    /// without the terminal frontier guaranteed by scheduling reconstitution.
    InternalPredecessorTerminalFrontierMissing {
        /// The predecessor whose validated terminal frontier was absent.
        predecessor: TurnId,
    },
    /// Internal preparation could not append the fresh origin entry to the
    /// predecessor frontier guaranteed by scheduling reconstitution.
    InternalStartingFrontierDerivationFailed,
}

/// Rejected eligibility preparation retaining the complete projection and
/// supplied identities unchanged.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AcceptedInputEligibilityError {
    pub(super) projection: Box<AcceptedInputSchedulingProjection>,
    pub(super) identities: AcceptedInputTurnActivationIdentities,
    pub(super) failure: AcceptedInputEligibilityFailure,
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

/// Why the complete scheduling projection cannot prepare startup failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AcceptedInputTurnFailureFailure {
    /// No turn currently owns the session's progressing slot.
    NoActiveTurn,
    /// The supplied successor identities do not match the pending steering.
    PendingSteeringReclassificationMismatch,
    /// The proposed failed-marker identity is already present.
    FailureEntryIdentityAlreadyExists,
    /// The proposed terminal-frontier identity is already present.
    TerminalFrontierIdentityAlreadyExists,
    /// Canonical active attempt facts unexpectedly rejected a Lost end.
    ActiveAttemptCannotEndLost,
    /// Canonical active scheduling facts unexpectedly omitted their start.
    ActiveStartMissing,
    /// Canonical scheduling facts unexpectedly omitted the starting snapshot.
    StartingSnapshotMissing,
    /// Canonical fresh failure facts unexpectedly could not append.
    TerminalFrontierCannotAppend,
}

/// Rejected startup-failure preparation retaining every input unchanged.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AcceptedInputTurnFailureError {
    pub(super) projection: Box<AcceptedInputSchedulingProjection>,
    pub(super) identities: AcceptedInputTurnFailureIdentities,
    pub(super) failure: AcceptedInputTurnFailureFailure,
}

impl AcceptedInputTurnFailureError {
    /// Borrows the unchanged complete scheduling projection.
    pub const fn projection(&self) -> &AcceptedInputSchedulingProjection {
        &self.projection
    }

    /// Borrows the unchanged supplied identities.
    pub const fn identities(&self) -> &AcceptedInputTurnFailureIdentities {
        &self.identities
    }

    /// Returns the exact preparation failure.
    pub const fn failure(&self) -> AcceptedInputTurnFailureFailure {
        self.failure
    }

    /// Returns every unchanged input and the exact failure.
    pub fn into_parts(
        self,
    ) -> (
        AcceptedInputSchedulingProjection,
        AcceptedInputTurnFailureIdentities,
        AcceptedInputTurnFailureFailure,
    ) {
        (*self.projection, self.identities, self.failure)
    }
}
