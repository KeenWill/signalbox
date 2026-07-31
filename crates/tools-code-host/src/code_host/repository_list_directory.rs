//! `repository_list_directory` registry declaration and typed arguments.

use signalbox_domain::NormalizedToolArguments;
use signalbox_tool_contract::ToolContract;

use super::{
    CodeHostOperation,
    arguments::{
        CodeHostFilePath, CodeHostRepository, CodeHostRevision, InvalidCodeHostArguments,
        decode as decode_arguments,
    },
};

/// Registry declaration effect posture: read-only, `Auto`, and
/// `ExternalEffect` because GitHub observes the authenticated request.
pub(super) const NAME: &str = "repository_list_directory";
pub(super) const DESCRIPTION: &str =
    "Lists bounded entries from one repository directory at a required exact revision.";

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RepositoryListDirectoryArguments {
    /// Exact owner/repository spelling.
    repository: CodeHostRepository,
    /// Exact repository-relative directory path; `.` lists the repository root.
    path: CodeHostFilePath,
    /// Required exact lowercase 40-hex commit revision.
    revision: CodeHostRevision,
}

pub(super) struct Contract;

impl ToolContract for Contract {
    type Arguments = RepositoryListDirectoryArguments;
    const NAME: &'static str = NAME;
    const DESCRIPTION: &'static str = DESCRIPTION;
}

impl RepositoryListDirectoryArguments {
    /// Borrows the exact repository selector.
    pub fn repository(&self) -> &CodeHostRepository {
        &self.repository
    }

    /// Borrows the exact repository-relative path.
    pub fn path(&self) -> &CodeHostFilePath {
        &self.path
    }

    /// Borrows the required exact revision.
    pub fn revision(&self) -> &CodeHostRevision {
        &self.revision
    }
}

pub(super) fn decode(
    arguments: &NormalizedToolArguments,
) -> Result<CodeHostOperation, InvalidCodeHostArguments> {
    decode_arguments(arguments).map(CodeHostOperation::ListDirectory)
}
