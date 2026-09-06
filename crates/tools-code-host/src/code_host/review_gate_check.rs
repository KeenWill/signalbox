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
/// `ExternalEffect` because it reads fresh GitHub evidence.
pub(super) const NAME: &str = "review_gate_check";
pub(super) const DESCRIPTION: &str =
    "Reports the shared convergence verdict from one complete GitHub snapshot.";

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReviewGateCheckArguments {
    /// Exact owner/repository spelling.
    repository: CodeHostRepository,
    /// Change-request number.
    number: CodeHostChangeRequestNumber,
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
}

pub(super) fn decode(
    arguments: &NormalizedToolArguments,
) -> Result<CodeHostOperation, InvalidCodeHostArguments> {
    decode_arguments(arguments).map(CodeHostOperation::ReviewGateCheck)
}
