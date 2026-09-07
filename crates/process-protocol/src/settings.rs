//! Settings wire representations and validation.

use crate::scalars::{
    CanonicalU64, CanonicalUuid, FrameValidationError, deserialize_required_nullable,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// Direct or alias model-selection request at the process boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ModelSelection {
    /// Stable direct selection key.
    Direct {
        /// Exact configured direct-selection identity.
        selection_id: CanonicalUuid,
    },
    /// Stable alias key resolved by the hub.
    Alias {
        /// Exact configured alias identity.
        alias_id: CanonicalUuid,
    },
}

/// Provider-neutral reasoning effort at the process boundary.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningLevel {
    None,
    Minimal,
    Low,
    Medium,
    High,
    #[serde(rename = "xhigh")]
    XHigh,
    Max,
    Ultra,
}

/// Whether fast serving is selected.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FastMode {
    Disabled,
    Enabled,
}

/// Anthropic Messages service tier.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnthropicServiceTier {
    Auto,
    StandardOnly,
}

/// OpenAI service tier.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OpenAiServiceTier {
    Auto,
    Default,
    Flex,
    Scale,
    Priority,
    Fast,
}

/// Codex CLI service tier.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CodexCliServiceTier {
    Default,
    Priority,
    Flex,
}

/// Provider-tagged service tier.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(
    tag = "provider",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum ServiceTier {
    Anthropic(AnthropicServiceTier),
    OpenAi(OpenAiServiceTier),
    CodexCli(CodexCliServiceTier),
}

/// One precedence-layer contribution for a setting.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum SettingOverlay<ValueT> {
    Inherit,
    ProviderDefault,
    Value(ValueT),
}

/// One fast-mode contribution at a precedence layer.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum FastModeOverlay {
    Inherit,
    Value(FastMode),
}

/// Three provenance-preserving setting contributions at one layer.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelSettingsOverlay {
    pub reasoning_level: SettingOverlay<ReasoningLevel>,
    pub fast_mode: FastModeOverlay,
    pub service_tier: SettingOverlay<ServiceTier>,
}

impl ModelSettingsOverlay {
    /// Constructs an overlay that inherits every setting.
    pub const fn inherit_all() -> Self {
        Self {
            reasoning_level: SettingOverlay::Inherit,
            fast_mode: FastModeOverlay::Inherit,
            service_tier: SettingOverlay::Inherit,
        }
    }
}

impl Default for ModelSettingsOverlay {
    fn default() -> Self {
        Self::inherit_all()
    }
}

/// Complete effective setting values.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EffectiveModelSettings {
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub reasoning_level: Option<ReasoningLevel>,
    pub fast_mode: FastMode,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub service_tier: Option<ServiceTier>,
}

/// Precedence layer that supplied one effective value.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelSettingSource {
    PerCall,
    Session,
    Profile,
    GlobalDefault,
}

/// Exact four-layer setting contributions.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelSettingsPrecedence {
    pub per_call: ModelSettingsOverlay,
    pub session: ModelSettingsOverlay,
    pub profile: ModelSettingsOverlay,
    pub global_default: ModelSettingsOverlay,
}

/// Complete resolved settings and their validation provenance.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelSettingsSnapshot {
    pub precedence: ModelSettingsPrecedence,
    pub effective: EffectiveModelSettings,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub reasoning_source: Option<ModelSettingSource>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub fast_mode_source: Option<ModelSettingSource>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub service_tier_source: Option<ModelSettingSource>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub validated_for_selection_id: Option<CanonicalUuid>,
}

/// Complete frozen settings evidence for one transcript turn.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TurnModelSettingsSnapshot {
    /// Turn that owns this frozen settings evidence.
    pub turn_id: CanonicalUuid,
    /// Accepted input that originated the turn.
    pub accepted_input_id: CanonicalUuid,
    /// Session-defaults epoch resolved for the origin.
    pub defaults_version: CanonicalU64,
    /// Model request before alias freezing.
    pub requested_model: ModelSelection,
    /// Direct model selected for execution.
    pub selected_direct_id: CanonicalUuid,
    /// Exact per-call settings contribution.
    pub per_call_override: ModelSettingsOverlay,
    /// Complete validated settings frozen for execution.
    pub settings: ModelSettingsSnapshot,
    /// Prior direct selection adjusted by a model change, or null.
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub adjusted_from_selection_id: Option<CanonicalUuid>,
    /// Ordered automatic model-change adjustments.
    pub adjustments: Vec<ModelChangeAdjustment>,
}

