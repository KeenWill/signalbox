//! Typed delegated-session relation, messages, outcomes, and wait subject.
//!
//! Delegation follows `docs/spec/sessions-and-transcript.md`.

mod content;
mod event;
mod message;
mod outcome;
mod provenance;
mod reconstitution;
mod relation;
mod request;
mod terminal_turn;
mod vocabulary;
mod wait;

#[cfg(test)]
mod aggregate_tests;
#[cfg(test)]
mod tests;

pub use content::{DelegationContent, DelegationContentError, DelegationContentFailure};
pub use event::{DelegationEvent, DelegationEventOrdinal, DelegationLifecycle};
pub use message::{DelegationMessage, DelegationMessageDirection, DelegationMessageEndpoints};
pub use outcome::{
    DelegationOutcome, DelegationOutcomeKind, DelegationOutcomeReason,
    DelegationProvenanceReconstitutionInput,
};
pub use provenance::{DelegationProvenance, DelegationProvenanceProjection};
pub use reconstitution::{
    SessionDelegationReconstitutionError, SessionDelegationReconstitutionFailure,
    SessionDelegationReconstitutionInput,
};
pub use relation::{
    DelegationTransitionError, DelegationTransitionFailure, RejectedDelegationTransition,
    SessionDelegation,
};
pub use request::{
    DelegatedSpawnRequest, DelegationAwaitRequest, DelegationMessageRequest,
    DelegationRequestError, DelegationRequestFailure, await_session_tool_name,
    send_session_message_tool_name, spawn_session_tool_name,
};
pub use terminal_turn::TerminalChildTurn;
pub use vocabulary::{
    BoundChildAction, ChildRelationshipPolicy, DelegationWaitMode, DescendantTerminationScope,
    ParentTerminationAuthority, ParentTerminationCommandSource, ParentTerminationKind,
};
pub use wait::{ChildWait, DelegationWait};
