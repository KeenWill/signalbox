//! Closed versioned JSON-lines process protocol.
//!
//! This crate owns wire representations and frame validation only. Domain,
//! persistence, and client presentation values remain distinct mappings
//! (docs/spec/process-protocol.md).

mod credential_exclusions;
mod delegation;
mod error;
mod event;
mod frame;
mod goal;
mod operator_status;
mod program;
mod request;
mod response;
mod review;
mod runner;
mod runner_status;
mod scalars;
mod session;
mod settings;
mod shared_validation;
mod transcript;
mod user_input;

pub use credential_exclusions::*;
pub use delegation::*;
pub use error::*;
pub use event::*;
pub use frame::*;
pub use goal::*;
pub use operator_status::*;
pub use request::*;
pub use response::*;
pub use review::*;
pub use runner::*;
pub use runner_status::*;
pub use scalars::*;
pub use session::*;
pub use settings::*;
pub use transcript::*;
pub use user_input::*;

/// Path-free daemon-local workspace binding evidence.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionWorkspaceRootKind {
    Derived,
    Configured,
    Provisioned,
}

#[cfg(test)]
mod tests;

pub use program::{
    EvaluationCorpusFormat, EvaluationInput, ProgramByteExtent, ProgramExecutableInput,
    ProgramGrant, ProgramRegistrationInput, ProgramRun, ProgramRunCancellationOutcome,
    ProgramRunCancelledState, ProgramRunState, ProgramRunTerminalState,
};

mod credential_pool;
pub use credential_pool::{
    CredentialPoolExclusion, CredentialPoolMemberEvidence, MAX_HEADROOM_RESERVE_PERCENT,
    valid_credential_pool_evidence, valid_credential_pool_members,
};
