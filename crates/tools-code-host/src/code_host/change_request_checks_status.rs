//! `change_request_checks_status` registry declaration and typed arguments.

use signalbox_domain::NormalizedToolArguments;

use super::{
    CodeHostOperation,
    arguments::{
        CodeHostRepository, CodeHostRevision, InvalidCodeHostArguments, object, take_string,
    },
};

/// Registry declaration effect posture: read-only, `Auto`, and
/// `ExternalEffect` because GitHub observes the authenticated request.
pub(super) const NAME: &str = "change_request_checks_status";
pub(super) const DESCRIPTION: &str =
    "Returns the first bounded check-run page for a frozen change-request revision.";
pub(super) const SCHEMA: &str = r#"{
    "type": "object",
    "properties": {
        "repository": {"type": "string", "description": "Exact owner/repository spelling."},
        "revision": {"type": "string", "description": "Exact lowercase 40-hex head revision."}
    },
    "required": ["repository", "revision"],
    "additionalProperties": false
}"#;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChecksStatusArguments {
    repository: CodeHostRepository,
    revision: CodeHostRevision,
}

impl ChecksStatusArguments {
    /// Borrows the exact repository selector.
    pub fn repository(&self) -> &CodeHostRepository {
        &self.repository
    }

    /// Borrows the frozen head revision.
    pub fn revision(&self) -> &CodeHostRevision {
        &self.revision
    }
}

pub(super) fn decode(
    arguments: &NormalizedToolArguments,
) -> Result<CodeHostOperation, InvalidCodeHostArguments> {
    let mut object = object(arguments, 2)?;
    let repository = CodeHostRepository::try_new(take_string(&mut object, "repository")?)?;
    let revision = CodeHostRevision::try_new(take_string(&mut object, "revision")?)?;
    Ok(CodeHostOperation::ChecksStatus(ChecksStatusArguments {
        repository,
        revision,
    }))
}
