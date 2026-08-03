//! Provider-neutral model settings, capability declarations, and compatibility.
//!
//! The normative contract is `docs/spec/model-session-settings.md`. These are
//! pure values: deployment parsing, persistence, wire encoding, and provider
//! translation remain boundary responsibilities.

use std::collections::{BTreeMap, BTreeSet};

use crate::{
    AcceptedInputId, DirectModelSelection, DurableCommandId, FrozenModelSelection,
    ModelSelectionRequest, ResolvedProviderTarget, SessionConfigurationDefaultsVersion, SessionId,
    TurnId,
};

/// Provider-neutral reasoning effort in ascending spend/depth order.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum ReasoningLevel {
    /// Explicitly request no reasoning effort.
    None,
    /// The smallest provider reasoning effort above none.
    Minimal,
    /// Low reasoning effort.
    Low,
    /// Medium reasoning effort.
    Medium,
    /// High reasoning effort.
    High,
    /// Extra-high reasoning effort.
    XHigh,
    /// Maximum reasoning effort.
    Max,
    /// Codex Ultra reasoning effort.
    Ultra,
}

/// Whether the provider's fast request/session control is selected.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum FastMode {
    /// Use ordinary serving speed.
    Disabled,
    /// Request the provider's fast serving mode.
    Enabled,
}

/// Anthropic service-tier values.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum AnthropicServiceTier {
    /// Use Priority capacity when available.
    Auto,
    /// Use only standard capacity.
    StandardOnly,
}

/// OpenAI service-tier values.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum OpenAiServiceTier {
    /// Use the project-configured automatic tier.
    Auto,
    /// Use default processing.
    Default,
    /// Use Flex processing.
    Flex,
    /// Use Scale processing.
    Scale,
    /// Use Priority processing.
    Priority,
    /// Use Fast processing.
    Fast,
}

/// Codex CLI service-tier values.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum CodexCliServiceTier {
    /// Use default processing.
    Default,
    /// Use Priority processing.
    Priority,
    /// Use Flex processing.
    Flex,
}

/// Provider-tagged service tier.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum ServiceTier {
    /// Anthropic Messages service tier.
    Anthropic(AnthropicServiceTier),
    /// OpenAI service tier.
    OpenAi(OpenAiServiceTier),
    /// Codex CLI service tier.
    CodexCli(CodexCliServiceTier),
}

/// One setting contribution at a precedence layer.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum SettingOverlay<T> {
    /// Consult the next lower-precedence layer.
    Inherit,
    /// Explicitly select the provider default and stop resolution.
    ProviderDefault,
    /// Explicitly select this value and stop resolution.
    Value(T),
}

/// The three model-setting contributions at one precedence layer.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ModelSettingsOverlay {
    reasoning_level: SettingOverlay<ReasoningLevel>,
    fast_mode: SettingOverlay<FastMode>,
    service_tier: SettingOverlay<ServiceTier>,
}

impl ModelSettingsOverlay {
    /// An overlay that inherits every setting.
    pub const fn inherit_all() -> Self {
        Self {
            reasoning_level: SettingOverlay::Inherit,
            fast_mode: SettingOverlay::Inherit,
            service_tier: SettingOverlay::Inherit,
        }
    }

    /// Constructs a complete labeled overlay.
    pub const fn new(
        reasoning_level: SettingOverlay<ReasoningLevel>,
        fast_mode: SettingOverlay<FastMode>,
        service_tier: SettingOverlay<ServiceTier>,
    ) -> Self {
        Self {
            reasoning_level,
            fast_mode,
            service_tier,
        }
    }

    /// States every member of a complete effective value at this layer.
    pub const fn from_effective(settings: EffectiveModelSettings) -> Self {
        Self {
            reasoning_level: match settings.reasoning_level {
                Some(value) => SettingOverlay::Value(value),
                None => SettingOverlay::ProviderDefault,
            },
            fast_mode: SettingOverlay::Value(settings.fast_mode),
            service_tier: match settings.service_tier {
                Some(value) => SettingOverlay::Value(value),
                None => SettingOverlay::ProviderDefault,
            },
        }
    }

    /// Returns the reasoning contribution.
    pub const fn reasoning_level(&self) -> SettingOverlay<ReasoningLevel> {
        self.reasoning_level
    }

    /// Returns the fast-mode contribution.
    pub const fn fast_mode(&self) -> SettingOverlay<FastMode> {
        self.fast_mode
    }

