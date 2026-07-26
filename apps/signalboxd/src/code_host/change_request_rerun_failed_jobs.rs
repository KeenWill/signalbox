//! `change_request_rerun_failed_jobs` registry declaration and typed arguments.

use signalbox_domain::NormalizedToolArguments;

use super::{
    CodeHostOperation,
    arguments::{
        CodeHostRepository, InvalidCodeHostArguments, object, take_positive_id, take_string,
    },
};

/// Registry declaration effect posture: mutation, `Confirm`, and
/// `ExternalEffect`; dispatch loss is commit-ambiguous.
pub(super) const NAME: &str = "change_request_rerun_failed_jobs";
pub(super) const DESCRIPTION: &str =
    "Requests rerun of only failed jobs in one GitHub Actions workflow run.";
pub(super) const SCHEMA: &str = r#"{
    "type": "object",
    "properties": {
        "repository": {"type": "string", "description": "Exact owner/repository spelling."},
        "run_id": {"type": "integer", "minimum": 1, "description": "GitHub Actions workflow-run identity."}
    },
    "required": ["repository", "run_id"],
    "additionalProperties": false
}"#;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RerunFailedJobsArguments {
    repository: CodeHostRepository,
    run_id: u64,
}

impl RerunFailedJobsArguments {
    /// Borrows the exact repository selector.
    pub fn repository(&self) -> &CodeHostRepository {
        &self.repository
    }

    /// Returns the exact GitHub Actions workflow-run identity.
    pub const fn run_id(&self) -> u64 {
        self.run_id
    }
}

pub(super) fn decode(
    arguments: &NormalizedToolArguments,
) -> Result<CodeHostOperation, InvalidCodeHostArguments> {
    let mut object = object(arguments, 2)?;
    let repository = CodeHostRepository::try_new(take_string(&mut object, "repository")?)?;
    let run_id = take_positive_id(&mut object, "run_id")?;
    Ok(CodeHostOperation::RerunFailedJobs(
        RerunFailedJobsArguments { repository, run_id },
    ))
}
