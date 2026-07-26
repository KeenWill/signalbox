//! `change_request_review_threads` registry declaration and typed arguments.

use signalbox_domain::NormalizedToolArguments;

use super::{
    CodeHostOperation,
    arguments::{
        CodeHostChangeRequestNumber, CodeHostRepository, InvalidCodeHostArguments, object,
        take_positive_id, take_string,
    },
};

/// Registry declaration effect posture: read-only, `Auto`, and
/// `ExternalEffect` because GitHub observes the authenticated request.
pub(super) const NAME: &str = "change_request_review_threads";
pub(super) const DESCRIPTION: &str =
    "Returns the first bounded review-thread page for one GitHub change request.";
pub(super) const SCHEMA: &str = r#"{
    "type": "object",
    "properties": {
        "repository": {"type": "string", "description": "Exact owner/repository spelling."},
        "number": {"type": "integer", "minimum": 1, "description": "Change-request number."}
    },
    "required": ["repository", "number"],
    "additionalProperties": false
}"#;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReviewThreadsArguments {
    repository: CodeHostRepository,
    number: CodeHostChangeRequestNumber,
}

impl ReviewThreadsArguments {
    /// Borrows the exact repository selector.
    pub fn repository(&self) -> &CodeHostRepository {
        &self.repository
    }

    /// Returns the change-request number.
    pub const fn number(&self) -> CodeHostChangeRequestNumber {
        self.number
    }
}

pub(super) fn decode(
    arguments: &NormalizedToolArguments,
) -> Result<CodeHostOperation, InvalidCodeHostArguments> {
    let mut object = object(arguments, 2)?;
    let repository = CodeHostRepository::try_new(take_string(&mut object, "repository")?)?;
    let number = CodeHostChangeRequestNumber::try_new(take_positive_id(&mut object, "number")?)?;
    Ok(CodeHostOperation::ReviewThreads(ReviewThreadsArguments {
        repository,
        number,
    }))
}
