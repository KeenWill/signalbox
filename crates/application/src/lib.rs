//! Application orchestration boundary for Signalbox.
//!
//! This crate coordinates domain decisions and external effects while
//! depending inward on `signalbox-domain`.

mod create_session;
mod load_session;
mod replace_session_defaults;
mod start_eligible_turn;
mod submit_input;

pub use create_session::{
    CreateSessionError, CreateSessionOutcome, CreateSessionRequest, CreateSessionService,
    CreateSessionTransaction, InvalidDurableCommandId, SessionIdGenerator,
    UuidV7SessionIdGenerator,
};
pub use load_session::{LoadSessionService, SessionReader};
pub use replace_session_defaults::{
    ReplaceSessionDefaultsOutcome, ReplaceSessionDefaultsRequest, ReplaceSessionDefaultsService,
    ReplaceSessionDefaultsTransaction,
};
pub use start_eligible_turn::{
    StartEligibleTurnIdGenerator, StartEligibleTurnOutcome, StartEligibleTurnService,
    StartEligibleTurnTransaction, UuidV7StartEligibleTurnIdGenerator,
};
pub use submit_input::{
    SubmitInputIdGenerator, SubmitInputOutcome, SubmitInputRequest, SubmitInputService,
    SubmitInputTransaction, UuidV7SubmitInputIdGenerator,
};
