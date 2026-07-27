//! `change_request_rerun_failed_jobs` registry declaration and typed arguments.

use signalbox_domain::NormalizedToolArguments;
use signalbox_tool_contract::ToolContract;

use super::{
    CodeHostOperation,
    arguments::{
        CodeHostPositiveId, CodeHostRepository, InvalidCodeHostArguments,
        decode as decode_arguments,
    },
};

/// Registry declaration effect posture: mutation, `Confirm`, and
/// `ExternalEffect`; dispatch loss is commit-ambiguous.
pub(super) const NAME: &str = "change_request_rerun_failed_jobs";
pub(super) const DESCRIPTION: &str =
    "Requests rerun of only failed jobs in one GitHub Actions workflow run.";

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RerunFailedJobsArguments {
    /// Exact owner/repository spelling.
    repository: CodeHostRepository,
    /// GitHub Actions workflow-run identity.
    run_id: CodeHostPositiveId,
}

pub(super) struct Contract;

impl ToolContract for Contract {
    type Arguments = RerunFailedJobsArguments;
    const NAME: &'static str = NAME;
    const DESCRIPTION: &'static str = DESCRIPTION;
}

impl RerunFailedJobsArguments {
    /// Borrows the exact repository selector.
    pub fn repository(&self) -> &CodeHostRepository {
        &self.repository
    }

    /// Returns the exact GitHub Actions workflow-run identity.
    pub const fn run_id(&self) -> u64 {
        self.run_id.get()
    }
}

pub(super) fn decode(
    arguments: &NormalizedToolArguments,
) -> Result<CodeHostOperation, InvalidCodeHostArguments> {
    decode_arguments(arguments).map(CodeHostOperation::RerunFailedJobs)
}
