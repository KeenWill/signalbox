//! `change_request_comment` registry declaration and typed arguments.

use signalbox_domain::NormalizedToolArguments;
use signalbox_tool_contract::ToolContract;

use super::{
    CodeHostOperation,
    arguments::{
        CodeHostChangeRequestNumber, CodeHostCommentBody, CodeHostRepository,
        InvalidCodeHostArguments, decode as decode_arguments,
    },
};

/// Registry declaration effect posture: mutation, `Confirm`, and
/// `ExternalEffect`; dispatch loss is commit-ambiguous.
pub(super) const NAME: &str = "change_request_comment";
pub(super) const DESCRIPTION: &str = "Posts one top-level comment to a GitHub change request.";

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ChangeRequestCommentArguments {
    /// Exact owner/repository spelling.
    repository: CodeHostRepository,
    /// Change-request number.
    number: CodeHostChangeRequestNumber,
    /// Exact nonempty comment body.
    body: CodeHostCommentBody,
}

pub(super) struct Contract;

impl ToolContract for Contract {
    type Arguments = ChangeRequestCommentArguments;
    const NAME: &'static str = NAME;
    const DESCRIPTION: &'static str = DESCRIPTION;
}

impl ChangeRequestCommentArguments {
    /// Borrows the exact repository selector.
    pub fn repository(&self) -> &CodeHostRepository {
        &self.repository
    }

    /// Returns the change-request number.
    pub const fn number(&self) -> CodeHostChangeRequestNumber {
        self.number
    }

    /// Borrows the exact comment body.
    pub fn body(&self) -> &CodeHostCommentBody {
        &self.body
    }
}

pub(super) fn decode(
    arguments: &NormalizedToolArguments,
) -> Result<CodeHostOperation, InvalidCodeHostArguments> {
    decode_arguments(arguments).map(CodeHostOperation::Comment)
}
