//! Exact per-model settings capabilities used during request preparation.

use std::collections::BTreeSet;

use crate::{FastMode, ModelSettings, ReasoningLevel, ResolvedTarget, ServiceTier};

/// How one exact runtime target implements fast mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FastModeTarget {
    /// Fast mode is a request control on the selected target.
    SameTarget,
    /// Fast mode uses this separately declared serving target.
    Mapped(ResolvedTarget),
}

/// Exact settings capabilities for one runtime target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelCapabilities {
    reasoning_levels: BTreeSet<ReasoningLevel>,
    fast_mode: Option<FastModeTarget>,
    service_tiers: BTreeSet<ServiceTier>,
}

impl ModelCapabilities {
    /// Constructs a capability record from exact duplicate-free sets.
    pub const fn new(
        reasoning_levels: BTreeSet<ReasoningLevel>,
        fast_mode: Option<FastModeTarget>,
        service_tiers: BTreeSet<ServiceTier>,
    ) -> Self {
        Self {
            reasoning_levels,
            fast_mode,
            service_tiers,
        }
    }

    /// Borrows supported reasoning levels.
    pub const fn reasoning_levels(&self) -> &BTreeSet<ReasoningLevel> {
        &self.reasoning_levels
    }

    /// Borrows the fast-mode implementation, when supported.
    pub const fn fast_mode(&self) -> Option<&FastModeTarget> {
        self.fast_mode.as_ref()
    }

    /// Borrows supported service tiers.
    pub const fn service_tiers(&self) -> &BTreeSet<ServiceTier> {
        &self.service_tiers
    }

    /// Returns the declared effective provider target and the fast-mode value
    /// that remains to be emitted as a request control.
    ///
    /// A mapped target implements fast mode by serving identity, so its
    /// request-control value is disabled after the target is selected.
    pub fn effective_target<'a>(
        &'a self,
        selected: &'a ResolvedTarget,
        fast_mode: FastMode,
    ) -> Result<(&'a ResolvedTarget, FastMode), ModelCapabilityError> {
        match (fast_mode, &self.fast_mode) {
            (FastMode::Disabled, _) => Ok((selected, FastMode::Disabled)),
            (FastMode::Enabled, Some(FastModeTarget::SameTarget)) => {
                Ok((selected, FastMode::Enabled))
            }
            (FastMode::Enabled, Some(FastModeTarget::Mapped(target))) => {
                Ok((target, FastMode::Disabled))
            }
            (FastMode::Enabled, None) => Err(ModelCapabilityError::UnsupportedFastMode),
        }
    }
}

/// One exact target and its capability record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelCapabilityDefinition {
    target: ResolvedTarget,
    capabilities: ModelCapabilities,
}

impl ModelCapabilityDefinition {
    /// Associates one exact target with its capabilities.
    pub const fn new(target: ResolvedTarget, capabilities: ModelCapabilities) -> Self {
        Self {
            target,
            capabilities,
        }
    }

    /// Borrows the exact target.
    pub const fn target(&self) -> &ResolvedTarget {
        &self.target
    }

    /// Borrows the target's capabilities.
    pub const fn capabilities(&self) -> &ModelCapabilities {
        &self.capabilities
    }
}

/// Immutable runtime capability catalog keyed by exact target spelling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelCapabilityCatalog {
    definitions: Box<[ModelCapabilityDefinition]>,
}

impl ModelCapabilityCatalog {
    /// Constructs an empty catalog for callers that use provider defaults.
    pub fn empty() -> Self {
        Self {
            definitions: Box::new([]),
        }
    }

    /// Constructs a catalog and rejects a repeated exact target.
    pub fn try_from_definitions(
        definitions: impl IntoIterator<Item = ModelCapabilityDefinition>,
    ) -> Result<Self, ModelCapabilityCatalogError> {
        let mut definitions = definitions.into_iter().collect::<Vec<_>>();
        let self_mapped = definitions.iter().find_map(|definition| {
            matches!(
                definition.capabilities.fast_mode(),
                Some(FastModeTarget::Mapped(target)) if target == &definition.target
            )
            .then(|| definition.target.clone())
        });
        if let Some(target) = self_mapped {
            return Err(ModelCapabilityCatalogError::SelfMappedFastTarget { target });
        }
        definitions.sort_by(|left, right| left.target.as_str().cmp(right.target.as_str()));
        let duplicate = definitions
            .windows(2)
            .find(|pair| pair[0].target == pair[1].target)
            .map(|pair| pair[0].target.clone());
        if let Some(target) = duplicate {
            return Err(ModelCapabilityCatalogError::DuplicateTarget { target });
        }
        Ok(Self {
            definitions: definitions.into_boxed_slice(),
        })
    }

    /// Looks up one exact target.
    pub fn resolve(&self, target: &ResolvedTarget) -> Option<&ModelCapabilities> {
        self.definitions
            .binary_search_by(|definition| definition.target.as_str().cmp(target.as_str()))
            .ok()
            .map(|index| &self.definitions[index].capabilities)
    }

