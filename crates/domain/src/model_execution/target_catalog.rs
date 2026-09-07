//! Model-call target catalog for `docs/spec/model-call-execution.md`.

use crate::{DirectModelSelection, FrozenModelSelection, ResolvedProviderTarget};
use std::collections::BTreeMap;

/// One immutable configured direct-selection to exact-target definition.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ModelTargetDefinition {
    selection: DirectModelSelection,
    target: ResolvedProviderTarget,
}

impl ModelTargetDefinition {
    /// Associates one immutable direct-selection key with its exact target.
    pub const fn new(selection: DirectModelSelection, target: ResolvedProviderTarget) -> Self {
        Self { selection, target }
    }

    /// Returns the immutable selection key.
    pub const fn selection(&self) -> DirectModelSelection {
        self.selection
    }

    /// Returns the exact configured target.
    pub const fn target(&self) -> ResolvedProviderTarget {
        self.target
    }
}

/// Immutable domain projection of configured direct-selection targets.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelTargetCatalog {
    targets: BTreeMap<DirectModelSelection, ResolvedProviderTarget>,
}

impl ModelTargetCatalog {
    /// Constructs a catalog, rejecting a repeated direct-selection key.
    pub fn try_from_definitions(
        definitions: impl IntoIterator<Item = ModelTargetDefinition>,
    ) -> Result<Self, ModelTargetCatalogError> {
        let mut targets = BTreeMap::new();
        for definition in definitions {
            if targets
                .insert(definition.selection, definition.target)
                .is_some()
            {
                return Err(ModelTargetCatalogError::DuplicateSelection {
                    selection: definition.selection,
                });
            }
        }
        Ok(Self { targets })
    }

    /// Resolves exactly the direct key frozen into a direct or alias request.
    pub fn resolve(
        &self,
        selection: FrozenModelSelection,
    ) -> Result<ResolvedModelSelection, ModelTargetResolutionError> {
        let direct = match selection {
            FrozenModelSelection::Direct(direct) => direct,
            FrozenModelSelection::FrozenAlias { definition, .. } => definition.selected(),
        };
        let Some(target) = self.targets.get(&direct).copied() else {
            return Err(ModelTargetResolutionError { selection, direct });
        };
        Ok(ResolvedModelSelection { selection, target })
    }
}

/// Why configured model targets could not form one catalog.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModelTargetCatalogError {
    /// The same immutable direct-selection key appeared twice.
    DuplicateSelection {
        /// The duplicated selection.
        selection: DirectModelSelection,
    },
}

/// A frozen selection whose exact target was resolved from the catalog.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ResolvedModelSelection {
    selection: FrozenModelSelection,
    pub(super) target: ResolvedProviderTarget,
}

impl ResolvedModelSelection {
    /// Returns the exact frozen requested selection.
    pub const fn selection(&self) -> FrozenModelSelection {
        self.selection
    }

    /// Returns the exact resolved target.
    pub const fn target(&self) -> ResolvedProviderTarget {
        self.target
    }
}

/// A frozen selection unavailable in the immutable configured catalog.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ModelTargetResolutionError {
    selection: FrozenModelSelection,
    direct: DirectModelSelection,
}

impl ModelTargetResolutionError {
    /// Returns the unresolved frozen selection.
    pub const fn selection(&self) -> FrozenModelSelection {
        self.selection
    }

    /// Returns the exact direct key whose target was unavailable.
    pub const fn direct_selection(&self) -> DirectModelSelection {
        self.direct
    }
}
