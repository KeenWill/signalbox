//! Program cancellation receipts over retained journal terminal states.

use serde::{Deserialize, Serialize};

/// The state an applied cancellation establishes.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProgramRunCancelledState {
    Cancelled,
}

/// Terminal states and their retained results recorded by the program journal.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "terminal_state", rename_all = "snake_case", deny_unknown_fields)]
pub enum ProgramRunTerminalState {
    Cancelled { result: () },
    Faulted { result: () },
    Succeeded { result: Vec<u8> },
}

/// Closed result of a durable program cancellation command.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ProgramRunCancellationOutcome {
    Applied {
        terminal_state: ProgramRunCancelledState,
        result: (),
    },
    NotFound {},
    AlreadyTerminal(ProgramRunTerminalState),
}
