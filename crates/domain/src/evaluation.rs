//! Complete evaluation measurements; docs/spec/eval-system.md.

use crate::{JournalPosition, ProgramRegistrationId, ProgramRunId};
use serde_json::Value;

/// Host-derived snapshot whose identity resolves to one retained workflow input.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EvaluationSnapshot {
    pub run: ProgramRunId,
    pub registration: ProgramRegistrationId,
    pub input: Vec<u8>,
    pub metadata: Value,
    pub scorecard_kind: String,
    pub scorecard: Value,
    /// Complete manifest order, with zero-based trial ordinals.
    pub trials: Vec<EvaluationTrial>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EvaluationTrial {
    pub ordinal: u32,
    pub case_position: u32,
    pub repeat: u32,
    /// Decoded corpus case, including its expectation and label provenance.
    pub case: Value,
    /// Answer position in this snapshot's run journal.
    pub evidence_position: JournalPosition,
    pub outcome: EvaluationOutcome,
}

/// Successful and failed observations retain their complete program-specific evidence.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EvaluationOutcome {
    Verdict(Value),
    Failed(Value),
    Ambiguous,
}

/// Stable receipt shared by the initial seal and every equal retry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EvaluationReceipt {
    pub run: ProgramRunId,
}
