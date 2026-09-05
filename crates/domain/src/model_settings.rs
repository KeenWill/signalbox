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

/// One fast-mode contribution at a precedence layer.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum FastModeOverlay {
    /// Consult the next lower-precedence layer.
    Inherit,
    /// Explicitly select enabled or disabled fast mode and stop resolution.
    Value(FastMode),
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
    fast_mode: FastModeOverlay,
    service_tier: SettingOverlay<ServiceTier>,
}

impl ModelSettingsOverlay {
    /// An overlay that inherits every setting.
    pub const fn inherit_all() -> Self {
        Self {
            reasoning_level: SettingOverlay::Inherit,
            fast_mode: FastModeOverlay::Inherit,
            service_tier: SettingOverlay::Inherit,
        }
    }

    /// Constructs a complete labeled overlay.
    pub const fn new(
        reasoning_level: SettingOverlay<ReasoningLevel>,
        fast_mode: FastModeOverlay,
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
            fast_mode: FastModeOverlay::Value(settings.fast_mode),
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
    pub const fn fast_mode(&self) -> FastModeOverlay {
        self.fast_mode
    }

    /// Returns the service-tier contribution.
    pub const fn service_tier(&self) -> SettingOverlay<ServiceTier> {
        self.service_tier
    }

    const fn inheriting_from(self, prior: Self) -> Self {
        Self {
            reasoning_level: match self.reasoning_level {
                SettingOverlay::Inherit => prior.reasoning_level,
                SettingOverlay::ProviderDefault | SettingOverlay::Value(_) => self.reasoning_level,
            },
            fast_mode: match self.fast_mode {
                FastModeOverlay::Inherit => prior.fast_mode,
                FastModeOverlay::Value(_) => self.fast_mode,
            },
            service_tier: match self.service_tier {
                SettingOverlay::Inherit => prior.service_tier,
                SettingOverlay::ProviderDefault | SettingOverlay::Value(_) => self.service_tier,
            },
        }
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
    precedence: ModelSettingsPrecedence,
    resolved: ResolvedModelSettings,
    validated_for: Option<DirectModelSelection>,
}

impl ValidatedModelSettings {
    /// Model-independent provider defaults.
    pub const fn provider_defaults() -> Self {
        Self {
            precedence: ModelSettingsPrecedence::provider_defaults(),
            resolved: ResolvedModelSettings {
                effective: EffectiveModelSettings::provider_defaults(),
                reasoning_source: None,
                fast_mode_source: None,
                service_tier_source: None,
            },
            validated_for: None,
        }
    }

    /// Binds a complete value to the exact direct selection that validated it.
    const fn for_selection(
        precedence: ModelSettingsPrecedence,
        resolved: ResolvedModelSettings,
        validated_for: DirectModelSelection,
    ) -> Self {
        Self {
            precedence,
            resolved,
            validated_for: Some(validated_for),
        }
    }

    /// Reconstitutes stored validation evidence only when its complete
    /// precedence chain resolves to the stored effective value and sources.
    #[allow(clippy::too_many_arguments)]
    pub fn reconstitute(
        precedence: ModelSettingsPrecedence,
        effective: EffectiveModelSettings,
        reasoning_source: Option<ModelSettingSource>,
        fast_mode_source: Option<ModelSettingSource>,
        service_tier_source: Option<ModelSettingSource>,
        validated_for: Option<DirectModelSelection>,
    ) -> Option<Self> {
        let resolved = ResolvedModelSettings {
            effective,
            reasoning_source,
            fast_mode_source,
            service_tier_source,
        };
        let reconstituted = Self {
            precedence,
            resolved,
            validated_for,
        };
        (precedence.resolve() == resolved
            && (validated_for.is_some() || reconstituted == Self::provider_defaults()))
        .then_some(reconstituted)
    }

    /// Returns the exact four-layer contributions that produced this value.
    pub const fn precedence(&self) -> ModelSettingsPrecedence {
        self.precedence
    }

    /// Returns the resolved value and per-knob source evidence.
    pub const fn resolved(&self) -> ResolvedModelSettings {
        self.resolved
    }

    /// Returns the complete effective value.
    pub const fn effective(&self) -> EffectiveModelSettings {
        self.resolved.effective
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
    /// A chain that selects provider defaults at every layer.
    pub const fn provider_defaults() -> Self {
        Self {
            per_call: ModelSettingsOverlay::inherit_all(),
            session: ModelSettingsOverlay::inherit_all(),
            profile: ModelSettingsOverlay::inherit_all(),
            global_default: ModelSettingsOverlay::inherit_all(),
        }
    }

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

    /// Returns the per-call contribution.
    pub const fn per_call(&self) -> ModelSettingsOverlay {
        self.per_call
    }

    /// Returns the durable session contribution.
    pub const fn session(&self) -> ModelSettingsOverlay {
        self.session
    }

    /// Returns the copied named-profile contribution.
    pub const fn profile(&self) -> ModelSettingsOverlay {
        self.profile
    }

    /// Returns the copied global-default contribution.
    pub const fn global_default(&self) -> ModelSettingsOverlay {
        self.global_default
    }

    /// Replaces only the per-call layer while retaining the copied durable
    /// session, profile, and global layers.
    pub const fn with_per_call(self, per_call: ModelSettingsOverlay) -> Self {
        Self { per_call, ..self }
    }

    /// Replaces only the durable session layer while retaining the other
    /// copied precedence contributions.
    const fn with_session(self, session: ModelSettingsOverlay) -> Self {
        Self { session, ..self }
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

    pub(crate) fn with_effective_adjustment(
        mut self,
        prior: ResolvedModelSettings,
        adjusted: EffectiveModelSettings,
    ) -> Self {
        if prior.effective.reasoning_level != adjusted.reasoning_level {
            self.set_reasoning_at_source(prior.reasoning_source, adjusted.reasoning_level);
        }
        if prior.effective.fast_mode != adjusted.fast_mode {
            self.set_fast_mode_at_source(prior.fast_mode_source, adjusted.fast_mode);
        }
        if prior.effective.service_tier != adjusted.service_tier {
            self.set_service_tier_at_source(prior.service_tier_source, adjusted.service_tier);
        }
        self
    }

    fn set_reasoning_at_source(
        &mut self,
        source: Option<ModelSettingSource>,
        value: Option<ReasoningLevel>,
    ) {
        let overlay = match value {
            Some(value) => SettingOverlay::Value(value),
            None => SettingOverlay::ProviderDefault,
        };
        match source {
            Some(ModelSettingSource::PerCall) => self.per_call.reasoning_level = overlay,
            Some(ModelSettingSource::Session) => self.session.reasoning_level = overlay,
            Some(ModelSettingSource::Profile) => self.profile.reasoning_level = overlay,
            Some(ModelSettingSource::GlobalDefault) => {
                self.global_default.reasoning_level = overlay
            }
            None => {}
        }
    }

    fn set_fast_mode_at_source(&mut self, source: Option<ModelSettingSource>, value: FastMode) {
        let overlay = FastModeOverlay::Value(value);
        match source {
            Some(ModelSettingSource::PerCall) => self.per_call.fast_mode = overlay,
            Some(ModelSettingSource::Session) => self.session.fast_mode = overlay,
            Some(ModelSettingSource::Profile) => self.profile.fast_mode = overlay,
            Some(ModelSettingSource::GlobalDefault) => self.global_default.fast_mode = overlay,
            None => {}
        }
    }

    fn set_service_tier_at_source(
        &mut self,
        source: Option<ModelSettingSource>,
        value: Option<ServiceTier>,
    ) {
        let overlay = match value {
            Some(value) => SettingOverlay::Value(value),
            None => SettingOverlay::ProviderDefault,
        };
        match source {
            Some(ModelSettingSource::PerCall) => self.per_call.service_tier = overlay,
            Some(ModelSettingSource::Session) => self.session.service_tier = overlay,
            Some(ModelSettingSource::Profile) => self.profile.service_tier = overlay,
            Some(ModelSettingSource::GlobalDefault) => self.global_default.service_tier = overlay,
            None => {}
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
    layers: impl IntoIterator<Item = (ModelSettingSource, FastModeOverlay)>,
) -> (FastMode, Option<ModelSettingSource>) {
    for (source, overlay) in layers {
        match overlay {
            FastModeOverlay::Inherit => {}
            FastModeOverlay::Value(value) => return (value, Some(source)),
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
        if overlay.fast_mode == FastModeOverlay::Value(FastMode::Enabled)
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

    /// Resolves and validates a complete precedence chain, retaining its
    /// override provenance in the sealed result.
    pub fn validate_precedence(
        &self,
        selection: DirectModelSelection,
        precedence: ModelSettingsPrecedence,
    ) -> Result<ValidatedModelSettings, UnsupportedModelSetting> {
        if precedence == ModelSettingsPrecedence::provider_defaults() {
            return Ok(ValidatedModelSettings::provider_defaults());
        }
        let resolved = precedence.resolve();
        self.validate_explicit(
            selection,
            ModelSettingsOverlay::from_effective(resolved.effective),
        )?;
        Ok(ValidatedModelSettings::for_selection(
            precedence, resolved, selection,
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

    /// Validates the caller-owned layer and adjusts only incompatibility that
    /// remains because inherited layers were carried across a model change.
    pub fn validate_model_change(
        &self,
        selection: DirectModelSelection,
        precedence: ModelSettingsPrecedence,
        caller_overlay: ModelSettingsOverlay,
    ) -> Result<AdjustedModelSettings, UnsupportedModelSetting> {
        self.validate_explicit(selection, caller_overlay)?;
        let prior = precedence.resolve();
        let compatible = self.adjust_for_model_change(prior.effective());
        let (effective, adjustments) = compatible.into_parts();
        let precedence = precedence.with_effective_adjustment(prior, effective);
        let resolved = precedence.resolve();
        Ok(AdjustedModelSettings {
            settings: ValidatedModelSettings::for_selection(precedence, resolved, selection),
            adjustments,
        })
    }

    /// Selects the capability-authorized serving target for fast mode.
    pub fn serving_target(
        &self,
        selected: ResolvedProviderTarget,
        fast_mode: FastMode,
    ) -> Option<ResolvedProviderTarget> {
        match (fast_mode, self.fast_mode) {
            (FastMode::Disabled, _) | (FastMode::Enabled, FastModeSupport::RequestControl) => {
                Some(selected)
            }
            (FastMode::Enabled, FastModeSupport::AlternateTarget(target)) => {
                (target != selected).then_some(target)
            }
            (FastMode::Enabled, FastModeSupport::Unsupported) => None,
        }
    }
}

/// Complete validated settings plus ordered model-change adjustment evidence.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AdjustedModelSettings {
    settings: ValidatedModelSettings,
    adjustments: Box<[ModelChangeAdjustment]>,
}

impl AdjustedModelSettings {
    /// Returns the complete validated settings after adjustment.
    pub const fn settings(&self) -> ValidatedModelSettings {
        self.settings
    }

    /// Borrows adjustments in reasoning, fast, service-tier order.
    pub fn adjustments(&self) -> &[ModelChangeAdjustment] {
        &self.adjustments
    }

    /// Returns the validated snapshot and ordered adjustment evidence.
    pub fn into_parts(self) -> (ValidatedModelSettings, Box<[ModelChangeAdjustment]>) {
        (self.settings, self.adjustments)
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
    /// Reasoning moved to the nearest supported level at or below the prior
    /// value, or to the lowest supported level when none lies below it.
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
        let prior_validation = prior_settings.validated_for();
        let installed_validation = installed_settings.validated_for();
        let validation_changed = matches!(
            (prior_validation, installed_validation),
            (Some(prior), Some(installed)) if prior != installed
        );
        let prior_model_matches = match (prior_model, prior_validation) {
            (ModelSelectionRequest::Direct(expected), Some(validated)) => expected == validated,
            (ModelSelectionRequest::Direct(_), None) | (ModelSelectionRequest::Alias(_), _) => true,
        };
        let installed_model_matches = match (installed_model, installed_validation) {
            (ModelSelectionRequest::Direct(expected), Some(validated)) => expected == validated,
            (ModelSelectionRequest::Direct(_), None) | (ModelSelectionRequest::Alias(_), _) => true,
        };
        let prior_precedence = prior_settings.precedence();
        let installed_precedence = installed_settings.precedence();
        let defaults_snapshots_have_no_per_call_layer = prior_precedence.per_call()
            == ModelSettingsOverlay::inherit_all()
            && installed_precedence.per_call() == ModelSettingsOverlay::inherit_all();
        let copied_precedence = ModelSettingsPrecedence::new(
            ModelSettingsOverlay::inherit_all(),
            prior_precedence.session(),
            installed_precedence.profile(),
            installed_precedence.global_default(),
        );
        let unadjusted_precedence = copied_precedence
            .with_session(caller_override.inheriting_from(prior_precedence.session()));
        let unadjusted = unadjusted_precedence.resolve();
        let adjusted = apply_recorded_model_change_adjustments(unadjusted, &adjustments);
        let adjustments_target_inherited_values =
            adjustments.iter().all(|adjustment| match adjustment {
                ModelChangeAdjustment::ReasoningLevelClamped { .. }
                | ModelChangeAdjustment::ReasoningLevelCleared { .. } => {
                    caller_override.reasoning_level() == SettingOverlay::Inherit
                }
                ModelChangeAdjustment::FastModeDisabled => {
                    caller_override.fast_mode() == FastModeOverlay::Inherit
                }
                ModelChangeAdjustment::ServiceTierCleared { .. } => {
                    caller_override.service_tier() == SettingOverlay::Inherit
                }
            });
        let provenance_matches = adjusted.is_some_and(|adjusted| {
            let expected = unadjusted_precedence.with_effective_adjustment(unadjusted, adjusted);
            installed_settings.precedence() == expected
                && installed_settings.resolved() == expected.resolve()
        });
        let adjustments_match_model_change = adjustments.is_empty() || validation_changed;
        (is_successor
            && records_change
            && prior_model_matches
            && installed_model_matches
            && defaults_snapshots_have_no_per_call_layer
            && provenance_matches
            && adjustments_target_inherited_values
            && adjustments_match_model_change)
            .then(|| Self {
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
    adjusted_from_selection: Option<DirectModelSelection>,
    adjustments: Box<[ModelChangeAdjustment]>,
}

impl TurnModelSettingsResolved {
    /// Constructs an event when the settings evidence belongs to the frozen
    /// direct selection. Provider-default evidence is model-independent.
    #[allow(clippy::too_many_arguments)]
    pub fn try_new(
        accepted_input: AcceptedInputId,
        turn: TurnId,
        defaults_version: SessionConfigurationDefaultsVersion,
        selection: FrozenModelSelection,
        per_call_override: ModelSettingsOverlay,
        settings: ValidatedModelSettings,
        adjusted_from_selection: Option<DirectModelSelection>,
        adjustments: Vec<ModelChangeAdjustment>,
    ) -> Option<Self> {
        let settings_selection = settings.validated_for();
        let selection_matches =
            settings_selection.is_none() || settings_selection == Some(selection.selected_direct());
        let caller_matches = settings.precedence().per_call() == per_call_override;
        let adjustments_match = unapply_recorded_model_change_adjustments(settings, &adjustments)
            .is_some_and(|unadjusted_precedence| {
                let unadjusted = unadjusted_precedence.resolve();
                apply_recorded_model_change_adjustments(unadjusted, &adjustments).is_some_and(
                    |adjusted| {
                        let expected =
                            unadjusted_precedence.with_effective_adjustment(unadjusted, adjusted);
                        settings.precedence() == expected
                            && settings.resolved() == expected.resolve()
                    },
                )
            });
        let adjustments_match_selection = match adjustments.is_empty() {
            true => adjusted_from_selection.is_none(),
            false => {
                adjusted_from_selection.is_some_and(|prior| prior != selection.selected_direct())
            }
        };
        (selection_matches && caller_matches && adjustments_match && adjustments_match_selection)
            .then(|| Self {
                accepted_input,
                turn,
                defaults_version,
                selection,
                per_call_override,
                settings,
                adjusted_from_selection,
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

    /// Returns the prior direct validation identity that caused adjustments.
    pub const fn adjusted_from_selection(&self) -> Option<DirectModelSelection> {
        self.adjusted_from_selection
    }

    /// Borrows ordered automatic model-change adjustments.
    pub fn adjustments(&self) -> &[ModelChangeAdjustment] {
        &self.adjustments
    }
}

fn unapply_recorded_model_change_adjustments(
    settings: ValidatedModelSettings,
    adjustments: &[ModelChangeAdjustment],
) -> Option<ModelSettingsPrecedence> {
    let settled = settings.resolved();
    let mut prior = settled.effective();
    for adjustment in adjustments {
        prior = match adjustment {
            ModelChangeAdjustment::ReasoningLevelClamped { from, to }
                if settled.reasoning_source() != Some(ModelSettingSource::PerCall)
                    && settled.effective().reasoning_level() == Some(*to) =>
            {
                EffectiveModelSettings::new(Some(*from), prior.fast_mode(), prior.service_tier())
            }
            ModelChangeAdjustment::ReasoningLevelCleared { from }
                if settled.reasoning_source() != Some(ModelSettingSource::PerCall)
                    && settled.effective().reasoning_level().is_none() =>
            {
                EffectiveModelSettings::new(Some(*from), prior.fast_mode(), prior.service_tier())
            }
            ModelChangeAdjustment::FastModeDisabled
                if settled.fast_mode_source() != Some(ModelSettingSource::PerCall)
                    && settled.effective().fast_mode() == FastMode::Disabled =>
            {
                EffectiveModelSettings::new(
                    prior.reasoning_level(),
                    FastMode::Enabled,
                    prior.service_tier(),
                )
            }
            ModelChangeAdjustment::ServiceTierCleared { from }
                if settled.service_tier_source() != Some(ModelSettingSource::PerCall)
                    && settled.effective().service_tier().is_none() =>
            {
                EffectiveModelSettings::new(prior.reasoning_level(), prior.fast_mode(), Some(*from))
            }
            ModelChangeAdjustment::ReasoningLevelClamped { .. }
            | ModelChangeAdjustment::ReasoningLevelCleared { .. }
            | ModelChangeAdjustment::FastModeDisabled
            | ModelChangeAdjustment::ServiceTierCleared { .. } => return None,
        };
    }
    Some(
        settings
            .precedence()
            .with_effective_adjustment(settled, prior),
    )
}

pub(crate) fn apply_recorded_model_change_adjustments(
    prior: ResolvedModelSettings,
    adjustments: &[ModelChangeAdjustment],
) -> Option<EffectiveModelSettings> {
    let mut effective = prior.effective();
    let mut last_knob = 0_u8;
    for adjustment in adjustments {
        let knob = match adjustment {
            ModelChangeAdjustment::ReasoningLevelClamped { from, to } => {
                if last_knob > 1
                    || prior.reasoning_source() == Some(ModelSettingSource::PerCall)
                    || effective.reasoning_level() != Some(*from)
                    || to == from
                {
                    return None;
                }
                effective = EffectiveModelSettings::new(
                    Some(*to),
                    effective.fast_mode(),
                    effective.service_tier(),
                );
                1
            }
            ModelChangeAdjustment::ReasoningLevelCleared { from } => {
                if last_knob > 1
                    || prior.reasoning_source() == Some(ModelSettingSource::PerCall)
                    || effective.reasoning_level() != Some(*from)
                {
                    return None;
                }
                effective = EffectiveModelSettings::new(
                    None,
                    effective.fast_mode(),
                    effective.service_tier(),
                );
                1
            }
            ModelChangeAdjustment::FastModeDisabled => {
                if last_knob > 2
                    || prior.fast_mode_source() == Some(ModelSettingSource::PerCall)
                    || effective.fast_mode() != FastMode::Enabled
                {
                    return None;
                }
                effective = EffectiveModelSettings::new(
                    effective.reasoning_level(),
                    FastMode::Disabled,
                    effective.service_tier(),
                );
                2
            }
            ModelChangeAdjustment::ServiceTierCleared { from } => {
                if last_knob > 3
                    || prior.service_tier_source() == Some(ModelSettingSource::PerCall)
                    || effective.service_tier() != Some(*from)
                {
                    return None;
                }
                effective = EffectiveModelSettings::new(
                    effective.reasoning_level(),
                    effective.fast_mode(),
                    None,
                );
                3
            }
        };
        if knob == last_knob {
            return None;
        }
        last_knob = knob;
    }
    Some(effective)
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
        AnthropicServiceTier, EffectiveModelSettings, FastMode, FastModeOverlay, FastModeSupport,
        ModelCapabilities, ModelChangeAdjustment, ModelSettingSource, ModelSettingsOverlay,
        ModelSettingsPrecedence, OpenAiServiceTier, ReasoningLevel, ServiceTier,
        SessionModelSettingsChanged, SettingOverlay, UnsupportedModelSetting,
    };
    use crate::test_support::{command_id, direct, provider_model_identity, session_id};
    use crate::{
        AcceptedInputId, FrozenModelSelection, ModelSelectionRequest, ResolvedProviderTarget,
        SessionConfigurationDefaultsVersion, TurnId, TurnModelSettingsResolved,
    };
    use uuid::Uuid;

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

    /// S37: each knob resolves independently through per-call,
    /// session, profile, then global precedence, and an explicit provider
    /// default stops lower-layer inheritance.
    #[test]
    fn s37_resolves_the_fixed_precedence_chain_with_explicit_clearing() {
        let per_call = ModelSettingsOverlay::new(
            SettingOverlay::ProviderDefault,
            FastModeOverlay::Inherit,
            SettingOverlay::Inherit,
        );
        let session = ModelSettingsOverlay::new(
            SettingOverlay::Value(ReasoningLevel::High),
            FastModeOverlay::Value(FastMode::Enabled),
            SettingOverlay::Inherit,
        );
        let profile = ModelSettingsOverlay::new(
            SettingOverlay::Value(ReasoningLevel::Medium),
            FastModeOverlay::Value(FastMode::Disabled),
            SettingOverlay::Value(ServiceTier::OpenAi(OpenAiServiceTier::Priority)),
        );
        let global = ModelSettingsOverlay::new(
            SettingOverlay::Value(ReasoningLevel::Low),
            FastModeOverlay::Value(FastMode::Disabled),
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

    /// stored settings reconstitute from self-contained
    /// structural facts without consulting a mutable capability catalog.
    #[test]
    fn validated_settings_reconstitute_from_exact_stored_facts() {
        let selected = direct(1);
        let supported = capabilities([ReasoningLevel::Medium], FastModeSupport::Unsupported, []);
        let precedence = ModelSettingsPrecedence::new(
            ModelSettingsOverlay::inherit_all(),
            ModelSettingsOverlay::new(
                SettingOverlay::Value(ReasoningLevel::Medium),
                FastModeOverlay::Inherit,
                SettingOverlay::Inherit,
            ),
            ModelSettingsOverlay::inherit_all(),
            ModelSettingsOverlay::inherit_all(),
        );
        let stored = supported
            .validate_precedence(selected, precedence)
            .expect("the stored setting was capability-validated before persistence");
        let resolved = stored.resolved();

        let reconstituted = super::ValidatedModelSettings::reconstitute(
            stored.precedence(),
            stored.effective(),
            resolved.reasoning_source(),
            resolved.fast_mode_source(),
            resolved.service_tier_source(),
            stored.validated_for(),
        );

        assert_eq!(reconstituted, Some(stored));
    }

    /// a stored effective value that disagrees with its precedence
    /// chain cannot claim validated settings provenance.
    #[test]
    fn inconsistent_stored_settings_fail_reconstitution() {
        let precedence = ModelSettingsPrecedence::provider_defaults();

        let reconstituted = super::ValidatedModelSettings::reconstitute(
            precedence,
            EffectiveModelSettings::new(Some(ReasoningLevel::High), FastMode::Disabled, None),
            Some(ModelSettingSource::Session),
            None,
            None,
            Some(direct(1)),
        );

        assert_eq!(reconstituted, None);
    }

    /// only exact provider defaults may omit the direct
    /// selection that validated stored settings.
    #[test]
    fn non_default_stored_settings_require_a_validation_selection() {
        let precedence = ModelSettingsPrecedence::new(
            ModelSettingsOverlay::inherit_all(),
            ModelSettingsOverlay::new(
                SettingOverlay::Value(ReasoningLevel::High),
                FastModeOverlay::Inherit,
                SettingOverlay::Inherit,
            ),
            ModelSettingsOverlay::inherit_all(),
            ModelSettingsOverlay::inherit_all(),
        );
        let resolved = precedence.resolve();

        let reconstituted = super::ValidatedModelSettings::reconstitute(
            precedence,
            resolved.effective(),
            resolved.reasoning_source(),
            resolved.fast_mode_source(),
            resolved.service_tier_source(),
            None,
        );

        assert_eq!(reconstituted, None);
    }

    /// validating the exact all-inherit chain preserves the
    /// canonical model-independent provider-default snapshot.
    #[test]
    fn exact_provider_defaults_remain_model_independent() {
        let selected = direct(1);
        let supported = capabilities([ReasoningLevel::High], FastModeSupport::Unsupported, []);

        let settings = supported
            .validate_precedence(selected, ModelSettingsPrecedence::provider_defaults())
            .expect("provider defaults require no model-specific capability");

        assert_eq!(settings, super::ValidatedModelSettings::provider_defaults());
        assert_eq!(settings.validated_for(), None);
    }

    /// S37: an explicit unsupported level is a typed error rather
    /// than delegated to an open provider enum or silent clamp.
    #[test]
    fn s37_explicit_unsupported_reasoning_is_rejected() {
        let selected = direct(1);
        let supported = capabilities(
            [ReasoningLevel::Low, ReasoningLevel::Medium],
            FastModeSupport::Unsupported,
            [],
        );
        let requested = ModelSettingsOverlay::new(
            SettingOverlay::Value(ReasoningLevel::High),
            FastModeOverlay::Inherit,
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

    /// S37: model-change incompatibility clamps reasoning downward,
    /// disables fast mode, clears an unordered tier, and records each change.
    #[test]
    fn s37_model_change_adjusts_downward_off_and_default() {
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

    /// S37: when no supported level lies below the requested level,
    /// the model change chooses the lowest supported level.
    #[test]
    fn s37_model_change_uses_lowest_only_when_nothing_is_below() {
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

    /// S37: inherited incompatibility rewrites the inherited source
    /// in the validated snapshot while preserving ordered adjustment evidence.
    #[test]
    fn s37_model_change_installs_a_self_consistent_adjusted_snapshot() {
        let selected = direct(1);
        let supported = capabilities([ReasoningLevel::Low], FastModeSupport::Unsupported, []);
        let session = ModelSettingsOverlay::new(
            SettingOverlay::Value(ReasoningLevel::High),
            FastModeOverlay::Inherit,
            SettingOverlay::Inherit,
        );
        let precedence = ModelSettingsPrecedence::new(
            ModelSettingsOverlay::inherit_all(),
            session,
            ModelSettingsOverlay::inherit_all(),
            ModelSettingsOverlay::inherit_all(),
        );

        let adjusted = supported
            .validate_model_change(selected, precedence, ModelSettingsOverlay::inherit_all())
            .expect("the incompatible value is inherited across the model change");

        assert_eq!(
            adjusted.settings().effective().reasoning_level(),
            Some(ReasoningLevel::Low)
        );
        assert_eq!(
            adjusted.settings().precedence().session().reasoning_level(),
            SettingOverlay::Value(ReasoningLevel::Low)
        );
        assert_eq!(
            adjusted.adjustments(),
            [ModelChangeAdjustment::ReasoningLevelClamped {
                from: ReasoningLevel::High,
                to: ReasoningLevel::Low,
            }]
        );
    }

    /// S37: the same unsupported value remains an error when the
    /// model-change caller explicitly supplies it.
    #[test]
    fn s37_model_change_does_not_adjust_an_explicit_unsupported_value() {
        let selected = direct(1);
        let supported = capabilities([ReasoningLevel::Low], FastModeSupport::Unsupported, []);
        let caller = ModelSettingsOverlay::new(
            SettingOverlay::Value(ReasoningLevel::High),
            FastModeOverlay::Inherit,
            SettingOverlay::Inherit,
        );
        let precedence = ModelSettingsPrecedence::new(
            ModelSettingsOverlay::inherit_all(),
            caller,
            ModelSettingsOverlay::inherit_all(),
            ModelSettingsOverlay::inherit_all(),
        );

        let error = supported
            .validate_model_change(selected, precedence, caller)
            .expect_err("the caller-owned unsupported value is not adjusted");

        assert_eq!(
            error,
            UnsupportedModelSetting::ReasoningLevel {
                selection: selected,
                requested: ReasoningLevel::High,
            }
        );
    }

    /// S37: an alternate fast target is selected only from the
    /// declared capability record.
    #[test]
    fn s37_fast_mode_uses_only_the_declared_alternate_target() {
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

    /// S37: the alternate-target variant cannot silently authorize
    /// ordinary serving through a self-map.
    #[test]
    fn s37_fast_mode_rejects_a_self_mapped_alternate_target() {
        let selected = ResolvedProviderTarget::naming(provider_model_identity(1));
        let supported = capabilities([], FastModeSupport::AlternateTarget(selected), []);

        assert_eq!(supported.serving_target(selected, FastMode::Enabled), None);
    }

    /// S37: an automatic adjustment is a durable event field and
    /// cannot disappear after settings preparation.
    #[test]
    fn s37_defaults_event_retains_ordered_automatic_adjustments() {
        let selection = direct(1);
        let prior_selection = direct(2);
        let supported = capabilities([ReasoningLevel::Low], FastModeSupport::Unsupported, []);
        let prior = capabilities([ReasoningLevel::High], FastModeSupport::Unsupported, [])
            .validate_precedence(
                prior_selection,
                ModelSettingsPrecedence::new(
                    ModelSettingsOverlay::inherit_all(),
                    ModelSettingsOverlay::new(
                        SettingOverlay::Value(ReasoningLevel::High),
                        FastModeOverlay::Inherit,
                        SettingOverlay::Inherit,
                    ),
                    ModelSettingsOverlay::inherit_all(),
                    ModelSettingsOverlay::inherit_all(),
                ),
            )
            .expect("the prior fixture level is declared supported");
        let installed = supported
            .validate_precedence(
                selection,
                ModelSettingsPrecedence::new(
                    ModelSettingsOverlay::inherit_all(),
                    ModelSettingsOverlay::new(
                        SettingOverlay::Value(ReasoningLevel::Low),
                        FastModeOverlay::Inherit,
                        SettingOverlay::Inherit,
                    ),
                    ModelSettingsOverlay::inherit_all(),
                    ModelSettingsOverlay::inherit_all(),
                ),
            )
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
            ModelSelectionRequest::Direct(prior_selection),
            ModelSelectionRequest::Direct(selection),
            prior,
            installed,
            ModelSettingsOverlay::inherit_all(),
            vec![adjustment],
        )
        .expect("the fixture is a model-changing successor epoch");

        assert_eq!(event.adjustments(), [adjustment]);
        assert_eq!(event.installed_settings(), installed);
    }

    /// S37: an explicit caller value is rejected as unsupported and
    /// cannot be rewritten by automatic model-change adjustment evidence.
    #[test]
    fn s37_defaults_event_rejects_adjustment_of_explicit_caller_value() {
        let prior_selection = direct(1);
        let installed_selection = direct(2);
        let prior = capabilities([ReasoningLevel::High], FastModeSupport::Unsupported, [])
            .validate_precedence(
                prior_selection,
                ModelSettingsPrecedence::new(
                    ModelSettingsOverlay::inherit_all(),
                    ModelSettingsOverlay::new(
                        SettingOverlay::Value(ReasoningLevel::High),
                        FastModeOverlay::Inherit,
                        SettingOverlay::Inherit,
                    ),
                    ModelSettingsOverlay::inherit_all(),
                    ModelSettingsOverlay::inherit_all(),
                ),
            )
            .expect("the prior fixture level is supported");
        let installed = capabilities([ReasoningLevel::Low], FastModeSupport::Unsupported, [])
            .validate_precedence(
                installed_selection,
                ModelSettingsPrecedence::new(
                    ModelSettingsOverlay::inherit_all(),
                    ModelSettingsOverlay::new(
                        SettingOverlay::Value(ReasoningLevel::Low),
                        FastModeOverlay::Inherit,
                        SettingOverlay::Inherit,
                    ),
                    ModelSettingsOverlay::inherit_all(),
                    ModelSettingsOverlay::inherit_all(),
                ),
            )
            .expect("the installed fixture level is supported");
        let prior_version = SessionConfigurationDefaultsVersion::first();
        let installed_version = prior_version
            .checked_next()
            .expect("the first version has a successor");
        let caller_override = ModelSettingsOverlay::new(
            SettingOverlay::Value(ReasoningLevel::Ultra),
            FastModeOverlay::Inherit,
            SettingOverlay::Inherit,
        );

        let event = SessionModelSettingsChanged::try_new(
            session_id(1),
            command_id(1),
            prior_version,
            installed_version,
            ModelSelectionRequest::Direct(prior_selection),
            ModelSelectionRequest::Direct(installed_selection),
            prior,
            installed,
            caller_override,
            vec![ModelChangeAdjustment::ReasoningLevelClamped {
                from: ReasoningLevel::Ultra,
                to: ReasoningLevel::Low,
            }],
        );

        assert_eq!(event, None);
    }

    /// S37: retaining the same alias spelling can still record an
    /// adjustment when its validated direct selection changed.
    #[test]
    fn s37_defaults_event_detects_alias_retarget_from_validation_identity() {
        let alias = crate::ModelAlias::from_uuid(Uuid::from_u128(3));
        let prior_selection = direct(1);
        let installed_selection = direct(2);
        let prior = capabilities([ReasoningLevel::High], FastModeSupport::Unsupported, [])
            .validate_precedence(
                prior_selection,
                ModelSettingsPrecedence::new(
                    ModelSettingsOverlay::inherit_all(),
                    ModelSettingsOverlay::new(
                        SettingOverlay::Value(ReasoningLevel::High),
                        FastModeOverlay::Inherit,
                        SettingOverlay::Inherit,
                    ),
                    ModelSettingsOverlay::inherit_all(),
                    ModelSettingsOverlay::inherit_all(),
                ),
            )
            .expect("the prior fixture level is supported");
        let installed = capabilities([ReasoningLevel::Low], FastModeSupport::Unsupported, [])
            .validate_precedence(
                installed_selection,
                ModelSettingsPrecedence::new(
                    ModelSettingsOverlay::inherit_all(),
                    ModelSettingsOverlay::new(
                        SettingOverlay::Value(ReasoningLevel::Low),
                        FastModeOverlay::Inherit,
                        SettingOverlay::Inherit,
                    ),
                    ModelSettingsOverlay::inherit_all(),
                    ModelSettingsOverlay::inherit_all(),
                ),
            )
            .expect("the installed fixture level is supported");
        let prior_version = SessionConfigurationDefaultsVersion::first();
        let installed_version = prior_version
            .checked_next()
            .expect("the first version has a successor");

        let event = SessionModelSettingsChanged::try_new(
            session_id(1),
            command_id(1),
            prior_version,
            installed_version,
            ModelSelectionRequest::Alias(alias),
            ModelSelectionRequest::Alias(alias),
            prior,
            installed,
            ModelSettingsOverlay::inherit_all(),
            vec![ModelChangeAdjustment::ReasoningLevelClamped {
                from: ReasoningLevel::High,
                to: ReasoningLevel::Low,
            }],
        );

        assert!(event.is_some());
    }

    /// changing only the alias request spelling cannot justify an
    /// automatic compatibility adjustment when the direct selection stayed fixed.
    #[test]
    fn defaults_event_rejects_adjustment_for_alias_spelling_change() {
        let selection = direct(1);
        let prior = capabilities([ReasoningLevel::High], FastModeSupport::Unsupported, [])
            .validate_precedence(
                selection,
                ModelSettingsPrecedence::new(
                    ModelSettingsOverlay::inherit_all(),
                    ModelSettingsOverlay::new(
                        SettingOverlay::Value(ReasoningLevel::High),
                        FastModeOverlay::Inherit,
                        SettingOverlay::Inherit,
                    ),
                    ModelSettingsOverlay::inherit_all(),
                    ModelSettingsOverlay::inherit_all(),
                ),
            )
            .expect("the prior level is supported");
        let installed = capabilities([ReasoningLevel::Low], FastModeSupport::Unsupported, [])
            .validate_precedence(
                selection,
                ModelSettingsPrecedence::new(
                    ModelSettingsOverlay::inherit_all(),
                    ModelSettingsOverlay::new(
                        SettingOverlay::Value(ReasoningLevel::Low),
                        FastModeOverlay::Inherit,
                        SettingOverlay::Inherit,
                    ),
                    ModelSettingsOverlay::inherit_all(),
                    ModelSettingsOverlay::inherit_all(),
                ),
            )
            .expect("the installed level is supported");
        let prior_version = SessionConfigurationDefaultsVersion::first();
        let installed_version = prior_version
            .checked_next()
            .expect("the first version has a successor");

        let event = SessionModelSettingsChanged::try_new(
            session_id(1),
            command_id(1),
            prior_version,
            installed_version,
            ModelSelectionRequest::Alias(crate::ModelAlias::from_uuid(Uuid::from_u128(3))),
            ModelSelectionRequest::Alias(crate::ModelAlias::from_uuid(Uuid::from_u128(4))),
            prior,
            installed,
            ModelSettingsOverlay::inherit_all(),
            vec![ModelChangeAdjustment::ReasoningLevelClamped {
                from: ReasoningLevel::High,
                to: ReasoningLevel::Low,
            }],
        );

        assert_eq!(event, None);
    }

    /// S37: a replacement model contributes its newly copied
    /// profile and global layers to settings-change provenance.
    #[test]
    fn s37_defaults_event_uses_replacement_model_lower_layers() {
        let prior_selection = direct(1);
        let installed_selection = direct(2);
        let prior = capabilities([ReasoningLevel::High], FastModeSupport::Unsupported, [])
            .validate_precedence(
                prior_selection,
                ModelSettingsPrecedence::new(
                    ModelSettingsOverlay::inherit_all(),
                    ModelSettingsOverlay::inherit_all(),
                    ModelSettingsOverlay::new(
                        SettingOverlay::Value(ReasoningLevel::High),
                        FastModeOverlay::Inherit,
                        SettingOverlay::Inherit,
                    ),
                    ModelSettingsOverlay::inherit_all(),
                ),
            )
            .expect("the prior profile is supported");
        let installed = capabilities([ReasoningLevel::Low], FastModeSupport::Unsupported, [])
            .validate_precedence(
                installed_selection,
                ModelSettingsPrecedence::new(
                    ModelSettingsOverlay::inherit_all(),
                    ModelSettingsOverlay::inherit_all(),
                    ModelSettingsOverlay::new(
                        SettingOverlay::Value(ReasoningLevel::Low),
                        FastModeOverlay::Inherit,
                        SettingOverlay::Inherit,
                    ),
                    ModelSettingsOverlay::inherit_all(),
                ),
            )
            .expect("the replacement profile is supported");
        let prior_version = SessionConfigurationDefaultsVersion::first();
        let installed_version = prior_version
            .checked_next()
            .expect("the first version has a successor");

        let event = SessionModelSettingsChanged::try_new(
            session_id(1),
            command_id(1),
            prior_version,
            installed_version,
            ModelSelectionRequest::Direct(prior_selection),
            ModelSelectionRequest::Direct(installed_selection),
            prior,
            installed,
            ModelSettingsOverlay::inherit_all(),
            Vec::new(),
        );

        assert!(event.is_some());
    }

    /// S37: every successor epoch records its newly copied profile
    /// and global layers even when its direct model is unchanged.
    #[test]
    fn s37_defaults_event_uses_same_model_successor_lower_layers() {
        let selection = direct(1);
        let supported = capabilities(
            [ReasoningLevel::Low, ReasoningLevel::High],
            FastModeSupport::Unsupported,
            [],
        );
        let prior = supported
            .validate_precedence(
                selection,
                ModelSettingsPrecedence::new(
                    ModelSettingsOverlay::inherit_all(),
                    ModelSettingsOverlay::inherit_all(),
                    ModelSettingsOverlay::new(
                        SettingOverlay::Value(ReasoningLevel::High),
                        FastModeOverlay::Inherit,
                        SettingOverlay::Inherit,
                    ),
                    ModelSettingsOverlay::inherit_all(),
                ),
            )
            .expect("the prior profile is supported");
        let installed = supported
            .validate_precedence(
                selection,
                ModelSettingsPrecedence::new(
                    ModelSettingsOverlay::inherit_all(),
                    ModelSettingsOverlay::inherit_all(),
                    ModelSettingsOverlay::new(
                        SettingOverlay::Value(ReasoningLevel::Low),
                        FastModeOverlay::Inherit,
                        SettingOverlay::Inherit,
                    ),
                    ModelSettingsOverlay::inherit_all(),
                ),
            )
            .expect("the installed profile is supported");
        let prior_version = SessionConfigurationDefaultsVersion::first();
        let installed_version = prior_version
            .checked_next()
            .expect("the first version has a successor");

        let event = SessionModelSettingsChanged::try_new(
            session_id(1),
            command_id(1),
            prior_version,
            installed_version,
            ModelSelectionRequest::Direct(selection),
            ModelSelectionRequest::Direct(selection),
            prior,
            installed,
            ModelSettingsOverlay::inherit_all(),
            Vec::new(),
        );

        assert!(event.is_some());
    }

    /// durable defaults snapshots cannot contain a
    /// request-scoped settings contribution.
    #[test]
    fn defaults_event_rejects_per_call_layers() {
        let prior_selection = direct(1);
        let installed_selection = direct(2);
        let precedence = ModelSettingsPrecedence::new(
            ModelSettingsOverlay::new(
                SettingOverlay::Value(ReasoningLevel::High),
                FastModeOverlay::Inherit,
                SettingOverlay::Inherit,
            ),
            ModelSettingsOverlay::inherit_all(),
            ModelSettingsOverlay::inherit_all(),
            ModelSettingsOverlay::inherit_all(),
        );
        let supported = capabilities([ReasoningLevel::High], FastModeSupport::Unsupported, []);
        let prior = supported
            .validate_precedence(prior_selection, precedence)
            .expect("the prior fixture level is supported");
        let installed = supported
            .validate_precedence(installed_selection, precedence)
            .expect("the installed fixture level is supported");
        let prior_version = SessionConfigurationDefaultsVersion::first();
        let installed_version = prior_version
            .checked_next()
            .expect("the first version has a successor");

        let event = SessionModelSettingsChanged::try_new(
            session_id(1),
            command_id(1),
            prior_version,
            installed_version,
            ModelSelectionRequest::Direct(prior_selection),
            ModelSelectionRequest::Direct(installed_selection),
            prior,
            installed,
            ModelSettingsOverlay::inherit_all(),
            Vec::new(),
        );

        assert_eq!(event, None);
    }

    /// alias adjustment evidence names the distinct prior
    /// direct selection whose capability change caused it.
    #[test]
    fn turn_event_accepts_distinct_alias_adjustment_source() {
        let prior_selection = direct(1);
        let installed_selection = direct(2);
        let alias = crate::ModelAlias::from_uuid(Uuid::from_u128(3));
        let settings = capabilities([ReasoningLevel::Low], FastModeSupport::Unsupported, [])
            .validate_precedence(
                installed_selection,
                ModelSettingsPrecedence::new(
                    ModelSettingsOverlay::inherit_all(),
                    ModelSettingsOverlay::new(
                        SettingOverlay::Value(ReasoningLevel::Low),
                        FastModeOverlay::Inherit,
                        SettingOverlay::Inherit,
                    ),
                    ModelSettingsOverlay::inherit_all(),
                    ModelSettingsOverlay::inherit_all(),
                ),
            )
            .expect("the adjusted fixture level is supported");

        let event = TurnModelSettingsResolved::try_new(
            AcceptedInputId::from_uuid(Uuid::from_u128(1)),
            TurnId::from_uuid(Uuid::from_u128(2)),
            SessionConfigurationDefaultsVersion::first(),
            FrozenModelSelection::FrozenAlias {
                alias,
                definition: crate::FrozenAliasDefinition::selecting(installed_selection),
            },
            ModelSettingsOverlay::inherit_all(),
            settings,
            Some(prior_selection),
            vec![ModelChangeAdjustment::ReasoningLevelClamped {
                from: ReasoningLevel::High,
                to: ReasoningLevel::Low,
            }],
        );

        assert_eq!(
            event
                .expect("the distinct prior selection authenticates the adjustment")
                .adjusted_from_selection(),
            Some(prior_selection)
        );
    }

    /// an alias spelling cannot authenticate an adjustment
    /// when its prior validation identity equals its selected direct model.
    #[test]
    fn turn_event_rejects_unchanged_alias_adjustment_source() {
        let selection = direct(1);
        let alias = crate::ModelAlias::from_uuid(Uuid::from_u128(2));
        let settings = capabilities([ReasoningLevel::Low], FastModeSupport::Unsupported, [])
            .validate_precedence(
                selection,
                ModelSettingsPrecedence::new(
                    ModelSettingsOverlay::inherit_all(),
                    ModelSettingsOverlay::new(
                        SettingOverlay::Value(ReasoningLevel::Low),
                        FastModeOverlay::Inherit,
                        SettingOverlay::Inherit,
                    ),
                    ModelSettingsOverlay::inherit_all(),
                    ModelSettingsOverlay::inherit_all(),
                ),
            )
            .expect("the adjusted fixture level is supported");

        let event = TurnModelSettingsResolved::try_new(
            AcceptedInputId::from_uuid(Uuid::from_u128(1)),
            TurnId::from_uuid(Uuid::from_u128(2)),
            SessionConfigurationDefaultsVersion::first(),
            FrozenModelSelection::FrozenAlias {
                alias,
                definition: crate::FrozenAliasDefinition::selecting(selection),
            },
            ModelSettingsOverlay::inherit_all(),
            settings,
            Some(selection),
            vec![ModelChangeAdjustment::ReasoningLevelClamped {
                from: ReasoningLevel::High,
                to: ReasoningLevel::Low,
            }],
        );

        assert_eq!(event, None);
    }

    /// a turn settings event cannot contradict the
    /// per-call contribution sealed into its complete precedence chain.
    #[test]
    fn turn_event_rejects_mismatched_per_call_provenance() {
        let selection = direct(1);
        let settings = capabilities([ReasoningLevel::High], FastModeSupport::Unsupported, [])
            .validate_precedence(selection, ModelSettingsPrecedence::provider_defaults())
            .expect("provider defaults are supported");
        let contradictory = ModelSettingsOverlay::new(
            SettingOverlay::Value(ReasoningLevel::High),
            FastModeOverlay::Inherit,
            SettingOverlay::Inherit,
        );

        let event = TurnModelSettingsResolved::try_new(
            AcceptedInputId::from_uuid(Uuid::from_u128(1)),
            TurnId::from_uuid(Uuid::from_u128(2)),
            SessionConfigurationDefaultsVersion::first(),
            FrozenModelSelection::Direct(selection),
            contradictory,
            settings,
            None,
            Vec::new(),
        );

        assert_eq!(event, None);
    }

    /// a turn event cannot attach automatic adjustment
    /// evidence that does not derive its sealed settings snapshot.
    #[test]
    fn turn_event_rejects_contradictory_adjustment_evidence() {
        let selection = direct(1);
        let settings = capabilities([ReasoningLevel::Low], FastModeSupport::Unsupported, [])
            .validate_precedence(selection, ModelSettingsPrecedence::provider_defaults())
            .expect("provider defaults are supported");

        let event = TurnModelSettingsResolved::try_new(
            AcceptedInputId::from_uuid(Uuid::from_u128(1)),
            TurnId::from_uuid(Uuid::from_u128(2)),
            SessionConfigurationDefaultsVersion::first(),
            FrozenModelSelection::Direct(selection),
            ModelSettingsOverlay::inherit_all(),
            settings,
            Some(direct(2)),
            vec![ModelChangeAdjustment::ReasoningLevelClamped {
                from: ReasoningLevel::High,
                to: ReasoningLevel::Low,
            }],
        );

        assert_eq!(event, None);
    }

    /// adjustment evidence requires a changed direct model,
    /// regardless of whether the frozen selection is direct or aliased.
    #[test]
    fn turn_event_rejects_adjustments_without_direct_model_change() {
        let selection = direct(1);
        let settings = capabilities([ReasoningLevel::Low], FastModeSupport::Unsupported, [])
            .validate_precedence(
                selection,
                ModelSettingsPrecedence::new(
                    ModelSettingsOverlay::inherit_all(),
                    ModelSettingsOverlay::new(
                        SettingOverlay::Value(ReasoningLevel::Low),
                        FastModeOverlay::Inherit,
                        SettingOverlay::Inherit,
                    ),
                    ModelSettingsOverlay::inherit_all(),
                    ModelSettingsOverlay::inherit_all(),
                ),
            )
            .expect("the adjusted fixture level is supported");

        let event = TurnModelSettingsResolved::try_new(
            AcceptedInputId::from_uuid(Uuid::from_u128(1)),
            TurnId::from_uuid(Uuid::from_u128(2)),
            SessionConfigurationDefaultsVersion::first(),
            FrozenModelSelection::Direct(selection),
            ModelSettingsOverlay::inherit_all(),
            settings,
            Some(selection),
            vec![ModelChangeAdjustment::ReasoningLevelClamped {
                from: ReasoningLevel::High,
                to: ReasoningLevel::Low,
            }],
        );

        assert_eq!(event, None);
    }

    /// a defaults event cannot claim a caller override
    /// that does not derive its installed settings snapshot.
    #[test]
    fn defaults_event_rejects_contradictory_caller_provenance() {
        let selection = direct(1);
        let supported = capabilities(
            [ReasoningLevel::Low, ReasoningLevel::High],
            FastModeSupport::Unsupported,
            [],
        );
        let prior = supported
            .validate_precedence(selection, ModelSettingsPrecedence::provider_defaults())
            .expect("provider defaults are supported");
        let installed = supported
            .validate_precedence(
                selection,
                ModelSettingsPrecedence::new(
                    ModelSettingsOverlay::inherit_all(),
                    ModelSettingsOverlay::new(
                        SettingOverlay::Value(ReasoningLevel::Low),
                        FastModeOverlay::Inherit,
                        SettingOverlay::Inherit,
                    ),
                    ModelSettingsOverlay::inherit_all(),
                    ModelSettingsOverlay::inherit_all(),
                ),
            )
            .expect("the installed fixture level is supported");
        let contradictory = ModelSettingsOverlay::new(
            SettingOverlay::Value(ReasoningLevel::High),
            FastModeOverlay::Inherit,
            SettingOverlay::Inherit,
        );
        let prior_version = SessionConfigurationDefaultsVersion::first();
        let installed_version = prior_version
            .checked_next()
            .expect("the first version has a successor");

        let event = SessionModelSettingsChanged::try_new(
            session_id(1),
            command_id(1),
            prior_version,
            installed_version,
            ModelSelectionRequest::Direct(selection),
            ModelSelectionRequest::Direct(selection),
            prior,
            installed,
            contradictory,
            Vec::new(),
        );

        assert_eq!(event, None);
    }
}