impl ModelSettingsSnapshot {
    pub(crate) fn validate(&self) -> Result<(), FrameValidationError> {
        let resolved = resolve_wire_settings(self.precedence);
        if resolved.effective != self.effective
            || resolved.reasoning_source != self.reasoning_source
            || resolved.fast_mode_source != self.fast_mode_source
            || resolved.service_tier_source != self.service_tier_source
            || (self.validated_for_selection_id.is_none()
                && !self.is_model_independent_provider_defaults())
        {
            return Err(FrameValidationError::ModelSettingsShape);
        }
        Ok(())
    }

    pub(crate) fn validate_defaults(&self) -> Result<(), FrameValidationError> {
        self.validate()?;
        if self.precedence.per_call != ModelSettingsOverlay::inherit_all() {
            return Err(FrameValidationError::ModelSettingsShape);
        }
        Ok(())
    }

    /// Reports whether this snapshot can belong to the supplied model selection.
    pub fn matches_model(&self, model: &ModelSelection) -> bool {
        snapshot_matches_model(model, self)
    }

    fn is_model_independent_provider_defaults(&self) -> bool {
        self.precedence
            == (ModelSettingsPrecedence {
                per_call: ModelSettingsOverlay::inherit_all(),
                session: ModelSettingsOverlay::inherit_all(),
                profile: ModelSettingsOverlay::inherit_all(),
                global_default: ModelSettingsOverlay::inherit_all(),
            })
            && self.effective
                == (EffectiveModelSettings {
                    reasoning_level: None,
                    fast_mode: FastMode::Disabled,
                    service_tier: None,
                })
            && self.reasoning_source.is_none()
            && self.fast_mode_source.is_none()
            && self.service_tier_source.is_none()
    }
}

impl TurnModelSettingsSnapshot {
    pub(crate) fn validate(&self) -> Result<(), FrameValidationError> {
        validate_turn_settings_payload(
            self.defaults_version,
            &self.requested_model,
            self.selected_direct_id,
            self.per_call_override,
            &self.settings,
            self.adjusted_from_selection_id,
            &self.adjustments,
        )
    }
}

#[derive(Clone, Copy)]
struct WireResolvedModelSettings {
    effective: EffectiveModelSettings,
    reasoning_source: Option<ModelSettingSource>,
    fast_mode_source: Option<ModelSettingSource>,
    service_tier_source: Option<ModelSettingSource>,
}

fn resolve_wire_settings(precedence: ModelSettingsPrecedence) -> WireResolvedModelSettings {
    let layers = [
        (ModelSettingSource::PerCall, precedence.per_call),
        (ModelSettingSource::Session, precedence.session),
        (ModelSettingSource::Profile, precedence.profile),
        (ModelSettingSource::GlobalDefault, precedence.global_default),
    ];
    let (reasoning_level, reasoning_source) =
        resolve_wire_nullable(layers.map(|(source, settings)| (source, settings.reasoning_level)));
    let (fast_mode, fast_mode_source) =
        resolve_wire_fast(layers.map(|(source, settings)| (source, settings.fast_mode)));
    let (service_tier, service_tier_source) =
        resolve_wire_nullable(layers.map(|(source, settings)| (source, settings.service_tier)));
    WireResolvedModelSettings {
        effective: EffectiveModelSettings {
            reasoning_level,
            fast_mode,
            service_tier,
        },
        reasoning_source,
        fast_mode_source,
        service_tier_source,
    }
}

fn resolve_wire_nullable<ValueT: Copy>(
    layers: impl IntoIterator<Item = (ModelSettingSource, SettingOverlay<ValueT>)>,
) -> (Option<ValueT>, Option<ModelSettingSource>) {
    for (source, setting) in layers {
        match setting {
            SettingOverlay::Inherit => {}
            SettingOverlay::ProviderDefault => return (None, Some(source)),
            SettingOverlay::Value(value) => return (Some(value), Some(source)),
        }
    }
    (None, None)
}

