//! Complete accepted-input scheduling projection and pure eligibility.
//!
//! docs/spec/turn-lifecycle-and-scheduling.md,
//! docs/spec/sessions-and-transcript.md, and
//! docs/spec/persistence-protocol.md are normative. This purpose-specific
//! projection reconstructs every fact that can change accepted-input
//! eligibility or slot ownership in the implemented semantic-entry slice. It
//! supports an ancestry-free session or a fully reconstituted imported seed
//! whose durable total order consists of a terminal prefix, at most one active
//! slot, and a queued suffix.
//!
//! Active records carry one exact checked phase and a validated,
//! session-scoped acceptance tail. Prepared and running attempts need no
//! external evidence; stop-requested and recovery phases require their complete
//! correlated model-call and applied-interrupt facts.

mod acceptance_tail;
mod activated_turn;
mod correlation;
mod failure;
mod prepare;
mod prepared_activation;
mod projection;
mod reconstitute;
mod reconstitution_input;
mod record;
mod turn_failure;

#[cfg(test)]
mod tests;

pub use activated_turn::{
    AcceptedInputTurnActivationIdentities, ActivatedAcceptedInputTurn, ActivatedDelegatedTurn,
    ActivatedTurn,
};
pub use failure::{
    AcceptedInputEligibilityError, AcceptedInputEligibilityFailure,
    AcceptedInputSchedulingReconstitutionError, AcceptedInputSchedulingReconstitutionFailure,
    AcceptedInputTurnFailureError, AcceptedInputTurnFailureFailure,
};
pub use prepared_activation::{
    DelegatedTurnActivationInput, DelegatedWakeTurnActivationInput,
    PreparedAcceptedInputTurnActivation, PreparedDelegatedTurnActivation, PreparedTurnActivation,
};
pub use projection::{
    AcceptedInputSchedulingProjection, AcceptedInputTurnSchedulingProjection,
    AcceptedInputTurnSchedulingStatus, ConsumedSteeringInput, PendingSteeringInput,
};
pub use reconstitution_input::{
    AcceptedInputSchedulingReconstitutionInput, ActiveTurnSchedulingReconstitutionInput,
    CancelledTurnExecutionReconstitutionInput, ConsumedSteeringReconstitutionInput,
    ContinuationRoundReconstitutionInput, DelegatedModelCallRecoveryReconstitutionInput,
    FailedTurnExecutionReconstitutionInput, SessionAcceptanceTailEntryReconstitutionInput,
    SessionAcceptanceTailReconstitutionInput, SteeringContinuationRoundReconstitutionInput,
    TerminalAttemptEndReconstitutionInput,
};
pub use record::{
    AcceptedInputTurnSchedulingRecord, AcceptedInputTurnSchedulingRecordState,
    AutomaticReconciliationAuthority, DelegatedTurnSchedulingFact, DelegatedTurnSchedulingState,
};
pub use turn_failure::{
    AcceptedInputTurnFailureIdentities, FailedAcceptedInputTurn, PreparedAcceptedInputTurnFailure,
};