    /// Returns the service-tier contribution.
    pub const fn service_tier(&self) -> SettingOverlay<ServiceTier> {
        self.service_tier
    }
}

impl Default for ModelSettingsOverlay {
    fn default() -> Self {
        Self::inherit_all()
    }
}

/// Complete effective model settings after precedence resolution.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct EffectiveModelSettings {
    reasoning_level: Option<ReasoningLevel>,
    fast_mode: FastMode,
    service_tier: Option<ServiceTier>,
}

impl EffectiveModelSettings {
    /// Provider-default reasoning/tier and ordinary speed.
    pub const fn provider_defaults() -> Self {
        Self {
            reasoning_level: None,
            fast_mode: FastMode::Disabled,
            service_tier: None,
        }
    }

    /// Constructs one complete effective value.
    pub const fn new(
        reasoning_level: Option<ReasoningLevel>,
        fast_mode: FastMode,
        service_tier: Option<ServiceTier>,
    ) -> Self {
        Self {
            reasoning_level,
            fast_mode,
            service_tier,
        }
    }

    /// Returns the explicit reasoning level, or provider default when absent.
    pub const fn reasoning_level(&self) -> Option<ReasoningLevel> {
        self.reasoning_level
    }

    /// Returns the effective fast mode.
    pub const fn fast_mode(&self) -> FastMode {
        self.fast_mode
    }

    /// Returns the explicit service tier, or provider default when absent.
    pub const fn service_tier(&self) -> Option<ServiceTier> {
        self.service_tier
    }
}

impl Default for EffectiveModelSettings {
    fn default() -> Self {
        Self::provider_defaults()
    }
}

/// The precedence layer that supplied one effective setting.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ModelSettingSource {
    /// Per-call override.
    PerCall,
    /// Durable session override.
    Session,
    /// Selected model's named settings profile.
    Profile,
    /// Deployment global default.
    GlobalDefault,
}

/// A complete resolved value plus per-knob provenance.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ResolvedModelSettings {
    effective: EffectiveModelSettings,
    reasoning_source: Option<ModelSettingSource>,
    fast_mode_source: Option<ModelSettingSource>,
    service_tier_source: Option<ModelSettingSource>,
}

/// Complete settings together with the direct selection that validated them.
///
/// Provider defaults are model-independent and carry no validation selection.
/// Every non-default installed value is constructed with the exact direct
/// selection whose capability record admitted it.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ValidatedModelSettings {
    effective: EffectiveModelSettings,
    validated_for: Option<DirectModelSelection>,
}

impl ValidatedModelSettings {
    /// Model-independent provider defaults.
    pub const fn provider_defaults() -> Self {
        Self {
            effective: EffectiveModelSettings::provider_defaults(),
            validated_for: None,
        }
    }

    /// Binds a complete value to the exact direct selection that validated it.
    const fn for_selection(
        effective: EffectiveModelSettings,
        validated_for: DirectModelSelection,
    ) -> Self {
        Self {
            effective,
            validated_for: Some(validated_for),
        }
    }

    /// Returns the complete effective value.
    pub const fn effective(&self) -> EffectiveModelSettings {
        self.effective
    }

    /// Returns the direct selection that validated non-default settings.
    pub const fn validated_for(&self) -> Option<DirectModelSelection> {
        self.validated_for
    }
}

impl Default for ValidatedModelSettings {
    fn default() -> Self {
        Self::provider_defaults()
    }
}

impl ResolvedModelSettings {
    /// Returns the complete effective value.
    pub const fn effective(&self) -> EffectiveModelSettings {
        self.effective
    }

    /// Returns the layer that selected reasoning, if any.
    pub const fn reasoning_source(&self) -> Option<ModelSettingSource> {
        self.reasoning_source
    }

    /// Returns the layer that selected fast mode, if any.
    pub const fn fast_mode_source(&self) -> Option<ModelSettingSource> {
        self.fast_mode_source
    }

    /// Returns the layer that selected service tier, if any.
    pub const fn service_tier_source(&self) -> Option<ModelSettingSource> {
        self.service_tier_source
    }
}

/// The fixed four-layer model-settings precedence chain.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ModelSettingsPrecedence {
    per_call: ModelSettingsOverlay,
    session: ModelSettingsOverlay,
    profile: ModelSettingsOverlay,
    global_default: ModelSettingsOverlay,
}

