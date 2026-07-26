//! `change_request_thread_resolve` registry declaration and typed arguments.

use signalbox_domain::NormalizedToolArguments;

use super::{
    CodeHostOperation,
    arguments::{CodeHostOpaqueId, InvalidCodeHostArguments, object, take_string},
};

/// Registry declaration effect posture: mutation, `Confirm`, and
/// `ExternalEffect`; dispatch loss is commit-ambiguous.
pub(super) const NAME: &str = "change_request_thread_resolve";
pub(super) const DESCRIPTION: &str = "Resolves one exact GitHub review-thread node.";
pub(super) const SCHEMA: &str = r#"{
    "type": "object",
    "properties": {
        "thread_id": {"type": "string", "description": "Opaque review-thread node identity."}
    },
    "required": ["thread_id"],
    "additionalProperties": false
}"#;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ThreadResolveArguments {
    thread_id: CodeHostOpaqueId,
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
    let mut object = object(arguments, 1)?;
    let thread_id = CodeHostOpaqueId::try_new(take_string(&mut object, "thread_id")?)?;
    Ok(CodeHostOperation::ThreadResolve(ThreadResolveArguments {
        thread_id,
    }))
}
