//! `change_request_file_patch` registry declaration and typed arguments.

use signalbox_domain::NormalizedToolArguments;
use signalbox_tool_contract::ToolContract;

use super::{
    CodeHostOperation,
    arguments::{
        CodeHostChangeRequestNumber, CodeHostChangedFilePath, CodeHostFilePath, CodeHostRepository,
        InvalidCodeHostArguments, decode as decode_arguments,
    },
};

/// Registry declaration effect posture: read-only, `Auto`, and
/// `ExternalEffect` because GitHub observes the authenticated request.
pub(super) const NAME: &str = "change_request_file_patch";
pub(super) const DESCRIPTION: &str =
    "Returns the bounded code-host patch for one exact changed file.";

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FilePatchArguments {
    /// Exact owner/repository spelling.
    repository: CodeHostRepository,
    /// Change-request number.
    number: CodeHostChangeRequestNumber,
    /// Exact repository-relative changed path.
    path: CodeHostChangedFilePath,
}

pub(super) struct Contract;

impl ToolContract for Contract {
    type Arguments = FilePatchArguments;
    const NAME: &'static str = NAME;
    const DESCRIPTION: &'static str = DESCRIPTION;
}

impl FilePatchArguments {
    /// Borrows the exact repository selector.
    pub fn repository(&self) -> &CodeHostRepository {
        &self.repository
    }

    /// Returns the change-request number.
    pub const fn number(&self) -> CodeHostChangeRequestNumber {
        self.number
    }

    /// Borrows the exact repository-relative path.
    pub fn path(&self) -> &CodeHostFilePath {
        self.path.as_file_path()
    }
}

pub(super) fn decode(
    arguments: &NormalizedToolArguments,
) -> Result<CodeHostOperation, InvalidCodeHostArguments> {
    decode_arguments(arguments).map(CodeHostOperation::FilePatch)
}
