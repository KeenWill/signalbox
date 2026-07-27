//! `change_request_thread_resolve` registry declaration and typed arguments.

use signalbox_domain::NormalizedToolArguments;
use signalbox_tool_contract::ToolContract;

use super::{
    CodeHostOperation,
    arguments::{CodeHostOpaqueId, InvalidCodeHostArguments, decode as decode_arguments},
};

/// Registry declaration effect posture: mutation, `Confirm`, and
/// `ExternalEffect`; dispatch loss is commit-ambiguous.
pub(super) const NAME: &str = "change_request_thread_resolve";
pub(super) const DESCRIPTION: &str = "Resolves one exact GitHub review-thread node.";

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ThreadResolveArguments {
    /// Opaque review-thread node identity.
    thread_id: CodeHostOpaqueId,
}

pub(super) struct Contract;

impl ToolContract for Contract {
    type Arguments = ThreadResolveArguments;
    const NAME: &'static str = NAME;
    const DESCRIPTION: &'static str = DESCRIPTION;
}

impl ThreadResolveArguments {
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
