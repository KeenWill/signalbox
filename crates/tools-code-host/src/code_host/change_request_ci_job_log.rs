//! `change_request_ci_job_log` registry declaration and typed arguments.

use signalbox_domain::NormalizedToolArguments;
use signalbox_tool_contract::ToolContract;

use super::{
    CodeHostOperation,
    arguments::{
        CodeHostPositiveId, CodeHostRepository, InvalidCodeHostArguments,
        decode as decode_arguments,
    },
};

/// Registry declaration effect posture: read-only, `Auto`, and
/// `ExternalEffect` because GitHub observes the authenticated request.
pub(super) const NAME: &str = "change_request_ci_job_log";
pub(super) const DESCRIPTION: &str = "Returns a bounded text prefix of one GitHub Actions job log.";

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CiJobLogArguments {
    /// Exact owner/repository spelling.
    repository: CodeHostRepository,
    /// GitHub Actions job identity.
    job_id: CodeHostPositiveId,
}

pub(super) struct Contract;

impl ToolContract for Contract {
    type Arguments = CiJobLogArguments;
    const NAME: &'static str = NAME;
    const DESCRIPTION: &'static str = DESCRIPTION;
}

impl CiJobLogArguments {
    /// Borrows the exact repository selector.
    pub fn repository(&self) -> &CodeHostRepository {
        &self.repository
    }

    /// Returns the exact GitHub Actions job identity.
    pub const fn job_id(&self) -> u64 {
        self.job_id.get()
    }
}

pub(super) fn decode(
    arguments: &NormalizedToolArguments,
) -> Result<CodeHostOperation, InvalidCodeHostArguments> {
    decode_arguments(arguments).map(CodeHostOperation::CiJobLog)
}