impl ModelSettingsPrecedence {
    /// Constructs the fixed per-call, session, profile, global chain.
    pub const fn new(
        per_call: ModelSettingsOverlay,
        session: ModelSettingsOverlay,
        profile: ModelSettingsOverlay,
        global_default: ModelSettingsOverlay,
    ) -> Self {
        Self {
            per_call,
            session,
            profile,
            global_default,
        }
    }

    /// Resolves each knob independently through the fixed precedence chain.
    pub fn resolve(self) -> ResolvedModelSettings {
        let layers = [
            (ModelSettingSource::PerCall, self.per_call),
            (ModelSettingSource::Session, self.session),
            (ModelSettingSource::Profile, self.profile),
            (ModelSettingSource::GlobalDefault, self.global_default),
        ];
        let (reasoning_level, reasoning_source) =
            resolve_nullable(layers.map(|(source, settings)| (source, settings.reasoning_level)));
        let (fast_mode, fast_mode_source) =
            resolve_fast(layers.map(|(source, settings)| (source, settings.fast_mode)));
        let (service_tier, service_tier_source) =
            resolve_nullable(layers.map(|(source, settings)| (source, settings.service_tier)));
        ResolvedModelSettings {
            effective: EffectiveModelSettings::new(reasoning_level, fast_mode, service_tier),
            reasoning_source,
            fast_mode_source,
            service_tier_source,
        }
    }
}

fn resolve_nullable<T: Copy>(
    layers: impl IntoIterator<Item = (ModelSettingSource, SettingOverlay<T>)>,
) -> (Option<T>, Option<ModelSettingSource>) {
    for (source, overlay) in layers {
        match overlay {
            SettingOverlay::Inherit => {}
            SettingOverlay::ProviderDefault => return (None, Some(source)),
            SettingOverlay::Value(value) => return (Some(value), Some(source)),
        }
    }
    (None, None)
}

fn resolve_fast(
    layers: impl IntoIterator<Item = (ModelSettingSource, SettingOverlay<FastMode>)>,
) -> (FastMode, Option<ModelSettingSource>) {
    for (source, overlay) in layers {
        match overlay {
            SettingOverlay::Inherit => {}
            SettingOverlay::ProviderDefault => return (FastMode::Disabled, Some(source)),
            SettingOverlay::Value(value) => return (value, Some(source)),
        }
    }
    (FastMode::Disabled, None)
}

/// How one configured model implements fast mode.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum FastModeSupport {
    /// Fast mode is unavailable.
    Unsupported,
    /// Fast mode is a request control on the selected target.
    RequestControl,
    /// Fast mode uses this separately declared serving target.
    AlternateTarget(ResolvedProviderTarget),
}

/// Exact settings capabilities for one configured model.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelCapabilities {
    reasoning_levels: BTreeSet<ReasoningLevel>,
    fast_mode: FastModeSupport,
    service_tiers: BTreeSet<ServiceTier>,
}

impl ModelCapabilities {
    /// Constructs an exact capability record from already duplicate-free sets.
    pub const fn new(
        reasoning_levels: BTreeSet<ReasoningLevel>,
        fast_mode: FastModeSupport,
        service_tiers: BTreeSet<ServiceTier>,
    ) -> Self {
        Self {
            reasoning_levels,
            fast_mode,
            service_tiers,
        }
    }

    /// Borrows the supported reasoning-level set.
    pub const fn reasoning_levels(&self) -> &BTreeSet<ReasoningLevel> {
        &self.reasoning_levels
    }

    /// Returns the fast-mode capability.
    pub const fn fast_mode(&self) -> FastModeSupport {
        self.fast_mode
    }

    /// Borrows the supported service-tier set.
    pub const fn service_tiers(&self) -> &BTreeSet<ServiceTier> {
        &self.service_tiers
    }

    /// Validates every explicitly selected overlay value.
    pub fn validate_explicit(
        &self,
        selection: DirectModelSelection,
        overlay: ModelSettingsOverlay,
    ) -> Result<(), UnsupportedModelSetting> {
        if let SettingOverlay::Value(level) = overlay.reasoning_level
            && !self.reasoning_levels.contains(&level)
        {
            return Err(UnsupportedModelSetting::ReasoningLevel {
                selection,
                requested: level,
            });
        }
        if overlay.fast_mode == SettingOverlay::Value(FastMode::Enabled)
            && self.fast_mode == FastModeSupport::Unsupported
        {
            return Err(UnsupportedModelSetting::FastMode { selection });
        }
        if let SettingOverlay::Value(tier) = overlay.service_tier
            && !self.service_tiers.contains(&tier)
        {
            return Err(UnsupportedModelSetting::ServiceTier {
                selection,
                requested: tier,
            });
        }
        Ok(())
    }

