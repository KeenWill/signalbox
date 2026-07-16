//! Baseline model-selection configuration.
//!
//! ADR-0027 (`docs/decisions/0027-input-delivery-lifecycle.md`) is the
//! normative specification. The first implementable effective configuration
//! is deliberately model-selection-only: one frozen direct or alias model
//! selection, provider-default parameters, and disabled known-provider-failure
//! retry and model fallback. Custom parameters, instructions, tool
//! enablement, placement constraints, per-turn resources, and
//! interpreting-policy selections are unavailable baseline capabilities, not
//! latent optional fields.
//!
//! # Scope
//!
//! This module defines pure configuration values. It omits input acceptance
//! transactions, command deduplication, selection of the current alias
//! definition from mutable state, exact provider/model target resolution
//! (ADR-0005 pins the target as a separate turn fact), and storage, wire,
//! deployment-key, and display encodings. Aggregate transitions and boundary
//! code own those ADR-0027 requirements.

crate::define_identity!(
    /// Names exactly one configured provider/model selection as a canonical
    /// domain-owned key with immutable semantic meaning.
    ///
    /// Deployment may make the selection unavailable, causing resolution
    /// failure, but cannot retarget the same key. It is never an alias, a
    /// policy, a fallback set, a provider-native unnormalized identifier, or
    /// a provider-reported identity.
    DirectModelSelection
);

crate::define_identity!(
    /// Names one owner-configured model alias whose definition can change
    /// over time.
    ///
    /// Selecting an alias freezes its current definition at acceptance; the
    /// alias key itself carries no target.
    ModelAlias
);

/// The immutable frozen form of an alias definition.
///
/// A frozen definition selects exactly one [`DirectModelSelection`].
/// Resolution later validates that frozen selection and pins one exact
/// target or fails; it cannot reread mutable alias policy to choose another
/// selection.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct FrozenAliasDefinition {
    selected: DirectModelSelection,
}

impl FrozenAliasDefinition {
    /// Freezes a definition that selects exactly one direct selection.
    pub const fn selecting(selected: DirectModelSelection) -> Self {
        Self { selected }
    }

    /// Returns the exact direct selection this definition selects.
    pub const fn selected(&self) -> DirectModelSelection {
        self.selected
    }
}

/// One complete normalized model-selection request.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ModelSelectionRequest {
    /// Request one canonical direct provider/model selection.
    Direct(DirectModelSelection),
    /// Request whatever the named alias means at acceptance time.
    Alias(ModelAlias),
}

/// A model selection whose semantic meaning is frozen.
///
/// Direct and alias selections remain semantically unequal even when they
/// resolve to the same exact target, because requested selection and alias
/// provenance differ.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum FrozenModelSelection {
    /// A canonical direct selection.
    Direct(DirectModelSelection),
    /// An alias together with the immutable definition frozen at acceptance.
    FrozenAlias {
        /// The requested alias.
        alias: ModelAlias,
        /// The definition version frozen for this selection.
        definition: FrozenAliasDefinition,
    },
}

/// The single constructible baseline model-parameter choice: Signalbox
/// supplies no model-parameter overrides.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum ModelParameters {
    /// Provider defaults with no overrides.
    ProviderDefaults,
}

/// The single constructible baseline known-provider-failure retry choice.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum KnownProviderFailureRetry {
    /// No automatic retry after a known provider failure.
    Disabled,
}

/// The single constructible baseline model-fallback choice.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum ModelFallback {
    /// No automatic model substitution.
    Disabled,
}

/// The complete frozen baseline effective configuration for one turn.
///
/// Equality is structural semantic value equality over the frozen model
/// selection and the unit policy values; any model-selection difference
/// requires new logical work. The exact provider/model target is not a
/// field: ADR-0005 pins it as a separate turn fact before the first model
/// call is created.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct EffectiveConfiguration {
    model: FrozenModelSelection,
    parameters: ModelParameters,
    known_provider_failure_retry: KnownProviderFailureRetry,
    model_fallback: ModelFallback,
}

impl EffectiveConfiguration {
    /// Constructs the complete baseline value for a frozen model selection.
    pub const fn baseline(model: FrozenModelSelection) -> Self {
        Self {
            model,
            parameters: ModelParameters::ProviderDefaults,
            known_provider_failure_retry: KnownProviderFailureRetry::Disabled,
            model_fallback: ModelFallback::Disabled,
        }
    }

