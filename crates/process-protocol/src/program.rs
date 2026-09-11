//! Program admission and retained run results.

use serde::{Deserialize, Serialize};

/// Corpus and trial selection submitted to daemon evaluation composition.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvaluationInput {
    pub corpus: crate::CanonicalBlobDigest,
    pub format: EvaluationCorpusFormat,
    pub cases: Vec<u32>,
    pub repeats: u32,
    pub recorded_responses: Option<crate::CanonicalBlobDigest>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvaluationCorpusFormat {
    Offline,
    Live,
}

/// User-supplied executable selected at registration.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ProgramExecutableInput {
    #[serde(rename = "javascript")]
    JavaScript {
        source: Vec<u8>,
        artifact: String,
    },
    Native {
        entry: String,
        revision: String,
    },
}

/// Closed workflow capability vocabulary at the process boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProgramGrant {
    Time,
    Random,
    Sleep,
    Subscribe,
    Session,
    Judge,
    ExecStage,
    Corpus,
    EvalRecord,
    Blob,
    Register,
    /// Checked repository-watch module operations.
    RepoWatch,
}

/// Exact registration intent; native binary identity is supplied by the daemon.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProgramRegistrationInput {
    pub name: String,
    pub revision: String,
    pub executable: ProgramExecutableInput,
    pub grants: Vec<ProgramGrant>,
}

/// Whether a response carries all retained bytes or a prefix bounded by its frame.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ProgramByteExtent {
    Complete {},
    Truncated { total_bytes: u64 },
}

/// Retained execution state, independent of executable availability.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum ProgramRunState {
    Running {},
    Cancelled {},
    Faulted {},
    Succeeded {
        result: Vec<u8>,
        result_extent: ProgramByteExtent,
    },
}

/// Immutable run admission and its observed journal outcome.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProgramRun {
    pub registration_id: crate::CanonicalUuid,
    pub input: Vec<u8>,
    pub input_extent: ProgramByteExtent,
    pub outcome: ProgramRunState,
}

/// The state an applied cancellation establishes.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProgramRunCancelledState {
    Cancelled,
}

/// Terminal states and frame-bounded results projected from the program journal.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "terminal_state", rename_all = "snake_case", deny_unknown_fields)]
pub enum ProgramRunTerminalState {
    Cancelled {
        result: (),
    },
    Faulted {
        result: (),
    },
    Succeeded {
        result: Vec<u8>,
        result_extent: ProgramByteExtent,
    },
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