    /// Validates a complete resolved value and returns its sealed evidence.
    pub fn validate_resolved(
        &self,
        selection: DirectModelSelection,
        resolved: ResolvedModelSettings,
    ) -> Result<ValidatedModelSettings, UnsupportedModelSetting> {
        self.validate_explicit(
            selection,
            ModelSettingsOverlay::from_effective(resolved.effective),
        )?;
        Ok(ValidatedModelSettings::for_selection(
            resolved.effective,
            selection,
        ))
    }

    /// Adjusts inherited settings made incompatible by a model change.
    pub fn adjust_for_model_change(
        &self,
        inherited: EffectiveModelSettings,
    ) -> CompatibleModelSettings {
        let mut effective = inherited;
        let mut adjustments = Vec::new();
        if let Some(requested) = inherited.reasoning_level
            && !self.reasoning_levels.contains(&requested)
        {
            let applied = self
                .reasoning_levels
                .range(..=requested)
                .next_back()
                .copied()
                .or_else(|| self.reasoning_levels.first().copied());
            effective.reasoning_level = applied;
            adjustments.push(match applied {
                Some(applied) => ModelChangeAdjustment::ReasoningLevelClamped {
                    from: requested,
                    to: applied,
                },
                None => ModelChangeAdjustment::ReasoningLevelCleared { from: requested },
            });
        }
        if inherited.fast_mode == FastMode::Enabled
            && self.fast_mode == FastModeSupport::Unsupported
        {
            effective.fast_mode = FastMode::Disabled;
            adjustments.push(ModelChangeAdjustment::FastModeDisabled);
        }
        if let Some(requested) = inherited.service_tier
            && !self.service_tiers.contains(&requested)
        {
            effective.service_tier = None;
            adjustments.push(ModelChangeAdjustment::ServiceTierCleared { from: requested });
        }
        CompatibleModelSettings {
            effective,
            adjustments: adjustments.into_boxed_slice(),
        }
    }

    /// Selects the capability-authorized serving target for fast mode.
    pub const fn serving_target(
        &self,
        selected: ResolvedProviderTarget,
        fast_mode: FastMode,
    ) -> Option<ResolvedProviderTarget> {
        match (fast_mode, self.fast_mode) {
            (FastMode::Disabled, _) | (FastMode::Enabled, FastModeSupport::RequestControl) => {
                Some(selected)
            }
            (FastMode::Enabled, FastModeSupport::AlternateTarget(target)) => Some(target),
            (FastMode::Enabled, FastModeSupport::Unsupported) => None,
        }
    }
}

/// One explicit setting unsupported by the selected model.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UnsupportedModelSetting {
    /// The explicit reasoning level is absent from the model's set.
    ReasoningLevel {
        /// Selected direct model.
        selection: DirectModelSelection,
        /// Unsupported requested level.
        requested: ReasoningLevel,
    },
    /// Enabled fast mode is unsupported.
    FastMode {
        /// Selected direct model.
        selection: DirectModelSelection,
    },
    /// The explicit service tier is absent from the model's set.
    ServiceTier {
        /// Selected direct model.
        selection: DirectModelSelection,
        /// Unsupported requested tier.
        requested: ServiceTier,
    },
}

impl std::fmt::Display for UnsupportedModelSetting {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("the selected model does not support an explicitly requested setting")
    }
}

impl std::error::Error for UnsupportedModelSetting {}

/// One automatic adjustment caused by changing model capability.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ModelChangeAdjustment {
    /// Reasoning moved to a supported level without increasing it.
    ReasoningLevelClamped {
        /// Prior inherited level.
        from: ReasoningLevel,
        /// Applied supported level.
        to: ReasoningLevel,
    },
    /// Reasoning cleared because the model exposes no reasoning setting.
    ReasoningLevelCleared {
        /// Prior inherited level.
        from: ReasoningLevel,
    },
    /// Fast mode became disabled.
    FastModeDisabled,
    /// Service tier cleared to provider default.
    ServiceTierCleared {
        /// Prior inherited service tier.
        from: ServiceTier,
    },
}