fn resolve_wire_fast(
    layers: impl IntoIterator<Item = (ModelSettingSource, FastModeOverlay)>,
) -> (FastMode, Option<ModelSettingSource>) {
    for (source, setting) in layers {
        match setting {
            FastModeOverlay::Inherit => {}
            FastModeOverlay::Value(value) => return (value, Some(source)),
        }
    }
    (FastMode::Disabled, None)
}

pub(crate) fn overlay_inheriting_from(
    overlay: ModelSettingsOverlay,
    prior: ModelSettingsOverlay,
) -> ModelSettingsOverlay {
    ModelSettingsOverlay {
        reasoning_level: match overlay.reasoning_level {
            SettingOverlay::Inherit => prior.reasoning_level,
            SettingOverlay::ProviderDefault | SettingOverlay::Value(_) => overlay.reasoning_level,
        },
        fast_mode: match overlay.fast_mode {
            FastModeOverlay::Inherit => prior.fast_mode,
            FastModeOverlay::Value(_) => overlay.fast_mode,
        },
        service_tier: match overlay.service_tier {
            SettingOverlay::Inherit => prior.service_tier,
            SettingOverlay::ProviderDefault | SettingOverlay::Value(_) => overlay.service_tier,
        },
    }
}

fn with_wire_effective_adjustment(
    mut precedence: ModelSettingsPrecedence,
    prior: WireResolvedModelSettings,
    adjusted: EffectiveModelSettings,
) -> ModelSettingsPrecedence {
    if prior.effective.reasoning_level != adjusted.reasoning_level {
        let value = match adjusted.reasoning_level {
            Some(value) => SettingOverlay::Value(value),
            None => SettingOverlay::ProviderDefault,
        };
        match prior.reasoning_source {
            Some(ModelSettingSource::PerCall) => precedence.per_call.reasoning_level = value,
            Some(ModelSettingSource::Session) => precedence.session.reasoning_level = value,
            Some(ModelSettingSource::Profile) => precedence.profile.reasoning_level = value,
            Some(ModelSettingSource::GlobalDefault) => {
                precedence.global_default.reasoning_level = value;
            }
            None => {}
        }
    }
    if prior.effective.fast_mode != adjusted.fast_mode {
        let value = FastModeOverlay::Value(adjusted.fast_mode);
        match prior.fast_mode_source {
            Some(ModelSettingSource::PerCall) => precedence.per_call.fast_mode = value,
            Some(ModelSettingSource::Session) => precedence.session.fast_mode = value,
            Some(ModelSettingSource::Profile) => precedence.profile.fast_mode = value,
            Some(ModelSettingSource::GlobalDefault) => precedence.global_default.fast_mode = value,
            None => {}
        }
    }
    if prior.effective.service_tier != adjusted.service_tier {
        let value = match adjusted.service_tier {
            Some(value) => SettingOverlay::Value(value),
            None => SettingOverlay::ProviderDefault,
        };
        match prior.service_tier_source {
            Some(ModelSettingSource::PerCall) => precedence.per_call.service_tier = value,
            Some(ModelSettingSource::Session) => precedence.session.service_tier = value,
            Some(ModelSettingSource::Profile) => precedence.profile.service_tier = value,
            Some(ModelSettingSource::GlobalDefault) => {
                precedence.global_default.service_tier = value;
            }
            None => {}
        }
    }
    precedence
}