    /// Borrows the frozen model selection.
    pub const fn model(&self) -> &FrozenModelSelection {
        &self.model
    }

    /// Returns the model-parameter choice.
    pub const fn parameters(&self) -> ModelParameters {
        self.parameters
    }

    /// Returns the known-provider-failure retry choice.
    pub const fn known_provider_failure_retry(&self) -> KnownProviderFailureRetry {
        self.known_provider_failure_retry
    }

    /// Returns the model-fallback choice.
    pub const fn model_fallback(&self) -> ModelFallback {
        self.model_fallback
    }
}

#[cfg(test)]
mod tests {
    use super::{
        DirectModelSelection, EffectiveConfiguration, FrozenAliasDefinition, FrozenModelSelection,
        KnownProviderFailureRetry, ModelAlias, ModelFallback, ModelParameters,
        ModelSelectionRequest,
    };
    use uuid::Uuid;

    fn direct(value: u128) -> DirectModelSelection {
        DirectModelSelection::from_uuid(Uuid::from_u128(value))
    }

    fn alias(value: u128) -> ModelAlias {
        ModelAlias::from_uuid(Uuid::from_u128(value))
    }

    #[test]
    fn selection_keys_expose_their_uuid_values() {
        let uuid = Uuid::from_u128(1);

        assert_eq!(direct(1), DirectModelSelection::from_uuid(uuid));
        assert_ne!(direct(1), direct(2));
        assert_eq!(direct(1).as_uuid(), &uuid);
        assert_eq!(alias(1).into_uuid(), uuid);
        assert_ne!(alias(1), alias(2));
    }

    #[test]
    fn frozen_alias_definition_selects_exactly_one_direct_selection() {
        let definition = FrozenAliasDefinition::selecting(direct(1));

        assert_eq!(definition.selected(), direct(1));
        assert_ne!(definition, FrozenAliasDefinition::selecting(direct(2)));
    }

    /// INV-008: comparison uses constructible semantic values; a direct
    /// request and an alias request remain distinct.
    #[test]
    fn direct_and_alias_requests_remain_semantically_distinct() {
        assert_ne!(
            ModelSelectionRequest::Direct(direct(1)),
            ModelSelectionRequest::Alias(alias(1))
        );
    }

    /// INV-008: direct and alias selections remain semantically unequal even
    /// when they resolve to the same exact target.
    #[test]
    fn frozen_direct_and_frozen_alias_selecting_the_same_target_remain_unequal() {
        let target = direct(1);
        let frozen_alias = FrozenModelSelection::FrozenAlias {
            alias: alias(2),
            definition: FrozenAliasDefinition::selecting(target),
        };

        assert_ne!(FrozenModelSelection::Direct(target), frozen_alias);
    }

    /// INV-008: alias provenance is part of the frozen selection's semantic
    /// value.
    #[test]
    fn frozen_aliases_with_different_provenance_remain_unequal() {
        let definition = FrozenAliasDefinition::selecting(direct(1));
        let first = FrozenModelSelection::FrozenAlias {
            alias: alias(2),
            definition,
        };
        let second = FrozenModelSelection::FrozenAlias {
            alias: alias(3),
            definition,
        };

        assert_ne!(first, second);
    }

    #[test]
    fn baseline_effective_configuration_fixes_the_unit_policy_values() {
        let configuration =
            EffectiveConfiguration::baseline(FrozenModelSelection::Direct(direct(1)));

        assert_eq!(
            configuration.model(),
            &FrozenModelSelection::Direct(direct(1))
        );
        assert_eq!(
            configuration.parameters(),
            ModelParameters::ProviderDefaults
        );
        assert_eq!(
            configuration.known_provider_failure_retry(),
            KnownProviderFailureRetry::Disabled
        );
        assert_eq!(configuration.model_fallback(), ModelFallback::Disabled);
    }

    /// INV-008: configuration equality is structural semantic value equality
    /// over the frozen model selection and the unit policy values.
    #[test]
    fn effective_configuration_equality_is_structural_over_the_frozen_selection() {
        let selection = FrozenModelSelection::FrozenAlias {
            alias: alias(1),
            definition: FrozenAliasDefinition::selecting(direct(2)),
        };

        assert_eq!(
            EffectiveConfiguration::baseline(selection),
            EffectiveConfiguration::baseline(selection)
        );
        assert_ne!(
            EffectiveConfiguration::baseline(selection),
            EffectiveConfiguration::baseline(FrozenModelSelection::Direct(direct(2)))
        );
    }
}
