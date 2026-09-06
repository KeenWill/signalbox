//! Canonical durable input submission and authoritative-state preparation.
//!
//! docs/spec/turn-lifecycle-and-scheduling.md owns accepted-input delivery,
//! ordering, and disposition semantics;
//! docs/spec/configuration-and-credentials.md owns configuration
//! validation; docs/spec/identity-and-commands.md owns structural replay
//! equality and actor attribution; docs/spec/persistence-protocol.md owns
//! checked reconstitution; and docs/spec/sessions-and-transcript.md owns
//! content. This slice prepares accepted origin work with no active
//! turn or after the exact active turn, and pending steering for the exact
//! active turn. Applied and rejected replay validate complete canonical source
//! or predecessor origin facts, including the current lifecycle and queue facts
//! that make an immutable pending-steering receipt visible as reclassified
//! origin work. Replaying the pending receipt itself remains independent of its
//! later mutable disposition.

mod command;
mod prepared;
mod reconstituted;
mod reconstitution;
mod reconstitution_input;
mod result;
mod validation;

#[cfg(test)]
mod tests;

pub use command::SubmitInput;

pub use prepared::{
    PreparedSubmitInput, SubmitInputPreparationError, SubmitInputPreparationFailure,
};

pub use result::{
    SubmitInputAppliedResult, SubmitInputPendingSteeringAppliedResult, SubmitInputRejectedResult,
    SubmitInputResult, SubmitInputTurnOriginAppliedResult,
};

pub use reconstituted::{
    ReconstitutedSubmitInput, SubmitInputReconstitutionError, SubmitInputReconstitutionFailure,
};

pub use reconstitution::SubmitInputReconstitutionInput;

pub use reconstitution_input::{
    GoalTurnOriginConstructionInput, NonAcceptedTurnPredecessorReconstitutionInput,
    SubmitInputAppliedPendingSteeringReconstitutionInput,
    SubmitInputAppliedTurnOriginReconstitutionInput,
    SubmitInputAutomaticReconciliationConstructionInput,
    SubmitInputDirectTurnOriginConstructionInput,
    SubmitInputInterruptedModelCallReconciliationConstructionInput,
    SubmitInputInterruptedToolReconciliationConstructionInput,
    SubmitInputReclassifiedTurnOriginConstructionInput,
    SubmitInputRejectedAcceptancePositionExhaustedReconstitutionInput,
    SubmitInputRejectedActiveTurnMismatchReconstitutionInput,
    SubmitInputRejectedActiveTurnPresentReconstitutionInput,
    SubmitInputRejectedAttachmentBlobNotFoundReconstitutionInput,
    SubmitInputRejectedAttachmentByteBudgetExceededReconstitutionInput,
    SubmitInputRejectedDefaultsVersionMismatchReconstitutionInput,
    SubmitInputRejectedInterruptAlreadyAppliedReconstitutionInput,
    SubmitInputRejectedInterruptUnavailableWhileAwaitingApprovalReconstitutionInput,
    SubmitInputRejectedNoActiveTurnReconstitutionInput,
    SubmitInputRejectedSafePointUnavailableWhileStoppingReconstitutionInput,
    SubmitInputRejectedSessionNotFoundReconstitutionInput,
    SubmitInputRejectedUnknownModelAliasReconstitutionInput,
    SubmitInputTerminalSourceConstructionInput, SubmitInputTerminalSourceReconstitutionInput,
    SubmitInputTurnOriginReconstitutionInput,
};