pub(crate) fn apply_wire_adjustments(
    precedence: ModelSettingsPrecedence,
    adjustments: &[ModelChangeAdjustment],
) -> Option<ModelSettingsPrecedence> {
    validate_adjustments(adjustments).ok()?;
    let prior = resolve_wire_settings(precedence);
    let mut effective = prior.effective;
    for adjustment in adjustments {
        effective = match adjustment {
            ModelChangeAdjustment::ReasoningLevelClamped { from, to }
                if prior.reasoning_source != Some(ModelSettingSource::PerCall)
                    && effective.reasoning_level == Some(*from)
                    && from != to =>
            {
                EffectiveModelSettings {
                    reasoning_level: Some(*to),
                    ..effective
                }
            }
            ModelChangeAdjustment::ReasoningLevelCleared { from }
                if prior.reasoning_source != Some(ModelSettingSource::PerCall)
                    && effective.reasoning_level == Some(*from) =>
            {
                EffectiveModelSettings {
                    reasoning_level: None,
                    ..effective
                }
            }
            ModelChangeAdjustment::FastModeDisabled {}
                if prior.fast_mode_source != Some(ModelSettingSource::PerCall)
                    && effective.fast_mode == FastMode::Enabled =>
            {
                EffectiveModelSettings {
                    fast_mode: FastMode::Disabled,
                    ..effective
                }
            }
            ModelChangeAdjustment::ServiceTierCleared { from }
                if prior.service_tier_source != Some(ModelSettingSource::PerCall)
                    && effective.service_tier == Some(*from) =>
            {
                EffectiveModelSettings {
                    service_tier: None,
                    ..effective
                }
            }
            ModelChangeAdjustment::ReasoningLevelClamped { .. }
            | ModelChangeAdjustment::ReasoningLevelCleared { .. }
            | ModelChangeAdjustment::FastModeDisabled {}
            | ModelChangeAdjustment::ServiceTierCleared { .. } => return None,
        };
    }
    Some(with_wire_effective_adjustment(precedence, prior, effective))
}

fn unapply_wire_adjustments(
    settings: &ModelSettingsSnapshot,
    adjustments: &[ModelChangeAdjustment],
) -> Option<ModelSettingsPrecedence> {
    let settled = resolve_wire_settings(settings.precedence);
    let mut prior = settled.effective;
    for adjustment in adjustments {
        prior = match adjustment {
            ModelChangeAdjustment::ReasoningLevelClamped { from, to }
                if settled.reasoning_source != Some(ModelSettingSource::PerCall)
                    && settled.effective.reasoning_level == Some(*to) =>
            {
                EffectiveModelSettings {
                    reasoning_level: Some(*from),
                    ..prior
                }
            }
            ModelChangeAdjustment::ReasoningLevelCleared { from }
                if settled.reasoning_source != Some(ModelSettingSource::PerCall)
                    && settled.effective.reasoning_level.is_none() =>
            {
                EffectiveModelSettings {
                    reasoning_level: Some(*from),
                    ..prior
                }
            }
            ModelChangeAdjustment::FastModeDisabled {}
                if settled.fast_mode_source != Some(ModelSettingSource::PerCall)
                    && settled.effective.fast_mode == FastMode::Disabled =>
            {
                EffectiveModelSettings {
                    fast_mode: FastMode::Enabled,
                    ..prior
                }
            }
            ModelChangeAdjustment::ServiceTierCleared { from }
                if settled.service_tier_source != Some(ModelSettingSource::PerCall)
                    && settled.effective.service_tier.is_none() =>
            {
                EffectiveModelSettings {
                    service_tier: Some(*from),
                    ..prior
                }
            }
            ModelChangeAdjustment::ReasoningLevelClamped { .. }
            | ModelChangeAdjustment::ReasoningLevelCleared { .. }
            | ModelChangeAdjustment::FastModeDisabled {}
            | ModelChangeAdjustment::ServiceTierCleared { .. } => return None,
        };
    }
    Some(with_wire_effective_adjustment(
        settings.precedence,
        settled,
        prior,
    ))
}

pub(crate) fn snapshot_matches_model(
    model: &ModelSelection,
    settings: &ModelSettingsSnapshot,
) -> bool {
    match (model, settings.validated_for_selection_id) {
        (ModelSelection::Direct { selection_id }, Some(validated)) => *selection_id == validated,
        (ModelSelection::Direct { .. }, None) | (ModelSelection::Alias { .. }, _) => true,
    }
}

/// One automatic compatibility adjustment caused by a model change.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ModelChangeAdjustment {
    ReasoningLevelClamped {
        from: ReasoningLevel,
        to: ReasoningLevel,
    },
    ReasoningLevelCleared {
        from: ReasoningLevel,
    },
    FastModeDisabled {},
    ServiceTierCleared {
        from: ServiceTier,
    },
}