    /// Validates explicit settings against the exact target record.
    pub fn validate(
        &self,
        target: &ResolvedTarget,
        settings: &ModelSettings,
    ) -> Result<&ModelCapabilities, ModelCapabilityError> {
        let capabilities =
            self.resolve(target)
                .ok_or_else(|| ModelCapabilityError::UnknownTarget {
                    target: target.clone(),
                })?;
        if let Some(reasoning_level) = settings.reasoning_level
            && !capabilities.reasoning_levels.contains(&reasoning_level)
        {
            return Err(ModelCapabilityError::UnsupportedReasoningLevel { reasoning_level });
        }
        if settings.fast_mode == FastMode::Enabled && capabilities.fast_mode.is_none() {
            return Err(ModelCapabilityError::UnsupportedFastMode);
        }
        if let Some(service_tier) = settings.service_tier
            && !capabilities.service_tiers.contains(&service_tier)
        {
            return Err(ModelCapabilityError::UnsupportedServiceTier { service_tier });
        }
        Ok(capabilities)
    }

    /// Validates catalog-governed controls when the operation sets one.
    ///
    /// Operations carrying only provider defaults need no catalog entry;
    /// every explicit provider control requires the exact target record.
    pub fn validate_explicit(
        &self,
        target: &ResolvedTarget,
        settings: &ModelSettings,
    ) -> Result<Option<&ModelCapabilities>, ModelCapabilityError> {
        if settings.has_explicit_provider_controls() {
            self.validate(target, settings).map(Some)
        } else {
            Ok(None)
        }
    }

    /// Iterates in canonical target-spelling order.
    pub fn iter(&self) -> impl Iterator<Item = &ModelCapabilityDefinition> {
        self.definitions.iter()
    }
}

impl Default for ModelCapabilityCatalog {
    fn default() -> Self {
        Self::empty()
    }
}

/// Capability catalog construction or request-validation failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelCapabilityError {
    /// The operation target has no capability record.
    UnknownTarget {
        /// Exact unconfigured target.
        target: ResolvedTarget,
    },
    /// The requested reasoning level is unsupported.
    UnsupportedReasoningLevel {
        /// Unsupported reasoning level.
        reasoning_level: ReasoningLevel,
    },
    /// Enabled fast mode is unsupported.
    UnsupportedFastMode,
    /// The requested service tier is unsupported.
    UnsupportedServiceTier {
        /// Unsupported service tier.
        service_tier: ServiceTier,
    },
}

impl std::fmt::Display for ModelCapabilityError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownTarget { target } => write!(
                formatter,
                "exact runtime target {:?} has no model-capability record",
                target.as_str()
            ),
            Self::UnsupportedReasoningLevel { reasoning_level } => write!(
                formatter,
                "explicit reasoning level {} is unsupported by the exact runtime target",
                reasoning_level_label(*reasoning_level)
            ),
            Self::UnsupportedFastMode => {
                formatter.write_str("fast mode is unsupported by the exact runtime target")
            }
            Self::UnsupportedServiceTier { service_tier } => write!(
                formatter,
                "explicit service tier {} is unsupported by the exact runtime target",
                service_tier_label(*service_tier)
            ),
        }
    }
}

fn reasoning_level_label(level: ReasoningLevel) -> &'static str {
    match level {
        ReasoningLevel::None => "none",
        ReasoningLevel::Minimal => "minimal",
        ReasoningLevel::Low => "low",
        ReasoningLevel::Medium => "medium",
        ReasoningLevel::High => "high",
        ReasoningLevel::XHigh => "xhigh",
        ReasoningLevel::Max => "max",
        ReasoningLevel::Ultra => "ultra",
    }
}

fn service_tier_label(tier: ServiceTier) -> &'static str {
    match tier {
        ServiceTier::Anthropic(crate::AnthropicServiceTier::Auto) => "anthropic:auto",
        ServiceTier::Anthropic(crate::AnthropicServiceTier::StandardOnly) => {
            "anthropic:standard_only"
        }
        ServiceTier::OpenAi(crate::OpenAiServiceTier::Auto) => "openai:auto",
        ServiceTier::OpenAi(crate::OpenAiServiceTier::Default) => "openai:default",
        ServiceTier::OpenAi(crate::OpenAiServiceTier::Flex) => "openai:flex",
        ServiceTier::OpenAi(crate::OpenAiServiceTier::Scale) => "openai:scale",
        ServiceTier::OpenAi(crate::OpenAiServiceTier::Priority) => "openai:priority",
        ServiceTier::OpenAi(crate::OpenAiServiceTier::Fast) => "openai:fast",
        ServiceTier::CodexCli(crate::CodexCliServiceTier::Default) => "codex_cli:default",
        ServiceTier::CodexCli(crate::CodexCliServiceTier::Priority) => "codex_cli:priority",
        ServiceTier::CodexCli(crate::CodexCliServiceTier::Flex) => "codex_cli:flex",
    }
}