/// Model-change compatibility result and its ordered adjustment evidence.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompatibleModelSettings {
    effective: EffectiveModelSettings,
    adjustments: Box<[ModelChangeAdjustment]>,
}

impl CompatibleModelSettings {
    /// Returns the adjusted complete settings.
    pub const fn effective(&self) -> EffectiveModelSettings {
        self.effective
    }

    /// Borrows adjustments in reasoning, fast, service-tier order.
    pub fn adjustments(&self) -> &[ModelChangeAdjustment] {
        &self.adjustments
    }

    /// Returns the adjusted value and ordered adjustment evidence.
    pub fn into_parts(self) -> (EffectiveModelSettings, Box<[ModelChangeAdjustment]>) {
        (self.effective, self.adjustments)
    }
}

/// Durable evidence that one defaults replacement changed model settings.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct SessionModelSettingsChanged {
    session: SessionId,
    command_id: DurableCommandId,
    prior_defaults_version: SessionConfigurationDefaultsVersion,
    installed_defaults_version: SessionConfigurationDefaultsVersion,
    prior_model: ModelSelectionRequest,
    installed_model: ModelSelectionRequest,
    prior_settings: ValidatedModelSettings,
    installed_settings: ValidatedModelSettings,
    caller_override: ModelSettingsOverlay,
    adjustments: Box<[ModelChangeAdjustment]>,
}

impl SessionModelSettingsChanged {
    /// Constructs an event only for a successor epoch whose model or settings
    /// differ from the prior epoch.
    #[allow(clippy::too_many_arguments)]
    pub fn try_new(
        session: SessionId,
        command_id: DurableCommandId,
        prior_defaults_version: SessionConfigurationDefaultsVersion,
        installed_defaults_version: SessionConfigurationDefaultsVersion,
        prior_model: ModelSelectionRequest,
        installed_model: ModelSelectionRequest,
        prior_settings: ValidatedModelSettings,
        installed_settings: ValidatedModelSettings,
        caller_override: ModelSettingsOverlay,
        adjustments: Vec<ModelChangeAdjustment>,
    ) -> Option<Self> {
        let is_successor =
            prior_defaults_version.checked_next() == Some(installed_defaults_version);
        let records_change = prior_model != installed_model || prior_settings != installed_settings;
        (is_successor && records_change).then(|| Self {
            session,
            command_id,
            prior_defaults_version,
            installed_defaults_version,
            prior_model,
            installed_model,
            prior_settings,
            installed_settings,
            caller_override,
            adjustments: adjustments.into_boxed_slice(),
        })
    }

    /// Returns the affected session.
    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// Returns the durable command whose application produced the event.
    pub const fn command_id(&self) -> DurableCommandId {
        self.command_id
    }

    /// Returns the replaced defaults version.
    pub const fn prior_defaults_version(&self) -> SessionConfigurationDefaultsVersion {
        self.prior_defaults_version
    }

    /// Returns the installed successor defaults version.
    pub const fn installed_defaults_version(&self) -> SessionConfigurationDefaultsVersion {
        self.installed_defaults_version
    }

    /// Returns the prior model-selection request.
    pub const fn prior_model(&self) -> ModelSelectionRequest {
        self.prior_model
    }

    /// Returns the installed model-selection request.
    pub const fn installed_model(&self) -> ModelSelectionRequest {
        self.installed_model
    }

    /// Returns the prior complete settings snapshot.
    pub const fn prior_settings(&self) -> ValidatedModelSettings {
        self.prior_settings
    }

    /// Returns the installed complete settings snapshot.
    pub const fn installed_settings(&self) -> ValidatedModelSettings {
        self.installed_settings
    }

    /// Returns the caller's provenance-preserving settings contribution.
    pub const fn caller_override(&self) -> ModelSettingsOverlay {
        self.caller_override
    }

    /// Borrows ordered automatic model-change adjustments.
    pub fn adjustments(&self) -> &[ModelChangeAdjustment] {
        &self.adjustments
    }
}