/// Client-visible exact capabilities for one direct selection.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelCapabilities {
    pub reasoning_levels: Vec<ReasoningLevel>,
    pub fast_mode_supported: bool,
    pub service_tiers: Vec<ServiceTier>,
}

impl ModelCapabilities {
    pub(crate) fn validate(&self) -> Result<(), FrameValidationError> {
        let reasoning = self
            .reasoning_levels
            .iter()
            .copied()
            .collect::<BTreeSet<_>>();
        let tiers = self.service_tiers.iter().copied().collect::<BTreeSet<_>>();
        if reasoning.len() != self.reasoning_levels.len()
            || tiers.len() != self.service_tiers.len()
            || !self.reasoning_levels.is_sorted()
            || !self.service_tiers.is_sorted()
        {
            return Err(FrameValidationError::ModelSettingsShape);
        }
        Ok(())
    }
}

pub(crate) fn validate_turn_settings_payload(
    defaults_version: CanonicalU64,
    requested_model: &ModelSelection,
    selected_direct_id: CanonicalUuid,
    per_call_override: ModelSettingsOverlay,
    settings: &ModelSettingsSnapshot,
    adjusted_from_selection_id: Option<CanonicalUuid>,
    adjustments: &[ModelChangeAdjustment],
) -> Result<(), FrameValidationError> {
    settings.validate()?;
    validate_adjustments(adjustments)?;
    let direct_selection_mismatch = matches!(
        requested_model,
        ModelSelection::Direct { selection_id } if *selection_id != selected_direct_id
    );
    let validation_mismatch = match settings.validated_for_selection_id {
        Some(selection_id) => selection_id != selected_direct_id,
        None => !settings.is_model_independent_provider_defaults(),
    };
    let adjustment_provenance_mismatch = unapply_wire_adjustments(settings, adjustments)
        .and_then(|unadjusted| apply_wire_adjustments(unadjusted, adjustments))
        != Some(settings.precedence);
    if defaults_version.value() == 0
        || direct_selection_mismatch
        || validation_mismatch
        || settings.precedence.per_call != per_call_override
        || match adjustments.is_empty() {
            true => adjusted_from_selection_id.is_some(),
            false => adjusted_from_selection_id.is_none_or(|prior| prior == selected_direct_id),
        }
        || adjustment_provenance_mismatch
    {
        return Err(FrameValidationError::ModelSettingsShape);
    }
    Ok(())
}

pub(crate) fn adjustments_target_explicit_overlay(
    overlay: ModelSettingsOverlay,
    adjustments: &[ModelChangeAdjustment],
) -> bool {
    adjustments.iter().any(|adjustment| match adjustment {
        ModelChangeAdjustment::ReasoningLevelClamped { .. }
        | ModelChangeAdjustment::ReasoningLevelCleared { .. } => {
            overlay.reasoning_level != SettingOverlay::Inherit
        }
        ModelChangeAdjustment::FastModeDisabled {} => overlay.fast_mode != FastModeOverlay::Inherit,
        ModelChangeAdjustment::ServiceTierCleared { .. } => {
            overlay.service_tier != SettingOverlay::Inherit
        }
    })
}

pub(crate) fn validate_adjustments(
    adjustments: &[ModelChangeAdjustment],
) -> Result<(), FrameValidationError> {
    if adjustments.len() > 3 {
        return Err(FrameValidationError::ModelSettingsShape);
    }
    let mut minimum_rank = 0;
    for adjustment in adjustments {
        let rank = match adjustment {
            ModelChangeAdjustment::ReasoningLevelClamped { .. }
            | ModelChangeAdjustment::ReasoningLevelCleared { .. } => 0,
            ModelChangeAdjustment::FastModeDisabled {} => 1,
            ModelChangeAdjustment::ServiceTierCleared { .. } => 2,
        };
        if rank < minimum_rank {
            return Err(FrameValidationError::ModelSettingsShape);
        }
        minimum_rank = rank + 1;
    }
    Ok(())
}
