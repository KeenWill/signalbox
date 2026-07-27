//! `change_request_ci_job_log` registry declaration and typed arguments.

use signalbox_domain::NormalizedToolArguments;

use super::{
    CodeHostOperation,
    arguments::{
        CodeHostRepository, InvalidCodeHostArguments, object, take_positive_id, take_string,
    },
};

/// Registry declaration effect posture: read-only, `Auto`, and
/// `ExternalEffect` because GitHub observes the authenticated request.
pub(super) const NAME: &str = "change_request_ci_job_log";
pub(super) const DESCRIPTION: &str = "Returns a bounded text prefix of one GitHub Actions job log.";
pub(super) const SCHEMA: &str = r#"{
    "type": "object",
    "properties": {
        "repository": {"type": "string", "description": "Exact owner/repository spelling."},
        "job_id": {"type": "integer", "minimum": 1, "description": "GitHub Actions job identity."}
    },
    "required": ["repository", "job_id"],
    "additionalProperties": false
}"#;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CiJobLogArguments {
    repository: CodeHostRepository,
    job_id: u64,
}

impl CiJobLogArguments {
    /// Borrows the exact repository selector.
    pub fn repository(&self) -> &CodeHostRepository {
        &self.repository
    }

    /// Returns the exact GitHub Actions job identity.
    pub const fn job_id(&self) -> u64 {
        self.job_id
    }
}

pub(super) fn decode(
    arguments: &NormalizedToolArguments,
) -> Result<CodeHostOperation, InvalidCodeHostArguments> {
    let mut object = object(arguments, 2)?;
    let repository = CodeHostRepository::try_new(take_string(&mut object, "repository")?)?;
    let job_id = take_positive_id(&mut object, "job_id")?;
    Ok(CodeHostOperation::CiJobLog(CiJobLogArguments {
        repository,
        job_id,
    }))
}