/// Durable evidence of the settings frozen for one accepted origin turn.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct TurnModelSettingsResolved {
    accepted_input: AcceptedInputId,
    turn: TurnId,
    defaults_version: SessionConfigurationDefaultsVersion,
    selection: FrozenModelSelection,
    per_call_override: ModelSettingsOverlay,
    settings: ValidatedModelSettings,
    adjustments: Box<[ModelChangeAdjustment]>,
}

impl TurnModelSettingsResolved {
    /// Constructs an event when the settings evidence belongs to the frozen
    /// direct selection. Provider-default evidence is model-independent.
    pub fn try_new(
        accepted_input: AcceptedInputId,
        turn: TurnId,
        defaults_version: SessionConfigurationDefaultsVersion,
        selection: FrozenModelSelection,
        per_call_override: ModelSettingsOverlay,
        settings: ValidatedModelSettings,
        adjustments: Vec<ModelChangeAdjustment>,
    ) -> Option<Self> {
        let settings_selection = settings.validated_for();
        let selection_matches =
            settings_selection.is_none() || settings_selection == Some(selection.selected_direct());
        selection_matches.then(|| Self {
            accepted_input,
            turn,
            defaults_version,
            selection,
            per_call_override,
            settings,
            adjustments: adjustments.into_boxed_slice(),
        })
    }

    /// Returns the accepted input whose origin resolution produced the event.
    pub const fn accepted_input(&self) -> AcceptedInputId {
        self.accepted_input
    }

    /// Returns the origin turn identity.
    pub const fn turn(&self) -> TurnId {
        self.turn
    }

    /// Returns the exact session-defaults epoch used for resolution.
    pub const fn defaults_version(&self) -> SessionConfigurationDefaultsVersion {
        self.defaults_version
    }

    /// Borrows the frozen model selection.
    pub const fn selection(&self) -> &FrozenModelSelection {
        &self.selection
    }

    /// Returns the per-call contribution with explicit provenance intact.
    pub const fn per_call_override(&self) -> ModelSettingsOverlay {
        self.per_call_override
    }

    /// Returns the complete validated settings frozen for the turn.
    pub const fn settings(&self) -> ValidatedModelSettings {
        self.settings
    }

    /// Borrows ordered automatic alias-retarget adjustments.
    pub fn adjustments(&self) -> &[ModelChangeAdjustment] {
        &self.adjustments
    }
}

/// One direct selection and its exact settings capability record.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelCapabilityDefinition {
    selection: DirectModelSelection,
    capabilities: ModelCapabilities,
}

impl ModelCapabilityDefinition {
    /// Associates capabilities with one immutable direct selection.
    pub const fn new(selection: DirectModelSelection, capabilities: ModelCapabilities) -> Self {
        Self {
            selection,
            capabilities,
        }
    }

    /// Returns the direct selection.
    pub const fn selection(&self) -> DirectModelSelection {
        self.selection
    }

    /// Borrows the exact capabilities.
    pub const fn capabilities(&self) -> &ModelCapabilities {
        &self.capabilities
    }
}

/// Immutable capability catalog keyed by direct selection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelCapabilityCatalog {
    capabilities: BTreeMap<DirectModelSelection, ModelCapabilities>,
}

impl ModelCapabilityCatalog {
    /// Constructs a catalog and rejects duplicate selection keys.
    pub fn try_from_definitions(
        definitions: impl IntoIterator<Item = ModelCapabilityDefinition>,
    ) -> Result<Self, ModelCapabilityCatalogError> {
        let mut capabilities = BTreeMap::new();
        for definition in definitions {
            if capabilities
                .insert(definition.selection, definition.capabilities)
                .is_some()
            {
                return Err(ModelCapabilityCatalogError::DuplicateSelection {
                    selection: definition.selection,
                });
            }
        }
        Ok(Self { capabilities })
    }

    /// Looks up one direct selection's capabilities.
    pub fn resolve(&self, selection: DirectModelSelection) -> Option<&ModelCapabilities> {
        self.capabilities.get(&selection)
    }

    /// Iterates in canonical direct-selection order.
    pub fn iter(&self) -> impl Iterator<Item = (DirectModelSelection, &ModelCapabilities)> {
        self.capabilities
            .iter()
            .map(|(selection, capabilities)| (*selection, capabilities))
    }
}

/// Why capability definitions could not form one catalog.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModelCapabilityCatalogError {
    /// The same direct selection appeared twice.
    DuplicateSelection {
        /// Duplicated selection.
        selection: DirectModelSelection,
    },
}

