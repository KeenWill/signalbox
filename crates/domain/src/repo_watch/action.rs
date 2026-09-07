//! Repository watch action for `docs/spec/repo-watch.md`.

use crate::SessionTemplateName;

/// Configured version-one rule action before a triggering event exists.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RepoWatchRuleActionV1 {
    DispatchSession { template: SessionTemplateName },
}

impl RepoWatchRuleActionV1 {
    pub const fn template(&self) -> &SessionTemplateName {
        match self {
            Self::DispatchSession { template } => template,
        }
    }
}
