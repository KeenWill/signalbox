//! `change_request_thread_resolve` registry declaration and typed arguments.

use signalbox_domain::NormalizedToolArguments;
use signalbox_tool_contract::ToolContract;

use super::{
    CodeHostOperation,
    arguments::{
        CodeHostChangeRequestNumber, CodeHostOpaqueId, CodeHostRepository,
        InvalidCodeHostArguments, decode as decode_arguments,
    },
};

/// Registry declaration effect posture: mutation, `Confirm`, and
/// `ExternalEffect`; dispatch loss is commit-ambiguous.
pub(super) const NAME: &str = "change_request_thread_resolve";
pub(super) const DESCRIPTION: &str =
    "Resolves one exact review-thread node owned by the named GitHub change request.";

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ThreadResolveArguments {
    /// Exact owner/repository spelling of the owning change request.
    repository: CodeHostRepository,
    /// Number of the owning change request.
    number: CodeHostChangeRequestNumber,
    /// Opaque review-thread node identity inside the named change request.
    thread_id: CodeHostOpaqueId,
}

pub(super) struct Contract;

impl ToolContract for Contract {
    type Arguments = ThreadResolveArguments;
    const NAME: &'static str = NAME;
    const DESCRIPTION: &'static str = DESCRIPTION;
}

impl ThreadResolveArguments {
    /// Borrows the exact repository selector of the owning change request.
    pub fn repository(&self) -> &CodeHostRepository {
        &self.repository
    }

    /// Returns the owning change-request number.
    pub const fn number(&self) -> CodeHostChangeRequestNumber {
        self.number
    }

    /// Borrows the exact review-thread node identity.
    pub fn thread_id(&self) -> &CodeHostOpaqueId {
        &self.thread_id
    }
}

pub(super) fn decode(
    arguments: &NormalizedToolArguments,
) -> Result<CodeHostOperation, InvalidCodeHostArguments> {
    decode_arguments(arguments).map(CodeHostOperation::ThreadResolve)
}