impl std::fmt::Display for ModelCapabilityCatalogError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("model capability catalog contains a duplicate selection")
    }
}

impl std::error::Error for ModelCapabilityCatalogError {}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::{
        AnthropicServiceTier, EffectiveModelSettings, FastMode, FastModeSupport, ModelCapabilities,
        ModelChangeAdjustment, ModelSettingSource, ModelSettingsOverlay, ModelSettingsPrecedence,
        OpenAiServiceTier, ReasoningLevel, ServiceTier, SessionModelSettingsChanged,
        SettingOverlay, UnsupportedModelSetting,
    };
    use crate::test_support::{command_id, direct, provider_model_identity, session_id};
    use crate::{
        ModelSelectionRequest, ResolvedProviderTarget, SessionConfigurationDefaultsVersion,
    };

    fn capabilities(
        levels: impl IntoIterator<Item = ReasoningLevel>,
        fast_mode: FastModeSupport,
        tiers: impl IntoIterator<Item = ServiceTier>,
    ) -> ModelCapabilities {
        ModelCapabilities::new(
            BTreeSet::from_iter(levels),
            fast_mode,
            BTreeSet::from_iter(tiers),
        )
    }

    /// S37 / INV-051: each knob resolves independently through per-call,
    /// session, profile, then global precedence, and an explicit provider
    /// default stops lower-layer inheritance.
    #[test]
    fn s37_inv051_resolves_the_fixed_precedence_chain_with_explicit_clearing() {
        let per_call = ModelSettingsOverlay::new(
            SettingOverlay::ProviderDefault,
            SettingOverlay::Inherit,
            SettingOverlay::Inherit,
        );
        let session = ModelSettingsOverlay::new(
            SettingOverlay::Value(ReasoningLevel::High),
            SettingOverlay::Value(FastMode::Enabled),
            SettingOverlay::Inherit,
        );
        let profile = ModelSettingsOverlay::new(
            SettingOverlay::Value(ReasoningLevel::Medium),
            SettingOverlay::Value(FastMode::Disabled),
            SettingOverlay::Value(ServiceTier::OpenAi(OpenAiServiceTier::Priority)),
        );
        let global = ModelSettingsOverlay::new(
            SettingOverlay::Value(ReasoningLevel::Low),
            SettingOverlay::Value(FastMode::Disabled),
            SettingOverlay::Value(ServiceTier::Anthropic(AnthropicServiceTier::Auto)),
        );

        let resolved = ModelSettingsPrecedence::new(per_call, session, profile, global).resolve();

        assert_eq!(resolved.effective().reasoning_level(), None);
        assert_eq!(
            resolved.reasoning_source(),
            Some(ModelSettingSource::PerCall)
        );
        assert_eq!(resolved.effective().fast_mode(), FastMode::Enabled);
        assert_eq!(
            resolved.fast_mode_source(),
            Some(ModelSettingSource::Session)
        );
        assert_eq!(
            resolved.effective().service_tier(),
            Some(ServiceTier::OpenAi(OpenAiServiceTier::Priority))
        );
        assert_eq!(
            resolved.service_tier_source(),
            Some(ModelSettingSource::Profile)
        );
    }

    /// S37 / INV-051: an explicit unsupported level is a typed error rather
    /// than delegated to an open provider enum or silent clamp.
    #[test]
    fn s37_inv051_explicit_unsupported_reasoning_is_rejected() {
        let selected = direct(1);
        let supported = capabilities(
            [ReasoningLevel::Low, ReasoningLevel::Medium],
            FastModeSupport::Unsupported,
            [],
        );
        let requested = ModelSettingsOverlay::new(
            SettingOverlay::Value(ReasoningLevel::High),
            SettingOverlay::Inherit,
            SettingOverlay::Inherit,
        );

        let error = supported
            .validate_explicit(selected, requested)
            .expect_err("an absent reasoning level is rejected explicitly");

        assert_eq!(
            error,
            UnsupportedModelSetting::ReasoningLevel {
                selection: selected,
                requested: ReasoningLevel::High,
            }
        );
    }

    /// S37 / INV-052: model-change incompatibility clamps reasoning downward,
    /// disables fast mode, clears an unordered tier, and records each change.
    #[test]
    fn s37_inv052_model_change_adjusts_downward_off_and_default() {
        let supported = capabilities(
            [ReasoningLevel::Low, ReasoningLevel::High],
            FastModeSupport::Unsupported,
            [ServiceTier::OpenAi(OpenAiServiceTier::Default)],
        );
        let inherited = EffectiveModelSettings::new(
            Some(ReasoningLevel::XHigh),
            FastMode::Enabled,
            Some(ServiceTier::OpenAi(OpenAiServiceTier::Priority)),
        );

        let compatible = supported.adjust_for_model_change(inherited);

        assert_eq!(
            compatible.effective(),
            EffectiveModelSettings::new(Some(ReasoningLevel::High), FastMode::Disabled, None,)
        );
        assert_eq!(
            compatible.adjustments(),
            [
                ModelChangeAdjustment::ReasoningLevelClamped {
                    from: ReasoningLevel::XHigh,
                    to: ReasoningLevel::High,
                },
                ModelChangeAdjustment::FastModeDisabled,
                ModelChangeAdjustment::ServiceTierCleared {
                    from: ServiceTier::OpenAi(OpenAiServiceTier::Priority),
                },
            ]
        );
    }

    /// S37 / INV-052: when no supported level lies below the requested level,
    /// the model change chooses the lowest supported level.
    #[test]
    fn s37_inv052_model_change_uses_lowest_only_when_nothing_is_below() {
        let supported = capabilities(
            [ReasoningLevel::Medium, ReasoningLevel::High],
            FastModeSupport::Unsupported,
            [],
        );
        let inherited =
            EffectiveModelSettings::new(Some(ReasoningLevel::Minimal), FastMode::Disabled, None);

        let compatible = supported.adjust_for_model_change(inherited);

        assert_eq!(
            compatible.effective().reasoning_level(),
            Some(ReasoningLevel::Medium)
        );
        assert_eq!(
            compatible.adjustments(),
            [ModelChangeAdjustment::ReasoningLevelClamped {
                from: ReasoningLevel::Minimal,
                to: ReasoningLevel::Medium,
            }]
        );
    }

    /// S37 / INV-054: an alternate fast target is selected only from the
    /// declared capability record.
    #[test]
    fn s37_inv054_fast_mode_uses_only_the_declared_alternate_target() {
        let selected = ResolvedProviderTarget::naming(provider_model_identity(1));
        let fast = ResolvedProviderTarget::naming(provider_model_identity(2));
        let supported = capabilities([], FastModeSupport::AlternateTarget(fast), []);

        assert_eq!(
            supported.serving_target(selected, FastMode::Enabled),
            Some(fast)
        );
        assert_eq!(
            supported.serving_target(selected, FastMode::Disabled),
            Some(selected)
        );
    }

    /// S37 / INV-053: an automatic adjustment is a durable event field and
    /// cannot disappear after settings preparation.
    #[test]
    fn s37_inv053_defaults_event_retains_ordered_automatic_adjustments() {
        let selection = direct(1);
        let supported = capabilities([ReasoningLevel::Low], FastModeSupport::Unsupported, []);
        let resolved = ModelSettingsPrecedence::new(
            ModelSettingsOverlay::inherit_all(),
            ModelSettingsOverlay::new(
                SettingOverlay::Value(ReasoningLevel::Low),
                SettingOverlay::Inherit,
                SettingOverlay::Inherit,
            ),
            ModelSettingsOverlay::inherit_all(),
            ModelSettingsOverlay::inherit_all(),
        )
        .resolve();
        let installed = supported
            .validate_resolved(selection, resolved)
            .expect("the fixture level is declared supported");
        let prior_version = SessionConfigurationDefaultsVersion::first();
        let installed_version = prior_version
            .checked_next()
            .expect("the first version has a successor");
        let adjustment = ModelChangeAdjustment::ReasoningLevelClamped {
            from: ReasoningLevel::High,
            to: ReasoningLevel::Low,
        };

        let event = SessionModelSettingsChanged::try_new(
            session_id(1),
            command_id(1),
            prior_version,
            installed_version,
            ModelSelectionRequest::Direct(direct(2)),
            ModelSelectionRequest::Direct(selection),
            super::ValidatedModelSettings::provider_defaults(),
            installed,
            ModelSettingsOverlay::inherit_all(),
            vec![adjustment],
        )
        .expect("the fixture is a model-changing successor epoch");

        assert_eq!(event.adjustments(), [adjustment]);
        assert_eq!(event.installed_settings(), installed);
    }
}
