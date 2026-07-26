//! `change_request_comment` registry declaration and typed arguments.

use signalbox_domain::NormalizedToolArguments;

use super::{
    CodeHostOperation,
    arguments::{
        CodeHostChangeRequestNumber, CodeHostCommentBody, CodeHostRepository,
        InvalidCodeHostArguments, object, take_positive_id, take_string,
    },
};

/// Registry declaration effect posture: mutation, `Confirm`, and
/// `ExternalEffect`; dispatch loss is commit-ambiguous.
pub(super) const NAME: &str = "change_request_comment";
pub(super) const DESCRIPTION: &str = "Posts one top-level comment to a GitHub change request.";
pub(super) const SCHEMA: &str = r#"{
    "type": "object",
    "properties": {
        "repository": {"type": "string", "description": "Exact owner/repository spelling."},
        "number": {"type": "integer", "minimum": 1, "description": "Change-request number."},
        "body": {"type": "string", "description": "Exact nonempty comment body."}
    },
    "required": ["repository", "number", "body"],
    "additionalProperties": false
}"#;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChangeRequestCommentArguments {
    repository: CodeHostRepository,
    number: CodeHostChangeRequestNumber,
    body: CodeHostCommentBody,
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
    let mut object = object(arguments, 3)?;
    let repository = CodeHostRepository::try_new(take_string(&mut object, "repository")?)?;
    let number = CodeHostChangeRequestNumber::try_new(take_positive_id(&mut object, "number")?)?;
    let body = CodeHostCommentBody::try_new(take_string(&mut object, "body")?)?;
    Ok(CodeHostOperation::Comment(ChangeRequestCommentArguments {
        repository,
        number,
        body,
    }))
}
