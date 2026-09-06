//! Tool fixtures for `docs/spec/tool-loop.md`.

use super::approval::{
    ToolApprovalDecision, ToolApprovalResolution, ToolApprovalResolutionReconstitutionInput,
};
use super::decide::{
    DecideToolRequest, DecideToolRequestAppliedResult, DecideToolRequestResult,
    PreparedDecideToolRequest,
};
use crate::{DurableCommandId, ToolRequestId};

impl ToolApprovalResolutionReconstitutionInput {
    #[cfg(test)]
    pub(crate) fn user_fixture(request: ToolRequestId, decision: ToolApprovalDecision) -> Self {
        const USER_COMMAND_SEED: u128 = 1;

        let command_id = DurableCommandId::from_uuid(uuid::Uuid::from_u128(USER_COMMAND_SEED));
        let command = DecideToolRequest::try_new(command_id, request, decision.clone())
            .expect("the fixture command identity is admitted");
        Self::user_command(PreparedDecideToolRequest {
            command,
            result: DecideToolRequestResult::Applied(DecideToolRequestAppliedResult {
                resolution: ToolApprovalResolution::user(command_id, request, decision),
            }),
        })
    }
}

impl DecideToolRequest {
    #[cfg(test)]
    pub(crate) fn new(
        command_id: DurableCommandId,
        request: ToolRequestId,
        decision: ToolApprovalDecision,
    ) -> Self {
        Self::try_new(command_id, request, decision)
            .expect("the fixture command identity is admitted")
    }
}
