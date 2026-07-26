//! `change_request_file_patch` registry declaration and typed arguments.

use signalbox_domain::NormalizedToolArguments;

use super::{
    CodeHostOperation,
    arguments::{
        CodeHostChangeRequestNumber, CodeHostFilePath, CodeHostRepository,
        InvalidCodeHostArguments, object, take_positive_id, take_string,
    },
};

/// Registry declaration effect posture: read-only, `Auto`, and
/// `ExternalEffect` because GitHub observes the authenticated request.
pub(super) const NAME: &str = "change_request_file_patch";
pub(super) const DESCRIPTION: &str =
    "Returns the bounded code-host patch for one exact changed file.";
pub(super) const SCHEMA: &str = r#"{
    "type": "object",
    "properties": {
        "repository": {"type": "string", "description": "Exact owner/repository spelling."},
        "number": {"type": "integer", "minimum": 1, "description": "Change-request number."},
        "path": {"type": "string", "description": "Exact repository-relative changed path."}
    },
    "required": ["repository", "number", "path"],
    "additionalProperties": false
}"#;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FilePatchArguments {
    repository: CodeHostRepository,
    number: CodeHostChangeRequestNumber,
    path: CodeHostFilePath,
}

impl FilePatchArguments {
    /// Borrows the exact repository selector.
    pub fn repository(&self) -> &CodeHostRepository {
        &self.repository
    }

    /// Returns the change-request number.
    pub const fn number(&self) -> CodeHostChangeRequestNumber {
        self.number
    }

    /// Borrows the exact repository-relative path.
    pub fn path(&self) -> &CodeHostFilePath {
        &self.path
    }
}

pub(super) fn decode(
    arguments: &NormalizedToolArguments,
) -> Result<CodeHostOperation, InvalidCodeHostArguments> {
    let mut object = object(arguments, 3)?;
    let repository = CodeHostRepository::try_new(take_string(&mut object, "repository")?)?;
    let number = CodeHostChangeRequestNumber::try_new(take_positive_id(&mut object, "number")?)?;
    let path = CodeHostFilePath::try_new(take_string(&mut object, "path")?)?;
    Ok(CodeHostOperation::FilePatch(FilePatchArguments {
        repository,
        number,
        path,
    }))
}
