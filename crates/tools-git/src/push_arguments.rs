use schemars::JsonSchema;
use serde::Deserialize;

use crate::limits::MAX_BRANCH_BYTES;

/// Push arguments contain only a local branch; the destination is deployment
/// configuration and cannot be supplied by a model.
#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GitPushArguments {
    /// Existing local branch to push without force.
    #[schemars(length(min = 1, max = MAX_BRANCH_BYTES))]
    pub(super) branch: String,
}

impl GitPushArguments {
    #[cfg(test)]
    pub(super) fn for_test(branch: impl Into<String>) -> Self {
        Self {
            branch: branch.into(),
        }
    }
}
