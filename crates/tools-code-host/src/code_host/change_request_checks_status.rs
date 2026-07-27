//! `change_request_checks_status` registry declaration and typed arguments.

use signalbox_domain::NormalizedToolArguments;
use signalbox_tool_contract::ToolContract;

use super::{
    CodeHostOperation,
    arguments::{
        CodeHostRepository, CodeHostRevision, InvalidCodeHostArguments, decode as decode_arguments,
    },
};

/// Registry declaration effect posture: read-only, `Auto`, and
/// `ExternalEffect` because GitHub observes the authenticated request.
pub(super) const NAME: &str = "change_request_checks_status";
pub(super) const DESCRIPTION: &str =
    "Returns the first bounded check-run page for a frozen change-request revision.";

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ChecksStatusArguments {
    /// Exact owner/repository spelling.
    repository: CodeHostRepository,
    /// Exact lowercase 40-hex head revision.
    revision: CodeHostRevision,
}

pub(super) struct Contract;

impl ToolContract for Contract {
    type Arguments = ChecksStatusArguments;
    const NAME: &'static str = NAME;
    const DESCRIPTION: &'static str = DESCRIPTION;
}

impl ChecksStatusArguments {
    /// Borrows the exact repository selector.
    pub fn repository(&self) -> &CodeHostRepository {
        &self.repository
    }

    /// Borrows the frozen head revision.
    pub fn revision(&self) -> &CodeHostRevision {
        &self.revision
    }
}

pub(super) fn decode(
    arguments: &NormalizedToolArguments,
) -> Result<CodeHostOperation, InvalidCodeHostArguments> {
    decode_arguments(arguments).map(CodeHostOperation::ChecksStatus)
}
