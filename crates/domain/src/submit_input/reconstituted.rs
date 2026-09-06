//! Checked submit-input replay values and reconstruction failures for `docs/spec/turn-lifecycle-and-scheduling.md`.

use super::SubmitInput;
use super::SubmitInputResult;
use super::reconstitution::SubmitInputReconstitutionInput;

/// Why complete typed durable facts cannot reconstruct a recorded submission.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SubmitInputReconstitutionFailure {
    /// The stored actor attribution differs from the command.
    StoredActorMismatch,
    /// Turn-origin facts carry a delivery that creates no admitted origin.
    AppliedDeliveryIsNotTurnOrigin,
    /// Pending-steering facts carry a non-safe-point delivery.
    AppliedDeliveryIsNotNextSafePoint,
    /// A terminal result names another session.
    ResultSessionMismatch,
    /// A missing-blob rejection names no attachment in the command.
    AttachmentDigestMismatch,
    /// A byte-budget rejection has no positive maximum or attachment.
    AttachmentBudgetMismatch,
    /// The accepted-input effect names another command.
    AcceptedCommandMismatch,
    /// The result and accepted-input effect name different inputs.
    AcceptedInputMismatch,
    /// The accepted-input effect belongs to another session.
    AcceptedSessionMismatch,
    /// The stored accepted content differs from the command.
    AcceptedContentMismatch,
    /// The stored delivery treatment differs from the command.
    AcceptedDeliveryMismatch,
    /// A turn-origin record does not retain its exact origin disposition.
    AcceptedDispositionMismatch,
    /// The applied steering result names another source turn.
    SteeringSourceTurnMismatch,
    /// The supplied source receipt is not the exact same-session turn origin.
    SteeringSourceTurnOriginMismatch,
    /// Pending steering reuses its source origin's accepted-input identity.
    SteeringSourceAcceptedInputReused,
    /// Pending steering reuses its source origin's durable-command identity.
    SteeringSourceCommandReused,
    /// Pending steering does not follow its source origin in acceptance order.
    SteeringAcceptanceDoesNotFollowSourceOrigin,
    /// The queue fact belongs to another session.
    QueueSessionMismatch,
    /// The queue fact names another future turn or an after-current result
    /// reuses its active predecessor.
    QueueTurnMismatch,
    /// An after-current result omits or cross-wires its predecessor origin,
    /// or a vacant-slot start supplies one.
    AfterCurrentPredecessorOriginMismatch,
    /// An after-current result reuses its predecessor's accepted-input ID.
    AfterCurrentPredecessorAcceptedInputReused,
    /// An after-current result reuses its predecessor's durable-command ID.
    AfterCurrentPredecessorCommandReused,
    /// After-current acceptance does not follow its predecessor origin.
    AfterCurrentAcceptanceDoesNotFollowPredecessorOrigin,
    /// The accepted-input and queue positions differ.
    QueuePositionMismatch,
    /// This slice's queue fact is not ordinary priority.
    QueuePriorityMismatch,
    /// An active-turn-present rejection carries a non-start command.
    ActiveTurnPresentRejectionMismatch,
    /// A no-active-turn result names a different expected turn or a start
    /// request.
    ExpectedActiveTurnMismatch,
    /// A stale-active rejection claims equal expected and actual turns.
    RejectedActiveTurnsAreEqual,
    /// Required same-session turn-origin evidence is missing or cross-wired.
    RejectionActiveTurnOriginMismatch,
    /// A rejected command reuses its actual turn origin's command identity.
    RejectionActiveTurnOriginCommandReused,
    /// A configuration rejection carries no explicit origin configuration.
    RejectionHasNoExplicitOriginConfiguration,
    /// A mismatch result repeats a different expected defaults version.
    ExpectedDefaultsVersionMismatch,
    /// A mismatch result claims equal expected and current versions.
    RejectedDefaultsVersionsAreEqual,
    /// The selected defaults record belongs to another session.
    DefaultsSessionMismatch,
    /// The selected defaults record carries another version.
    DefaultsVersionMismatch,
    /// The stored derived request differs from the version-checked request.
    RequestedModelMismatch,
    /// The stored frozen model differs from the checked request.
    FrozenModelMismatch,
    /// The recorded unknown alias differs from the alias that failed.
    UnknownAliasMismatch,
    /// The request did not select an alias.
    RejectionDidNotSelectAlias,
    /// The recorded last position still has a successor.
    PositionIsNotExhausted,
    /// A stopping-only rejection carries another delivery or active target.
    StoppingRejectionMismatch,
    /// The stored applied interrupt does not supply the exact earlier
    /// cancellation authority named by the rejection.
    ExistingInterruptMismatch,
}

/// Failed reconstitution retaining every typed input unchanged.
#[derive(Clone, Debug)]
pub struct SubmitInputReconstitutionError {
    pub(super) input: Box<SubmitInputReconstitutionInput>,
    pub(super) failure: SubmitInputReconstitutionFailure,
}

impl SubmitInputReconstitutionError {
    /// Returns why the complete projection was invalid.
    pub const fn failure(&self) -> SubmitInputReconstitutionFailure {
        self.failure
    }

    /// Borrows the complete unchanged input.
    pub const fn input(&self) -> &SubmitInputReconstitutionInput {
        &self.input
    }

    /// Returns the complete unchanged input and failure.
    pub fn into_parts(
        self,
    ) -> (
        SubmitInputReconstitutionInput,
        SubmitInputReconstitutionFailure,
    ) {
        (*self.input, self.failure)
    }
}

/// One complete recorded input handling reconstructed from matching facts.
///
/// This value authorizes no insertion, repair, transition, or command claim.
#[derive(Clone, Debug)]
pub struct ReconstitutedSubmitInput {
    pub(super) command: SubmitInput,
    pub(super) result: SubmitInputResult,
}

impl ReconstitutedSubmitInput {
    /// Borrows the reconstructed canonical command.
    pub const fn command(&self) -> &SubmitInput {
        &self.command
    }

    /// Borrows the reconstructed terminal result.
    pub const fn result(&self) -> &SubmitInputResult {
        &self.result
    }

    /// Returns the complete reconstructed command and result.
    pub fn into_parts(self) -> (SubmitInput, SubmitInputResult) {
        (self.command, self.result)
    }
}
