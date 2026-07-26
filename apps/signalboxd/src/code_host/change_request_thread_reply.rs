//! `change_request_thread_reply` registry declaration and typed arguments.

use signalbox_domain::NormalizedToolArguments;

use super::{
    CodeHostOperation,
    arguments::{
        CodeHostCommentBody, CodeHostOpaqueId, InvalidCodeHostArguments, object, take_string,
    },
};

/// Registry declaration effect posture: mutation, `Confirm`, and
/// `ExternalEffect`; dispatch loss is commit-ambiguous.
pub(super) const NAME: &str = "change_request_thread_reply";
pub(super) const DESCRIPTION: &str = "Posts one reply to an exact GitHub review-thread node.";
pub(super) const SCHEMA: &str = r#"{
    "type": "object",
    "properties": {
        "thread_id": {"type": "string", "description": "Opaque review-thread node identity."},
        "body": {"type": "string", "description": "Exact nonempty reply body."}
    },
    "required": ["thread_id", "body"],
    "additionalProperties": false
}"#;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ThreadReplyArguments {
    thread_id: CodeHostOpaqueId,
    body: CodeHostCommentBody,
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
    let mut object = object(arguments, 2)?;
    let thread_id = CodeHostOpaqueId::try_new(take_string(&mut object, "thread_id")?)?;
    let body = CodeHostCommentBody::try_new(take_string(&mut object, "body")?)?;
    Ok(CodeHostOperation::ThreadReply(ThreadReplyArguments {
        thread_id,
        body,
    }))
}
