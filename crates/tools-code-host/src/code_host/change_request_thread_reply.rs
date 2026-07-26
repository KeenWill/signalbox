//! `change_request_thread_reply` registry declaration and typed arguments.

use signalbox_domain::NormalizedToolArguments;
use signalbox_tool_contract::ToolContract;

use super::{
    CodeHostOperation,
    arguments::{
        CodeHostCommentBody, CodeHostOpaqueId, InvalidCodeHostArguments, decode as decode_arguments,
    },
};

/// Registry declaration effect posture: mutation, `Confirm`, and
/// `ExternalEffect`; dispatch loss is commit-ambiguous.
pub(super) const NAME: &str = "change_request_thread_reply";
pub(super) const DESCRIPTION: &str = "Posts one reply to an exact GitHub review-thread node.";

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ThreadReplyArguments {
    /// Opaque review-thread node identity.
    thread_id: CodeHostOpaqueId,
    /// Exact nonempty reply body.
    body: CodeHostCommentBody,
}

pub(super) struct Contract;

impl ToolContract for Contract {
    type Arguments = ThreadReplyArguments;
    const NAME: &'static str = NAME;
    const DESCRIPTION: &'static str = DESCRIPTION;
}

impl ThreadReplyArguments {
    /// Borrows the exact review-thread node identity.
    pub fn thread_id(&self) -> &CodeHostOpaqueId {
        &self.thread_id
    }

    /// Borrows the exact reply body.
    pub fn body(&self) -> &CodeHostCommentBody {
        &self.body
    }
}

pub(super) fn decode(
    arguments: &NormalizedToolArguments,
) -> Result<CodeHostOperation, InvalidCodeHostArguments> {
    decode_arguments(arguments).map(CodeHostOperation::ThreadReply)
}
