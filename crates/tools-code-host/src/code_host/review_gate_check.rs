//! `review_gate_check` registry declaration and typed arguments.

use signalbox_domain::NormalizedToolArguments;
use signalbox_tool_contract::ToolContract;

use super::{
    CodeHostOperation,
    arguments::{
        CodeHostChangeRequestNumber, CodeHostRepository, InvalidCodeHostArguments,
        decode as decode_arguments,
    },
};

/// Registry declaration effect posture: read-only, `Auto`, and
/// `ExternalEffect` because its pure composition reads fresh GitHub evidence.
pub(super) const NAME: &str = "review_gate_check";
pub(super) const DESCRIPTION: &str =
    "Composes convergence, stack, and thread evidence into a deterministic review-protocol gate.";

/// The protocol boundary the caller is checking.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReviewGatePurpose {
    /// Whether a new external review wave may be requested.
    RequestReviewWave,
    /// Whether the change request may be declared converged.
    DeclareConvergence,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReviewGateCheckArguments {
    /// Exact owner/repository spelling.
    repository: CodeHostRepository,
    /// Change-request number.
    number: CodeHostChangeRequestNumber,
    /// Protocol boundary to evaluate.
    purpose: ReviewGatePurpose,
}

pub(super) struct Contract;

impl ToolContract for Contract {
    type Arguments = ReviewGateCheckArguments;
    const NAME: &'static str = NAME;
    const DESCRIPTION: &'static str = DESCRIPTION;
}

impl ReviewGateCheckArguments {
    /// Borrows the exact repository selector.
    pub fn repository(&self) -> &CodeHostRepository {
        &self.repository
    }

    /// Returns the change-request number.
    pub const fn number(&self) -> CodeHostChangeRequestNumber {
        self.number
    }

    /// Returns the review-protocol boundary to evaluate.
    pub const fn purpose(&self) -> ReviewGatePurpose {
        self.purpose
    }
}

pub(super) fn decode(
    arguments: &NormalizedToolArguments,
) -> Result<CodeHostOperation, InvalidCodeHostArguments> {
    decode_arguments(arguments).map(CodeHostOperation::ReviewGateCheck)
}