impl std::error::Error for ModelCapabilityError {}

/// Why capability definitions could not form one catalog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelCapabilityCatalogError {
    /// One exact target appeared more than once.
    DuplicateTarget {
        /// Duplicated target.
        target: ResolvedTarget,
    },
    /// A mapped fast target repeated the selected target.
    SelfMappedFastTarget {
        /// Target that must use the same-target capability variant.
        target: ResolvedTarget,
    },
}

impl std::fmt::Display for ModelCapabilityCatalogError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DuplicateTarget { .. } => {
                formatter.write_str("model capability catalog contains a duplicate target")
            }
            Self::SelfMappedFastTarget { .. } => {
                formatter.write_str("model capability catalog maps fast mode to its serving target")
            }
        }
    }
}

impl std::error::Error for ModelCapabilityCatalogError {}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use expect_test::expect;

    use super::{
        FastModeTarget, ModelCapabilities, ModelCapabilityCatalog, ModelCapabilityDefinition,
        ModelCapabilityError,
    };
    use crate::{
        FastMode, ModelSettings, OpenAiServiceTier, ReasoningLevel, ResolvedTarget, ServiceTier,
    };

    fn catalog() -> ModelCapabilityCatalog {
        ModelCapabilityCatalog::try_from_definitions([ModelCapabilityDefinition::new(
            ResolvedTarget::new("fixture-model"),
            ModelCapabilities::new(
                BTreeSet::from([ReasoningLevel::Low, ReasoningLevel::High]),
                Some(FastModeTarget::SameTarget),
                BTreeSet::new(),
            ),
        )])
        .expect("the fixture catalog has one exact target")
    }

    /// S37: validation uses the exact target record and rejects an
    /// unsupported explicit level before an adapter can prepare traffic.
    #[test]
    fn s37_exact_target_capability_rejects_unsupported_reasoning() {
        let mut settings = ModelSettings::new(128);
        settings.reasoning_level = Some(ReasoningLevel::Medium);

        let error = catalog()
            .validate(&ResolvedTarget::new("fixture-model"), &settings)
            .expect_err("medium is absent from the exact fixture set");

        assert_eq!(
            error,
            ModelCapabilityError::UnsupportedReasoningLevel {
                reasoning_level: ReasoningLevel::Medium,
            }
        );
    }

    /// S37: a mapped fast target is returned only from its exact
    /// declared capability record.
    #[test]
    fn s37_capability_returns_only_the_declared_fast_target() {
        let selected = ResolvedTarget::new("fixture-standard");
        let mapped = ResolvedTarget::new("fixture-fast");
        let capabilities = ModelCapabilities::new(
            BTreeSet::new(),
            Some(FastModeTarget::Mapped(mapped.clone())),
            BTreeSet::new(),
        );
        let same_target_capabilities = ModelCapabilities::new(
            BTreeSet::new(),
            Some(FastModeTarget::SameTarget),
            BTreeSet::new(),
        );

        assert_eq!(
            capabilities.effective_target(&selected, FastMode::Enabled),
            Ok((&mapped, FastMode::Disabled))
        );
        assert_eq!(
            capabilities.effective_target(&selected, FastMode::Disabled),
            Ok((&selected, FastMode::Disabled))
        );
        assert_eq!(
            same_target_capabilities.effective_target(&selected, FastMode::Enabled),
            Ok((&selected, FastMode::Enabled))
        );
    }

    #[test]
    fn capability_errors_name_the_safe_offending_value() {
        let unknown = ModelCapabilityError::UnknownTarget {
            target: ResolvedTarget::new("fixture-unknown"),
        };
        let reasoning = ModelCapabilityError::UnsupportedReasoningLevel {
            reasoning_level: ReasoningLevel::Ultra,
        };
        let tier = ModelCapabilityError::UnsupportedServiceTier {
            service_tier: ServiceTier::OpenAi(OpenAiServiceTier::Priority),
        };

        expect!["exact runtime target \"fixture-unknown\" has no model-capability record"]
            .assert_eq(&unknown.to_string());
        expect!["explicit reasoning level ultra is unsupported by the exact runtime target"]
            .assert_eq(&reasoning.to_string());
        expect!["explicit service tier openai:priority is unsupported by the exact runtime target"]
            .assert_eq(&tier.to_string());
    }

    /// S37: a same-target request control cannot masquerade as a
    /// distinct serving-identity mapping.
    #[test]
    fn s37_capability_catalog_rejects_self_mapped_fast_target() {
        let selected = ResolvedTarget::new("fixture-model");
        let definition = ModelCapabilityDefinition::new(
            selected.clone(),
            ModelCapabilities::new(
                BTreeSet::new(),
                Some(FastModeTarget::Mapped(selected.clone())),
                BTreeSet::new(),
            ),
        );

        assert_eq!(
            ModelCapabilityCatalog::try_from_definitions([definition]),
            Err(super::ModelCapabilityCatalogError::SelfMappedFastTarget { target: selected })
        );
    }
}
