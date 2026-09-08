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
pub use scalars::*;
pub use session::*;
pub use settings::*;
pub use transcript::*;
pub use user_input::*;

#[cfg(test)]
mod tests;

pub use program::{
    ProgramRunCancellationOutcome, ProgramRunCancelledState, ProgramRunTerminalState,
};
