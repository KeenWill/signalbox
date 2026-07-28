//! `change_request_stack_state` registry declaration and typed arguments.

use signalbox_domain::NormalizedToolArguments;
use signalbox_tool_contract::ToolContract;

use super::{
    CodeHostOperation,
    arguments::{
        CodeHostChangeRequestNumber, CodeHostPageCursor, CodeHostRepository,
        InvalidCodeHostArguments, decode as decode_arguments,
    },
};

/// Registry declaration effect posture: read-only, `Auto`, and
/// `ExternalEffect` because GitHub observes the authenticated request.
pub(super) const NAME: &str = "change_request_stack_state";
pub(super) const DESCRIPTION: &str = "Returns bounded parent and immediate-child branch ancestry evidence for one GitHub change request.";

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StackStateArguments {
    /// Exact owner/repository spelling.
    repository: CodeHostRepository,
    /// Change-request number.
    number: CodeHostChangeRequestNumber,
    /// Optional opaque child-page continuation cursor.
    #[serde(default)]
    cursor: Option<CodeHostPageCursor>,
}

pub(super) struct Contract;

impl ToolContract for Contract {
    type Arguments = StackStateArguments;
    const NAME: &'static str = NAME;
    const DESCRIPTION: &'static str = DESCRIPTION;
}

impl StackStateArguments {
    /// Borrows the exact repository selector.
    pub fn repository(&self) -> &CodeHostRepository {
        &self.repository
    }

    /// Returns the change-request number.
    pub const fn number(&self) -> CodeHostChangeRequestNumber {
        self.number
    }

    /// Returns the requested child page, defaulting to the first page.
    pub const fn child_page(&self) -> u32 {
        match self.cursor {
            Some(cursor) => cursor.page(),
            None => 1,
        }
    }
}

pub(super) fn decode(
    arguments: &NormalizedToolArguments,
) -> Result<CodeHostOperation, InvalidCodeHostArguments> {
    decode_arguments(arguments).map(CodeHostOperation::StackState)
}
