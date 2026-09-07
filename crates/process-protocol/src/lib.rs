//! Closed versioned JSON-lines process protocol.
//!
//! This crate owns wire representations and frame validation only. Domain,
//! persistence, and client presentation values remain distinct mappings
//! (docs/spec/process-protocol.md).

mod operator_status;
mod review;
mod scalars;

pub use operator_status::*;
pub use review::*;
pub use scalars::*;

use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    fmt,
};

use serde::{
    Deserialize, Deserializer, Serialize, Serializer,
    de::{IgnoredAny, MapAccess, SeqAccess, Visitor},
};
use signalbox_domain::{
    CredentialProfileName as DomainCredentialProfileName,
    RunnerCapabilityClass as DomainRunnerCapabilityClass,
    RunnerWorkingDirectory as DomainRunnerWorkingDirectory, ToolDecisionRationale,
    ToolDenialReason, WorkspaceRepositoryKey as DomainWorkspaceRepositoryKey,
};

/// Maximum number of ordered parts in one process-protocol user input.
pub const MAX_USER_INPUT_PARTS: usize = signalbox_domain::UserContent::MAX_PARTS;
/// Maximum aggregate UTF-8 bytes across process-protocol text parts.
pub const MAX_USER_INPUT_TEXT_BYTES: usize = signalbox_domain::UserContent::MAX_TEXT_BYTES;
/// Maximum encoded bytes in one process-protocol attachment media type.
pub const MAX_USER_INPUT_MEDIA_TYPE_BYTES: usize = signalbox_domain::DeclaredMediaType::MAX_BYTES;
/// Maximum encoded bytes in one process-protocol attachment display filename.
pub const MAX_USER_INPUT_DISPLAY_FILENAME_BYTES: usize =
    signalbox_domain::AttachmentDisplayFilename::MAX_BYTES;

/// Closed semantic kind declared for one user attachment on the wire.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UserAttachmentKind {
    /// Image content.
    Image,
    /// Page- or document-oriented content.
    Document,
    /// Other file content.
    File,
}

/// One exact part in canonical ordered user input.
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum UserInputPart {
    /// Exact decoded text.
    Text {
        /// Nonempty text containing no U+0000.
        text: String,
    },
    /// Immutable blob reference and caller-declared metadata.
    Attachment {
        /// Canonical global blob identity.
        digest: CanonicalBlobDigest,
        /// Closed semantic attachment kind.
        kind: UserAttachmentKind,
        /// Exact visible-ASCII media-type declaration.
        media_type: String,
        /// Optional display basename, explicitly null when absent.
        #[serde(deserialize_with = "deserialize_required_nullable")]
        display_filename: Option<String>,
    },
}

impl fmt::Debug for UserInputPart {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Text { .. } => formatter
                .debug_struct("Text")
                .field("text", &"<redacted>")
                .finish(),
            Self::Attachment {
                digest,
                kind,
                media_type,
                display_filename,
            } => formatter
                .debug_struct("Attachment")
                .field("digest", digest)
                .field("kind", kind)
                .field("media_type", media_type)
                .field(
                    "display_filename",
                    &display_filename.as_ref().map(|_| "<redacted>"),
                )
                .finish(),
        }
    }
}

#[derive(signalbox_derive::Accessors)]
/// Canonical nonempty ordered user-input parts array.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct UserInputContent(
    /// Borrows the exact ordered parts.
    #[get(slice, as = "parts")]
    Vec<UserInputPart>,
);

struct UserInputContentVisitor;

impl<'de> Visitor<'de> for UserInputContentVisitor {
    type Value = UserInputContent;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "at most {MAX_USER_INPUT_PARTS} ordered user-input parts"
        )
    }

    fn visit_seq<AccessT>(self, mut sequence: AccessT) -> Result<Self::Value, AccessT::Error>
    where
        AccessT: SeqAccess<'de>,
    {
        let mut parts = Vec::with_capacity(
            sequence
                .size_hint()
                .unwrap_or_default()
                .min(MAX_USER_INPUT_PARTS),
        );
        while parts.len() < MAX_USER_INPUT_PARTS {
            match sequence.next_element::<UserInputPart>()? {
                Some(part) => parts.push(part),
                None => return Ok(UserInputContent(parts)),
            }
        }
        if sequence.next_element::<IgnoredAny>()?.is_some() {
            return Err(serde::de::Error::custom("too many user-input parts"));
        }
        Ok(UserInputContent(parts))
    }
}

impl<'de> Deserialize<'de> for UserInputContent {
    fn deserialize<DeserializerT>(deserializer: DeserializerT) -> Result<Self, DeserializerT::Error>
    where
        DeserializerT: Deserializer<'de>,
    {
        deserializer.deserialize_seq(UserInputContentVisitor)
    }
}

impl UserInputContent {
    /// Wraps one text part for text-only clients.
    pub fn text(value: String) -> Self {
        Self(vec![UserInputPart::Text { text: value }])
    }

    /// Wraps a complete parts array for structural validation at frame encode.
    pub fn from_parts(parts: Vec<UserInputPart>) -> Self {
        Self(parts)
    }

    /// Borrows text when this is exactly one text part.
    pub fn single_text(&self) -> Option<&str> {
        match self.0.as_slice() {
            [UserInputPart::Text { text }] => Some(text),
            _ => None,
        }
    }

    /// Transfers ownership of the exact ordered parts.
    pub fn into_parts(self) -> Vec<UserInputPart> {
        self.0
    }

    fn validate(&self) -> Result<(), FrameValidationError> {
        if self.0.is_empty() || self.0.len() > MAX_USER_INPUT_PARTS {
            return Err(FrameValidationError::UserContentShape);
        }

        let mut text_bytes = 0_usize;
        let mut previous_was_text = false;
        for part in &self.0 {
            match part {
                UserInputPart::Text { text } => {
                    if previous_was_text || text.is_empty() || text.contains('\0') {
                        return Err(FrameValidationError::UserContentShape);
                    }
                    text_bytes = text_bytes
                        .checked_add(text.len())
                        .ok_or(FrameValidationError::UserContentShape)?;
                    if text_bytes > MAX_USER_INPUT_TEXT_BYTES {
                        return Err(FrameValidationError::UserContentShape);
                    }
                    previous_was_text = true;
                }
                UserInputPart::Attachment {
                    media_type,
                    display_filename,
                    ..
                } => {
                    if media_type.is_empty()
                        || media_type.len() > MAX_USER_INPUT_MEDIA_TYPE_BYTES
                        || !media_type.bytes().all(|byte| (0x21..=0x7e).contains(&byte))
                    {
                        return Err(FrameValidationError::UserContentShape);
                    }
                    if display_filename.as_ref().is_some_and(|filename| {
                        filename.is_empty()
                            || filename.len() > MAX_USER_INPUT_DISPLAY_FILENAME_BYTES
                            || filename == "."
                            || filename == ".."
                            || filename.contains('/')
                            || filename.contains('\\')
                            || filename.contains('\0')
                    }) {
                        return Err(FrameValidationError::UserContentShape);
                    }
                    previous_was_text = false;
                }
            }
        }
        Ok(())
    }
}

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
    fn validate(&self) -> Result<(), FrameValidationError> {
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

    fn validate_defaults(&self) -> Result<(), FrameValidationError> {
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
    fn validate(&self) -> Result<(), FrameValidationError> {
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

fn overlay_inheriting_from(
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

fn apply_wire_adjustments(
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

fn snapshot_matches_model(model: &ModelSelection, settings: &ModelSettingsSnapshot) -> bool {
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
    fn validate(&self) -> Result<(), FrameValidationError> {
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

/// Explicit acknowledgement carried by a root-placement creation or update.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RootPlacementGlobalReadIntent {
    /// The caller explicitly accepts that root placement grants global read.
    Acknowledged,
}

/// One session's opt-in dotted placement decision.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SessionPlacement {
    /// Preserve legacy unrestricted conversation-read behavior.
    Pathless {},
    /// Place below root; the parent directory's subtree is readable.
    Scoped { path: String },
    /// Place at root with loud acknowledgement that this grants global read.
    RootGlobalRead {
        path: String,
        intent: RootPlacementGlobalReadIntent,
    },
}

impl SessionPlacement {
    fn is_pathless(&self) -> bool {
        matches!(self, Self::Pathless {})
    }
    /// Constructs and validates a non-root placement.
    pub fn try_scoped(path: String) -> Result<Self, CanonicalValueError> {
        let placement = Self::Scoped { path };
        validate_session_placement_shape(&placement).map_err(|_| CanonicalValueError::Placement)?;
        Ok(placement)
    }

    /// Constructs and validates the loud root-global-read decision.
    pub fn try_root_global_read(path: String) -> Result<Self, CanonicalValueError> {
        let placement = Self::RootGlobalRead {
            path,
            intent: RootPlacementGlobalReadIntent::Acknowledged,
        };
        validate_session_placement_shape(&placement).map_err(|_| CanonicalValueError::Placement)?;
        Ok(placement)
    }
}

impl Default for SessionPlacement {
    fn default() -> Self {
        Self::Pathless {}
    }
}

fn validate_session_placement_shape(
    placement: &SessionPlacement,
) -> Result<(), FrameValidationError> {
    let (path, root) = match placement {
        SessionPlacement::Pathless {} => return Ok(()),
        SessionPlacement::Scoped { path } => (path, false),
        SessionPlacement::RootGlobalRead { path, .. } => (path, true),
    };
    if path.len() > 64 * 64 + 63 {
        return Err(FrameValidationError::PlacementShape);
    }
    let segment_count = path.split('.').try_fold(0_usize, |count, segment| {
        let next_count = count + 1;
        (next_count <= 64
            && !segment.is_empty()
            && segment.len() <= 64
            && segment
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')))
        .then_some(next_count)
    });
    let shape_valid = segment_count.is_some_and(|count| (count == 1) == root);
    if shape_valid {
        Ok(())
    } else {
        Err(FrameValidationError::PlacementShape)
    }
}

/// One exact complete session-metadata object.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionMetadata {
    title: Option<String>,
    tags: Vec<String>,
    attributes: MetadataAttributes,
    archived: bool,
}

impl SessionMetadata {
    /// Validates and canonicalizes one complete metadata object.
    pub fn try_new(
        title: Option<String>,
        tags: Vec<String>,
        attributes: Vec<(String, String)>,
        archived: bool,
    ) -> Result<Self, CanonicalValueError> {
        Self::try_new_with_count_limits(title, tags, attributes, archived, None, None)
    }

    /// Validates canonical metadata and deployment tag and attribute policies.
    pub fn try_new_with_count_limits(
        title: Option<String>,
        tags: Vec<String>,
        attributes: Vec<(String, String)>,
        archived: bool,
        max_tags: Option<usize>,
        max_attributes: Option<usize>,
    ) -> Result<Self, CanonicalValueError> {
        let mut total_utf8_bytes = 0usize;
        if let Some(title) = title.as_deref() {
            validate_nonempty_metadata_text(title)?;
            add_metadata_utf8_bytes(&mut total_utf8_bytes, title)?;
        }
        let tags = canonical_metadata_tags(tags, max_tags)?;
        for tag in &tags {
            add_metadata_utf8_bytes(&mut total_utf8_bytes, tag)?;
        }
        let attributes = MetadataAttributes::try_new(attributes, max_attributes)?;
        for (key, value) in &attributes.0 {
            add_metadata_utf8_bytes(&mut total_utf8_bytes, key)?;
            add_metadata_utf8_bytes(&mut total_utf8_bytes, value)?;
        }
        Ok(Self {
            title,
            tags,
            attributes,
            archived,
        })
    }

    /// Constructs the unwritten empty, non-archived object.
    pub fn empty() -> Self {
        Self {
            title: None,
            tags: Vec::new(),
            attributes: MetadataAttributes(BTreeMap::new()),
            archived: false,
        }
    }

    /// Borrows the optional exact title.
    pub fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }

    /// Iterates tags in exact scalar order.
    pub fn tags(&self) -> impl ExactSizeIterator<Item = &str> {
        self.tags.iter().map(String::as_str)
    }

    /// Iterates attributes in exact key scalar order.
    pub fn attributes(&self) -> impl ExactSizeIterator<Item = (&str, &str)> {
        self.attributes
            .0
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_str()))
    }

    /// Returns whether the session is archived.
    pub const fn archived(&self) -> bool {
        self.archived
    }

    fn is_initial(&self) -> bool {
        self.title.is_none()
            && self.tags.is_empty()
            && self.attributes.0.is_empty()
            && !self.archived
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSessionMetadata {
    #[serde(deserialize_with = "deserialize_required_nullable")]
    title: Option<String>,
    #[serde(deserialize_with = "deserialize_session_metadata_tags")]
    tags: Vec<String>,
    attributes: MetadataAttributes,
    archived: bool,
}

impl<'de> Deserialize<'de> for SessionMetadata {
    fn deserialize<DeserializerT>(deserializer: DeserializerT) -> Result<Self, DeserializerT::Error>
    where
        DeserializerT: Deserializer<'de>,
    {
        let raw = RawSessionMetadata::deserialize(deserializer)?;
        Self::try_new(
            raw.title,
            raw.tags,
            raw.attributes.0.into_iter().collect(),
            raw.archived,
        )
        .map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(transparent)]
struct MetadataAttributes(BTreeMap<String, String>);

impl MetadataAttributes {
    fn try_new(
        values: Vec<(String, String)>,
        maximum: Option<usize>,
    ) -> Result<Self, CanonicalValueError> {
        if maximum.is_some_and(|maximum| values.len() > maximum) {
            return Err(CanonicalValueError::Metadata);
        }
        let mut attributes = BTreeMap::new();
        for (key, value) in values {
            validate_nonempty_metadata_text(&key)?;
            validate_indexed_metadata_text(&key)?;
            validate_metadata_text(&value)?;
            if attributes.insert(key, value).is_some() {
                return Err(CanonicalValueError::Metadata);
            }
        }
        Ok(Self(attributes))
    }
}

struct MetadataAttributesVisitor;

impl<'de> Visitor<'de> for MetadataAttributesVisitor {
    type Value = MetadataAttributes;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("an exact session metadata attribute object")
    }

    fn visit_map<AccessT>(self, mut map: AccessT) -> Result<Self::Value, AccessT::Error>
    where
        AccessT: MapAccess<'de>,
    {
        let mut attributes = BTreeMap::new();
        while let Some((key, value)) = map.next_entry::<String, String>()? {
            validate_nonempty_metadata_text(&key).map_err(serde::de::Error::custom)?;
            validate_indexed_metadata_text(&key).map_err(serde::de::Error::custom)?;
            validate_metadata_text(&value).map_err(serde::de::Error::custom)?;
            if attributes.insert(key, value).is_some() {
                return Err(serde::de::Error::custom(
                    "duplicate session metadata attribute key",
                ));
            }
        }
        Ok(MetadataAttributes(attributes))
    }
}

impl<'de> Deserialize<'de> for MetadataAttributes {
    fn deserialize<DeserializerT>(deserializer: DeserializerT) -> Result<Self, DeserializerT::Error>
    where
        DeserializerT: Deserializer<'de>,
    {
        deserializer.deserialize_map(MetadataAttributesVisitor)
    }
}

fn validate_metadata_text(value: &str) -> Result<(), CanonicalValueError> {
    if value.contains('\0') {
        Err(CanonicalValueError::Metadata)
    } else {
        Ok(())
    }
}

fn validate_nonempty_metadata_text(value: &str) -> Result<(), CanonicalValueError> {
    if value.is_empty() {
        Err(CanonicalValueError::Metadata)
    } else {
        validate_metadata_text(value)
    }
}

fn validate_indexed_metadata_text(value: &str) -> Result<(), CanonicalValueError> {
    if value.len() > MAX_SESSION_METADATA_INDEXED_UTF8_BYTES {
        Err(CanonicalValueError::Metadata)
    } else {
        Ok(())
    }
}

fn add_metadata_utf8_bytes(total: &mut usize, value: &str) -> Result<(), CanonicalValueError> {
    *total = total.saturating_add(value.len());
    if *total > MAX_SESSION_METADATA_TOTAL_UTF8_BYTES {
        Err(CanonicalValueError::Metadata)
    } else {
        Ok(())
    }
}

fn canonical_metadata_tags(
    values: Vec<String>,
    maximum: Option<usize>,
) -> Result<Vec<String>, CanonicalValueError> {
    if maximum.is_some_and(|maximum| values.len() > maximum) {
        return Err(CanonicalValueError::Metadata);
    }
    let mut tags = BTreeSet::new();
    for tag in values {
        validate_nonempty_metadata_text(&tag)?;
        validate_indexed_metadata_text(&tag)?;
        if !tags.insert(tag) {
            return Err(CanonicalValueError::Metadata);
        }
    }
    Ok(tags.into_iter().collect())
}

fn deserialize_session_metadata_tags<'de, DeserializerT>(
    deserializer: DeserializerT,
) -> Result<Vec<String>, DeserializerT::Error>
where
    DeserializerT: Deserializer<'de>,
{
    Vec::<String>::deserialize(deserializer)
}

fn deserialize_required_metadata_tags<'de, DeserializerT>(
    deserializer: DeserializerT,
) -> Result<Vec<String>, DeserializerT::Error>
where
    DeserializerT: Deserializer<'de>,
{
    Vec::<String>::deserialize(deserializer)
}

/// Closed actor provenance carried by a metadata last-writer stamp.
///
/// The variants mirror the domain actor inventory exactly, because durable
/// metadata already records every one of them: the tool-facing replacement
/// constructor stamps a tool writer, and a narrower wire enum would leave a
/// readable durable snapshot with no wire projection.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum MetadataActor {
    /// The user wrote the snapshot.
    User {},
    /// Daemon core wrote the snapshot without delegated agency.
    Core {},
    /// Model output from one exact turn wrote the snapshot.
    Model {
        /// The turn whose model output acted.
        turn_id: CanonicalUuid,
    },
    /// The startup recovery scan wrote the snapshot.
    Recovery {},
    /// Execution of one exact tool request wrote the snapshot.
    Tool {
        /// The tool request whose execution acted.
        tool_request_id: CanonicalUuid,
    },
}

#[derive(signalbox_derive::Accessors)]
/// The post-lock database statement time and actor of the latest replacement.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MetadataLastWriter {
    /// Returns the nonnegative Unix-microsecond transaction timestamp.
    #[get(copy)]
    updated_at_unix_micros: CanonicalU64,
    /// Returns the closed actor provenance.
    #[get(copy)]
    actor: MetadataActor,
}

/// How a new live session relates to one selected imported frontier.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImportedSessionRelationship {
    /// Continue from the selected imported boundary.
    Resume,
    /// Branch from the selected imported boundary.
    Fork,
}

/// One closed client-selected treatment for submitted input.
///
/// Omitting this value from `submit_input` preserves the baseline
/// start-when-idle treatment. Steering and queueing carry the exact active turn
/// the client observed so the domain can reject a stale target.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum InputDelivery {
    /// Start new work only while the session slot is idle.
    StartWhenIdle {},
    /// Bind the input to the active turn's next safe point.
    Steer {
        /// Exact active turn observed by the client.
        expected_active_turn_id: CanonicalUuid,
    },
    /// Queue new work behind the active turn.
    Queue {
        /// Exact active turn observed by the client.
        expected_active_turn_id: CanonicalUuid,
    },
}

fn deserialize_present_input_delivery<'de, DeserializerT>(
    deserializer: DeserializerT,
) -> Result<Option<InputDelivery>, DeserializerT::Error>
where
    DeserializerT: Deserializer<'de>,
{
    InputDelivery::deserialize(deserializer).map(Some)
}

impl MetadataLastWriter {
    /// Constructs one exact last-writer stamp.
    pub const fn new(updated_at_unix_micros: CanonicalU64, actor: MetadataActor) -> Self {
        Self {
            updated_at_unix_micros,
            actor,
        }
    }
}

/// One closed conversation origin class.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConversationOrigin {
    /// A native session.
    NativeSession,
    /// An immutable imported conversation.
    ImportedConversation,
}

/// Which conversation origin classes one unified list request selects.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConversationOriginFilter {
    /// Native sessions only.
    Native,
    /// Imported conversations only.
    Imported,
    /// Both origin classes.
    All,
}

#[derive(signalbox_derive::Accessors)]
/// One exclusive unified keyset cursor naming the last listed conversation.
///
/// The unified page order is by conversation identity UUID value, native
/// before imported for a theoretical equal identity, so the cursor names one
/// total position across both origin classes.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConversationCursor {
    /// Returns the origin class of the cursor position.
    #[get(copy)]
    /// Origin class of the cursor position.
    origin: ConversationOrigin,
    /// Returns the conversation identity at the cursor position.
    #[get(copy)]
    /// Conversation identity at the cursor position.
    conversation_id: CanonicalUuid,
}

impl ConversationCursor {
    /// Names one exact unified cursor position.
    pub const fn new(origin: ConversationOrigin, conversation_id: CanonicalUuid) -> Self {
        Self {
            origin,
            conversation_id,
        }
    }
}

/// One exact stored imported source format and converter version.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImportedConversationSourceFormat {
    /// Claude Code session JSONL interpreted by converter version 1.
    ClaudeCodeSessionJsonlV1,
    /// Claude Code session JSONL interpreted by converter version 2.
    ClaudeCodeSessionJsonlV2,
    /// Codex rollout JSONL interpreted by converter version 1.
    CodexRolloutJsonlV1,
}

/// One closed per-origin unified conversation summary.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "origin", rename_all = "snake_case", deny_unknown_fields)]
pub enum ConversationSummary {
    /// One native session with its current organizational facts.
    NativeSession {
        /// Session identity.
        session_id: CanonicalUuid,
        /// Exact optional metadata title.
        #[serde(deserialize_with = "deserialize_required_nullable")]
        title: Option<String>,
        /// Whether the session is archived.
        archived: bool,
        /// Current defaults version.
        defaults_version: CanonicalU64,
    },
    /// One immutable imported conversation snapshot.
    ImportedConversation {
        /// Imported conversation identity.
        imported_conversation_id: CanonicalUuid,
        /// Exact optional source-derived display title.
        #[serde(deserialize_with = "deserialize_required_nullable")]
        title: Option<String>,
        /// Total normalized entry count; the greatest position a
        /// continuation may select.
        entry_count: CanonicalU64,
        /// Exact stored source format and converter version.
        source_format: ImportedConversationSourceFormat,
    },
}

impl ConversationSummary {
    /// Returns the unified cursor position this summary occupies.
    pub const fn cursor(&self) -> ConversationCursor {
        match self {
            Self::NativeSession { session_id, .. } => {
                ConversationCursor::new(ConversationOrigin::NativeSession, *session_id)
            }
            Self::ImportedConversation {
                imported_conversation_id,
                ..
            } => ConversationCursor::new(
                ConversationOrigin::ImportedConversation,
                *imported_conversation_id,
            ),
        }
    }

    fn validate(&self) -> Result<(), FrameValidationError> {
        match self {
            Self::NativeSession {
                title,
                defaults_version,
                ..
            } => {
                if let Some(title) = title {
                    validate_nonempty_metadata_text(title)
                        .map_err(|_| FrameValidationError::ConversationListShape)?;
                    let mut total_utf8_bytes = 0usize;
                    add_metadata_utf8_bytes(&mut total_utf8_bytes, title)
                        .map_err(|_| FrameValidationError::ConversationListShape)?;
                }
                if defaults_version.value() == 0 {
                    return Err(FrameValidationError::ConversationListShape);
                }
                Ok(())
            }
            Self::ImportedConversation {
                title, entry_count, ..
            } => {
                if let Some(title) = title {
                    validate_imported_display_title(title)?;
                }
                if entry_count.value() == 0 {
                    return Err(FrameValidationError::ConversationListShape);
                }
                Ok(())
            }
        }
    }
}

fn validate_tool_approval_event_shape(
    decision: &ToolApprovalEventDecision,
    decider: &ToolApprovalEventDecider,
    rationale: &Option<String>,
) -> Result<(), FrameValidationError> {
    let shape_matches = match decider {
        ToolApprovalEventDecider::User { .. } => match decision {
            ToolApprovalEventDecision::Approve {}
            | ToolApprovalEventDecision::Deny { reason: None } => rationale.is_none(),
            ToolApprovalEventDecision::Deny {
                reason: Some(reason),
            } => rationale.is_none() && ToolDenialReason::try_new(reason.clone()).is_ok(),
        },
        ToolApprovalEventDecider::Delegate { .. } => match decision {
            ToolApprovalEventDecision::Approve {} => rationale
                .as_ref()
                .is_some_and(|rationale| ToolDecisionRationale::try_new(rationale.clone()).is_ok()),
            // A delegate denial's reason is exactly the derivation from its
            // rationale: absent only when the rationale derives nothing.
            ToolApprovalEventDecision::Deny { reason } => {
                rationale.as_ref().is_some_and(|rationale| {
                    ToolDecisionRationale::try_new(rationale.clone()).is_ok_and(|rationale| {
                        ToolDenialReason::from_rationale(&rationale)
                            .as_ref()
                            .map(ToolDenialReason::as_str)
                            == reason.as_deref()
                    })
                })
            }
        },
        ToolApprovalEventDecider::UserOverride { .. } => {
            matches!(decision, ToolApprovalEventDecision::Approve {}) && rationale.is_none()
        }
    };
    if !shape_matches {
        return Err(FrameValidationError::ToolApprovalShape);
    }
    Ok(())
}

/// Validates the structural display-title shape: nonempty single-line text
/// without U+0000 and no leading or trailing ASCII space or tab.
fn validate_imported_display_title(title: &str) -> Result<(), FrameValidationError> {
    if title.is_empty()
        || title.contains(['\0', '\n', '\r'])
        || title.starts_with([' ', '\t'])
        || title.ends_with([' ', '\t'])
    {
        return Err(FrameValidationError::ConversationListShape);
    }
    Ok(())
}

fn validate_session_template_name(value: &str) -> Result<(), FrameValidationError> {
    let first_is_admitted = value
        .as_bytes()
        .first()
        .is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit());
    if value.len() > 128
        || !first_is_admitted
        || value.bytes().any(|byte| {
            !byte.is_ascii_lowercase() && !byte.is_ascii_digit() && !b"._-".contains(&byte)
        })
    {
        return Err(FrameValidationError::TemplateShape);
    }
    Ok(())
}

fn validate_review_key(value: &str) -> Result<(), FrameValidationError> {
    if value.is_empty() || value.len() > 1_024 || value.contains('\0') {
        return Err(FrameValidationError::ReviewShape);
    }
    Ok(())
}

fn validate_review_text(value: &str) -> Result<(), FrameValidationError> {
    if value.is_empty() || value.len() > 65_536 || value.contains('\0') {
        return Err(FrameValidationError::ReviewShape);
    }
    Ok(())
}

fn validate_review_judgment_disposition(
    disposition: &ReviewJudgmentDisposition,
) -> Result<(), FrameValidationError> {
    if let ReviewJudgmentDisposition::Rejected { reason } = disposition {
        validate_review_text(reason)?;
    }
    Ok(())
}

fn validate_review_finding_event(event: &ReviewFindingEvent) -> Result<(), FrameValidationError> {
    match event {
        ReviewFindingEvent::Rejected { reason }
        | ReviewFindingEvent::BlockedWithReason { reason, .. } => validate_review_text(reason),
        ReviewFindingEvent::Accepted {}
        | ReviewFindingEvent::Duplicate { .. }
        | ReviewFindingEvent::Superseded { .. }
        | ReviewFindingEvent::Stale {}
        | ReviewFindingEvent::Fixed {} => Ok(()),
    }
}

fn validate_review_orchestration_snapshot(
    snapshot: &ReviewOrchestrationSnapshot,
) -> Result<(), FrameValidationError> {
    validate_review_key(&snapshot.concern_set_version)?;
    if snapshot.concerns.is_empty() || snapshot.concerns.len() > MAX_REVIEW_ORCHESTRATION_MEMBERS {
        return Err(FrameValidationError::ReviewShape);
    }
    let mut keys = HashSet::new();
    for concern in &snapshot.concerns {
        validate_review_key(&concern.key)?;
        if !keys.insert(&concern.key) {
            return Err(FrameValidationError::ReviewShape);
        }
        let valid_pass = match concern.status {
            ReviewOrchestrationConcernStatus::Pending => concern.pass_id.is_none(),
            ReviewOrchestrationConcernStatus::Succeeded
            | ReviewOrchestrationConcernStatus::Failed
            | ReviewOrchestrationConcernStatus::Blocked
            | ReviewOrchestrationConcernStatus::Superseded => concern.pass_id.is_some(),
            ReviewOrchestrationConcernStatus::Cancelled => true,
        };
        if !valid_pass {
            return Err(FrameValidationError::ReviewShape);
        }
    }
    let pending_concern_count = snapshot
        .concerns
        .iter()
        .filter(|concern| concern.status == ReviewOrchestrationConcernStatus::Pending)
        .count();
    let all_concerns_succeeded = snapshot
        .concerns
        .iter()
        .all(|concern| concern.status == ReviewOrchestrationConcernStatus::Succeeded);
    let counts = snapshot.counts;
    let no_judgment_or_terminal_counts = counts.judgment_member_count.value() == 0
        && counts.judgment_effect_applied_count.value() == 0
        && counts.repair_fixed_count.value() == 0
        && counts.publication_published_count.value() == 0;
    let judgment_is_complete =
        counts.judgment_effect_applied_count.value() == counts.judgment_member_count.value();
    let judgment_is_incomplete =
        counts.judgment_member_count.value() > counts.judgment_effect_applied_count.value();
    let state_matches_facts = match snapshot.state {
        ReviewOrchestrationState::AwaitingImport | ReviewOrchestrationState::ImportIncomplete => {
            pending_concern_count == snapshot.concerns.len()
                && counts.finding_count.value() == 0
                && no_judgment_or_terminal_counts
        }
        ReviewOrchestrationState::AwaitingConcerns => {
            pending_concern_count > 0 && no_judgment_or_terminal_counts
        }
        ReviewOrchestrationState::FanoutIncomplete => {
            pending_concern_count == 0 && !all_concerns_succeeded && no_judgment_or_terminal_counts
        }
        ReviewOrchestrationState::AwaitingJudgment => {
            all_concerns_succeeded && no_judgment_or_terminal_counts
        }
        ReviewOrchestrationState::AwaitingJudgmentEffects
        | ReviewOrchestrationState::JudgmentIncomplete => {
            all_concerns_succeeded
                && judgment_is_incomplete
                && counts.repair_fixed_count.value() == 0
                && counts.publication_published_count.value() == 0
        }
        ReviewOrchestrationState::AwaitingRepair => {
            all_concerns_succeeded
                && judgment_is_complete
                && counts.repair_fixed_count.value() == 0
                && counts.publication_published_count.value() == 0
        }
        ReviewOrchestrationState::RepairIncomplete => {
            all_concerns_succeeded
                && judgment_is_complete
                && counts.publication_published_count.value() == 0
        }
        ReviewOrchestrationState::AwaitingPublication => {
            all_concerns_succeeded
                && judgment_is_complete
                && counts.publication_published_count.value() == 0
        }
        ReviewOrchestrationState::PublicationIncomplete => {
            all_concerns_succeeded
                && judgment_is_complete
                && counts.publication_published_count.value() < counts.judgment_member_count.value()
        }
        ReviewOrchestrationState::Complete => all_concerns_succeeded && judgment_is_complete,
    };
    if !state_matches_facts {
        return Err(FrameValidationError::ReviewShape);
    }
    if counts.finding_count.value() > MAX_REVIEW_ORCHESTRATION_MEMBERS as u64
        || counts.judgment_member_count.value() > counts.finding_count.value()
        || counts.judgment_effect_applied_count.value() > counts.judgment_member_count.value()
        || counts.repair_fixed_count.value() > counts.judgment_member_count.value()
        || counts.publication_published_count.value() > counts.judgment_member_count.value()
        || counts
            .repair_fixed_count
            .value()
            .checked_add(counts.publication_published_count.value())
            .is_none_or(|terminal_count| terminal_count > counts.judgment_member_count.value())
    {
        return Err(FrameValidationError::ReviewShape);
    }
    Ok(())
}

/// Closed durable goal-command rejection vocabulary.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalCommandRejection {
    /// The target session does not exist.
    SessionNotFound,
    /// The session's closure is pending; the closure settles the goal.
    SessionClosing,
    /// A goal is already pursuing or blocked.
    GoalAlreadyAttached,
    /// The session has no goal lineage.
    GoalNotAttached,
    /// The session's selected model alias is absent from daemon configuration.
    UnknownModelAlias,
    /// The session accepted-input position cannot advance beyond `u64::MAX`.
    AcceptancePositionExhausted,
    /// Resume requires a blocked current generation.
    RequiresBlocked,
    /// Stop or supersede requires a pursuing or blocked generation.
    RequiresPursuingOrBlocked,
    /// No successor generation can be represented.
    GenerationExhausted,
    /// No successor event position can be represented.
    EventOrdinalExhausted,
}

/// Closed blocked-reason vocabulary at the process boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalBlockedReason {
    /// Progress requires information or a decision from the user.
    UserInputRequired,
    /// Progress requires an external state change.
    ExternalChangeRequired,
    /// Progress requires authority the session does not hold.
    AuthorizationRequired,
    /// The preceding goal turn failed and was not retried.
    ExecutionFailure,
    FinishCheckFailed,
}

/// Provenance for one blocked event at the process boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum GoalBlockedProvenance {
    /// The model declared the blocked transition through its correlated tool.
    Model {
        /// Exact invoking turn.
        turn_id: CanonicalUuid,
        /// Exact invoking tool request.
        tool_request_id: CanonicalUuid,
    },
    /// The scheduler observed one unsuccessfully terminalized goal turn.
    ExecutionFailure {
        /// Exact failed turn.
        turn_id: CanonicalUuid,
    },
}

/// One generation's derived lifecycle state at the process boundary.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum GoalLifecycleState {
    /// Autonomous scheduling continues.
    Pursuing {},
    /// Autonomous scheduling pauses pending an explicit user transition.
    Blocked {
        /// Closed blocked reason.
        reason: GoalBlockedReason,
        /// Exact statement of what is needed.
        need: String,
    },
    /// The model declared completion.
    Achieved {
        /// Turn containing the final-report declaration.
        turn_id: CanonicalUuid,
        /// Tool request immediately preceded by the final-report transcript part.
        tool_request_id: CanonicalUuid,
    },
    /// The user explicitly ended this generation.
    UserStopped {},
    /// Another immutable statement replaced this generation.
    Superseded {
        /// Successor generation commissioned by the same event.
        by_generation: CanonicalU64,
    },
    /// The session closed beneath this generation.
    SessionClosed {
        /// Closed session outcome that settled it.
        outcome: SessionClosureOutcome,
    },
}

/// The closed session outcomes that settle a live goal generation.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionClosureOutcome {
    /// Closed with a retryable cause standing.
    FailedRetryable,
    /// Closed with a structural cause standing.
    FailedStructural,
    /// Closed with no classified cause.
    FailedUnknown,
    /// A human or rule stopped the session.
    Stopped,
    /// A newer session owns the work, or the work is gone.
    Superseded,
    /// An operator wrote the session off.
    Abandoned,
    /// The session never did the work and never will.
    Retired,
}

/// The closed actor classification recorded with a lifecycle transition.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LifecycleActorClass {
    /// Daemon core.
    Core,
    /// The single user's authority.
    Operator,
    /// A module, without saying which: the classification is what the
    /// boundary carries, and the durable goal event keeps the exact module.
    Module,
    /// The recovery scan or liveness watchdog.
    Watchdog,
}

/// One append-only goal event payload at the process boundary.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum GoalHistoryEvent {
    /// The user commissioned an immutable statement.
    Commissioned {
        /// Exact immutable statement.
        statement: String,
        /// Durable user command provenance.
        command_id: CommandId,
    },
    /// Pursuit paused with a typed reason and exact need.
    Blocked {
        /// Closed reason.
        reason: GoalBlockedReason,
        /// Exact statement of what is needed.
        need: String,
        /// Typed transition provenance.
        provenance: GoalBlockedProvenance,
    },
    /// The user resumed blocked pursuit.
    Resumed {
        /// Optional exact next-turn guidance.
        #[serde(deserialize_with = "deserialize_required_nullable")]
        guidance: Option<String>,
        /// Durable user command provenance.
        command_id: CommandId,
    },
    /// The model declared achievement with its final report.
    Achieved {
        /// Exact final report.
        report: String,
        /// Invoking turn.
        turn_id: CanonicalUuid,
        /// Invoking tool request.
        tool_request_id: CanonicalUuid,
    },
    /// The user explicitly ended the generation.
    UserStopped {
        /// Durable user command provenance.
        command_id: CommandId,
    },
    /// The user atomically replaced the active statement.
    Superseded {
        /// Newly commissioned immutable statement.
        replacement_statement: String,
        /// Durable user command provenance.
        command_id: CommandId,
    },
    /// The session closed, settling this generation.
    SessionClosed {
        /// Closed session outcome that settled it.
        outcome: SessionClosureOutcome,
        /// Classified actor that closed the session.
        actor: LifecycleActorClass,
    },
}

fn validate_goal_text(value: &str) -> Result<(), FrameValidationError> {
    if value.is_empty() || value.len() > MAX_CONTENT_FRAGMENT_BYTES || value.contains('\0') {
        return Err(FrameValidationError::GoalShape);
    }
    Ok(())
}

fn validate_goal_state(state: &GoalLifecycleState) -> Result<(), FrameValidationError> {
    match state {
        GoalLifecycleState::Blocked { need, .. } => validate_goal_text(need),
        GoalLifecycleState::Superseded { by_generation } if by_generation.value() == 0 => {
            Err(FrameValidationError::GoalShape)
        }
        GoalLifecycleState::Pursuing {}
        | GoalLifecycleState::Achieved { .. }
        | GoalLifecycleState::UserStopped {}
        | GoalLifecycleState::Superseded { .. }
        | GoalLifecycleState::SessionClosed { .. } => Ok(()),
    }
}

fn validate_goal_event(event: &GoalHistoryEvent) -> Result<(), FrameValidationError> {
    match event {
        GoalHistoryEvent::Commissioned { statement, .. } => validate_goal_text(statement),
        GoalHistoryEvent::Blocked {
            reason,
            need,
            provenance,
        } => {
            validate_goal_text(need)?;
            let scheduler_reason = match reason {
                GoalBlockedReason::UserInputRequired
                | GoalBlockedReason::ExternalChangeRequired
                | GoalBlockedReason::AuthorizationRequired
                | GoalBlockedReason::FinishCheckFailed => false,
                GoalBlockedReason::ExecutionFailure => true,
            };
            let scheduler_provenance = match provenance {
                GoalBlockedProvenance::Model { .. } => false,
                GoalBlockedProvenance::ExecutionFailure { .. } => true,
            };
            if scheduler_reason != scheduler_provenance {
                return Err(FrameValidationError::GoalShape);
            }
            Ok(())
        }
        GoalHistoryEvent::Resumed {
            guidance: Some(guidance),
            ..
        } => validate_goal_text(guidance),
        GoalHistoryEvent::Achieved { report, .. } => validate_goal_text(report),
        GoalHistoryEvent::Superseded {
            replacement_statement,
            ..
        } => validate_goal_text(replacement_statement),
        GoalHistoryEvent::Resumed { guidance: None, .. }
        | GoalHistoryEvent::UserStopped { .. }
        | GoalHistoryEvent::SessionClosed { .. } => Ok(()),
    }
}

/// Explicit delegated-child scope selected by a parent termination request.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DescendantTerminationScope {
    /// Apply the stop only to the named parent session.
    ParentAlone,
    /// Evaluate every reachable delegated-child relationship.
    ParentAndDescendants,
}

/// Immutable authority fence a commissioned-session request records.
///
/// The shapes mirror the repository-watch dispatch fence: a pull-request fence
/// names the pull request, its exact head commit, the repository and branch
/// holding that head, and the base branch; a branch fence names the repository
/// and branch alone. Field admission (slug, commit, and branch grammar) is the
/// daemon's, at command construction.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "target", rename_all = "snake_case", deny_unknown_fields)]
pub enum CommissionedSessionFence {
    /// Exact pull-request authority for the commissioned session.
    PullRequest {
        /// Repository whose pull request the session is commissioned against.
        repository: String,
        /// Positive pull-request number within the repository.
        pull_request: CanonicalU64,
        /// Exact head commit authorized at commissioning time.
        head_sha: String,
        /// Repository containing the authorized head branch.
        head_repository: String,
        /// Authorized head branch.
        head_branch: String,
        /// Authorized base branch.
        base_branch: String,
    },
    /// Exact branch authority for the commissioned session.
    Branch {
        /// Repository whose branch the session is commissioned against.
        repository: String,
        /// Authorized branch.
        branch: String,
    },
}

/// Whether a creation holds its start gate.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StartGate {
    /// The session may dispatch as soon as it has input.
    #[default]
    Open,
    /// The session stays `created` until `release_start` or gate expiry.
    Held,
}

/// Whether the daemon holds a liveness obligation for the session.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionOwnership {
    /// The daemon drives the session to a declared terminal outcome.
    Owned,
    /// A conversation the daemon does not drive.
    #[default]
    Unmonitored,
}

/// Closed finish condition an owned session owes.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum FinishCondition {
    /// Completion is declared outside the session.
    ExternalGate,
    /// Completion is checked against exact declared text.
    Declared {
        /// Exact statement the finish check evaluates.
        statement: String,
    },
}

/// The lifecycle members of a creation: omission means an open gate, an
/// unmonitored conversation, and no finish condition.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionLifecycleMembers {
    /// Whether the creation holds its start gate.
    #[serde(default)]
    pub start_gate: StartGate,
    /// The ownership the creation establishes.
    #[serde(default)]
    pub ownership: SessionOwnership,
    /// The finish condition an owned session owes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finish_condition: Option<FinishCondition>,
}

impl SessionLifecycleMembers {
    /// Whether every member holds its omission value.
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

/// The standing failure cause a parked session closes with.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionFailureCause {
    ProviderTransient,
    ProviderQuotaExhausted,
    ProviderOverloaded,
    InfrastructureFailure,
    RetryBudgetExhausted,
    ContextCompactionWall,
    ContextHeadroomExhausted,
    BrokenToolchain,
    ModerationBlock,
}

/// Closed session-lifecycle command rejection vocabulary.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionLifecycleCommandRejection {
    SessionNotFound,
    TransitionNotAdmitted,
    RequiresParked,
    ReleaseWhileParked,
    OwnershipUnchanged,
    FinishConditionAlreadyDeclared,
    StandingCauseMismatch,
    SuccessorNotFound,
    SuccessorIsSelf,
    GoalResumeRequired,
    GoalOutcomeMismatch,
    PendingTerminalConflict,
}

/// What an applied lifecycle command did.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum SessionLifecycleEffect {
    /// The held start gate opened.
    StartReleased {},
    /// The session recorded terminal.
    Closed {},
    /// The outcome is committed; the named live turn settles first.
    ClosurePending {
        /// The turn the committed interrupt machinery settles.
        live_turn_id: CanonicalUuid,
    },
    /// The park lifted.
    Resumed {},
    /// The ownership bit flipped.
    OwnershipChanged {},
}

/// Closed versioned request family.

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ClientRequest {
    /// Create a user-initiated session.
    CreateSession {
        /// Durable mutation identity.
        command_id: CommandId,
        /// Initial session model-selection defaults.
        initial_model_selection: ModelSelection,
        /// Initial session-layer settings contribution.
        model_settings: ModelSettingsOverlay,
        /// Optional initial system prompt; required null-or-text member.
        #[serde(default, skip_serializing_if = "SystemPromptMember::is_absent")]
        system_prompt: SystemPromptMember,
        /// Explicit opt-in placement, defaulting to legacy pathless behavior.
        #[serde(default, skip_serializing_if = "SessionPlacement::is_pathless")]
        placement: SessionPlacement,
        /// Start gate, ownership, and finish condition.
        #[serde(default, skip_serializing_if = "SessionLifecycleMembers::is_default")]
        lifecycle: SessionLifecycleMembers,
    },
    /// Create a user-initiated session from one daemon-held template.
    CreateSessionFromTemplate {
        /// Durable mutation identity.
        command_id: CommandId,
        /// Validated static template name.
        template_name: String,
        /// Explicit opt-in placement, defaulting to legacy pathless behavior.
        #[serde(default, skip_serializing_if = "SessionPlacement::is_pathless")]
        placement: SessionPlacement,
        /// Start gate, ownership, and finish condition.
        #[serde(default, skip_serializing_if = "SessionLifecycleMembers::is_default")]
        lifecycle: SessionLifecycleMembers,
    },
    /// Atomically commission one session from a daemon-held template: create
    /// it under a recorded immutable authority fence, attach its goal, and
    /// submit its first input through the start-when-idle path.
    CommissionSession {
        /// Durable mutation identity for the whole composite.
        command_id: CommandId,
        /// Validated static template name.
        template_name: String,
        /// Immutable authority fence recorded for the created session.
        fence: CommissionedSessionFence,
        /// Exact immutable goal statement.
        statement: String,
        /// Exact first-input text carried to the created session.
        content: InputContent,
    },
    /// List available static templates by name and version.
    ListTemplates {},
    /// Read client-relevant deployment policy for this connection.
    ReadDeploymentLimits {},
    /// List current sessions.
    ListSessions {},
    /// Read one coherent operator-status snapshot.
    ReadOperatorStatus {},
    /// Append one explicit immutable session-placement update event.
    UpdateSessionPlacement {
        command_id: CommandId,
        session_id: CanonicalUuid,
        expected_placement_version: CanonicalU64,
        replacement: SessionPlacement,
    },
    /// Attach one immutable commissioned goal statement.
    AttachGoal {
        /// Durable mutation identity.
        command_id: CommandId,
        /// Target session.
        session_id: CanonicalUuid,
        /// Exact immutable statement.
        statement: String,
    },
    /// Read the current goal projection and complete ordered event history.
    ReadGoal {
        /// Target session.
        session_id: CanonicalUuid,
    },
    /// Resume a blocked goal with optional next-turn guidance.
    ResumeGoal {
        /// Durable mutation identity.
        command_id: CommandId,
        /// Target session.
        session_id: CanonicalUuid,
        /// Optional exact next-turn guidance.
        #[serde(deserialize_with = "deserialize_required_nullable")]
        guidance: Option<String>,
    },
    /// Explicitly stop a pursuing or blocked goal.
    StopGoal {
        /// Durable mutation identity.
        command_id: CommandId,
        /// Target session.
        session_id: CanonicalUuid,
        /// Explicit delegated-child scope.
        descendant_scope: DescendantTerminationScope,
    },
    /// Atomically replace the active immutable statement.
    SupersedeGoal {
        /// Durable mutation identity.
        command_id: CommandId,
        /// Target session.
        session_id: CanonicalUuid,
        /// Newly commissioned immutable statement.
        statement: String,
    },
    /// Close a session `stopped{sticky}` from any non-terminal state.
    StopSession {
        command_id: CommandId,
        session_id: CanonicalUuid,
        /// Whether re-dispatch stays suppressed until the source is updated.
        sticky: bool,
        /// Explicit delegated-child scope.
        descendant_scope: DescendantTerminationScope,
    },
    /// Close a session `superseded{by}` in favour of its successor.
    SupersedeSession {
        command_id: CommandId,
        session_id: CanonicalUuid,
        /// The session that takes the work.
        successor_session_id: CanonicalUuid,
    },
    /// Write off a parked session as `abandoned`.
    AbandonSession {
        command_id: CommandId,
        session_id: CanonicalUuid,
    },
    /// Close a parked session as failed; null closes with its standing cause.
    CloseSessionFailed {
        command_id: CommandId,
        session_id: CanonicalUuid,
        #[serde(deserialize_with = "deserialize_required_nullable")]
        cause: Option<SessionFailureCause>,
    },
    /// Return a parked session whose goal is not blocked to its mapped state.
    ResumeSession {
        command_id: CommandId,
        session_id: CanonicalUuid,
    },
    /// Take the liveness obligation, optionally supplying a finish condition.
    AdoptSession {
        command_id: CommandId,
        session_id: CanonicalUuid,
        #[serde(deserialize_with = "deserialize_required_nullable")]
        finish_condition: Option<FinishCondition>,
    },
    /// Drop the liveness obligation.
    ReleaseSession {
        command_id: CommandId,
        session_id: CanonicalUuid,
    },
    /// Open a held start gate so queued admission work may dispatch.
    ReleaseStart {
        command_id: CommandId,
        session_id: CanonicalUuid,
    },
    /// Submit user input with an admitted delivery treatment.
    SubmitInput {
        /// Durable mutation identity.
        command_id: CommandId,
        /// Target session.
        session_id: CanonicalUuid,
        /// Exact ordered user parts.
        content: UserInputContent,
        /// Caller-observed defaults version, or null for configuration-free
        /// steering.
        #[serde(deserialize_with = "deserialize_required_nullable")]
        expected_defaults_version: Option<CanonicalU64>,
        /// Per-call settings contribution; steering must inherit every knob.
        model_settings: ModelSettingsOverlay,
        /// Optional delivery treatment; absence selects the start-when-idle default.
        #[serde(
            default,
            deserialize_with = "deserialize_present_input_delivery",
            skip_serializing_if = "Option::is_none"
        )]
        delivery: Option<InputDelivery>,
    },
    /// Compact one session's model-visible history without rewriting it.
    CompactSession {
        /// Durable mutation identity.
        command_id: CommandId,
        /// Target session.
        session_id: CanonicalUuid,
        /// Optional one-based semantic position to summarize through; null
        /// selects the latest safe boundary.
        #[serde(deserialize_with = "deserialize_required_nullable")]
        through_position: Option<CanonicalU64>,
    },
    /// Read one durable transcript snapshot.
    ReadTranscript {
        /// Target session.
        session_id: CanonicalUuid,
    },
    /// Read a snapshot and follow later durable updates.
    FollowSession {
        /// Target session.
        session_id: CanonicalUuid,
    },
    /// Execute one exact already-issued delegated-session spawn request.
    SpawnSession {
        /// Invoking parent session.
        session_id: CanonicalUuid,
        /// Turn that issued the tool request.
        turn_id: CanonicalUuid,
        /// Exact logical spawn tool request.
        tool_request_id: CanonicalUuid,
        /// Exact bounded child task.
        task: String,
        /// Parent-chosen lifecycle relationship.
        relationship: DelegationPolicy,
    },
    /// Register delivery for one related child.
    AwaitSession {
        /// Invoking parent session.
        session_id: CanonicalUuid,
        /// Turn that issued the tool request.
        turn_id: CanonicalUuid,
        /// Exact logical await tool request.
        tool_request_id: CanonicalUuid,
        /// Related child whose result is awaited.
        child_session_id: CanonicalUuid,
        /// Foreground or background delivery mode.
        mode: DelegationWaitMode,
    },
    /// Send one bounded message across an existing delegation relationship.
    SendSessionMessage {
        /// Invoking session.
        session_id: CanonicalUuid,
        /// Turn that issued the tool request.
        turn_id: CanonicalUuid,
        /// Exact logical message tool request.
        tool_request_id: CanonicalUuid,
        /// Related peer receiving the message.
        peer_session_id: CanonicalUuid,
        /// Exact bounded message content.
        content: String,
    },
    /// Read one filtered bounded metadata-summary page.
    ListSessionMetadata {
        /// Exact tags every result must carry.
        #[serde(deserialize_with = "deserialize_required_metadata_tags")]
        required_tags: Vec<String>,
        /// Optional exact case-sensitive title substring.
        #[serde(deserialize_with = "deserialize_required_nullable")]
        title_contains: Option<String>,
        /// Whether archived sessions participate.
        include_archived: bool,
        /// Inclusive result bound from one through one hundred.
        page_size: CanonicalU64,
        /// Exclusive session-identity cursor.
        #[serde(deserialize_with = "deserialize_required_nullable")]
        after_session_id: Option<CanonicalUuid>,
    },
    /// Read one filtered bounded unified conversation-summary page.
    ListConversations {
        /// Optional exact case-sensitive title substring.
        #[serde(deserialize_with = "deserialize_required_nullable")]
        title_contains: Option<String>,
        /// Which origin classes participate.
        origin: ConversationOriginFilter,
        /// Whether archived native sessions participate.
        include_archived: bool,
        /// Inclusive result bound from one through one hundred.
        page_size: CanonicalU64,
        /// Exclusive unified keyset cursor.
        #[serde(deserialize_with = "deserialize_required_nullable")]
        after: Option<ConversationCursor>,
    },
    /// Read the deployment's complete configured model-alias catalog.
    ListModelAliases {},
    /// Read the deployment's complete per-model capability catalog.
    ListModelCapabilities {},
    /// Read one complete current metadata snapshot.
    ReadSessionMetadata {
        /// Target session.
        session_id: CanonicalUuid,
    },
    /// Durably replace one complete metadata snapshot.
    ReplaceSessionMetadata {
        /// Durable mutation identity.
        command_id: CommandId,
        /// Target session.
        session_id: CanonicalUuid,
        /// Complete replacement object.
        metadata: SessionMetadata,
    },
    /// Replace one session's complete defaults with a new immutable epoch.
    ReplaceSessionDefaults {
        /// Durable mutation identity.
        command_id: CommandId,
        /// Target session.
        session_id: CanonicalUuid,
        /// Exact caller-observed current epoch.
        expected_defaults_version: CanonicalU64,
        /// Complete replacement model selection.
        model_selection: ModelSelection,
        /// Complete replacement session-layer settings contribution.
        model_settings: ModelSettingsOverlay,
        /// Complete replacement dangerous-tool blanket-auto posture.
        dangerous_tool_auto_approval: bool,
        /// Complete replacement system prompt; required null-or-text member.
        #[serde(default, skip_serializing_if = "SystemPromptMember::is_absent")]
        system_prompt: SystemPromptMember,
    },
    /// Read one session's complete current or named immutable defaults epoch.
    ReadSessionDefaults {
        /// Target session.
        session_id: CanonicalUuid,
        /// Exact immutable epoch to read, or null for the current epoch.
        #[serde(deserialize_with = "deserialize_required_nullable")]
        defaults_version: Option<CanonicalU64>,
    },
    /// Import one complete external conversation snapshot.
    ImportConversation {
        /// Explicit format-versioned converter selection.
        format: ConversationImportFormat,
        /// Exact complete source bytes.
        source: ConversationImportSource,
    },
    /// Begin one per-connection chunked conversation import.
    BeginConversationImport {
        /// Explicit format-versioned converter selection.
        format: ConversationImportFormat,
        /// Exact total source size the caller will append.
        declared_size_bytes: CanonicalU64,
    },
    /// Append one source chunk to the connection's in-progress import.
    AppendConversationImport {
        /// Next exact source bytes in physical order.
        chunk: ConversationImportSource,
    },
    /// Convert and store the connection's completely appended source.
    CommitConversationImport {},
    /// Discard the connection's in-progress import without conversion.
    AbortConversationImport {},
    /// Begin one connection-local immutable user-attachment upload.
    BeginBlobUpload {
        /// Exact content identity the caller computed before upload.
        expected_digest: CanonicalBlobDigest,
        /// Exact positive byte length the caller will append.
        expected_length_bytes: CanonicalU64,
    },
    /// Append one bounded chunk to the connection's active blob upload.
    AppendBlobUpload {
        /// Next exact bytes in physical order.
        chunk: BlobChunk,
    },
    /// Verify, publish, and catalogue the active blob upload.
    CommitBlobUpload {},
    /// Discard the connection's active blob upload.
    AbortBlobUpload {},
    /// Read bounded catalog metadata for one immutable blob.
    ReadBlobMetadata { digest: CanonicalBlobDigest },
    /// Read one exact bounded range after full replica verification.
    ReadBlobChunk {
        digest: CanonicalBlobDigest,
        offset_bytes: CanonicalU64,
        length_bytes: CanonicalU64,
    },
    /// Read one immutable imported conversation's complete entry inventory.
    ///
    /// The read exposes the ordinals `create_session_from_imported_frontier`
    /// consumes; it creates nothing and seeds nothing.
    ReadImportedConversation {
        /// Immutable imported conversation to inspect.
        imported_conversation_id: CanonicalUuid,
    },
    /// Create a live session from one inclusive imported entry boundary.
    CreateSessionFromImportedFrontier {
        /// Durable mutation identity.
        command_id: CommandId,
        /// Immutable imported conversation to continue.
        imported_conversation_id: CanonicalUuid,
        /// Inclusive one-based imported entry position.
        through_position: CanonicalU64,
        /// Creation-time resume or fork intent.
        relationship: ImportedSessionRelationship,
        /// Initial session model-selection defaults.
        initial_model_selection: ModelSelection,
        /// Initial session-layer settings contribution.
        model_settings: ModelSettingsOverlay,
    },
    /// Reconcile the exact active turn parked on an ambiguous model call.
    ///
    /// The named turn must be the session's active turn and must be parked in
    /// the model-call recovery wait. The request supplies the user interrupt
    /// authority that turn's terminal disposition requires and carries the
    /// successor input the session continues with.
    ReconcileTurn {
        /// Durable mutation identity.
        command_id: CommandId,
        /// Target session.
        session_id: CanonicalUuid,
        /// The turn the caller observed parked awaiting reconciliation.
        expected_active_turn_id: CanonicalUuid,
        /// Exact ordered user parts for the immediate successor turn.
        content: UserInputContent,
        /// Caller-observed defaults version.
        expected_defaults_version: CanonicalU64,
        /// Per-call settings contribution for the immediate successor origin.
        model_settings: ModelSettingsOverlay,
    },
    /// Register one immutable external review target snapshot.
    CreateReviewTarget {
        command_id: CommandId,
        target_id: CanonicalUuid,
        provider: String,
        repository: String,
        subject: ReviewTargetSubject,
        head_revision: String,
        #[serde(deserialize_with = "deserialize_required_nullable")]
        base_revision: Option<String>,
        #[serde(deserialize_with = "deserialize_required_nullable")]
        stack_parent_target_id: Option<CanonicalUuid>,
    },
    /// Admit one run and its sole session-backed pass.
    StartReviewRun {
        command_id: CommandId,
        target_id: CanonicalUuid,
        run_id: CanonicalUuid,
        pass_id: CanonicalUuid,
        workflow: ReviewWorkflow,
        session_id: CanonicalUuid,
        accepted_input_id: CanonicalUuid,
    },
    /// Atomically bind one queued run and pass to their already-active turn.
    ActivateReviewPass {
        /// Durable mutation identity.
        command_id: CommandId,
        /// Owning run.
        run_id: CanonicalUuid,
        /// Pass to activate.
        pass_id: CanonicalUuid,
        /// Canonical active turn created from the pass's accepted input.
        turn_id: CanonicalUuid,
    },
    /// Conclude one pass that carries no other typed result payload.
    CompleteReviewPass {
        command_id: CommandId,
        run_id: CanonicalUuid,
        pass_id: CanonicalUuid,
        #[serde(deserialize_with = "deserialize_required_nullable")]
        turn_id: Option<CanonicalUuid>,
        #[serde(deserialize_with = "deserialize_required_nullable")]
        output_frontier_id: Option<CanonicalUuid>,
        outcome: ReviewPassTerminalOutcome,
    },
    RecordReviewFindings {
        command_id: CommandId,
        run_id: CanonicalUuid,
        pass_id: CanonicalUuid,
        turn_id: CanonicalUuid,
        output_frontier_id: CanonicalUuid,
        findings: Vec<ReviewFindingInput>,
    },
    /// Atomically conclude a result-bearing pass and append one finding event.
    RecordReviewFindingEvent {
        command_id: CommandId,
        run_id: CanonicalUuid,
        pass_id: CanonicalUuid,
        turn_id: CanonicalUuid,
        #[serde(deserialize_with = "deserialize_required_nullable")]
        output_frontier_id: Option<CanonicalUuid>,
        finding_id: CanonicalUuid,
        /// Exact contiguous event ordinal the appended disposition occupies.
        event_ordinal: CanonicalU64,
        event: ReviewFindingEvent,
    },
    /// Reserve one provider object identity before an external write.
    ReserveReviewExternalLink {
        command_id: CommandId,
        external_link_id: CanonicalUuid,
        finding_id: CanonicalUuid,
        provider: String,
        object_kind: ReviewExternalObjectKind,
    },
    /// Attach a provider identity through an exact publish-pass result.
    AttachReviewExternalLink {
        command_id: CommandId,
        external_link_id: CanonicalUuid,
        run_id: CanonicalUuid,
        pass_id: CanonicalUuid,
        turn_id: CanonicalUuid,
        output_frontier_id: CanonicalUuid,
        external_object: String,
        event_ordinal: CanonicalU64,
    },
    /// Read one immutable target snapshot.
    ReadReviewTarget { target_id: CanonicalUuid },
    /// Read one run and its sole pass projection.
    ReadReviewRun { run_id: CanonicalUuid },
    /// Read one complete finding aggregate projection.
    ReadReviewFinding { finding_id: CanonicalUuid },
    /// List findings produced by one exact run in identity order.
    ListReviewFindings { run_id: CanonicalUuid },
    /// Start one immutable client-driven orchestration attempt.
    StartReviewOrchestration {
        command_id: CommandId,
        attempt_id: CanonicalUuid,
        target_id: CanonicalUuid,
        concern_set_version: String,
        import_template_name: String,
        judgment_template_name: String,
        repair_template_name: String,
        publication_template_name: String,
        concerns: Vec<ReviewOrchestrationConcernInput>,
    },
    /// Seal the import stage outcome.
    RecordReviewImportOutcome {
        command_id: CommandId,
        attempt_id: CanonicalUuid,
        #[serde(deserialize_with = "deserialize_required_nullable")]
        pass_id: Option<CanonicalUuid>,
        #[serde(deserialize_with = "deserialize_required_nullable")]
        external_link_id: Option<CanonicalUuid>,
        #[serde(deserialize_with = "deserialize_required_nullable")]
        context_digest: Option<CanonicalDigest>,
        outcome: ReviewImportTerminalOutcome,
    },
    /// Seal one frozen concern member outcome.
    RecordReviewConcernOutcome {
        command_id: CommandId,
        attempt_id: CanonicalUuid,
        concern: String,
        #[serde(deserialize_with = "deserialize_required_nullable")]
        pass_id: Option<CanonicalUuid>,
        outcome: ReviewConcernTerminalOutcome,
    },
    /// Seal the complete judgment plan over a succeeded fan-out.
    RecordReviewJudgmentPlan {
        command_id: CommandId,
        attempt_id: CanonicalUuid,
        analysis_pass_id: CanonicalUuid,
        members: Vec<ReviewJudgmentPlanMember>,
    },
    /// Seal the result of applying one judgment-plan member.
    RecordReviewJudgmentEffect {
        command_id: CommandId,
        attempt_id: CanonicalUuid,
        finding_id: CanonicalUuid,
        #[serde(deserialize_with = "deserialize_required_nullable")]
        event_pass_id: Option<CanonicalUuid>,
        outcome: ReviewJudgmentEffectTerminalOutcome,
    },
    /// Seal the complete repair-stage member inventory.
    RecordReviewRepairOutcomes {
        command_id: CommandId,
        attempt_id: CanonicalUuid,
        outcomes: Vec<ReviewRepairOutcome>,
    },
    /// Seal the complete publication-stage member inventory.
    RecordReviewPublicationOutcomes {
        command_id: CommandId,
        attempt_id: CanonicalUuid,
        outcomes: Vec<ReviewPublicationOutcome>,
    },
    /// Read one complete orchestration attempt projection.
    ReadReviewOrchestration { attempt_id: CanonicalUuid },
    /// Stop the exact active turn through the accepted interrupt treatment.
    ///
    /// The request applies the `Interrupt` delivery to the named active turn:
    /// its stop is durably requested and terminalization flows through the
    /// existing lifecycle, while `content` becomes the immediate-successor
    /// origin the session continues with. No standalone cancellation command
    /// exists; this verb is the interrupt treatment on the wire.
    StopTurn {
        /// Durable mutation identity.
        command_id: CommandId,
        /// Target session.
        session_id: CanonicalUuid,
        /// The turn the caller observed active in the session.
        expected_active_turn_id: CanonicalUuid,
        /// Exact ordered user parts for the immediate successor turn.
        content: UserInputContent,
        /// Caller-observed defaults version.
        expected_defaults_version: CanonicalU64,
        /// Explicit delegated-child scope.
        descendant_scope: DescendantTerminationScope,
        /// Per-call settings contribution for the immediate successor origin.
        model_settings: ModelSettingsOverlay,
    },
    /// Supply the user decision for one pending tool request.
    DecideToolRequest {
        /// Durable mutation identity.
        command_id: CommandId,
        /// Session the caller expects to own the request.
        session_id: CanonicalUuid,
        /// Exact logical tool request.
        tool_request_id: CanonicalUuid,
        /// Exact closed approval decision.
        decision: ToolDecision,
    },
    /// Record one one-shot user override of a delegate-denied tool request.
    OverrideDeniedToolRequest {
        /// Durable mutation identity.
        command_id: CommandId,
        /// Session the override covers; part of the canonical payload.
        session_id: CanonicalUuid,
        /// Exact delegate-denied logical tool request.
        tool_request_id: CanonicalUuid,
    },
}

/// One closed wire approval decision for a pending tool request.
///
/// The wire surface requires a denial reason; the daemon validates it against
/// the domain's denial-reason contract before command construction.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ToolDecision {
    /// Execution is permitted subject to current aggregate guards.
    Approve {},
    /// Execution is permanently prohibited for this request.
    Deny {
        /// Exact user explanation rendered to the model.
        reason: String,
    },
}

impl ClientRequest {
    fn validate(&self) -> Result<(), FrameValidationError> {
        match self {
            Self::AttachGoal { statement, .. }
            | Self::SupersedeGoal { statement, .. }
            | Self::CommissionSession { statement, .. } => {
                validate_goal_text(statement)?;
            }
            Self::ResumeGoal {
                guidance: Some(guidance),
                ..
            } => validate_goal_text(guidance)?,
            Self::AdoptSession {
                finish_condition: Some(FinishCondition::Declared { statement }),
                ..
            } => validate_goal_text(statement)?,
            Self::CreateSession {
                lifecycle:
                    SessionLifecycleMembers {
                        finish_condition: Some(FinishCondition::Declared { statement }),
                        ..
                    },
                ..
            }
            | Self::CreateSessionFromTemplate {
                lifecycle:
                    SessionLifecycleMembers {
                        finish_condition: Some(FinishCondition::Declared { statement }),
                        ..
                    },
                ..
            } => validate_goal_text(statement)?,
            Self::CreateSession { .. }
            | Self::CreateSessionFromTemplate { .. }
            | Self::ListTemplates {}
            | Self::ReadDeploymentLimits {}
            | Self::ListSessions {}
            | Self::ReadOperatorStatus {}
            | Self::UpdateSessionPlacement { .. }
            | Self::ReadGoal { .. }
            | Self::ResumeGoal { guidance: None, .. }
            | Self::StopGoal { .. }
            | Self::StopSession { .. }
            | Self::SupersedeSession { .. }
            | Self::AbandonSession { .. }
            | Self::CloseSessionFailed { .. }
            | Self::ResumeSession { .. }
            | Self::AdoptSession {
                finish_condition: None,
                ..
            }
            | Self::AdoptSession {
                finish_condition: Some(FinishCondition::ExternalGate),
                ..
            }
            | Self::ReleaseSession { .. }
            | Self::ReleaseStart { .. }
            | Self::SubmitInput { .. }
            | Self::CompactSession { .. }
            | Self::ReadTranscript { .. }
            | Self::FollowSession { .. }
            | Self::SpawnSession { .. }
            | Self::AwaitSession { .. }
            | Self::SendSessionMessage { .. }
            | Self::ListSessionMetadata { .. }
            | Self::ListConversations { .. }
            | Self::ListModelAliases {}
            | Self::ListModelCapabilities {}
            | Self::ReadSessionMetadata { .. }
            | Self::ReplaceSessionMetadata { .. }
            | Self::ReplaceSessionDefaults { .. }
            | Self::ReadSessionDefaults { .. }
            | Self::ImportConversation { .. }
            | Self::BeginConversationImport { .. }
            | Self::AppendConversationImport { .. }
            | Self::CommitConversationImport {}
            | Self::AbortConversationImport {}
            | Self::BeginBlobUpload { .. }
            | Self::AppendBlobUpload { .. }
            | Self::CommitBlobUpload {}
            | Self::AbortBlobUpload {}
            | Self::ReadBlobMetadata { .. }
            | Self::ReadBlobChunk { .. }
            | Self::ReadImportedConversation { .. }
            | Self::CreateSessionFromImportedFrontier { .. }
            | Self::ReconcileTurn { .. }
            | Self::CreateReviewTarget { .. }
            | Self::StartReviewRun { .. }
            | Self::ActivateReviewPass { .. }
            | Self::CompleteReviewPass { .. }
            | Self::RecordReviewFindings { .. }
            | Self::RecordReviewFindingEvent { .. }
            | Self::ReserveReviewExternalLink { .. }
            | Self::AttachReviewExternalLink { .. }
            | Self::ReadReviewTarget { .. }
            | Self::ReadReviewRun { .. }
            | Self::ReadReviewFinding { .. }
            | Self::ListReviewFindings { .. }
            | Self::StartReviewOrchestration { .. }
            | Self::RecordReviewImportOutcome { .. }
            | Self::RecordReviewConcernOutcome { .. }
            | Self::RecordReviewJudgmentPlan { .. }
            | Self::RecordReviewJudgmentEffect { .. }
            | Self::RecordReviewRepairOutcomes { .. }
            | Self::RecordReviewPublicationOutcomes { .. }
            | Self::ReadReviewOrchestration { .. }
            | Self::StopTurn { .. }
            | Self::DecideToolRequest { .. }
            | Self::OverrideDeniedToolRequest { .. } => {}
        }
        match self {
            Self::CreateSession { placement, .. }
            | Self::CreateSessionFromTemplate { placement, .. }
            | Self::UpdateSessionPlacement {
                replacement: placement,
                ..
            } => validate_session_placement_shape(placement)?,
            _ => {}
        }
        if let Self::UpdateSessionPlacement {
            expected_placement_version,
            ..
        } = self
            && expected_placement_version.value() == 0
        {
            return Err(FrameValidationError::PlacementShape);
        }
        if let Self::CommissionSession {
            fence:
                CommissionedSessionFence::PullRequest {
                    pull_request: number,
                    ..
                },
            ..
        } = self
            && number.value() == 0
        {
            return Err(FrameValidationError::DispatchFenceShape);
        }
        if let Self::SubmitInput {
            expected_defaults_version,
            delivery,
            model_settings,
            content,
            ..
        } = self
        {
            content.validate()?;
            let valid = matches!(
                (delivery, expected_defaults_version),
                (None | Some(InputDelivery::StartWhenIdle {}), Some(_))
                    | (Some(InputDelivery::Steer { .. }), None)
                    | (Some(InputDelivery::Queue { .. }), Some(_))
            );
            if !valid {
                return Err(FrameValidationError::InputDeliveryShape);
            }
            if matches!(delivery, Some(InputDelivery::Steer { .. }))
                && *model_settings != ModelSettingsOverlay::inherit_all()
            {
                return Err(FrameValidationError::ModelSettingsShape);
            }
        }
        if let Self::ReconcileTurn { content, .. } | Self::StopTurn { content, .. } = self {
            content.validate()?;
        }
        if let Self::AppendConversationImport { chunk } = self
            && (chunk.as_bytes().is_empty()
                || chunk.as_bytes().len() > MAX_CONVERSATION_IMPORT_CHUNK_BYTES)
        {
            return Err(FrameValidationError::ConversationImportShape);
        }
        if let Self::AppendBlobUpload { chunk } = self
            && (chunk.as_bytes().is_empty() || chunk.as_bytes().len() > MAX_BLOB_CHUNK_BYTES)
        {
            return Err(FrameValidationError::BlobUploadShape);
        }
        if let Self::CreateSessionFromImportedFrontier {
            through_position, ..
        } = self
            && through_position.value() == 0
        {
            return Err(FrameValidationError::ImportedFrontierShape);
        }
        if let Self::CompactSession {
            through_position: Some(position),
            ..
        } = self
            && position.value() == 0
        {
            return Err(FrameValidationError::ContextCompactionShape);
        }
        if let Self::ListSessionMetadata {
            required_tags,
            title_contains,
            ..
        } = self
        {
            let canonical_tags = canonical_metadata_tags(required_tags.clone(), None)
                .map_err(|_| FrameValidationError::MetadataShape)?;
            let mut total_utf8_bytes = 0usize;
            for tag in &canonical_tags {
                add_metadata_utf8_bytes(&mut total_utf8_bytes, tag)
                    .map_err(|_| FrameValidationError::MetadataShape)?;
            }
            if let Some(query) = title_contains {
                validate_nonempty_metadata_text(query)
                    .map_err(|_| FrameValidationError::MetadataShape)?;
                add_metadata_utf8_bytes(&mut total_utf8_bytes, query)
                    .map_err(|_| FrameValidationError::MetadataShape)?;
            }
        }
        if let Self::ListConversations {
            title_contains: Some(query),
            ..
        } = self
        {
            validate_nonempty_metadata_text(query)
                .map_err(|_| FrameValidationError::ConversationListShape)?;
            let mut total_utf8_bytes = 0usize;
            add_metadata_utf8_bytes(&mut total_utf8_bytes, query)
                .map_err(|_| FrameValidationError::ConversationListShape)?;
        }
        if let Self::CreateSessionFromTemplate { template_name, .. } = self {
            validate_session_template_name(template_name)?;
        }
        if let Self::CommissionSession { template_name, .. } = self {
            validate_session_template_name(template_name)?;
        }
        if let Self::CompleteReviewPass {
            turn_id,
            output_frontier_id,
            outcome,
            ..
        } = self
        {
            let valid = matches!(
                (outcome, turn_id, output_frontier_id),
                (ReviewPassTerminalOutcome::Succeeded, Some(_), Some(_))
                    | (
                        ReviewPassTerminalOutcome::Failed | ReviewPassTerminalOutcome::Blocked,
                        Some(_),
                        None
                    )
                    | (ReviewPassTerminalOutcome::Cancelled, _, None)
            );
            if !valid {
                return Err(FrameValidationError::ReviewShape);
            }
        }
        if let Self::RecordReviewFindings { findings, .. } = self
            && findings.len() > MAX_REVIEW_ORCHESTRATION_MEMBERS
        {
            return Err(FrameValidationError::ReviewShape);
        }
        if let Self::RecordReviewFindingEvent {
            finding_id,
            output_frontier_id,
            event,
            ..
        } = self
        {
            validate_review_finding_event(event)?;
            let blocked = matches!(event, ReviewFindingEvent::BlockedWithReason { .. });
            if blocked == output_frontier_id.is_some() {
                return Err(FrameValidationError::ReviewShape);
            }

            let self_reference = match event {
                ReviewFindingEvent::Duplicate {
                    canonical_finding_id,
                } => *canonical_finding_id == *finding_id,
                ReviewFindingEvent::Superseded {
                    successor_finding_id,
                } => *successor_finding_id == *finding_id,
                _ => false,
            };
            if self_reference {
                return Err(FrameValidationError::ReviewShape);
            }
        }
        if let Self::StartReviewOrchestration {
            concern_set_version,
            import_template_name,
            judgment_template_name,
            repair_template_name,
            publication_template_name,
            concerns,
            ..
        } = self
        {
            validate_review_key(concern_set_version)?;
            validate_session_template_name(import_template_name)?;
            validate_session_template_name(judgment_template_name)?;
            validate_session_template_name(repair_template_name)?;
            validate_session_template_name(publication_template_name)?;
            if concerns.is_empty() || concerns.len() > MAX_REVIEW_ORCHESTRATION_MEMBERS {
                return Err(FrameValidationError::ReviewShape);
            }
            let mut keys = HashSet::new();
            let mut templates = HashSet::new();
            for concern in concerns {
                validate_review_key(&concern.key)?;
                validate_session_template_name(&concern.template_name)?;
                if !keys.insert(&concern.key) || !templates.insert(&concern.template_name) {
                    return Err(FrameValidationError::ReviewShape);
                }
            }
        }
        if let Self::RecordReviewImportOutcome {
            pass_id,
            external_link_id,
            context_digest,
            outcome,
            ..
        } = self
        {
            let valid = match outcome {
                ReviewImportTerminalOutcome::Succeeded => {
                    pass_id.is_some() && context_digest.is_some()
                }
                ReviewImportTerminalOutcome::Failed | ReviewImportTerminalOutcome::Blocked => {
                    pass_id.is_some() && external_link_id.is_none() && context_digest.is_none()
                }
                ReviewImportTerminalOutcome::Cancelled => {
                    external_link_id.is_none() && context_digest.is_none()
                }
            };
            if !valid
                || (*outcome != ReviewImportTerminalOutcome::Succeeded
                    && external_link_id.is_some())
            {
                return Err(FrameValidationError::ReviewShape);
            }
        }
        if let Self::RecordReviewConcernOutcome {
            concern,
            pass_id,
            outcome,
            ..
        } = self
        {
            validate_review_key(concern)?;
            if *outcome != ReviewConcernTerminalOutcome::Cancelled && pass_id.is_none() {
                return Err(FrameValidationError::ReviewShape);
            }
        }
        if let Self::RecordReviewJudgmentPlan { members, .. } = self {
            if members.len() > MAX_REVIEW_ORCHESTRATION_MEMBERS {
                return Err(FrameValidationError::ReviewShape);
            }
            let mut findings = HashSet::new();
            for member in members {
                validate_review_judgment_disposition(&member.disposition)?;
                if !findings.insert(member.finding_id) {
                    return Err(FrameValidationError::ReviewShape);
                }
            }
        }
        if let Self::RecordReviewJudgmentEffect {
            event_pass_id,
            outcome,
            ..
        } = self
        {
            let valid = (*outcome == ReviewJudgmentEffectTerminalOutcome::Applied)
                == event_pass_id.is_some();
            if !valid {
                return Err(FrameValidationError::ReviewShape);
            }
        }
        if let Self::RecordReviewRepairOutcomes { outcomes, .. } = self {
            if outcomes.len() > MAX_REVIEW_ORCHESTRATION_MEMBERS {
                return Err(FrameValidationError::ReviewShape);
            }
            let mut findings = HashSet::new();
            for outcome in outcomes {
                let valid = (outcome.outcome == ReviewRepairTerminalOutcome::Fixed)
                    == outcome.event_pass_id.is_some();
                if !valid || !findings.insert(outcome.finding_id) {
                    return Err(FrameValidationError::ReviewShape);
                }
            }
        }
        if let Self::RecordReviewPublicationOutcomes { outcomes, .. } = self {
            if outcomes.len() > MAX_REVIEW_ORCHESTRATION_MEMBERS {
                return Err(FrameValidationError::ReviewShape);
            }
            let mut findings = HashSet::new();
            for outcome in outcomes {
                let valid = (outcome.outcome == ReviewPublicationTerminalOutcome::Published)
                    == outcome.external_link_id.is_some();
                if !valid || !findings.insert(outcome.finding_id) {
                    return Err(FrameValidationError::ReviewShape);
                }
            }
        }
        Ok(())
    }
}

#[derive(signalbox_derive::Accessors)]
/// One validated client frame.

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ClientFrame {
    version: ProtocolVersion,
    request_id: RequestId,
    /// Borrows the closed request.
    #[get]
    request: ClientRequest,
}

impl ClientFrame {
    /// Constructs a single-version frame with a correlated request identity.
    pub fn try_new(
        request_id: RequestId,
        request: ClientRequest,
    ) -> Result<Self, FrameValidationError> {
        Self::try_new_for_version(ProtocolVersion::One, request_id, request)
    }

    /// Constructs a frame in one admitted protocol version.
    pub fn try_new_for_version(
        version: ProtocolVersion,
        request_id: RequestId,
        request: ClientRequest,
    ) -> Result<Self, FrameValidationError> {
        let frame = Self {
            version,
            request_id,
            request,
        };
        frame.validate()?;
        Ok(frame)
    }

    /// Returns the admitted protocol version.
    pub const fn version(&self) -> ProtocolVersion {
        self.version
    }

    /// Returns the correlation identity.
    pub const fn request_id(&self) -> RequestId {
        self.request_id
    }

    /// Transfers the admitted version, correlation identity, and closed
    /// request out of the frame.
    pub fn into_parts(self) -> (ProtocolVersion, RequestId, ClientRequest) {
        (self.version, self.request_id, self.request)
    }

    fn validate(&self) -> Result<(), FrameValidationError> {
        if !self.request_id.is_correlated() {
            return Err(FrameValidationError::UncorrelatedClientRequest);
        }
        if let ClientRequest::CreateSession { system_prompt, .. }
        | ClientRequest::ReplaceSessionDefaults { system_prompt, .. } = &self.request
        {
            validate_system_prompt_member(system_prompt)?;
        }
        self.request.validate()
    }
}

/// Requires the presence-checked system-prompt member.
fn validate_system_prompt_member(member: &SystemPromptMember) -> Result<(), FrameValidationError> {
    if member.is_absent() {
        return Err(FrameValidationError::SystemPromptShape);
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawClientFrame {
    version: ProtocolVersion,
    request_id: RequestId,
    request: ClientRequest,
}

impl<'de> Deserialize<'de> for ClientFrame {
    fn deserialize<DeserializerT>(deserializer: DeserializerT) -> Result<Self, DeserializerT::Error>
    where
        DeserializerT: Deserializer<'de>,
    {
        let raw = RawClientFrame::deserialize(deserializer)?;
        let frame = Self {
            version: raw.version,
            request_id: raw.request_id,
            request: raw.request,
        };
        frame.validate().map_err(serde::de::Error::custom)?;
        Ok(frame)
    }
}

/// Stable server error code.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// JSON, UTF-8, framing, field, or size validation failed.
    MalformedFrame,
    /// Frame version is not admitted by this implementation.
    UnsupportedVersion,
    /// A boundary value cannot construct the application input.
    InvalidRequest,
    /// A read target does not exist.
    NotFound,
    /// Every recorded replica was proven absent.
    BlobMissing,
    /// Every usable recorded replica failed content verification.
    BlobCorrupt,
    /// A durable identity already names different intent.
    ConflictingReuse,
    /// Canonical command handling recorded a typed rejection.
    Rejected,
    /// A follower fell behind bounded fan-out.
    ResyncRequired,
    /// Infrastructure prevented completion.
    Unavailable,
    /// A remote store may have accepted a deterministic publication.
    PublicationAmbiguous,
    /// Infrastructure obscured whether a requested mutation committed.
    CommitAmbiguous,
    /// Fail-closed corruption or a hub defect stopped the request.
    Internal,
}

/// Closed connection-local holder of the process-wide bulk-ingest permit.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BulkIngestKind {
    ConversationImport,
    BlobUpload,
}

impl BulkIngestKind {
    /// Returns the exact lowercase wire token for terminal diagnostics.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ConversationImport => "conversation_import",
            Self::BlobUpload => "blob_upload",
        }
    }
}

/// Typed durable submit rejection details.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum RejectionDetail {
    /// Another chunked bulk-ingest kind already owns this connection.
    BulkIngestAlreadyInProgress { active_kind: BulkIngestKind },
    /// An explicit reasoning value is unsupported by the selected model.
    UnsupportedReasoningLevel {
        selection_id: CanonicalUuid,
        requested: ReasoningLevel,
    },
    /// Enabled fast mode is unsupported by the selected model.
    UnsupportedFastMode { selection_id: CanonicalUuid },
    /// An explicit service tier is unsupported by the selected model.
    UnsupportedServiceTier {
        selection_id: CanonicalUuid,
        requested: ServiceTier,
    },
    /// The target session did not exist at command handling.
    SessionNotFound {
        /// Absent target.
        session_id: CanonicalUuid,
    },
    /// An attachment digest had no catalogued verified replica.
    AttachmentBlobNotFound {
        /// The unavailable immutable byte identity.
        digest: CanonicalBlobDigest,
    },
    /// Distinct attachment bytes exceeded the deployment admission ceiling.
    AttachmentByteBudgetExceeded {
        /// Configured maximum aggregate byte count.
        maximum_bytes: PositiveCanonicalU64,
    },
    /// The placement head advanced beyond the caller-observed version.
    SessionPlacementCurrentVersionMismatch {
        session_id: CanonicalUuid,
        expected_placement_version: CanonicalU64,
        current_placement_version: CanonicalU64,
    },
    /// The positive placement-version space was exhausted.
    SessionPlacementVersionExhausted {
        session_id: CanonicalUuid,
        current_placement_version: CanonicalU64,
    },
    /// A durable goal command was rejected by current goal state.
    GoalCommandRejected {
        /// Target session.
        session_id: CanonicalUuid,
        /// Closed goal-specific reason.
        reason: GoalCommandRejection,
    },
    /// A turn already held the session slot.
    ActiveTurnPresent {
        /// Target session.
        session_id: CanonicalUuid,
        /// Authoritative active turn.
        active_turn_id: CanonicalUuid,
    },
    /// A commissioned target already has a live session.
    CommissionTargetBusy {
        /// Authoritative live session currently owning the target.
        session_id: CanonicalUuid,
    },
    /// The caller named a turn that no longer holds the session slot.
    ActiveTurnMismatch {
        /// Target session.
        session_id: CanonicalUuid,
        /// Turn the caller expected to be active.
        expected_active_turn_id: CanonicalUuid,
        /// Authoritative active turn.
        active_turn_id: CanonicalUuid,
    },
    /// No turn held the session slot when the caller named one.
    NoActiveTurn {
        /// Target session.
        session_id: CanonicalUuid,
        /// Turn the caller expected to be active.
        expected_active_turn_id: CanonicalUuid,
    },
    /// The named turn is not parked on the model-call recovery wait, so no
    /// reconciliation decision is owed for it.
    ///
    /// This precondition is refused before a durable command is recorded; a
    /// caller that races the authoritative state instead receives one of the
    /// recorded rejections above.
    TurnNotAwaitingReconciliation {
        /// Target session.
        session_id: CanonicalUuid,
        /// Turn the caller named.
        turn_id: CanonicalUuid,
    },
    /// A distinct earlier stop was already applied to the active turn.
    InterruptAlreadyApplied {
        /// Target session.
        session_id: CanonicalUuid,
        /// Authoritative active turn.
        active_turn_id: CanonicalUuid,
        /// Command whose applied result already carries the stop proof.
        existing_command_id: CanonicalUuid,
    },
    /// The active turn is parked on a tool-approval wait, which a stop can
    /// neither decide nor bypass; the caller denies the pending request first.
    InterruptUnavailableWhileAwaitingApproval {
        /// Target session.
        session_id: CanonicalUuid,
        /// Authoritative active turn.
        active_turn_id: CanonicalUuid,
    },
    /// A next-safe-point input targeted a turn that is already stopping.
    SafePointUnavailableWhileStopping {
        /// Target session.
        session_id: CanonicalUuid,
        /// Authoritative stopping turn.
        active_turn_id: CanonicalUuid,
        /// Command whose applied result already carries the stop proof.
        existing_command_id: CanonicalUuid,
    },
    /// No logical tool request had the named identity.
    ToolRequestNotFound {
        /// Absent logical tool request.
        tool_request_id: CanonicalUuid,
    },
    /// The named tool request already had a terminal approval resolution.
    ToolRequestAlreadyResolved {
        /// Resolved logical tool request.
        tool_request_id: CanonicalUuid,
    },
    /// An earlier request in the same batch still awaited its decision.
    ToolRequestNotEarliestUndecided {
        /// Named logical tool request.
        tool_request_id: CanonicalUuid,
        /// Earliest undecided request owed a decision first.
        earliest_tool_request_id: CanonicalUuid,
    },
    /// The named tool request is not owned by the named session, so no
    /// decision is admitted for it.
    ///
    /// This precondition is refused before a durable command is recorded; a
    /// correctly correlated request instead reaches the canonical decision
    /// command and its recorded rejections above.
    ToolRequestNotInSession {
        /// Session the caller named.
        session_id: CanonicalUuid,
        /// Tool request the caller named.
        tool_request_id: CanonicalUuid,
    },
    /// The named tool request carries no delegate denial, so no override is
    /// admitted for it.
    ToolRequestNotDelegateDenied {
        /// Tool request without a delegate denial.
        tool_request_id: CanonicalUuid,
    },
    /// The named delegate denial has not reached its terminal denied result.
    ToolRequestNotTerminallyDenied {
        /// Tool request whose denial is still resolving.
        tool_request_id: CanonicalUuid,
    },
    /// An override is already recorded for the named delegate denial.
    ToolDenialAlreadyOverridden {
        /// Already-overridden tool request.
        tool_request_id: CanonicalUuid,
    },
    /// The named delegation request belongs to another turn.
    DelegationRequestNotInTurn {
        /// Session the caller named.
        session_id: CanonicalUuid,
        /// Turn the caller named.
        turn_id: CanonicalUuid,
        /// Delegation request owned by another turn.
        tool_request_id: CanonicalUuid,
    },
    /// A first execution named a request without executable attempt authority.
    DelegationToolRequestNotExecutable {
        /// Logical delegation tool request.
        tool_request_id: CanonicalUuid,
        /// Exact durable state that prevented first execution.
        state: DelegationToolRequestState,
    },
    /// A spawn request replay changed its immutable arguments.
    DelegationSpawnConflict {
        /// Conflicting logical spawn request.
        tool_request_id: CanonicalUuid,
    },
    /// A generated child identity was already occupied.
    DelegatedChildIdentityCollision {
        /// Colliding child identity.
        child_session_id: CanonicalUuid,
    },
    /// No delegation relationship joined the named session and peer.
    DelegationRelationNotFound {
        /// Invoking session.
        session_id: CanonicalUuid,
        /// Named related peer.
        peer_session_id: CanonicalUuid,
    },
    /// An await request replay changed its immutable arguments.
    DelegationAwaitConflict {
        /// Conflicting logical await request.
        tool_request_id: CanonicalUuid,
    },
    /// A message request replay changed its immutable arguments.
    DelegationMessageConflict {
        /// Conflicting logical message request.
        tool_request_id: CanonicalUuid,
    },
    /// A daemon-minted message identity was already claimed.
    DelegationMessageIdentityCollision {
        /// Colliding message identity.
        message_id: CanonicalUuid,
    },
    /// A relationship cannot allocate another positive event ordinal.
    DelegationEventOrdinalExhausted {
        /// Relationship's spawning request identity.
        spawning_request_id: CanonicalUuid,
        /// Last representable event ordinal.
        last: CanonicalU64,
    },
    /// A recipient cannot allocate another positive delivery sequence.
    DelegationDeliverySequenceExhausted {
        /// Recipient whose delivery sequence is exhausted.
        recipient_session_id: CanonicalUuid,
        /// Last representable delivery sequence.
        last: CanonicalU64,
    },
    /// The caller observed stale defaults.
    DefaultsVersionMismatch {
        /// Target session.
        session_id: CanonicalUuid,
        /// Caller version.
        expected: CanonicalU64,
        /// Current authoritative version.
        current: CanonicalU64,
    },
    /// The selected alias had no current definition.
    UnknownModelAlias {
        /// Target session.
        session_id: CanonicalUuid,
        /// Unknown alias.
        alias_id: CanonicalUuid,
    },
    /// The session acceptance ordinal was exhausted.
    AcceptancePositionExhausted {
        /// Target session.
        session_id: CanonicalUuid,
        /// Last representable position.
        last: CanonicalU64,
    },
    /// The session defaults epoch ordinal was exhausted.
    DefaultsVersionExhausted {
        /// Target session.
        session_id: CanonicalUuid,
        /// Last representable epoch.
        current: CanonicalU64,
    },
    /// No imported conversation had the named identity.
    ///
    /// The absent target is an imported conversation, never a session: an
    /// imported conversation is durable record and creates no session.
    ImportedConversationNotFound {
        /// Absent imported conversation.
        imported_conversation_id: CanonicalUuid,
    },
    /// The named imported conversation exists but has no such position.
    ///
    /// Imported positions are the one-based contiguous sequence
    /// `1..=last_position`; the identity was valid and only the ordinal was
    /// outside it.
    ImportedFrontierPositionOutOfRange {
        /// Imported conversation whose positions bound the request.
        imported_conversation_id: CanonicalUuid,
        /// Exact position the caller named.
        requested_position: CanonicalU64,
        /// Greatest selectable position on that conversation.
        last_position: CanonicalU64,
    },
    /// This connection already has one in-progress conversation import.
    ConversationImportAlreadyInProgress {},
    /// This connection has no in-progress conversation import.
    ConversationImportNotInProgress {},
    /// The declared or observed source size exceeds the configured total bound.
    ConversationImportSourceTooLarge {
        /// Configured maximum assembled source size.
        limit_bytes: CanonicalU64,
        /// Exact total source size declared at begin.
        declared_size_bytes: CanonicalU64,
        /// Exact observed size at append or commit, or null at begin.
        #[serde(deserialize_with = "deserialize_required_nullable")]
        actual_size_bytes: Option<CanonicalU64>,
    },
    /// The observed source size did not equal the size declared at begin.
    ConversationImportSourceSizeMismatch {
        /// Exact total source size declared at begin.
        declared_size_bytes: CanonicalU64,
        /// Exact number of source bytes observed across append requests.
        actual_size_bytes: CanonicalU64,
    },
    /// A converter rejected the complete source with content-silent evidence.
    ConversationImportConversionFailed {
        /// Closed converter failure class.
        class: ConversationImportRejectionClass,
        /// One-based offending physical record, or null when not applicable.
        #[serde(deserialize_with = "deserialize_required_nullable")]
        record_ordinal: Option<CanonicalU64>,
    },
    /// This connection already has one in-progress blob upload.
    BlobUploadAlreadyInProgress {},
    /// This connection has no in-progress blob upload.
    BlobUploadNotInProgress {},
    /// The declared blob length fell outside the configured inclusive range.
    BlobUploadLengthOutOfRange {
        min_length_bytes: CanonicalU64,
        max_length_bytes: CanonicalU64,
        declared_length_bytes: CanonicalU64,
    },
    /// Appending the chunk would exceed the length declared at begin.
    BlobUploadSizeExceeded {
        expected_length_bytes: CanonicalU64,
        actual_length_bytes: CanonicalU64,
    },
    /// The appended byte count differed from the length declared at begin.
    BlobUploadLengthMismatch {
        expected_length_bytes: CanonicalU64,
        actual_length_bytes: CanonicalU64,
    },
    /// The assembled bytes differed from the digest declared at begin.
    BlobUploadDigestMismatch {
        expected_digest: CanonicalBlobDigest,
        actual_digest: CanonicalBlobDigest,
    },
    /// The requested direct-read length fell outside the inclusive wire bound.
    BlobReadLengthOutOfRange {
        min_length_bytes: CanonicalU64,
        max_length_bytes: CanonicalU64,
        requested_length_bytes: CanonicalU64,
    },
    /// The requested exact half-open range is not contained by the blob.
    BlobReadRangeOutOfBounds {
        offset_bytes: CanonicalU64,
        length_bytes: CanonicalU64,
        blob_length_bytes: CanonicalU64,
    },
    /// A durable session-lifecycle command was rejected by current state.
    SessionLifecycleCommandRejected {
        /// Target session.
        session_id: CanonicalUuid,
        /// Closed reason.
        reason: SessionLifecycleCommandRejection,
    },
}

impl RejectionDetail {
    const fn is_bulk_ingest(self) -> bool {
        matches!(self, Self::BulkIngestAlreadyInProgress { .. })
    }

    const fn is_blob_upload(self) -> bool {
        matches!(
            self,
            Self::BlobUploadAlreadyInProgress {}
                | Self::BlobUploadNotInProgress {}
                | Self::BlobUploadLengthOutOfRange { .. }
                | Self::BlobUploadSizeExceeded { .. }
                | Self::BlobUploadLengthMismatch { .. }
                | Self::BlobUploadDigestMismatch { .. }
        )
    }

    const fn is_blob_read(self) -> bool {
        matches!(
            self,
            Self::BlobReadLengthOutOfRange { .. } | Self::BlobReadRangeOutOfBounds { .. }
        )
    }

    const fn is_conversation_import(self) -> bool {
        match self {
            Self::ConversationImportAlreadyInProgress {}
            | Self::ConversationImportNotInProgress {}
            | Self::ConversationImportSourceTooLarge { .. }
            | Self::ConversationImportSourceSizeMismatch { .. }
            | Self::ConversationImportConversionFailed { .. } => true,
            Self::BlobUploadAlreadyInProgress {}
            | Self::BlobUploadNotInProgress {}
            | Self::BlobUploadLengthOutOfRange { .. }
            | Self::BlobUploadSizeExceeded { .. }
            | Self::BlobUploadLengthMismatch { .. }
            | Self::BlobUploadDigestMismatch { .. }
            | Self::BlobReadLengthOutOfRange { .. }
            | Self::BlobReadRangeOutOfBounds { .. }
            | Self::BulkIngestAlreadyInProgress { .. }
            | Self::SessionNotFound { .. }
            | Self::AttachmentBlobNotFound { .. }
            | Self::AttachmentByteBudgetExceeded { .. }
            | Self::UnsupportedReasoningLevel { .. }
            | Self::UnsupportedFastMode { .. }
            | Self::UnsupportedServiceTier { .. }
            | Self::SessionPlacementCurrentVersionMismatch { .. }
            | Self::SessionPlacementVersionExhausted { .. }
            | Self::GoalCommandRejected { .. }
            | Self::SessionLifecycleCommandRejected { .. }
            | Self::ActiveTurnPresent { .. }
            | Self::CommissionTargetBusy { .. }
            | Self::ActiveTurnMismatch { .. }
            | Self::NoActiveTurn { .. }
            | Self::TurnNotAwaitingReconciliation { .. }
            | Self::InterruptAlreadyApplied { .. }
            | Self::InterruptUnavailableWhileAwaitingApproval { .. }
            | Self::SafePointUnavailableWhileStopping { .. }
            | Self::ToolRequestNotFound { .. }
            | Self::ToolRequestAlreadyResolved { .. }
            | Self::ToolRequestNotEarliestUndecided { .. }
            | Self::ToolRequestNotInSession { .. }
            | Self::ToolRequestNotDelegateDenied { .. }
            | Self::ToolRequestNotTerminallyDenied { .. }
            | Self::ToolDenialAlreadyOverridden { .. }
            | Self::DelegationRequestNotInTurn { .. }
            | Self::DelegationToolRequestNotExecutable { .. }
            | Self::DelegationSpawnConflict { .. }
            | Self::DelegatedChildIdentityCollision { .. }
            | Self::DelegationRelationNotFound { .. }
            | Self::DelegationAwaitConflict { .. }
            | Self::DelegationMessageConflict { .. }
            | Self::DelegationMessageIdentityCollision { .. }
            | Self::DelegationEventOrdinalExhausted { .. }
            | Self::DelegationDeliverySequenceExhausted { .. }
            | Self::DefaultsVersionMismatch { .. }
            | Self::UnknownModelAlias { .. }
            | Self::AcceptancePositionExhausted { .. }
            | Self::DefaultsVersionExhausted { .. }
            | Self::ImportedConversationNotFound { .. }
            | Self::ImportedFrontierPositionOutOfRange { .. } => false,
        }
    }
}

#[derive(signalbox_derive::Accessors)]
/// Presence-checked rejection detail on an error message.
///
/// An absent value omits the JSON member. A present JSON `null` is rejected
/// rather than being treated as absence.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ErrorDetail(
    /// Returns the typed rejection detail when present.
    #[get(copy, as = "value")]
    Option<RejectionDetail>,
);

impl ErrorDetail {
    /// Omits rejection detail from a non-rejection error.
    pub const fn none() -> Self {
        Self(None)
    }

    /// Includes exact durable-rejection detail.
    pub const fn rejected(detail: RejectionDetail) -> Self {
        Self(Some(detail))
    }

    /// Includes typed import evidence on an invalid request.
    pub const fn invalid_request(detail: RejectionDetail) -> Self {
        Self(Some(detail))
    }

    const fn is_absent(&self) -> bool {
        self.0.is_none()
    }
}

impl Serialize for ErrorDetail {
    fn serialize<SerializerT>(
        &self,
        serializer: SerializerT,
    ) -> Result<SerializerT::Ok, SerializerT::Error>
    where
        SerializerT: Serializer,
    {
        match self.0 {
            Some(detail) => detail.serialize(serializer),
            None => serializer.serialize_unit(),
        }
    }
}

impl<'de> Deserialize<'de> for ErrorDetail {
    fn deserialize<DeserializerT>(deserializer: DeserializerT) -> Result<Self, DeserializerT::Error>
    where
        DeserializerT: Deserializer<'de>,
    {
        RejectionDetail::deserialize(deserializer).map(Self::rejected)
    }
}

/// Durable nonterminal model-call state carried by a transcript snapshot.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum CurrentModelCallState {
    /// Call is prepared but unsent.
    Prepared {},
    /// Call crossed the send boundary.
    InFlight {},
    /// Cancellation was durably requested for the issued call.
    CancellationRequested {},
}

/// Current model call attached to one running turn.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CurrentModelCall {
    model_call_id: CanonicalUuid,
    state: CurrentModelCallState,
}

impl CurrentModelCall {
    /// Constructs one exact current-call projection.
    pub const fn new(model_call_id: CanonicalUuid, state: CurrentModelCallState) -> Self {
        Self {
            model_call_id,
            state,
        }
    }

    /// Returns the current model-call identity.
    pub const fn model_call_id(&self) -> CanonicalUuid {
        self.model_call_id
    }

    /// Returns the exact durable nonterminal state.
    pub const fn state(&self) -> CurrentModelCallState {
        self.state
    }
}

/// Terminal model-call dispositions admitted by a failed transcript turn.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailedModelCallDisposition {
    /// The provider interaction failed with definitive evidence.
    KnownFailed,
    /// The provider call was cancelled without terminalizing the turn as
    /// cancelled.
    Cancelled,
}

/// Closed terminal model-call failure classifications exposed to clients.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailedModelCallCause {
    /// Distinct rendered attachments exceeded the deployment verification bound.
    AttachmentTooLarge,
    /// No recorded replica contained a required rendered attachment.
    AttachmentMissing,
    /// Recorded replicas failed attachment identity verification.
    AttachmentCorrupt,
    /// The provider rejected the request credential.
    CredentialRejected,
    /// The credential lacked permission.
    PermissionDenied,
    /// The provider judged the request invalid.
    InvalidRequest,
    /// The requested model or resource was not found.
    TargetNotFound,
    /// The request exceeded a provider size limit.
    RequestTooLarge,
    /// The provider applied a transient rate limit.
    RateLimited,
    /// The account's available quota was exhausted.
    QuotaExhausted,
    /// The provider reported overload.
    Overloaded,
    /// The provider reported an internal error.
    ProviderInternal,
    /// The adapter did not recognize the definitive provider error.
    Unrecognized,
}

/// Optional terminal call evidence carried by a failed transcript turn.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FailedTerminalModelCall {
    model_call_id: CanonicalUuid,
    disposition: FailedModelCallDisposition,
    #[serde(skip_serializing_if = "Option::is_none")]
    cause: Option<FailedModelCallCause>,
}

impl FailedTerminalModelCall {
    /// Constructs one exact failed-turn terminal-call projection.
    pub const fn new(
        model_call_id: CanonicalUuid,
        disposition: FailedModelCallDisposition,
    ) -> Self {
        Self {
            model_call_id,
            disposition,
            cause: None,
        }
    }

    /// Constructs one known-failed call with its closed failure classification.
    pub const fn known_failed_with_cause(
        model_call_id: CanonicalUuid,
        cause: FailedModelCallCause,
    ) -> Self {
        Self {
            model_call_id,
            disposition: FailedModelCallDisposition::KnownFailed,
            cause: Some(cause),
        }
    }

    /// Returns the terminal model-call identity.
    pub const fn model_call_id(&self) -> CanonicalUuid {
        self.model_call_id
    }

    /// Returns the exact terminal call disposition.
    pub const fn disposition(&self) -> FailedModelCallDisposition {
        self.disposition
    }

    /// Returns the closed failure classification when retained.
    pub const fn cause(&self) -> Option<FailedModelCallCause> {
        self.cause
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFailedTerminalModelCall {
    model_call_id: CanonicalUuid,
    disposition: FailedModelCallDisposition,
    #[serde(
        default,
        deserialize_with = "deserialize_present_failed_model_call_cause"
    )]
    cause: Option<FailedModelCallCause>,
}

// Field default handles omission; invoking this decoder means the member was
// present, so a JSON null must fail instead of collapsing into `None`.
fn deserialize_present_failed_model_call_cause<'de, DeserializerT>(
    deserializer: DeserializerT,
) -> Result<Option<FailedModelCallCause>, DeserializerT::Error>
where
    DeserializerT: Deserializer<'de>,
{
    FailedModelCallCause::deserialize(deserializer).map(Some)
}

impl<'de> Deserialize<'de> for FailedTerminalModelCall {
    fn deserialize<DeserializerT>(deserializer: DeserializerT) -> Result<Self, DeserializerT::Error>
    where
        DeserializerT: Deserializer<'de>,
    {
        let raw = RawFailedTerminalModelCall::deserialize(deserializer)?;
        if raw.cause.is_some() && raw.disposition != FailedModelCallDisposition::KnownFailed {
            return Err(serde::de::Error::custom(
                "failure cause requires a known-failed disposition",
            ));
        }
        Ok(Self {
            model_call_id: raw.model_call_id,
            disposition: raw.disposition,
            cause: raw.cause,
        })
    }
}

/// Authoritative turn state carried by a transcript snapshot.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum TurnState {
    /// Accepted work has not activated.
    Queued {
        /// Accepted input that created the queued turn.
        accepted_input_id: CanonicalUuid,
        /// Exact ordered accepted user parts.
        content: UserInputContent,
    },
    /// Delegated work has not activated.
    QueuedDelegated {
        /// Tool request that spawned the delegated session.
        spawning_request_id: CanonicalUuid,
        /// Parent session that issued the spawn request.
        parent_session_id: CanonicalUuid,
        /// Parent turn that issued the spawn request.
        parent_turn_id: CanonicalUuid,
        /// Exact delegated task text.
        content: InputContent,
    },
    /// Delivered delegation content is queued to wake an idle recipient.
    QueuedDelegationWake {
        /// First recipient-wide delivery sequence included by the wake.
        first_delivery_sequence: CanonicalU64,
        /// Last recipient-wide delivery sequence included by the wake.
        through_delivery_sequence: CanonicalU64,
    },
    /// A parent command logically terminalized delegated work while retained
    /// physical execution evidence remains inert.
    DelegationTerminated {
        /// Tool request that spawned the child.
        spawning_request_id: CanonicalUuid,
        /// Typed stopped or cancelled outcome.
        outcome: DelegationOutcome,
        /// Exact parent terminal reason.
        reason: DelegationReason,
        /// Exact parent-command provenance.
        provenance: DelegationProvenance,
    },
    /// The turn is running its current attempt.
    ActiveRunning {
        /// Current live attempt.
        current_attempt_id: CanonicalUuid,
        /// Current provider call, or null before one is prepared.
        #[serde(deserialize_with = "deserialize_required_nullable")]
        current_model_call: Option<CurrentModelCall>,
    },
    /// The turn is parked on an ambiguous model call.
    ActiveAwaitingModelCallRecovery {
        /// Ended attempt that issued the call.
        ended_attempt_id: CanonicalUuid,
        /// Ambiguous call awaiting recovery.
        recovery_model_call_id: CanonicalUuid,
        /// Durable automatic reconciliation attempts already claimed.
        automatic_reconciliation_attempts: CanonicalU64,
        /// True only when the automatic attempt budget is exhausted.
        operator_action_required: bool,
    },
    /// The turn is parked on a user decision for a tool request.
    ActiveAwaitingToolApproval {
        /// Earliest undecided tool request.
        tool_request_id: CanonicalUuid,
    },
    /// The turn is parked on a foreground delegated-child result.
    ActiveAwaitingChild {
        /// Tool request that issued the await.
        await_request_id: CanonicalUuid,
        /// Spawn request naming the relationship.
        spawning_request_id: CanonicalUuid,
        /// Exact child whose result releases the turn.
        child_session_id: CanonicalUuid,
    },
    /// The turn is parked on an ambiguous tool attempt.
    ActiveAwaitingToolRecovery {
        /// Ended turn attempt that issued the tool effect.
        ended_attempt_id: CanonicalUuid,
        /// Ambiguous tool attempt awaiting recovery.
        recovery_tool_attempt_id: CanonicalUuid,
        /// Durable automatic reconciliation attempts already claimed.
        automatic_reconciliation_attempts: CanonicalU64,
        /// True only when the automatic attempt budget is exhausted.
        operator_action_required: bool,
    },
    /// The turn is parked on replacement of one exact lost runner placement.
    ActiveAwaitingRunnerRecovery {
        /// Runner whose durable loss owns this wait.
        runner_id: CanonicalUuid,
        /// Positive placement revision against which loss was projected.
        placement_revision: PositiveCanonicalU64,
        /// Physical tool attempt interrupted by loss, or null when none exists.
        #[serde(deserialize_with = "deserialize_required_nullable")]
        tool_attempt_id: Option<CanonicalUuid>,
    },
    /// The turn terminalized as failed.
    Failed {
        /// Exact terminal frontier.
        terminal_frontier_id: CanonicalUuid,
        /// Terminal physical attempt, or null for an evidence-free recovery
        /// failure.
        terminal_attempt_id: Option<CanonicalUuid>,
        /// Terminal call evidence, or null when no call existed.
        terminal_model_call: Option<FailedTerminalModelCall>,
    },
    /// The turn terminalized as completed.
    Completed {
        /// Exact terminal frontier.
        terminal_frontier_id: CanonicalUuid,
        /// Authoritative terminal attempt.
        terminal_attempt_id: CanonicalUuid,
        /// Outcome-authoritative call.
        terminal_model_call_id: CanonicalUuid,
    },
    /// The turn terminalized as refused.
    Refused {
        /// Exact terminal frontier.
        terminal_frontier_id: CanonicalUuid,
        /// Authoritative terminal attempt.
        terminal_attempt_id: CanonicalUuid,
        /// Outcome-authoritative call.
        terminal_model_call_id: CanonicalUuid,
    },
    /// The turn terminalized after confirmed cancellation.
    Cancelled {
        /// Exact terminal frontier.
        terminal_frontier_id: CanonicalUuid,
        /// Authoritative terminal attempt.
        terminal_attempt_id: CanonicalUuid,
        /// Terminal call, or null when cancellation preceded preparation.
        terminal_model_call_id: Option<CanonicalUuid>,
    },
    /// The turn terminalized on an ambiguous model call.
    ReconciliationRequired {
        /// Exact terminal frontier.
        terminal_frontier_id: CanonicalUuid,
        /// Authoritative terminal attempt.
        terminal_attempt_id: CanonicalUuid,
        /// Exact ambiguous terminal model call.
        terminal_model_call_id: CanonicalUuid,
    },
    /// The turn terminalized on an ambiguous tool attempt.
    ToolReconciliationRequired {
        /// Exact terminal frontier.
        terminal_frontier_id: CanonicalUuid,
        /// Authoritative terminal turn attempt.
        terminal_attempt_id: CanonicalUuid,
        /// Exact terminal tool attempt.
        terminal_tool_attempt_id: CanonicalUuid,
    },
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum RawTurnState {
    Queued {
        accepted_input_id: CanonicalUuid,
        content: UserInputContent,
    },
    QueuedDelegated {
        spawning_request_id: CanonicalUuid,
        parent_session_id: CanonicalUuid,
        parent_turn_id: CanonicalUuid,
        content: InputContent,
    },
    QueuedDelegationWake {
        first_delivery_sequence: CanonicalU64,
        through_delivery_sequence: CanonicalU64,
    },
    DelegationTerminated {
        spawning_request_id: CanonicalUuid,
        outcome: DelegationOutcome,
        reason: DelegationReason,
        provenance: DelegationProvenance,
    },
    ActiveRunning {
        current_attempt_id: CanonicalUuid,
        #[serde(deserialize_with = "deserialize_required_nullable")]
        current_model_call: Option<CurrentModelCall>,
    },
    ActiveAwaitingModelCallRecovery {
        ended_attempt_id: CanonicalUuid,
        recovery_model_call_id: CanonicalUuid,
        automatic_reconciliation_attempts: CanonicalU64,
        operator_action_required: bool,
    },
    ActiveAwaitingToolApproval {
        tool_request_id: CanonicalUuid,
    },
    ActiveAwaitingChild {
        await_request_id: CanonicalUuid,
        spawning_request_id: CanonicalUuid,
        child_session_id: CanonicalUuid,
    },
    ActiveAwaitingToolRecovery {
        ended_attempt_id: CanonicalUuid,
        recovery_tool_attempt_id: CanonicalUuid,
        automatic_reconciliation_attempts: CanonicalU64,
        operator_action_required: bool,
    },
    ActiveAwaitingRunnerRecovery {
        runner_id: CanonicalUuid,
        placement_revision: CanonicalU64,
        #[serde(deserialize_with = "deserialize_required_nullable")]
        tool_attempt_id: Option<CanonicalUuid>,
    },
    Failed {
        terminal_frontier_id: CanonicalUuid,
        #[serde(deserialize_with = "deserialize_required_nullable")]
        terminal_attempt_id: Option<CanonicalUuid>,
        #[serde(deserialize_with = "deserialize_required_nullable")]
        terminal_model_call: Option<FailedTerminalModelCall>,
    },
    Completed {
        terminal_frontier_id: CanonicalUuid,
        terminal_attempt_id: CanonicalUuid,
        terminal_model_call_id: CanonicalUuid,
    },
    Refused {
        terminal_frontier_id: CanonicalUuid,
        terminal_attempt_id: CanonicalUuid,
        terminal_model_call_id: CanonicalUuid,
    },
    Cancelled {
        terminal_frontier_id: CanonicalUuid,
        terminal_attempt_id: CanonicalUuid,
        #[serde(deserialize_with = "deserialize_required_nullable")]
        terminal_model_call_id: Option<CanonicalUuid>,
    },
    ReconciliationRequired {
        terminal_frontier_id: CanonicalUuid,
        terminal_attempt_id: CanonicalUuid,
        terminal_model_call_id: CanonicalUuid,
    },
    ToolReconciliationRequired {
        terminal_frontier_id: CanonicalUuid,
        terminal_attempt_id: CanonicalUuid,
        terminal_tool_attempt_id: CanonicalUuid,
    },
}

impl<'de> Deserialize<'de> for TurnState {
    fn deserialize<DeserializerT>(deserializer: DeserializerT) -> Result<Self, DeserializerT::Error>
    where
        DeserializerT: Deserializer<'de>,
    {
        let state = match RawTurnState::deserialize(deserializer)? {
            RawTurnState::Queued {
                accepted_input_id,
                content,
            } => {
                content.validate().map_err(serde::de::Error::custom)?;
                Self::Queued {
                    accepted_input_id,
                    content,
                }
            }
            RawTurnState::QueuedDelegated {
                spawning_request_id,
                parent_session_id,
                parent_turn_id,
                content,
            } => Self::QueuedDelegated {
                spawning_request_id,
                parent_session_id,
                parent_turn_id,
                content,
            },
            RawTurnState::QueuedDelegationWake {
                first_delivery_sequence,
                through_delivery_sequence,
            } => {
                if first_delivery_sequence.value() == 0
                    || first_delivery_sequence > through_delivery_sequence
                {
                    return Err(serde::de::Error::custom(
                        "delegation wake requires a positive ordered delivery range",
                    ));
                }
                Self::QueuedDelegationWake {
                    first_delivery_sequence,
                    through_delivery_sequence,
                }
            }
            RawTurnState::DelegationTerminated {
                spawning_request_id,
                outcome,
                reason,
                provenance,
            } => {
                if !delegation_terminal_outcome_reason_is_admissible(outcome, reason)
                    || !parent_delegation_provenance_has_cascade(&provenance)
                {
                    return Err(serde::de::Error::custom(
                        "delegation terminal requires parent cascade authority",
                    ));
                }
                Self::DelegationTerminated {
                    spawning_request_id,
                    outcome,
                    reason,
                    provenance,
                }
            }
            RawTurnState::ActiveRunning {
                current_attempt_id,
                current_model_call,
            } => Self::ActiveRunning {
                current_attempt_id,
                current_model_call,
            },
            RawTurnState::ActiveAwaitingModelCallRecovery {
                ended_attempt_id,
                recovery_model_call_id,
                automatic_reconciliation_attempts,
                operator_action_required,
            } => Self::ActiveAwaitingModelCallRecovery {
                ended_attempt_id,
                recovery_model_call_id,
                automatic_reconciliation_attempts,
                operator_action_required,
            },
            RawTurnState::ActiveAwaitingToolApproval { tool_request_id } => {
                Self::ActiveAwaitingToolApproval { tool_request_id }
            }
            RawTurnState::ActiveAwaitingChild {
                await_request_id,
                spawning_request_id,
                child_session_id,
            } => Self::ActiveAwaitingChild {
                await_request_id,
                spawning_request_id,
                child_session_id,
            },
            RawTurnState::ActiveAwaitingToolRecovery {
                ended_attempt_id,
                recovery_tool_attempt_id,
                automatic_reconciliation_attempts,
                operator_action_required,
            } => Self::ActiveAwaitingToolRecovery {
                ended_attempt_id,
                recovery_tool_attempt_id,
                automatic_reconciliation_attempts,
                operator_action_required,
            },
            RawTurnState::ActiveAwaitingRunnerRecovery {
                runner_id,
                placement_revision,
                tool_attempt_id,
            } => Self::ActiveAwaitingRunnerRecovery {
                runner_id,
                placement_revision: PositiveCanonicalU64::try_new(placement_revision.value())
                    .map_err(|_| {
                        serde::de::Error::custom(
                            "runner recovery requires a positive placement revision",
                        )
                    })?,
                tool_attempt_id,
            },
            RawTurnState::Failed {
                terminal_frontier_id,
                terminal_attempt_id,
                terminal_model_call,
            } => {
                if terminal_model_call.is_some() && terminal_attempt_id.is_none() {
                    return Err(serde::de::Error::custom(
                        "failed terminal call requires a terminal attempt",
                    ));
                }
                Self::Failed {
                    terminal_frontier_id,
                    terminal_attempt_id,
                    terminal_model_call,
                }
            }
            RawTurnState::Completed {
                terminal_frontier_id,
                terminal_attempt_id,
                terminal_model_call_id,
            } => Self::Completed {
                terminal_frontier_id,
                terminal_attempt_id,
                terminal_model_call_id,
            },
            RawTurnState::Refused {
                terminal_frontier_id,
                terminal_attempt_id,
                terminal_model_call_id,
            } => Self::Refused {
                terminal_frontier_id,
                terminal_attempt_id,
                terminal_model_call_id,
            },
            RawTurnState::Cancelled {
                terminal_frontier_id,
                terminal_attempt_id,
                terminal_model_call_id,
            } => Self::Cancelled {
                terminal_frontier_id,
                terminal_attempt_id,
                terminal_model_call_id,
            },
            RawTurnState::ReconciliationRequired {
                terminal_frontier_id,
                terminal_attempt_id,
                terminal_model_call_id,
            } => Self::ReconciliationRequired {
                terminal_frontier_id,
                terminal_attempt_id,
                terminal_model_call_id,
            },
            RawTurnState::ToolReconciliationRequired {
                terminal_frontier_id,
                terminal_attempt_id,
                terminal_tool_attempt_id,
            } => Self::ToolReconciliationRequired {
                terminal_frontier_id,
                terminal_attempt_id,
                terminal_tool_attempt_id,
            },
        };
        Ok(state)
    }
}

impl TurnState {
    fn validate(&self) -> Result<(), FrameValidationError> {
        if let Self::QueuedDelegationWake {
            first_delivery_sequence,
            through_delivery_sequence,
        } = self
            && (first_delivery_sequence.value() == 0
                || first_delivery_sequence > through_delivery_sequence)
        {
            return Err(FrameValidationError::TurnStateShape);
        }
        if let Self::Failed {
            terminal_attempt_id: None,
            terminal_model_call: Some(_),
            ..
        } = self
        {
            return Err(FrameValidationError::TurnStateShape);
        }
        if let Self::DelegationTerminated {
            outcome,
            reason,
            provenance,
            ..
        } = self
            && (!delegation_terminal_outcome_reason_is_admissible(*outcome, *reason)
                || !parent_delegation_provenance_has_cascade(provenance))
        {
            return Err(FrameValidationError::TurnStateShape);
        }
        Ok(())
    }
}

/// Source speaker admitted by an imported transcript entry.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImportedSpeaker {
    /// The source identified the entry as user-authored.
    User,
    /// The source identified the entry as assistant-authored.
    Assistant,
}

/// Exact source attestation for an imported entry's speaker.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ImportedSourceSpeaker {
    /// The source omitted the speaker field.
    NotAttested {},
    /// The source explicitly supplied no speaker.
    AttestedAbsent {},
    /// The source supplied one admitted speaker.
    Attested {
        /// Exact source-supplied speaker.
        speaker: ImportedSpeaker,
    },
}

/// Closed discriminator naming one imported entry's normalized content
/// variant.
///
/// The transcript snapshot reaches the `Text` arm only for absent or
/// unattested text, because attested text takes the separate text-entry
/// message there. An imported-conversation inspection row has no such split
/// and uses `Text` for every `Text` content, carrying attestation in its
/// preview member instead.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImportedContentKind {
    /// One source-defined event.
    SourceEvent,
    /// One source-defined message block.
    SourceMessageBlock,
    /// Imported text content.
    Text,
    /// One imported tool call.
    ToolCall,
    /// One imported tool result.
    ToolResult,
    /// One imported thinking block.
    Thinking,
    /// One imported redacted-thinking block.
    RedactedThinking,
    /// One imported document block.
    Document,
    /// A typed absence for message content.
    MessageContentAbsent,
}

#[derive(signalbox_derive::Accessors)]
/// A leading excerpt of one imported entry's exact attested text.
///
/// The preview is the entry's exact leading Unicode scalar sequence cut at a
/// scalar boundary, never a summary, replacement, or re-encoding. It is a
/// recognition aid for choosing a position; the immutable imported aggregate
/// remains the authority for complete content.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "RawImportedTextPreview")]
pub struct ImportedTextPreview {
    /// Returns the exact emitted leading scalars.
    #[get(str)]
    /// Exact leading scalars within structural wire-text memory.
    preview: String,
    /// Whether exact text remains beyond the emitted scalars.
    truncated: bool,
}

/// The undecoded wire shape of a preview, before its bound and truncation
/// marker are checked.
///
/// Deserializing through this raw shape keeps the checked type unconstructible
/// from an invalid frame, so a direct `ImportedTextPreview` deserialization
/// cannot bypass the validation an embedded one performs.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawImportedTextPreview {
    preview: String,
    truncated: bool,
}

impl TryFrom<RawImportedTextPreview> for ImportedTextPreview {
    type Error = FrameValidationError;

    fn try_from(raw: RawImportedTextPreview) -> Result<Self, Self::Error> {
        let preview = Self {
            preview: raw.preview,
            truncated: raw.truncated,
        };
        preview.validate()?;
        Ok(preview)
    }
}

impl ImportedTextPreview {
    /// Constructs a structurally bounded preview of one exact attested text.
    ///
    /// The cut lands on a Unicode scalar boundary, so the preview is always a
    /// prefix of the source text rather than a truncated encoding.
    pub fn of_exact_text(text: &str) -> Self {
        Self::of_exact_text_with_limit(text, None)
    }

    /// Constructs a preview under the deployment's optional retained-detail policy.
    pub fn of_exact_text_with_limit(text: &str, limit: Option<usize>) -> Self {
        let effective_limit = limit
            .unwrap_or(MAX_CONTENT_FRAGMENT_BYTES)
            .min(MAX_CONTENT_FRAGMENT_BYTES);
        let mut end = effective_limit.min(text.len());
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        Self {
            preview: text[..end].to_owned(),
            truncated: end < text.len(),
        }
    }

    /// Returns whether exact text remains beyond the emitted scalars.
    pub const fn truncated(&self) -> bool {
        self.truncated
    }

    fn validate(&self) -> Result<(), FrameValidationError> {
        if self.preview.len() > MAX_CONTENT_FRAGMENT_BYTES {
            return Err(FrameValidationError::ImportedTextPreviewShape);
        }
        // Every nonempty text yields at least one scalar inside the bound, so
        // an empty preview cannot be the cut prefix of a longer text.
        if self.truncated && self.preview.is_empty() {
            return Err(FrameValidationError::ImportedTextPreviewShape);
        }
        Ok(())
    }
}

/// Non-text semantic transcript entry.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum TranscriptEntry {
    /// Exact delegated task that opened one child session.
    DelegatedTask {
        /// Tool request that spawned the child.
        spawning_request_id: CanonicalUuid,
        /// Parent session that issued the spawn request.
        parent_session_id: CanonicalUuid,
        /// Parent turn that issued the spawn request.
        parent_turn_id: CanonicalUuid,
        /// Exact delegated task text.
        content: String,
    },
    /// Exact bidirectional delegation message delivered to this frontier.
    DelegationMessage {
        /// Relationship identity.
        spawning_request_id: CanonicalUuid,
        /// Immutable message identity.
        message_id: CanonicalUuid,
        /// Sending session.
        sender_session_id: CanonicalUuid,
        /// Receiving session.
        recipient_session_id: CanonicalUuid,
        /// Relationship-local message ordinal.
        ordinal: CanonicalU64,
        /// Recipient-wide delivery sequence.
        delivery_sequence: CanonicalU64,
        /// Exact delivered content.
        content: String,
    },
    /// Exact child result delivered through one registered wait.
    DelegationResult {
        /// Await request receiving this result.
        await_request_id: CanonicalUuid,
        /// Relationship identity.
        spawning_request_id: CanonicalUuid,
        /// Terminal child session.
        child_session_id: CanonicalUuid,
        /// Foreground or background delivery mode.
        mode: DelegationWaitMode,
        /// Recipient-wide position for background delivery only.
        delivery_sequence: Option<CanonicalU64>,
        /// Typed terminal result outcome.
        outcome: DelegationOutcome,
        /// Delivered content for a successful result only.
        content: Option<String>,
        /// Typed lifecycle reason.
        reason: DelegationReason,
        /// Exact child-turn or parent-command proof.
        provenance: DelegationProvenance,
    },
    /// Injected boundary declaring the model identity newly in force.
    ModelIdentityChanged {
        /// Turn whose start first observes the new model identity.
        turn_id: CanonicalUuid,
        /// Immutable defaults epoch bound by the turn.
        defaults_version: CanonicalU64,
        /// Exact direct model identity frozen for the turn.
        selected_model_id: CanonicalUuid,
    },
    /// Provider-side compaction occurred; opaque replay bytes stay internal.
    ProviderCompaction {
        /// Owning turn.
        turn_id: CanonicalUuid,
        /// Producing model call.
        model_call_id: CanonicalUuid,
    },
    /// Assistant proposed one durable tool request.
    AssistantToolUse {
        /// Owning turn.
        turn_id: CanonicalUuid,
        /// Producing model call.
        model_call_id: CanonicalUuid,
        /// Exact logical tool request.
        tool_request_id: CanonicalUuid,
        /// Exact checked tool name.
        tool_name: String,
        /// Exact normalized or scrubbed-undecodable arguments.
        arguments: String,
        /// Explicit decision provenance, absent while pending and for automatic policy.
        #[serde(
            default,
            deserialize_with = "deserialize_optional_non_null",
            skip_serializing_if = "Option::is_none"
        )]
        approval: Option<TranscriptToolApproval>,
    },
    /// One physical tool attempt produced the logical result.
    ToolExecutionResult {
        /// Exact logical tool request.
        tool_request_id: CanonicalUuid,
        /// Exact physical tool attempt.
        tool_attempt_id: CanonicalUuid,
        /// Exact provider-visible result content.
        content: String,
    },
    /// One logical tool request was denied.
    ToolDenied {
        /// Exact denied tool request.
        tool_request_id: CanonicalUuid,
        /// Exact provider-visible denial content.
        content: String,
    },
    /// One logical tool request closed because its turn ended.
    ToolClosed {
        /// Exact closed tool request.
        tool_request_id: CanonicalUuid,
        /// Exact provider-visible terminal-closure content.
        content: String,
    },
    /// Explicit completed-turn marker.
    TurnCompleted {
        /// Completed turn.
        turn_id: CanonicalUuid,
    },
    /// Explicit failed-turn marker.
    TurnFailed {
        /// Failed turn.
        turn_id: CanonicalUuid,
    },
    /// Explicit cancelled-turn marker.
    TurnCancelled {
        /// Cancelled turn.
        turn_id: CanonicalUuid,
    },
    /// Conservative imported entry without rendered text.
    Imported {
        /// Owning imported conversation.
        imported_conversation_id: CanonicalUuid,
        /// Exact imported entry identity.
        imported_entry_id: CanonicalUuid,
        /// Exact source-speaker attestation.
        source_speaker: ImportedSourceSpeaker,
        /// Conservative normalized content kind.
        content_kind: ImportedContentKind,
    },
}

/// Metadata for a text-bearing semantic transcript entry.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum TranscriptTextEntry {
    /// Committed assistant text.
    Assistant {
        /// Owning turn.
        turn_id: CanonicalUuid,
        /// Producing model call.
        model_call_id: CanonicalUuid,
    },
    /// Model-produced summary of one exact earlier semantic range.
    ContextSummary {
        /// Dedicated model call that produced the summary.
        model_call_id: CanonicalUuid,
        /// Source session of the inclusive range's first entry.
        first_source_session_id: CanonicalUuid,
        /// Identity of the inclusive range's first entry.
        first_entry_id: CanonicalUuid,
        /// Source session of the inclusive range's final entry.
        through_source_session_id: CanonicalUuid,
        /// Identity of the inclusive range's final entry.
        through_entry_id: CanonicalUuid,
    },
    /// Imported text whose exact value was source-attested.
    Imported {
        /// Owning imported conversation.
        imported_conversation_id: CanonicalUuid,
        /// Exact imported entry identity.
        imported_entry_id: CanonicalUuid,
        /// Exact source-speaker attestation.
        source_speaker: ImportedSourceSpeaker,
    },
}

/// Durable model-call terminal disposition.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelCallDisposition {
    /// Provider call completed.
    Completed,
    /// Call failed with definitive evidence.
    KnownFailed,
    /// Provider refused.
    Refused,
    /// Call was cancelled.
    Cancelled,
    /// External outcome is ambiguous.
    Ambiguous,
}

/// Durable model-call state carried by a session event.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ModelCallState {
    /// Call is prepared but unsent.
    Prepared {},
    /// Call crossed the send boundary.
    InFlight {},
    /// Cancellation was durably requested for the issued call.
    CancellationRequested {},
    /// Call reached a terminal disposition.
    Terminal {
        /// Exact terminal disposition.
        disposition: ModelCallDisposition,
    },
}

/// Exact durable state of one tool batch presentation boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ToolBatchState {
    /// Assistant tool proposals committed.
    Proposed {
        /// Exact frontier containing the assistant tool-use entries.
        frontier_id: CanonicalUuid,
    },
    /// Proposal-ordered logical results committed.
    ResultsProjected {
        /// Exact frontier containing the result suffix.
        frontier_id: CanonicalUuid,
    },
    /// One ambiguous physical attempt requires user recovery.
    RecoveryRequired {
        /// Exact ambiguous tool attempt.
        tool_attempt_id: CanonicalUuid,
    },
}

/// Sandbox profile selected by one runner placement.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum RunnerSandboxProfile {
    /// Supervised execution with the invoking user's ambient filesystem and network access.
    #[serde(rename = "ambient")]
    Ambient,
    /// Execution restricted to the placement-owned writable root.
    #[serde(rename = "workspace-restricted")]
    WorkspaceRestricted,
}

#[derive(signalbox_derive::Accessors)]
/// Checked runner capability-class name carried by a session projection.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct RunnerCapabilityClass(
    /// Borrows the validated capability-class name.
    #[get(str, as = "as_str")]
    String,
);

impl RunnerCapabilityClass {
    /// Applies the runner domain's portable catalog-name validation.
    pub fn try_new(value: String) -> Result<Self, CanonicalValueError> {
        DomainRunnerCapabilityClass::try_new(value.clone())
            .map(|_| Self(value))
            .map_err(|_| CanonicalValueError::RunnerCatalogName)
    }
}

impl TryFrom<String> for RunnerCapabilityClass {
    type Error = CanonicalValueError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::try_new(value)
    }
}

impl From<RunnerCapabilityClass> for String {
    fn from(value: RunnerCapabilityClass) -> Self {
        value.0
    }
}

#[derive(signalbox_derive::Accessors)]
/// Checked runner credential-profile name carried by a session projection.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct RunnerCredentialProfileName(
    /// Borrows the validated credential-profile name.
    #[get(str, as = "as_str")]
    String,
);

impl RunnerCredentialProfileName {
    /// Applies the runner domain's portable catalog-name validation.
    pub fn try_new(value: String) -> Result<Self, CanonicalValueError> {
        DomainCredentialProfileName::try_new(value.clone())
            .map(|_| Self(value))
            .map_err(|_| CanonicalValueError::RunnerCatalogName)
    }
}

impl TryFrom<String> for RunnerCredentialProfileName {
    type Error = CanonicalValueError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::try_new(value)
    }
}

impl From<RunnerCredentialProfileName> for String {
    fn from(value: RunnerCredentialProfileName) -> Self {
        value.0
    }
}

#[derive(signalbox_derive::Accessors)]
/// Checked runner repository key carried by a session projection.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct RunnerRepositoryKey(
    /// Borrows the validated repository key.
    #[get(str, as = "as_str")]
    String,
);

impl RunnerRepositoryKey {
    /// Applies the runner domain's portable repository-key validation.
    pub fn try_new(value: String) -> Result<Self, CanonicalValueError> {
        DomainWorkspaceRepositoryKey::try_new(value.clone())
            .map(|_| Self(value))
            .map_err(|_| CanonicalValueError::RunnerCatalogName)
    }
}

impl TryFrom<String> for RunnerRepositoryKey {
    type Error = CanonicalValueError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::try_new(value)
    }
}

impl From<RunnerRepositoryKey> for String {
    fn from(value: RunnerRepositoryKey) -> Self {
        value.0
    }
}

/// Complete selector carried by an authoritative runner projection.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum RunnerProjectionSelector {
    /// Selects one exact runner identity.
    Runner { runner_id: CanonicalUuid },
    /// Selects a runner advertising one exact capability class.
    CapabilityClass { name: RunnerCapabilityClass },
}

/// Closed current connection health carried for a pinned runner.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunnerConnectionHealth {
    /// The runner connection is currently healthy.
    Connected,
    /// The connection missed a heartbeat and remains within its recovery window.
    Suspect,
    /// The connection closed through an orderly daemon or runner shutdown.
    Shutdown,
    /// The connection reached a terminal loss transition.
    Lost,
}

/// Closed current state carried by an authoritative runner projection.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunnerProjectionState {
    /// No runner has been pinned yet.
    Unpinned,
    /// The current placement is pinned.
    Pinned,
    /// The exact selected runner was lost before pinning.
    RunnerLostBeforePin,
    /// The pinned runner was lost.
    RunnerLost,
    /// The lost placement was explicitly abandoned.
    RunnerAbandoned,
}

#[derive(signalbox_derive::Accessors)]
/// Authoritative current runner placement projected in a transcript snapshot.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "RawRunnerProjection")]
pub struct RunnerProjection {
    /// Borrows the immutable requested selector.
    #[get]
    /// Immutable selector requested by this placement revision.
    selector: RunnerProjectionSelector,
    /// Current or lost exact runner when the state names one.
    runner_id: Option<CanonicalUuid>,
    /// Positive current placement revision.
    placement_revision: RunnerPlacementRevision,
    /// Explicit sandbox profile selected by the placement.
    sandbox_profile: RunnerSandboxProfile,
    /// Independently nullable requested credential profile.
    credential_profile: Option<RunnerCredentialProfileName>,
    /// Independently nullable requested repository key.
    repository: Option<RunnerRepositoryKey>,
    /// Independently nullable exact requested working directory.
    working_directory: Option<RunnerWorkingDirectory>,
    /// Current connection health, present exactly while the placement is pinned.
    connection_health: Option<RunnerConnectionHealth>,
    /// Exact current placement state.
    state: RunnerProjectionState,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRunnerProjection {
    selector: RunnerProjectionSelector,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    runner_id: Option<CanonicalUuid>,
    placement_revision: RunnerPlacementRevision,
    sandbox_profile: RunnerSandboxProfile,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    credential_profile: Option<RunnerCredentialProfileName>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    repository: Option<RunnerRepositoryKey>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    working_directory: Option<RunnerWorkingDirectory>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    connection_health: Option<RunnerConnectionHealth>,
    state: RunnerProjectionState,
}

impl RunnerProjection {
    /// Constructs one complete internally coherent current placement projection.
    #[expect(
        clippy::too_many_arguments,
        reason = "the constructor names every independent session-composition axis"
    )]
    pub fn try_new(
        selector: RunnerProjectionSelector,
        runner_id: Option<CanonicalUuid>,
        placement_revision: RunnerPlacementRevision,
        sandbox_profile: RunnerSandboxProfile,
        credential_profile: Option<RunnerCredentialProfileName>,
        repository: Option<RunnerRepositoryKey>,
        working_directory: Option<RunnerWorkingDirectory>,
        connection_health: Option<RunnerConnectionHealth>,
        state: RunnerProjectionState,
    ) -> Result<Self, CanonicalValueError> {
        let runner_shape_valid =
            matches!(state, RunnerProjectionState::Unpinned) == runner_id.is_none();
        let selector_valid = match (&selector, runner_id, state) {
            (
                RunnerProjectionSelector::Runner {
                    runner_id: selected,
                },
                Some(current),
                _,
            ) => *selected == current,
            (RunnerProjectionSelector::Runner { .. }, None, RunnerProjectionState::Unpinned)
            | (
                RunnerProjectionSelector::CapabilityClass { .. },
                _,
                RunnerProjectionState::Unpinned
                | RunnerProjectionState::Pinned
                | RunnerProjectionState::RunnerLost
                | RunnerProjectionState::RunnerAbandoned,
            ) => true,
            (RunnerProjectionSelector::Runner { .. }, None, _)
            | (
                RunnerProjectionSelector::CapabilityClass { .. },
                _,
                RunnerProjectionState::RunnerLostBeforePin,
            ) => false,
        };
        let connection_shape_valid =
            matches!(state, RunnerProjectionState::Pinned) == connection_health.is_some();
        if !runner_shape_valid || !selector_valid || !connection_shape_valid {
            return Err(CanonicalValueError::RunnerProjection);
        }
        Ok(Self {
            selector,
            runner_id,
            placement_revision,
            sandbox_profile,
            credential_profile,
            repository,
            working_directory,
            connection_health,
            state,
        })
    }

    /// Returns the current or lost exact runner when the state names one.
    pub const fn runner_id(&self) -> Option<CanonicalUuid> {
        self.runner_id
    }

    /// Returns the positive current placement revision.
    pub const fn placement_revision(&self) -> RunnerPlacementRevision {
        self.placement_revision
    }

    /// Returns the explicitly selected sandbox profile.
    pub const fn sandbox_profile(&self) -> RunnerSandboxProfile {
        self.sandbox_profile
    }

    /// Borrows the independently nullable requested credential profile.
    pub const fn credential_profile(&self) -> Option<&RunnerCredentialProfileName> {
        self.credential_profile.as_ref()
    }

    /// Borrows the independently nullable requested repository key.
    pub const fn repository(&self) -> Option<&RunnerRepositoryKey> {
        self.repository.as_ref()
    }

    /// Borrows the independently nullable exact requested working directory.
    pub const fn working_directory(&self) -> Option<&RunnerWorkingDirectory> {
        self.working_directory.as_ref()
    }

    /// Returns current connection health exactly while the placement is pinned.
    pub const fn connection_health(&self) -> Option<RunnerConnectionHealth> {
        self.connection_health
    }

    /// Returns the exact current placement state.
    pub const fn state(&self) -> RunnerProjectionState {
        self.state
    }
}

impl TryFrom<RawRunnerProjection> for RunnerProjection {
    type Error = CanonicalValueError;

    fn try_from(raw: RawRunnerProjection) -> Result<Self, Self::Error> {
        Self::try_new(
            raw.selector,
            raw.runner_id,
            raw.placement_revision,
            raw.sandbox_profile,
            raw.credential_profile,
            raw.repository,
            raw.working_directory,
            raw.connection_health,
            raw.state,
        )
    }
}

#[derive(signalbox_derive::Accessors)]
/// Exact bounded runner working-directory text carried on the process wire.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct RunnerWorkingDirectory(
    /// Borrows the exact validated directory text.
    #[get(str, as = "as_str")]
    String,
);

impl RunnerWorkingDirectory {
    /// Maximum UTF-8 bytes admitted by the runner domain and process wire.
    pub const MAX_UTF8_BYTES: usize = DomainRunnerWorkingDirectory::MAX_BYTES;

    /// Admits nonempty, NUL-free text within the exact byte bound.
    pub fn try_new(value: String) -> Result<Self, CanonicalValueError> {
        DomainRunnerWorkingDirectory::try_new(value.clone())
            .map_err(|_| CanonicalValueError::RunnerWorkingDirectory)?;
        Ok(Self(value))
    }
}

impl TryFrom<String> for RunnerWorkingDirectory {
    type Error = CanonicalValueError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::try_new(value)
    }
}

impl From<RunnerWorkingDirectory> for String {
    fn from(value: RunnerWorkingDirectory) -> Self {
        value.0
    }
}

/// Positive runner placement revision carried by follower-visible wire facts.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct RunnerPlacementRevision(CanonicalU64);

impl RunnerPlacementRevision {
    /// Admits one positive placement revision.
    pub const fn try_new(value: u64) -> Option<Self> {
        if value == 0 {
            None
        } else {
            Some(Self(CanonicalU64::new(value)))
        }
    }

    /// Returns the positive integer carried by this placement revision.
    pub const fn value(self) -> u64 {
        self.0.value()
    }
}

impl<'de> Deserialize<'de> for RunnerPlacementRevision {
    fn deserialize<DeserializerT>(deserializer: DeserializerT) -> Result<Self, DeserializerT::Error>
    where
        DeserializerT: Deserializer<'de>,
    {
        let value = CanonicalU64::deserialize(deserializer)?;
        Self::try_new(value.value())
            .ok_or_else(|| serde::de::Error::custom("runner placement revision must be positive"))
    }
}

/// Closed runner state carried by one session update.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunnerStateTransitionState {
    /// Initial dispatch pinned the selected runner.
    Pinned,
    /// The current runner connection missed its first heartbeat.
    Suspect,
    /// A heartbeat acknowledgement recovered that same suspect connection.
    Connected,
    /// An exact runner selection was lost before initial pinning.
    RunnerLostBeforePin,
    /// A pinned runner became unavailable.
    RunnerLost,
    /// A checked successor runner replaced the prior placement.
    Replaced,
    /// Checked recovery retained the runner but changed the selected directory.
    WorkingDirectoryChanged,
    /// The user abandoned a lost runner placement.
    Abandoned,
}

/// Action chosen for one bound child when its parent reaches a terminal state.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BoundChildAction {
    /// Leave the child running.
    KeepRunning,
    /// Stop the child with typed parent-policy provenance.
    Stop,
    /// Cancel the child with typed parent-policy provenance.
    Cancel,
}

/// Parent-chosen lifecycle policy carried by a child-spawned update.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum DelegationPolicy {
    /// The child keeps working independently of parent state.
    Background {},
    /// The child follows the two explicit parent-state actions.
    Bound {
        /// Action when the parent stops.
        on_parent_stopped: BoundChildAction,
        /// Action when the parent is cancelled.
        on_parent_cancelled: BoundChildAction,
    },
}

/// Delivery behavior chosen by one await request.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DelegationWaitMode {
    /// Keep the current parent turn open until delivery.
    Foreground,
    /// Return registration and deliver through a later wake.
    Background,
}

/// Direction of one message within its parent-child relationship.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DelegationMessageDirection {
    /// The relationship parent sent to its child.
    ParentToChild,
    /// The relationship child sent to its parent.
    ChildToParent,
}

/// Durable non-executable state of one delegation tool request.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DelegationToolRequestState {
    /// The request still requires an approval decision.
    AwaitingApproval,
    /// Approval was denied.
    Denied,
    /// Approval succeeded, but proposal-ordered execution has not prepared an attempt.
    Approved,
    /// A physical attempt exists but has not been authorized for execution.
    Prepared,
    /// The logical request already closed without executable work.
    Closed,
    /// Its current physical attempt already ended.
    AttemptEnded,
}

impl DelegationToolRequestState {
    /// Returns the stable wire spelling used by diagnostics.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AwaitingApproval => "awaiting_approval",
            Self::Denied => "denied",
            Self::Approved => "approved",
            Self::Prepared => "prepared",
            Self::Closed => "closed",
            Self::AttemptEnded => "attempt_ended",
        }
    }
}

/// Closed relationship outcome carried by delegation updates.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DelegationOutcome {
    /// Child content is available.
    Returned,
    /// Child execution failed or returned unusable content.
    Failed,
    /// Parent policy stopped the child.
    Stopped,
    /// Child or parent policy cancelled the child.
    Cancelled,
    /// Relationship policy left the child running.
    ContinueRunning,
    /// Parent policy reached an already-terminal child.
    AlreadyTerminal,
}

/// Exact reason carried alongside a delegation outcome.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DelegationReason {
    /// Child completed with delivered content.
    ChildCompleted,
    /// Child execution failed.
    ChildExecutionFailed,
    /// Completed child content could not form a result.
    ChildResultUnavailable,
    /// Child cancelled independently.
    ChildCancelled,
    /// A parent stop selected descendants.
    ParentStopped,
    /// A parent cancellation selected descendants.
    ParentCancelled,
}

/// Proof source retained by one lifecycle or result update.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum DelegationProvenance {
    /// Exact terminal child turn.
    ChildTurn {
        /// Child session.
        child_session_id: CanonicalUuid,
        /// Terminal delegated turn.
        child_turn_id: CanonicalUuid,
    },
    /// Exact parent turn command.
    ParentTurnCommand {
        /// Parent session.
        parent_session_id: CanonicalUuid,
        /// Parent turn named by the command.
        parent_turn_id: CanonicalUuid,
        /// Durable stop or interrupt command.
        command_id: CanonicalUuid,
        /// Explicit kill-time descendant choice.
        descendant_scope: DescendantTerminationScope,
    },
    /// Exact parent goal-generation command.
    ParentGoalCommand {
        /// Parent session.
        parent_session_id: CanonicalUuid,
        /// One-based goal generation.
        goal_generation: CanonicalU64,
        /// Durable goal stop command.
        command_id: CanonicalUuid,
        /// Explicit kill-time descendant choice.
        descendant_scope: DescendantTerminationScope,
    },
    /// Exact parent lifecycle command.
    ParentLifecycleCommand {
        /// Parent session.
        parent_session_id: CanonicalUuid,
        /// Durable lifecycle stop command.
        command_id: CanonicalUuid,
        /// Explicit kill-time descendant choice.
        descendant_scope: DescendantTerminationScope,
    },
}

/// Exact decision recorded for one explicit tool approval.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ToolApprovalEventDecision {
    /// Execution is permitted subject to current aggregate guards.
    Approve {},
    /// Execution is permanently prohibited for this request.
    Deny {
        /// Exact user explanation, absent for a delegate denial.
        #[serde(deserialize_with = "deserialize_required_nullable")]
        reason: Option<String>,
    },
}

/// Exact actor provenance for one explicit tool approval decision.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ToolApprovalEventDecider {
    /// The user acted through the named durable command.
    User {
        /// Exact durable command provenance.
        command_id: CanonicalUuid,
    },
    /// A configured model acted through the named dedicated judge call.
    Delegate {
        /// Exact direct model selection used by the judge.
        model_selection_id: CanonicalUuid,
        /// Exact recorded judge model call.
        model_call_id: CanonicalUuid,
    },
    /// The user pre-approved the re-proposed command by overriding one exact
    /// delegate denial through the named durable command.
    UserOverride {
        /// Exact durable override-command provenance.
        command_id: CanonicalUuid,
        /// The delegate-denied request whose recorded override was consumed.
        overridden_tool_request_id: CanonicalUuid,
    },
}

/// One explicit approval decision retained in an authoritative transcript.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TranscriptToolApproval {
    /// Exact recorded decision.
    pub decision: ToolApprovalEventDecision,
    /// Exact user or delegate provenance.
    pub decider: ToolApprovalEventDecider,
    /// Exact judge rationale, absent for a user decision.
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub rationale: Option<String>,
}

/// Closed durable update event family.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum SessionEvent {
    /// Session creation committed.
    SessionCreated {},
    /// One defaults replacement changed model selection or settings.
    SessionModelSettingsChanged {
        command_id: CommandId,
        prior_defaults_version: CanonicalU64,
        installed_defaults_version: CanonicalU64,
        prior_model: ModelSelection,
        installed_model: ModelSelection,
        prior_settings: ModelSettingsSnapshot,
        installed_settings: ModelSettingsSnapshot,
        caller_override: ModelSettingsOverlay,
        adjustments: Vec<ModelChangeAdjustment>,
    },
    /// One accepted origin turn froze complete model settings.
    TurnModelSettingsResolved {
        accepted_input_id: CanonicalUuid,
        turn_id: CanonicalUuid,
        defaults_version: CanonicalU64,
        requested_model: ModelSelection,
        selected_direct_id: CanonicalUuid,
        per_call_override: ModelSettingsOverlay,
        settings: ModelSettingsSnapshot,
        adjusted_from_selection_id: Option<CanonicalUuid>,
        adjustments: Vec<ModelChangeAdjustment>,
    },
    /// User input acceptance and its queued turn committed.
    InputAccepted {
        /// Accepted input.
        accepted_input_id: CanonicalUuid,
        /// Queued origin turn.
        turn_id: CanonicalUuid,
        /// Immutable session acceptance position.
        acceptance_position: CanonicalU64,
        /// Exact ordered accepted user parts.
        content: UserInputContent,
    },
    /// A queued goal turn became intentionally ineligible.
    GoalTurnRetired {
        /// Exact immutable queued turn retired by a goal transition.
        turn_id: CanonicalUuid,
    },
    /// A queued turn became active.
    TurnActivated {
        /// Activated turn.
        turn_id: CanonicalUuid,
        /// Initial current attempt.
        current_attempt_id: CanonicalUuid,
    },
    /// Model call advanced.
    ModelCallTransition {
        /// Owning turn.
        turn_id: CanonicalUuid,
        /// Advancing call.
        model_call_id: CanonicalUuid,
        /// Exact committed state.
        state: ModelCallState,
    },
    /// A tool batch crossed one durable presentation boundary.
    ToolBatchTransition {
        /// Owning turn.
        turn_id: CanonicalUuid,
        /// Model call that proposed the batch.
        model_call_id: CanonicalUuid,
        /// Exact committed batch state.
        state: ToolBatchState,
    },
    /// A runner placement or its exact connection changed follower-visible state.
    RunnerStateTransition {
        /// Exact runner named by the transition.
        runner_id: CanonicalUuid,
        /// Positive placement revision whose immutable facts are projected.
        placement_revision: RunnerPlacementRevision,
        /// Placement-selected sandbox profile.
        sandbox_profile: RunnerSandboxProfile,
        /// Caller-selected directory, null when the runner default was selected.
        #[serde(deserialize_with = "deserialize_required_nullable")]
        working_directory: Option<RunnerWorkingDirectory>,
        /// Exact closed transition state.
        state: RunnerStateTransitionState,
    },
    /// One explicit tool approval decision committed with full provenance.
    ToolApprovalDecided {
        /// Owning turn.
        turn_id: CanonicalUuid,
        /// Exact logical tool request.
        tool_request_id: CanonicalUuid,
        /// Exact recorded decision.
        decision: ToolApprovalEventDecision,
        /// Exact user or delegate decider.
        decider: ToolApprovalEventDecider,
        /// Exact judge rationale, absent for a user decision.
        #[serde(deserialize_with = "deserialize_required_nullable")]
        rationale: Option<String>,
    },
    /// One append-only context compaction committed.
    ContextCompacted {
        /// Exact compaction provenance record.
        context_compaction_id: CanonicalUuid,
        /// Dedicated producing model call.
        model_call_id: CanonicalUuid,
        /// One-based final summarized position.
        through_position: CanonicalU64,
        /// Appended semantic summary entry.
        summary_entry_id: CanonicalUuid,
        /// Complete result frontier.
        result_frontier_id: CanonicalUuid,
    },
    /// Turn completed.
    TurnCompleted {
        /// Completed turn.
        turn_id: CanonicalUuid,
        /// Outcome-authoritative call.
        model_call_id: CanonicalUuid,
        /// Final completion marker.
        completion_entry_id: CanonicalUuid,
        /// Exact terminal frontier.
        terminal_frontier_id: CanonicalUuid,
    },
    /// Turn failed.
    TurnFailed {
        /// Failed turn.
        turn_id: CanonicalUuid,
        /// Failure marker.
        failure_entry_id: CanonicalUuid,
        /// Exact terminal frontier.
        terminal_frontier_id: CanonicalUuid,
    },
    /// Turn was refused.
    TurnRefused {
        /// Refused turn.
        turn_id: CanonicalUuid,
        /// Outcome-authoritative call.
        model_call_id: CanonicalUuid,
        /// Exact terminal frontier.
        terminal_frontier_id: CanonicalUuid,
    },
    /// Turn was cancelled.
    TurnCancelled {
        /// Cancelled turn.
        turn_id: CanonicalUuid,
        /// Semantic cancellation marker.
        cancellation_entry_id: CanonicalUuid,
        /// Exact terminal frontier.
        terminal_frontier_id: CanonicalUuid,
    },
    /// Turn stopped with an ambiguous model call requiring reconciliation.
    TurnReconciliationRequired {
        /// Reconciliation-required turn.
        turn_id: CanonicalUuid,
        /// Exact ambiguous terminal model call.
        model_call_id: CanonicalUuid,
        /// Exact terminal frontier.
        terminal_frontier_id: CanonicalUuid,
    },
    /// Turn stopped with an ambiguous tool attempt requiring reconciliation.
    TurnToolReconciliationRequired {
        /// Reconciliation-required turn.
        turn_id: CanonicalUuid,
        /// Exact ambiguous terminal tool attempt.
        tool_attempt_id: CanonicalUuid,
        /// Exact terminal frontier.
        terminal_frontier_id: CanonicalUuid,
    },
    /// A parent committed one child relationship and lifecycle policy.
    ChildSpawned {
        /// Exact spawning tool request and relationship identity.
        spawning_request_id: CanonicalUuid,
        /// Spawned child session.
        child_session_id: CanonicalUuid,
        /// Parent-chosen relationship lifecycle policy.
        relationship: DelegationPolicy,
    },
    /// A parent registered one foreground or background wait.
    ChildWaiting {
        /// Exact await tool request.
        await_request_id: CanonicalUuid,
        /// Relationship identity.
        spawning_request_id: CanonicalUuid,
        /// Child being awaited.
        child_session_id: CanonicalUuid,
        /// Wait delivery mode.
        mode: DelegationWaitMode,
    },
    /// One bidirectional relationship message became durable for its recipient.
    SessionMessage {
        /// Relationship identity.
        spawning_request_id: CanonicalUuid,
        /// Message identity.
        message_id: CanonicalUuid,
        /// Sending session.
        sender_session_id: CanonicalUuid,
        /// Receiving session.
        recipient_session_id: CanonicalUuid,
        /// Relationship-local message ordinal.
        ordinal: CanonicalU64,
        /// Recipient-wide delivery sequence.
        delivery_sequence: CanonicalU64,
        /// Exact delivered content.
        content: String,
    },
    /// A terminal child result became durable for its parent.
    ChildResult {
        /// Relationship identity.
        spawning_request_id: CanonicalUuid,
        /// Terminal child.
        child_session_id: CanonicalUuid,
        /// Typed terminal result outcome.
        outcome: DelegationOutcome,
        /// Delivered content for a successful result only.
        content: Option<String>,
        /// Typed reason for the terminal result.
        reason: DelegationReason,
        /// Exact child-turn or parent-command provenance.
        provenance: DelegationProvenance,
    },
    /// Parent termination evaluated one relationship edge.
    ChildLifecycleDisposition {
        /// Relationship identity.
        spawning_request_id: CanonicalUuid,
        /// Evaluated child.
        child_session_id: CanonicalUuid,
        /// Typed relationship outcome.
        outcome: DelegationOutcome,
        /// Typed reason for evaluating this relationship edge.
        reason: DelegationReason,
        /// Exact parent command provenance.
        provenance: DelegationProvenance,
    },
}

fn validate_delegation_session_event(
    session_id: CanonicalUuid,
    event: &SessionEvent,
) -> Result<(), FrameValidationError> {
    let valid = match event {
        SessionEvent::ChildSpawned {
            child_session_id, ..
        }
        | SessionEvent::ChildWaiting {
            child_session_id, ..
        } => *child_session_id != session_id,
        SessionEvent::SessionMessage {
            sender_session_id,
            recipient_session_id,
            ordinal,
            delivery_sequence,
            content,
            ..
        } => {
            *recipient_session_id == session_id
                && sender_session_id != recipient_session_id
                && ordinal.value() > 0
                && delivery_sequence.value() > 0
                && delegation_content_is_valid(content)
        }
        SessionEvent::ChildResult {
            child_session_id,
            outcome,
            content,
            reason,
            provenance,
            ..
        } => {
            *child_session_id != session_id
                && child_result_shape_is_valid(
                    session_id,
                    *child_session_id,
                    *outcome,
                    content,
                    *reason,
                    provenance,
                )
        }
        SessionEvent::ChildLifecycleDisposition {
            child_session_id,
            outcome,
            reason,
            provenance,
            ..
        } => {
            matches!(
                reason,
                DelegationReason::ParentStopped | DelegationReason::ParentCancelled
            ) && if *child_session_id == session_id {
                // A descendant cascade also addresses the terminalization to
                // the child itself so that live child followers observe it.
                // That row carries the parent's cascade provenance, so the
                // provenance parent is a different session than this header.
                matches!(
                    outcome,
                    DelegationOutcome::Stopped | DelegationOutcome::Cancelled
                ) && delegation_provenance_parent(provenance)
                    .is_some_and(|parent| parent != session_id)
                    && parent_delegation_provenance_has_cascade(provenance)
            } else {
                matches!(
                    outcome,
                    DelegationOutcome::Stopped
                        | DelegationOutcome::Cancelled
                        | DelegationOutcome::AlreadyTerminal
                        | DelegationOutcome::ContinueRunning
                ) && parent_delegation_provenance_is_cascade(session_id, provenance)
            }
        }
        _ => true,
    };
    if valid {
        Ok(())
    } else {
        Err(FrameValidationError::DelegationShape)
    }
}

fn child_result_shape_is_valid(
    parent_session_id: CanonicalUuid,
    child_session_id: CanonicalUuid,
    outcome: DelegationOutcome,
    content: &Option<String>,
    reason: DelegationReason,
    provenance: &DelegationProvenance,
) -> bool {
    match (outcome, reason, provenance, content) {
        (
            DelegationOutcome::Returned,
            DelegationReason::ChildCompleted,
            DelegationProvenance::ChildTurn {
                child_session_id: provenance_child,
                ..
            },
            Some(content),
        ) => *provenance_child == child_session_id && delegation_content_is_valid(content),
        (
            DelegationOutcome::Failed,
            DelegationReason::ChildExecutionFailed | DelegationReason::ChildResultUnavailable,
            DelegationProvenance::ChildTurn {
                child_session_id: provenance_child,
                ..
            },
            None,
        )
        | (
            DelegationOutcome::Cancelled,
            DelegationReason::ChildCancelled,
            DelegationProvenance::ChildTurn {
                child_session_id: provenance_child,
                ..
            },
            None,
        ) => *provenance_child == child_session_id,
        (
            DelegationOutcome::Stopped | DelegationOutcome::Cancelled,
            DelegationReason::ParentStopped | DelegationReason::ParentCancelled,
            provenance,
            None,
        ) => parent_delegation_provenance_is_cascade(parent_session_id, provenance),
        _ => false,
    }
}

fn direct_child_result_shape_is_valid(
    child_session_id: CanonicalUuid,
    outcome: DelegationOutcome,
    content: &Option<String>,
    reason: DelegationReason,
    provenance: &DelegationProvenance,
) -> bool {
    match provenance {
        DelegationProvenance::ChildTurn { .. } => child_result_shape_is_valid(
            child_session_id,
            child_session_id,
            outcome,
            content,
            reason,
            provenance,
        ),
        DelegationProvenance::ParentTurnCommand {
            parent_session_id, ..
        }
        | DelegationProvenance::ParentGoalCommand {
            parent_session_id, ..
        }
        | DelegationProvenance::ParentLifecycleCommand {
            parent_session_id, ..
        } => {
            *parent_session_id != child_session_id
                && child_result_shape_is_valid(
                    *parent_session_id,
                    child_session_id,
                    outcome,
                    content,
                    reason,
                    provenance,
                )
        }
    }
}

fn parent_delegation_provenance_is_cascade(
    parent_session_id: CanonicalUuid,
    provenance: &DelegationProvenance,
) -> bool {
    match provenance {
        DelegationProvenance::ParentTurnCommand {
            parent_session_id: provenance_parent,
            descendant_scope: DescendantTerminationScope::ParentAndDescendants,
            ..
        } => {
            *provenance_parent == parent_session_id
                && parent_delegation_provenance_has_cascade(provenance)
        }
        DelegationProvenance::ParentGoalCommand {
            parent_session_id: provenance_parent,
            goal_generation,
            descendant_scope: DescendantTerminationScope::ParentAndDescendants,
            ..
        } => {
            *provenance_parent == parent_session_id
                && goal_generation.value() > 0
                && parent_delegation_provenance_has_cascade(provenance)
        }
        DelegationProvenance::ParentLifecycleCommand {
            parent_session_id: provenance_parent,
            descendant_scope: DescendantTerminationScope::ParentAndDescendants,
            ..
        } => {
            *provenance_parent == parent_session_id
                && parent_delegation_provenance_has_cascade(provenance)
        }
        _ => false,
    }
}

/// Reads the commanding parent session out of a cascade provenance.
fn delegation_provenance_parent(provenance: &DelegationProvenance) -> Option<CanonicalUuid> {
    match provenance {
        DelegationProvenance::ParentTurnCommand {
            parent_session_id, ..
        }
        | DelegationProvenance::ParentGoalCommand {
            parent_session_id, ..
        }
        | DelegationProvenance::ParentLifecycleCommand {
            parent_session_id, ..
        } => Some(*parent_session_id),
        _ => None,
    }
}

fn parent_delegation_provenance_has_cascade(provenance: &DelegationProvenance) -> bool {
    match provenance {
        DelegationProvenance::ParentTurnCommand {
            descendant_scope: DescendantTerminationScope::ParentAndDescendants,
            ..
        } => true,
        DelegationProvenance::ParentGoalCommand {
            descendant_scope: DescendantTerminationScope::ParentAndDescendants,
            goal_generation,
            ..
        } => goal_generation.value() > 0,
        DelegationProvenance::ParentLifecycleCommand {
            descendant_scope: DescendantTerminationScope::ParentAndDescendants,
            ..
        } => true,
        _ => false,
    }
}

/// Admits every terminal outcome a parent cascade can impose on a child.
///
/// A bound relationship carries its own termination policy, so the child
/// outcome is not required to match the parent reason: a parent cancellation
/// may map to a child `stop`, and a parent stop may map to a child `cancel`.
/// All four crossed pairs are therefore valid, exactly as `process_read`
/// projects them.
fn delegation_terminal_outcome_reason_is_admissible(
    outcome: DelegationOutcome,
    reason: DelegationReason,
) -> bool {
    matches!(
        outcome,
        DelegationOutcome::Stopped | DelegationOutcome::Cancelled
    ) && matches!(
        reason,
        DelegationReason::ParentStopped | DelegationReason::ParentCancelled
    )
}

fn delegation_content_is_valid(content: &str) -> bool {
    !content.is_empty() && content.len() <= MAX_CONTENT_FRAGMENT_BYTES && !content.contains('\0')
}

fn validate_delegation_transcript_entry(
    source_session_id: CanonicalUuid,
    entry: &TranscriptEntry,
) -> Result<(), FrameValidationError> {
    let valid = match entry {
        TranscriptEntry::DelegatedTask {
            parent_session_id,
            content,
            ..
        } => *parent_session_id != source_session_id && delegation_content_is_valid(content),
        TranscriptEntry::DelegationMessage {
            sender_session_id,
            recipient_session_id,
            ordinal,
            delivery_sequence,
            content,
            ..
        } => {
            *recipient_session_id == source_session_id
                && *sender_session_id != *recipient_session_id
                && ordinal.value() > 0
                && delivery_sequence.value() > 0
                && delegation_content_is_valid(content)
        }
        TranscriptEntry::DelegationResult {
            child_session_id,
            mode,
            delivery_sequence,
            outcome,
            content,
            reason,
            provenance,
            ..
        } => {
            *child_session_id != source_session_id
                && match mode {
                    DelegationWaitMode::Foreground => delivery_sequence.is_none(),
                    DelegationWaitMode::Background => {
                        delivery_sequence.is_some_and(|sequence| sequence.value() > 0)
                    }
                }
                && child_result_shape_is_valid(
                    source_session_id,
                    *child_session_id,
                    *outcome,
                    content,
                    *reason,
                    provenance,
                )
        }
        _ => true,
    };
    if valid {
        Ok(())
    } else {
        Err(FrameValidationError::DelegationShape)
    }
}

fn validate_turn_settings_payload(
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

fn validate_settings_event(event: &SessionEvent) -> Result<(), FrameValidationError> {
    match event {
        SessionEvent::SessionModelSettingsChanged {
            prior_defaults_version,
            installed_defaults_version,
            prior_model,
            installed_model,
            prior_settings,
            installed_settings,
            caller_override,
            adjustments,
            ..
        } => {
            prior_settings.validate_defaults()?;
            installed_settings.validate_defaults()?;
            validate_adjustments(adjustments)?;
            let validation_changed = matches!(
                (
                    prior_settings.validated_for_selection_id,
                    installed_settings.validated_for_selection_id,
                ),
                (Some(prior), Some(installed)) if prior != installed
            );
            let copied_precedence = ModelSettingsPrecedence {
                per_call: prior_settings.precedence.per_call,
                session: prior_settings.precedence.session,
                profile: installed_settings.precedence.profile,
                global_default: installed_settings.precedence.global_default,
            };
            let unadjusted_precedence = ModelSettingsPrecedence {
                session: overlay_inheriting_from(
                    *caller_override,
                    prior_settings.precedence.session,
                ),
                ..copied_precedence
            };
            let provenance_matches = apply_wire_adjustments(unadjusted_precedence, adjustments)
                .is_some_and(|expected| expected == installed_settings.precedence);
            if prior_defaults_version.value() == 0
                || prior_defaults_version.value().checked_add(1)
                    != Some(installed_defaults_version.value())
                || (prior_model == installed_model && prior_settings == installed_settings)
                || !snapshot_matches_model(prior_model, prior_settings)
                || !snapshot_matches_model(installed_model, installed_settings)
                || !provenance_matches
                || (!adjustments.is_empty() && !validation_changed)
                || adjustments_target_explicit_overlay(*caller_override, adjustments)
            {
                return Err(FrameValidationError::ModelSettingsShape);
            }
        }
        SessionEvent::TurnModelSettingsResolved {
            defaults_version,
            requested_model,
            selected_direct_id,
            per_call_override,
            settings,
            adjusted_from_selection_id,
            adjustments,
            ..
        } => validate_turn_settings_payload(
            *defaults_version,
            requested_model,
            *selected_direct_id,
            *per_call_override,
            settings,
            *adjusted_from_selection_id,
            adjustments,
        )?,
        SessionEvent::ToolApprovalDecided {
            decision,
            decider,
            rationale,
            ..
        } => validate_tool_approval_event_shape(decision, decider, rationale)?,
        SessionEvent::InputAccepted { content, .. } => content.validate()?,
        SessionEvent::SessionCreated {}
        | SessionEvent::GoalTurnRetired { .. }
        | SessionEvent::TurnActivated { .. }
        | SessionEvent::ModelCallTransition { .. }
        | SessionEvent::ToolBatchTransition { .. }
        | SessionEvent::RunnerStateTransition { .. }
        | SessionEvent::ContextCompacted { .. }
        | SessionEvent::TurnCompleted { .. }
        | SessionEvent::TurnFailed { .. }
        | SessionEvent::TurnRefused { .. }
        | SessionEvent::TurnCancelled { .. }
        | SessionEvent::TurnReconciliationRequired { .. }
        | SessionEvent::TurnToolReconciliationRequired { .. }
        | SessionEvent::ChildSpawned { .. }
        | SessionEvent::ChildWaiting { .. }
        | SessionEvent::SessionMessage { .. }
        | SessionEvent::ChildResult { .. }
        | SessionEvent::ChildLifecycleDisposition { .. } => {}
    }
    Ok(())
}

fn adjustments_target_explicit_overlay(
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

fn validate_adjustments(adjustments: &[ModelChangeAdjustment]) -> Result<(), FrameValidationError> {
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

/// Closed versioned server message family.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ServerMessage {
    /// Session creation receipt.
    SessionCreated {
        /// Created session.
        session_id: CanonicalUuid,
        /// Complete settings snapshot installed as defaults version one.
        model_settings: ModelSettingsSnapshot,
    },
    /// Commissioned-session receipt: the composite committed or replayed.
    SessionCommissioned {
        /// Created session.
        session_id: CanonicalUuid,
        /// Append-only commissioned-dispatch record carrying the fence.
        dispatch_id: CanonicalUuid,
    },
    /// A durable session-lifecycle command applied.
    SessionLifecycleCommandApplied {
        /// Target session.
        session_id: CanonicalUuid,
        /// What the command did.
        effect: SessionLifecycleEffect,
    },
    /// One delegated child spawn was recorded or equally replayed.
    SessionSpawned {
        /// Exact logical spawn tool request.
        tool_request_id: CanonicalUuid,
        /// Created child identity.
        child_session_id: CanonicalUuid,
        /// Exact immutable relationship policy.
        relationship: DelegationPolicy,
    },
    /// One child-delivery registration was recorded or equally replayed.
    SessionAwaitRegistered {
        /// Exact logical await tool request.
        tool_request_id: CanonicalUuid,
        /// Related child identity.
        child_session_id: CanonicalUuid,
        /// Exact registered delivery mode.
        mode: DelegationWaitMode,
    },
    /// One child outcome was delivered directly to a foreground await.
    ChildResult {
        /// Exact logical await tool request receiving the result.
        await_request_id: CanonicalUuid,
        /// Logical tool request that created the relationship.
        spawning_request_id: CanonicalUuid,
        /// Child whose terminal result was delivered.
        child_session_id: CanonicalUuid,
        /// Closed result outcome.
        outcome: DelegationOutcome,
        /// Exact returned content only for `returned`.
        #[serde(deserialize_with = "deserialize_required_nullable")]
        content: Option<String>,
        /// Closed reason correlated with the outcome.
        reason: DelegationReason,
        /// Exact child-turn or parent-command authority.
        provenance: DelegationProvenance,
    },
    /// One relationship message was recorded or equally replayed.
    SessionMessageSent {
        /// Exact logical message tool request.
        tool_request_id: CanonicalUuid,
        /// Immutable message identity.
        message_id: CanonicalUuid,
        /// Exact relationship direction.
        direction: DelegationMessageDirection,
        /// Positive contiguous relationship event ordinal.
        ordinal: CanonicalU64,
        /// Positive recipient-wide delivery sequence.
        delivery_sequence: CanonicalU64,
    },
    /// One immutable placement update was appended or equally replayed.
    SessionPlacementUpdated {
        session_id: CanonicalUuid,
        placement_version: CanonicalU64,
        placement: SessionPlacement,
    },
    /// Input acceptance receipt.
    InputSubmitted {
        /// Owning session.
        session_id: CanonicalUuid,
        /// Accepted input.
        accepted_input_id: CanonicalUuid,
        /// Immutable acceptance position.
        acceptance_position: CanonicalU64,
        /// Created origin turn.
        turn_id: CanonicalUuid,
        /// Complete settings snapshot frozen for the origin turn.
        model_settings: ModelSettingsSnapshot,
    },
    /// Configuration-free steering acceptance receipt.
    SteeringSubmitted {
        /// Owning session.
        session_id: CanonicalUuid,
        /// Accepted input.
        accepted_input_id: CanonicalUuid,
        /// Immutable acceptance position.
        acceptance_position: CanonicalU64,
        /// Exact active turn the steering is bound to.
        source_turn_id: CanonicalUuid,
    },
    /// A durable user goal command appended one event.
    GoalTransitionApplied {
        /// Owning session.
        session_id: CanonicalUuid,
        /// Appended event position.
        event_ordinal: CanonicalU64,
        /// Generation acted on by the event.
        generation: CanonicalU64,
    },
    /// Begins one complete goal-history snapshot.
    GoalHistoryStart {
        /// Owning session.
        session_id: CanonicalUuid,
        /// Current immutable statement generation.
        current_generation: CanonicalU64,
        /// Current immutable statement.
        current_statement: String,
    },
    /// Carries the current lifecycle state in a frame bounded independently from text.
    GoalHistoryState {
        /// Current derived lifecycle state.
        current_state: GoalLifecycleState,
    },
    /// One ordered event in a goal-history snapshot.
    GoalHistoryItem {
        /// Positive contiguous event position.
        event_ordinal: CanonicalU64,
        /// Statement generation acted on by the event.
        generation: CanonicalU64,
        /// Exact event payload and provenance.
        event: GoalHistoryEvent,
    },
    /// Completes one goal-history snapshot.
    GoalHistoryEnd {
        /// Number of preceding history items.
        event_count: CanonicalU64,
    },
    /// Begins a session-summary sequence.
    SessionsStart {},
    /// One current session summary.
    SessionSummary {
        /// Session identity.
        session_id: CanonicalUuid,
        /// Current defaults version.
        defaults_version: CanonicalU64,
        /// Current model-selection request.
        model_selection: ModelSelection,
        /// Current immutable placement-history version.
        placement_version: CanonicalU64,
        /// Current opt-in placement decision.
        placement: SessionPlacement,
        /// Complete current runner projection, null for daemon-only sessions.
        #[serde(deserialize_with = "deserialize_required_nullable")]
        runner: Option<RunnerProjection>,
    },
    /// Completes a session-summary sequence.
    SessionsEnd {
        /// Number of preceding summaries.
        session_count: CanonicalU64,
    },
    /// One member of a coherent operator-status snapshot.
    OperatorStatus(Box<OperatorStatusMessage>),
    /// Begins the available-template sequence.
    TemplatesStart {},
    /// One available static template summary.
    TemplateSummary {
        /// Validated template name.
        name: String,
        /// Positive operator-assigned bundle version.
        version: CanonicalU64,
    },
    /// Completes the available-template sequence.
    TemplatesEnd {
        /// Number of preceding summaries.
        template_count: CanonicalU64,
    },
    /// Client-relevant deployment policy, with null denoting unbounded.
    DeploymentLimits {
        #[serde(deserialize_with = "deserialize_required_nullable")]
        max_message_utf8_bytes: Option<CanonicalU64>,
        #[serde(deserialize_with = "deserialize_required_nullable")]
        max_system_prompt_utf8_bytes: Option<CanonicalU64>,
        #[serde(deserialize_with = "deserialize_required_nullable")]
        terminal_input_channel_capacity: Option<CanonicalU64>,
        #[serde(deserialize_with = "deserialize_required_nullable")]
        min_metadata_page_size: Option<CanonicalU64>,
        #[serde(deserialize_with = "deserialize_required_nullable")]
        max_metadata_page_size: Option<CanonicalU64>,
        #[serde(deserialize_with = "deserialize_required_nullable")]
        max_review_findings_per_run: Option<CanonicalU64>,
    },
    /// Begins one bounded metadata-summary page.
    SessionMetadataPageStart {},
    /// One current session metadata summary.
    SessionMetadataSummary {
        /// Session identity.
        session_id: CanonicalUuid,
        /// Current defaults version.
        defaults_version: CanonicalU64,
        /// Current model-selection request.
        model_selection: ModelSelection,
        /// Whether the current defaults blanket-approve dangerous tools.
        dangerous_tool_auto_approval: bool,
        /// Optional exact title.
        #[serde(deserialize_with = "deserialize_required_nullable")]
        title: Option<String>,
        /// Exact sorted flat tags.
        #[serde(deserialize_with = "deserialize_session_metadata_tags")]
        tags: Vec<String>,
        /// Whether the session is archived.
        archived: bool,
        /// Last replacement writer, absent only before the first write.
        #[serde(deserialize_with = "deserialize_required_nullable")]
        last_writer: Option<MetadataLastWriter>,
    },
    /// Completes one bounded metadata-summary page.
    SessionMetadataPageEnd {
        /// Number of preceding summaries.
        session_count: CanonicalU64,
        /// Exclusive cursor for another page, or null when no later match
        /// existed in this page snapshot.
        #[serde(deserialize_with = "deserialize_required_nullable")]
        next_after_session_id: Option<CanonicalUuid>,
    },
    /// Begins one bounded unified conversation-summary page.
    ConversationPageStart {},
    /// One unified conversation summary.
    ConversationSummary {
        /// Closed per-origin summary.
        conversation: ConversationSummary,
    },
    /// Completes one bounded unified conversation-summary page.
    ConversationPageEnd {
        /// Number of preceding summaries.
        conversation_count: CanonicalU64,
        /// Exclusive cursor for another page, or null when no later match
        /// existed in this page snapshot.
        #[serde(deserialize_with = "deserialize_required_nullable")]
        next_after: Option<ConversationCursor>,
    },
    /// Begins the configured model-alias sequence.
    ModelAliasesStart {},
    /// One configured alias and the direct selection it currently names.
    ModelAliasSummary {
        /// Stable alias identity selectable by creation commands.
        alias_id: CanonicalUuid,
        /// Current deployment-owned direct selection target.
        selection_id: CanonicalUuid,
    },
    /// Completes the configured model-alias sequence.
    ModelAliasesEnd {
        /// Number of preceding alias summaries.
        alias_count: CanonicalU64,
    },
    /// Begins the configured model-capability sequence.
    ModelCapabilitiesStart {},
    /// One direct selection and its exact client-visible capabilities.
    ModelCapabilityItem {
        selection_id: CanonicalUuid,
        capabilities: ModelCapabilities,
    },
    /// Completes the configured model-capability sequence.
    ModelCapabilitiesEnd { capability_count: CanonicalU64 },
    /// One complete current metadata read.
    SessionMetadata {
        /// Selected session.
        session_id: CanonicalUuid,
        /// Complete current metadata object.
        metadata: SessionMetadata,
        /// Last replacement writer, absent only before the first write.
        #[serde(deserialize_with = "deserialize_required_nullable")]
        last_writer: Option<MetadataLastWriter>,
    },
    /// One successful complete metadata replacement receipt.
    SessionMetadataReplaced {
        /// Updated session.
        session_id: CanonicalUuid,
        /// Complete committed metadata object.
        metadata: SessionMetadata,
        /// Non-null last replacement writer.
        last_writer: MetadataLastWriter,
    },
    /// One successful forward-only session-defaults replacement receipt.
    SessionDefaultsReplaced {
        /// Updated session.
        session_id: CanonicalUuid,
        /// Newly installed immutable defaults epoch.
        defaults_version: CanonicalU64,
        /// Complete committed model selection.
        model_selection: ModelSelection,
        /// Complete settings snapshot installed on the new epoch.
        model_settings: ModelSettingsSnapshot,
        /// Complete committed dangerous-tool blanket-auto posture.
        dangerous_tool_auto_approval: bool,
        /// Complete committed system prompt; required null-or-text member.
        #[serde(default, skip_serializing_if = "SystemPromptMember::is_absent")]
        system_prompt: SystemPromptMember,
    },
    /// One complete current or named immutable session-defaults epoch.
    SessionDefaults {
        /// Selected session.
        session_id: CanonicalUuid,
        /// The read immutable defaults epoch.
        defaults_version: CanonicalU64,
        /// Complete model selection on that epoch.
        model_selection: ModelSelection,
        /// Complete settings snapshot stored on the selected epoch.
        model_settings: ModelSettingsSnapshot,
        /// Complete dangerous-tool blanket-auto posture on that epoch.
        dangerous_tool_auto_approval: bool,
        /// Exact optional system prompt on that epoch.
        #[serde(deserialize_with = "deserialize_required_nullable")]
        system_prompt: Option<SystemPromptText>,
    },
    /// One recorded user tool-decision receipt.
    ///
    /// The receipt mirrors the recorded applied result exactly; an equal
    /// command replay returns this same projection.
    ToolRequestDecided {
        /// Decided logical tool request.
        tool_request_id: CanonicalUuid,
        /// Exact recorded decision.
        decision: ToolDecision,
    },
    /// One recorded recorded-override receipt.
    ///
    /// The receipt mirrors the recorded applied result exactly; an equal
    /// command replay returns this same projection.
    ToolDenialOverridden {
        /// Overridden delegate-denied tool request.
        tool_request_id: CanonicalUuid,
    },
    /// One completed append-only context-compaction receipt.
    SessionCompacted {
        /// Compacted session.
        session_id: CanonicalUuid,
        /// Immutable compaction identity.
        context_compaction_id: CanonicalUuid,
        /// Dedicated producing model call.
        model_call_id: CanonicalUuid,
        /// One-based exact through position in the source frontier.
        through_position: CanonicalU64,
        /// Appended summary semantic entry.
        summary_entry_id: CanonicalUuid,
        /// Complete source-plus-summary result frontier.
        result_frontier_id: CanonicalUuid,
    },
    /// One new immutable imported conversation was inserted.
    ConversationImportInserted {
        /// Newly durable imported-conversation identity.
        imported_conversation_id: CanonicalUuid,
    },
    /// The exact imported snapshot was already durable.
    ConversationImportAlreadyImported {
        /// Existing durable imported-conversation identity.
        imported_conversation_id: CanonicalUuid,
    },
    /// One per-connection chunked import was initialized.
    ConversationImportBegun {
        /// Exact total source size admitted from the begin request.
        declared_size_bytes: CanonicalU64,
    },
    /// One source chunk was appended to the in-progress import.
    ConversationImportAppended {
        /// Exact total source bytes observed after this append.
        assembled_size_bytes: CanonicalU64,
    },
    /// One per-connection chunked import was discarded.
    ConversationImportAborted {},
    /// One connection-local immutable-blob upload was initialized.
    BlobUploadBegun {
        expected_digest: CanonicalBlobDigest,
        expected_length_bytes: CanonicalU64,
    },
    /// The routed store already held a verified replica, so no chunks are owed.
    BlobUploadAlreadyPresent {
        digest: CanonicalBlobDigest,
        byte_length: CanonicalU64,
    },
    /// One bounded chunk was appended to the connection-local spool.
    BlobUploadAppended {
        assembled_length_bytes: CanonicalU64,
    },
    /// The exact assembled bytes were published and catalogued.
    BlobUploadCommitted {
        digest: CanonicalBlobDigest,
        byte_length: CanonicalU64,
    },
    /// One connection-local immutable-blob upload was discarded.
    BlobUploadAborted {},
    /// Bounded catalog facts for one immutable identity.
    BlobMetadata {
        digest: CanonicalBlobDigest,
        byte_length: CanonicalU64,
        replica_count: CanonicalU64,
    },
    /// One exact verified byte range.
    #[serde(rename = "blob_chunk")]
    BlobChunkRead {
        digest: CanonicalBlobDigest,
        offset_bytes: CanonicalU64,
        bytes: BlobChunk,
    },
    /// Begins one imported-conversation entry sequence.
    ImportedConversationStart {
        /// Inspected imported conversation.
        imported_conversation_id: CanonicalUuid,
    },
    /// One imported entry as the inspection projection presents it.
    ImportedConversationEntry {
        /// One-based imported position, exactly the ordinal
        /// `create_session_from_imported_frontier` consumes.
        position: CanonicalU64,
        /// Immutable imported entry identity.
        imported_entry_id: CanonicalUuid,
        /// Exact source-speaker attestation.
        source_speaker: ImportedSourceSpeaker,
        /// Normalized content variant.
        content_kind: ImportedContentKind,
        /// Bounded preview of exact attested text, or null when this entry
        /// carries no exact attested text.
        #[serde(deserialize_with = "deserialize_required_nullable")]
        text_preview: Option<ImportedTextPreview>,
    },
    /// Completes one imported-conversation entry sequence.
    ImportedConversationEnd {
        /// Inspected imported conversation.
        imported_conversation_id: CanonicalUuid,
        /// Number of preceding entries, equal to the greatest selectable
        /// position.
        entry_count: CanonicalU64,
    },
    /// Begins one transcript snapshot sequence.
    TranscriptSnapshotStart {
        /// Selected session.
        session_id: CanonicalUuid,
        /// Snapshot outbox cursor.
        cursor: CanonicalU64,
        /// Complete current runner placement, or null for a daemon-only session.
        #[serde(deserialize_with = "deserialize_required_nullable")]
        runner: Option<RunnerProjection>,
    },
    /// One authoritative turn projection.
    TranscriptTurn {
        /// Immutable turn identity.
        turn_id: CanonicalUuid,
        /// Immutable acceptance order.
        acceptance_position: CanonicalU64,
        /// Complete frozen settings for a settings-aware turn, or null for a
        /// turn committed before settings evidence existed.
        #[serde(deserialize_with = "deserialize_required_nullable")]
        model_settings: Option<TurnModelSettingsSnapshot>,
        /// Exact lifecycle state.
        state: TurnState,
    },
    /// Exact independently nullable token fields for one terminal model call.
    TranscriptModelCallUsage {
        /// Zero-based model-call evidence index in this snapshot.
        model_call_index: CanonicalU64,
        /// Turn that owns the terminal model call.
        turn_id: CanonicalUuid,
        /// Immutable model-call identity.
        model_call_id: CanonicalUuid,
        /// Closed source vocabulary for the independently nullable counts.
        usage_provenance: UsageProvenance,
        /// Exact independently nullable fields from the named provenance.
        usage: ModelCallTokenUsage,
        /// Read-time configured-rate derivation, required null when unavailable.
        #[serde(deserialize_with = "deserialize_required_nullable")]
        cost: Option<ModelCallDollarCost>,
    },
    /// Completes the model-call evidence section of one transcript snapshot.
    TranscriptModelCallsEnd {
        /// Number of preceding model-call usage messages.
        model_call_count: CanonicalU64,
    },
    /// One non-text frontier member.
    TranscriptEntry {
        /// Zero-based frontier member index.
        entry_index: CanonicalU64,
        /// Entry source session.
        source_session_id: CanonicalUuid,
        /// Semantic entry identity.
        entry_id: CanonicalUuid,
        /// Exact marker payload.
        entry: TranscriptEntry,
    },
    /// One atomic native user entry with exact ordered multipart content.
    TranscriptUserEntry {
        /// Zero-based frontier member index.
        entry_index: CanonicalU64,
        /// Entry source session.
        source_session_id: CanonicalUuid,
        /// Semantic entry identity.
        entry_id: CanonicalUuid,
        /// Exact accepted input.
        accepted_input_id: CanonicalUuid,
        /// Origin turn.
        turn_id: CanonicalUuid,
        /// Canonical ordered user content.
        content: UserInputContent,
    },
    /// Begins one text-bearing frontier member.
    TranscriptTextEntry {
        /// Zero-based frontier member index.
        entry_index: CanonicalU64,
        /// Entry source session.
        source_session_id: CanonicalUuid,
        /// Semantic entry identity.
        entry_id: CanonicalUuid,
        /// Exact text-entry metadata.
        entry: TranscriptTextEntry,
    },
    /// One bounded text fragment.
    TranscriptContent {
        /// Frontier member index.
        entry_index: CanonicalU64,
        /// Zero-based fragment index.
        fragment_index: CanonicalU64,
        /// Whether this is the entry's final fragment.
        final_fragment: bool,
        /// Exact content fragment.
        content_fragment: ContentFragment,
    },
    /// Completes one transcript snapshot.
    TranscriptSnapshotEnd {
        /// Selected session.
        session_id: CanonicalUuid,
        /// Snapshot outbox cursor.
        cursor: CanonicalU64,
        /// Number of preceding turn messages.
        turn_count: CanonicalU64,
        /// Number of complete semantic entries.
        entry_count: CanonicalU64,
    },
    /// One committed update after a follow snapshot.
    SessionEvent {
        /// Global durable cursor.
        cursor: CanonicalU64,
        /// Owning session.
        session_id: CanonicalUuid,
        /// Exact typed update.
        event: SessionEvent,
    },
    /// One cursorless, process-local provider text fragment.
    ProviderTextDelta {
        /// Owning session.
        session_id: CanonicalUuid,
        /// Active turn receiving the provider response.
        turn_id: CanonicalUuid,
        /// Correlated model call producing the response.
        model_call_id: CanonicalUuid,
        /// Provider part position this fragment extends.
        part_index: CanonicalU64,
        /// One bounded fragment of already-redacted provider text.
        content: ContentFragment,
    },
    /// One immutable target registration was recorded or equally replayed.
    ReviewTargetCreated {
        /// Registered target.
        target_id: CanonicalUuid,
    },
    /// One run and its sole pass were admitted or equally replayed.
    ReviewRunStarted {
        /// Admitted run.
        run_id: CanonicalUuid,
        /// Admitted pass.
        pass_id: CanonicalUuid,
    },
    /// One queued run and pass were atomically activated or equally replayed.
    ReviewPassActivated {
        /// Activated run.
        run_id: CanonicalUuid,
        /// Activated pass.
        pass_id: CanonicalUuid,
    },
    /// One pass without another typed result was terminalized.
    ReviewPassCompleted {
        run_id: CanonicalUuid,
        pass_id: CanonicalUuid,
        state: ReviewPassLifecycle,
    },
    /// One read-only result and complete finding inventory were committed.
    ReviewFindingsRecorded {
        /// Concluding run.
        run_id: CanonicalUuid,
        /// Concluding pass.
        pass_id: CanonicalUuid,
        /// Exact committed finding count.
        finding_count: CanonicalU64,
    },
    /// One finding disposition was committed.
    ReviewFindingEventRecorded {
        /// Updated finding.
        finding_id: CanonicalUuid,
        /// Current derived status.
        status: ReviewFindingStatus,
    },
    /// One pre-effect external-link reservation was recorded.
    ReviewExternalLinkReserved {
        /// Stable reservation identity.
        external_link_id: CanonicalUuid,
    },
    /// One provider object identity was attached.
    ReviewExternalLinkAttached {
        /// Consumed reservation identity.
        external_link_id: CanonicalUuid,
        /// Canonical provider object key.
        external_object: String,
    },
    /// One immutable target read.
    ReviewTarget {
        /// Complete target snapshot.
        target: ReviewTargetSnapshot,
    },
    /// One run and its optional pass read.
    ReviewRun {
        /// Complete run snapshot.
        run: ReviewRunSnapshot,
        /// Complete pass snapshot after admission.
        #[serde(deserialize_with = "deserialize_required_nullable")]
        pass: Option<ReviewPassSnapshot>,
    },
    /// One complete finding read.
    ReviewFinding {
        /// Complete finding snapshot.
        finding: ReviewFindingSnapshot,
    },
    /// Begins one finding list sequence.
    ReviewFindingsStart {
        /// Selected run.
        run_id: CanonicalUuid,
    },
    /// One finding in identity order.
    ReviewFindingItem {
        /// Complete finding snapshot.
        finding: ReviewFindingSnapshot,
    },
    /// Completes one finding list sequence.
    ReviewFindingsEnd {
        /// Number of preceding items.
        finding_count: CanonicalU64,
    },
    /// One orchestration attempt was admitted or equally replayed.
    ReviewOrchestrationStarted { attempt_id: CanonicalUuid },
    /// One orchestration attempt advanced or equally replayed.
    ReviewOrchestrationAdvanced {
        attempt_id: CanonicalUuid,
        state: ReviewOrchestrationState,
    },
    /// One complete orchestration attempt read.
    ReviewOrchestration {
        snapshot: ReviewOrchestrationSnapshot,
    },
    /// Stable, sanitized failure.
    Error {
        /// Stable error code.
        code: ErrorCode,
        /// Non-sensitive human diagnostic.
        message: String,
        /// Typed durable-rejection or conversation-import failure evidence.
        #[serde(default, skip_serializing_if = "ErrorDetail::is_absent")]
        detail: ErrorDetail,
    },
}

impl ServerMessage {
    fn validate(&self) -> Result<(), FrameValidationError> {
        validate_operator_status_message(self)?;
        match self {
            Self::SessionCreated { model_settings, .. } => model_settings.validate_defaults()?,
            Self::SessionAwaitRegistered {
                mode: DelegationWaitMode::Foreground,
                ..
            } => {
                return Err(FrameValidationError::DelegationShape);
            }
            Self::ChildResult {
                await_request_id,
                spawning_request_id,
                child_session_id,
                outcome,
                content,
                reason,
                provenance,
            } if await_request_id == spawning_request_id
                || !direct_child_result_shape_is_valid(
                    *child_session_id,
                    *outcome,
                    content,
                    *reason,
                    provenance,
                ) =>
            {
                return Err(FrameValidationError::DelegationShape);
            }
            Self::SessionMessageSent {
                ordinal,
                delivery_sequence,
                ..
            } if ordinal.value() < 2 || delivery_sequence.value() == 0 => {
                return Err(FrameValidationError::DelegationShape);
            }
            Self::SessionAwaitRegistered {
                mode: DelegationWaitMode::Background,
                ..
            }
            | Self::ChildResult { .. }
            | Self::SessionMessageSent { .. } => {}
            Self::InputSubmitted { model_settings, .. } => model_settings.validate()?,
            Self::SessionDefaultsReplaced {
                model_selection,
                model_settings,
                ..
            }
            | Self::SessionDefaults {
                model_selection,
                model_settings,
                ..
            } => {
                model_settings.validate_defaults()?;
                if !snapshot_matches_model(model_selection, model_settings) {
                    return Err(FrameValidationError::ModelSettingsShape);
                }
            }
            Self::SessionEvent {
                session_id, event, ..
            } => {
                validate_settings_event(event)?;
                validate_delegation_session_event(*session_id, event)?;
            }
            Self::TranscriptTurn {
                turn_id,
                model_settings,
                state,
                ..
            } => {
                if let TurnState::Queued { content, .. } = state {
                    content.validate()?;
                }
                if let Some(settings) = model_settings {
                    settings.validate()?;
                    if settings.turn_id != *turn_id
                        || (matches!(
                            state,
                            TurnState::Queued {
                                accepted_input_id,
                                ..
                            } if settings.accepted_input_id != *accepted_input_id
                        ))
                    {
                        return Err(FrameValidationError::ModelSettingsShape);
                    }
                }
            }
            Self::TranscriptEntry {
                entry:
                    TranscriptEntry::AssistantToolUse {
                        approval: Some(approval),
                        ..
                    },
                ..
            } => validate_tool_approval_event_shape(
                &approval.decision,
                &approval.decider,
                &approval.rationale,
            )?,
            Self::TranscriptUserEntry { content, .. } => content.validate()?,
            Self::GoalTransitionApplied {
                event_ordinal,
                generation,
                ..
            }
            | Self::GoalHistoryItem {
                event_ordinal,
                generation,
                ..
            } if event_ordinal.value() == 0 || generation.value() == 0 => {
                return Err(FrameValidationError::GoalShape);
            }
            Self::GoalHistoryStart {
                current_generation,
                current_statement,
                ..
            } => {
                if current_generation.value() == 0 {
                    return Err(FrameValidationError::GoalShape);
                }
                validate_goal_text(current_statement)?;
            }
            Self::GoalHistoryState { current_state } => validate_goal_state(current_state)?,
            Self::GoalHistoryItem { event, .. } => validate_goal_event(event)?,
            Self::GoalHistoryEnd { event_count } if event_count.value() == 0 => {
                return Err(FrameValidationError::GoalShape);
            }
            Self::SessionMetadataSummary {
                title,
                tags,
                archived,
                last_writer,
                ..
            } => {
                let mut total_utf8_bytes = 0usize;
                if let Some(title) = title {
                    validate_nonempty_metadata_text(title)
                        .map_err(|_| FrameValidationError::MetadataShape)?;
                    add_metadata_utf8_bytes(&mut total_utf8_bytes, title)
                        .map_err(|_| FrameValidationError::MetadataShape)?;
                }
                let canonical = canonical_metadata_tags(tags.clone(), None)
                    .map_err(|_| FrameValidationError::MetadataShape)?;
                if canonical != *tags {
                    return Err(FrameValidationError::MetadataShape);
                }
                for tag in tags {
                    add_metadata_utf8_bytes(&mut total_utf8_bytes, tag)
                        .map_err(|_| FrameValidationError::MetadataShape)?;
                }
                if last_writer.is_none() && (title.is_some() || !tags.is_empty() || *archived) {
                    return Err(FrameValidationError::MetadataShape);
                }
            }
            Self::SessionMetadataPageEnd {
                session_count,
                next_after_session_id,
            } => {
                if next_after_session_id.is_some() && session_count.value() == 0 {
                    return Err(FrameValidationError::MetadataShape);
                }
            }
            Self::ConversationSummary { conversation } => conversation.validate()?,
            Self::ModelCapabilityItem { capabilities, .. } => capabilities.validate()?,
            Self::ModelCapabilitiesEnd { capability_count }
                if capability_count.value() > MAX_MODEL_CAPABILITY_CATALOG_ENTRIES as u64 =>
            {
                return Err(FrameValidationError::ModelSettingsShape);
            }
            Self::ReviewPassCompleted {
                state: ReviewPassLifecycle::Queued | ReviewPassLifecycle::Running,
                ..
            } => {
                return Err(FrameValidationError::ReviewShape);
            }
            Self::ReviewOrchestration { snapshot } => {
                validate_review_orchestration_snapshot(snapshot)?;
            }
            Self::TemplateSummary { name, version } => {
                validate_session_template_name(name)?;
                if version.value() == 0 {
                    return Err(FrameValidationError::TemplateShape);
                }
            }
            Self::SessionSummary {
                placement_version,
                placement,
                ..
            } => {
                if placement_version.value() == 0 {
                    return Err(FrameValidationError::PlacementShape);
                }
                validate_session_placement_shape(placement)?;
            }
            Self::SessionPlacementUpdated {
                placement_version,
                placement,
                ..
            } => {
                if placement_version.value() == 0 {
                    return Err(FrameValidationError::PlacementShape);
                }
                validate_session_placement_shape(placement)?;
            }
            Self::ConversationPageEnd {
                conversation_count,
                next_after,
            } => {
                if next_after.is_some() && conversation_count.value() == 0 {
                    return Err(FrameValidationError::ConversationListShape);
                }
            }
            Self::SessionMetadata {
                metadata,
                last_writer,
                ..
            } if last_writer.is_none() && !metadata.is_initial() => {
                return Err(FrameValidationError::MetadataShape);
            }
            Self::ImportedConversationEntry {
                position,
                content_kind,
                text_preview,
                ..
            } => {
                if position.value() == 0 {
                    return Err(FrameValidationError::ImportedConversationEntryShape);
                }
                if let Some(preview) = text_preview {
                    // Only `Text` content has an exact attested text to
                    // preview, so a preview on any other kind contradicts the
                    // kind it accompanies.
                    if *content_kind != ImportedContentKind::Text {
                        return Err(FrameValidationError::ImportedConversationEntryShape);
                    }
                    preview.validate()?;
                }
            }
            Self::ConversationImportAppended {
                assembled_size_bytes,
            } if assembled_size_bytes.value() == 0 => {
                return Err(FrameValidationError::ConversationImportShape);
            }
            Self::BlobUploadBegun {
                expected_length_bytes,
                ..
            }
            | Self::BlobUploadAlreadyPresent {
                byte_length: expected_length_bytes,
                ..
            }
            | Self::BlobUploadCommitted {
                byte_length: expected_length_bytes,
                ..
            }
            | Self::BlobUploadAppended {
                assembled_length_bytes: expected_length_bytes,
            } if expected_length_bytes.value() == 0 => {
                return Err(FrameValidationError::BlobUploadShape);
            }
            Self::BlobMetadata { byte_length, .. } if byte_length.value() == 0 => {
                return Err(FrameValidationError::BlobReadShape);
            }
            Self::BlobChunkRead {
                offset_bytes,
                bytes,
                ..
            } if bytes.as_bytes().is_empty()
                || bytes.as_bytes().len() > MAX_BLOB_READ_BYTES
                || u64::try_from(bytes.as_bytes().len()).map_or(true, |length_bytes| {
                    offset_bytes.value().checked_add(length_bytes).is_none()
                }) =>
            {
                return Err(FrameValidationError::BlobReadShape);
            }
            Self::TranscriptModelCallUsage { usage, cost, .. }
                if cost.is_some()
                    && usage.input_tokens.is_none()
                    && usage.output_tokens.is_none()
                    && usage.cache_creation_input_tokens.is_none()
                    && usage.cache_read_input_tokens.is_none() =>
            {
                return Err(FrameValidationError::ModelCallUsageShape);
            }
            Self::TranscriptEntry {
                source_session_id,
                entry,
                ..
            } => validate_delegation_transcript_entry(*source_session_id, entry)?,
            Self::SessionSpawned { .. } => {}
            _ => {}
        }
        Ok(())
    }
}

#[derive(signalbox_derive::Accessors)]
/// One validated server frame.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ServerFrame {
    version: ProtocolVersion,
    request_id: RequestId,
    /// Borrows the closed server message.
    #[get]
    message: ServerMessage,
}

impl ServerFrame {
    /// Constructs a single-version response frame.
    pub fn try_new(
        request_id: RequestId,
        message: ServerMessage,
    ) -> Result<Self, FrameValidationError> {
        Self::try_new_for_version(ProtocolVersion::One, request_id, message)
    }

    /// Constructs one response in an admitted protocol version.
    pub fn try_new_for_version(
        version: ProtocolVersion,
        request_id: RequestId,
        message: ServerMessage,
    ) -> Result<Self, FrameValidationError> {
        let frame = Self {
            version,
            request_id,
            message,
        };
        frame.validate()?;
        Ok(frame)
    }

    /// Returns the admitted protocol version.
    pub const fn version(&self) -> ProtocolVersion {
        self.version
    }

    /// Returns the request correlation identity.
    pub const fn request_id(&self) -> RequestId {
        self.request_id
    }

    fn validate(&self) -> Result<(), FrameValidationError> {
        if let ServerMessage::TranscriptTurn { state, .. } = &self.message {
            state.validate()?;
        }
        if let ServerMessage::SessionDefaultsReplaced { system_prompt, .. } = &self.message {
            validate_system_prompt_member(system_prompt)?;
        }
        self.message.validate()?;
        match &self.message {
            ServerMessage::Error { code, detail, .. } => {
                if !self.request_id.is_correlated()
                    && !matches!(
                        code,
                        ErrorCode::MalformedFrame | ErrorCode::UnsupportedVersion
                    )
                {
                    return Err(FrameValidationError::UncorrelatedApplicationError);
                }
                if let Some(RejectionDetail::ImportedFrontierPositionOutOfRange {
                    requested_position,
                    last_position,
                    ..
                }) = detail.value()
                {
                    // An imported conversation's positions are the contiguous
                    // sequence `1..=last_position`, so a nonpositive bound or a
                    // requested ordinal inside that range contradicts the
                    // rejection the detail states.
                    if last_position.value() == 0
                        || requested_position.value() <= last_position.value()
                    {
                        return Err(FrameValidationError::ImportedFrontierRangeShape);
                    }
                }
                if let Some(detail) = detail.value() {
                    if detail.is_bulk_ingest() {
                        if *code != ErrorCode::InvalidRequest {
                            return Err(FrameValidationError::ErrorDetailShape);
                        }
                    } else if detail.is_conversation_import() {
                        if *code != ErrorCode::InvalidRequest {
                            return Err(FrameValidationError::ErrorDetailShape);
                        }
                        validate_conversation_import_detail(detail)?;
                    } else if detail.is_blob_upload() {
                        if *code != ErrorCode::InvalidRequest {
                            return Err(FrameValidationError::ErrorDetailShape);
                        }
                        validate_blob_upload_detail(detail)?;
                    } else if detail.is_blob_read() {
                        if *code != ErrorCode::InvalidRequest {
                            return Err(FrameValidationError::ErrorDetailShape);
                        }
                        validate_blob_read_detail(detail)?;
                    } else if *code != ErrorCode::Rejected {
                        return Err(FrameValidationError::ErrorDetailShape);
                    } else {
                        validate_rejection_detail(detail)?;
                    }
                } else if *code == ErrorCode::Rejected {
                    return Err(FrameValidationError::ErrorDetailShape);
                }
            }
            _ if !self.request_id.is_correlated() => {
                return Err(FrameValidationError::UncorrelatedSuccess);
            }
            _ => {}
        }
        Ok(())
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawServerFrame {
    version: ProtocolVersion,
    request_id: RequestId,
    message: ServerMessage,
}

impl<'de> Deserialize<'de> for ServerFrame {
    fn deserialize<DeserializerT>(deserializer: DeserializerT) -> Result<Self, DeserializerT::Error>
    where
        DeserializerT: Deserializer<'de>,
    {
        let raw = RawServerFrame::deserialize(deserializer)?;
        let frame = Self {
            version: raw.version,
            request_id: raw.request_id,
            message: raw.message,
        };
        frame.validate().map_err(serde::de::Error::custom)?;
        Ok(frame)
    }
}

fn validate_rejection_detail(detail: RejectionDetail) -> Result<(), FrameValidationError> {
    let valid = match detail {
        RejectionDetail::SessionPlacementCurrentVersionMismatch {
            expected_placement_version,
            current_placement_version,
            ..
        } => {
            expected_placement_version.value() > 0
                && current_placement_version.value() > 0
                && expected_placement_version != current_placement_version
        }
        RejectionDetail::SessionPlacementVersionExhausted {
            current_placement_version,
            ..
        } => current_placement_version.value() == u64::MAX,
        RejectionDetail::DelegationEventOrdinalExhausted { last, .. } => last.value() == u64::MAX,
        RejectionDetail::DelegationDeliverySequenceExhausted { last, .. } => {
            last.value() == u64::MAX
        }
        RejectionDetail::SessionNotFound { .. }
        | RejectionDetail::AttachmentBlobNotFound { .. }
        | RejectionDetail::AttachmentByteBudgetExceeded { .. }
        | RejectionDetail::UnsupportedReasoningLevel { .. }
        | RejectionDetail::UnsupportedFastMode { .. }
        | RejectionDetail::UnsupportedServiceTier { .. }
        | RejectionDetail::GoalCommandRejected { .. }
        | RejectionDetail::SessionLifecycleCommandRejected { .. }
        | RejectionDetail::ActiveTurnPresent { .. }
        | RejectionDetail::CommissionTargetBusy { .. }
        | RejectionDetail::ActiveTurnMismatch { .. }
        | RejectionDetail::NoActiveTurn { .. }
        | RejectionDetail::TurnNotAwaitingReconciliation { .. }
        | RejectionDetail::InterruptAlreadyApplied { .. }
        | RejectionDetail::InterruptUnavailableWhileAwaitingApproval { .. }
        | RejectionDetail::SafePointUnavailableWhileStopping { .. }
        | RejectionDetail::ToolRequestNotFound { .. }
        | RejectionDetail::ToolRequestAlreadyResolved { .. }
        | RejectionDetail::ToolRequestNotEarliestUndecided { .. }
        | RejectionDetail::ToolRequestNotInSession { .. }
        | RejectionDetail::ToolRequestNotDelegateDenied { .. }
        | RejectionDetail::ToolRequestNotTerminallyDenied { .. }
        | RejectionDetail::ToolDenialAlreadyOverridden { .. }
        | RejectionDetail::DelegationRequestNotInTurn { .. }
        | RejectionDetail::DelegationToolRequestNotExecutable { .. }
        | RejectionDetail::DelegationSpawnConflict { .. }
        | RejectionDetail::DelegatedChildIdentityCollision { .. }
        | RejectionDetail::DelegationRelationNotFound { .. }
        | RejectionDetail::DelegationAwaitConflict { .. }
        | RejectionDetail::DelegationMessageConflict { .. }
        | RejectionDetail::DelegationMessageIdentityCollision { .. }
        | RejectionDetail::DefaultsVersionMismatch { .. }
        | RejectionDetail::UnknownModelAlias { .. }
        | RejectionDetail::AcceptancePositionExhausted { .. }
        | RejectionDetail::DefaultsVersionExhausted { .. }
        | RejectionDetail::ImportedConversationNotFound { .. }
        | RejectionDetail::ImportedFrontierPositionOutOfRange { .. } => true,
        RejectionDetail::ConversationImportAlreadyInProgress {}
        | RejectionDetail::ConversationImportNotInProgress {}
        | RejectionDetail::ConversationImportSourceTooLarge { .. }
        | RejectionDetail::ConversationImportSourceSizeMismatch { .. }
        | RejectionDetail::ConversationImportConversionFailed { .. }
        | RejectionDetail::BulkIngestAlreadyInProgress { .. }
        | RejectionDetail::BlobUploadAlreadyInProgress {}
        | RejectionDetail::BlobUploadNotInProgress {}
        | RejectionDetail::BlobUploadLengthOutOfRange { .. }
        | RejectionDetail::BlobUploadSizeExceeded { .. }
        | RejectionDetail::BlobUploadLengthMismatch { .. }
        | RejectionDetail::BlobUploadDigestMismatch { .. }
        | RejectionDetail::BlobReadLengthOutOfRange { .. }
        | RejectionDetail::BlobReadRangeOutOfBounds { .. } => false,
    };
    if valid {
        Ok(())
    } else {
        Err(FrameValidationError::ErrorDetailShape)
    }
}

fn validate_conversation_import_detail(
    detail: RejectionDetail,
) -> Result<(), FrameValidationError> {
    let valid = match detail {
        RejectionDetail::ConversationImportAlreadyInProgress {}
        | RejectionDetail::ConversationImportNotInProgress {} => true,
        RejectionDetail::ConversationImportSourceTooLarge {
            limit_bytes,
            declared_size_bytes,
            actual_size_bytes,
        } => {
            limit_bytes.value() > 0
                && match actual_size_bytes {
                    Some(actual) => {
                        actual.value() > limit_bytes.value()
                            && (declared_size_bytes.value() <= limit_bytes.value()
                                || declared_size_bytes == actual)
                    }
                    None => declared_size_bytes.value() > limit_bytes.value(),
                }
        }
        RejectionDetail::ConversationImportSourceSizeMismatch {
            declared_size_bytes,
            actual_size_bytes,
        } => declared_size_bytes != actual_size_bytes,
        RejectionDetail::ConversationImportConversionFailed {
            class,
            record_ordinal,
        } => match class {
            ConversationImportRejectionClass::EmptySource => record_ordinal.is_none(),
            ConversationImportRejectionClass::BlankLine
            | ConversationImportRejectionClass::InvalidUtf8
            | ConversationImportRejectionClass::InvalidJson
            | ConversationImportRejectionClass::JsonDepthExceeded
            | ConversationImportRejectionClass::TopLevelNotObject
            | ConversationImportRejectionClass::InvalidRecordType
            | ConversationImportRejectionClass::InvalidSourceMetadata
            | ConversationImportRejectionClass::InvalidMessageEnvelope
            | ConversationImportRejectionClass::InvalidMessageRole
            | ConversationImportRejectionClass::MessageRoleMismatch
            | ConversationImportRejectionClass::InvalidMessageContent
            | ConversationImportRejectionClass::InvalidContentBlock
            | ConversationImportRejectionClass::InvalidToolResultBlock
            | ConversationImportRejectionClass::InvalidReasoning
            | ConversationImportRejectionClass::InvalidToolCall
            | ConversationImportRejectionClass::InvalidToolResult => {
                record_ordinal.is_some_and(|ordinal| ordinal.value() > 0)
            }
        },
        RejectionDetail::SessionNotFound { .. }
        | RejectionDetail::AttachmentBlobNotFound { .. }
        | RejectionDetail::AttachmentByteBudgetExceeded { .. }
        | RejectionDetail::UnsupportedReasoningLevel { .. }
        | RejectionDetail::UnsupportedFastMode { .. }
        | RejectionDetail::UnsupportedServiceTier { .. }
        | RejectionDetail::SessionPlacementCurrentVersionMismatch { .. }
        | RejectionDetail::SessionPlacementVersionExhausted { .. }
        | RejectionDetail::GoalCommandRejected { .. }
        | RejectionDetail::SessionLifecycleCommandRejected { .. }
        | RejectionDetail::ActiveTurnPresent { .. }
        | RejectionDetail::CommissionTargetBusy { .. }
        | RejectionDetail::ActiveTurnMismatch { .. }
        | RejectionDetail::NoActiveTurn { .. }
        | RejectionDetail::TurnNotAwaitingReconciliation { .. }
        | RejectionDetail::InterruptAlreadyApplied { .. }
        | RejectionDetail::InterruptUnavailableWhileAwaitingApproval { .. }
        | RejectionDetail::SafePointUnavailableWhileStopping { .. }
        | RejectionDetail::ToolRequestNotFound { .. }
        | RejectionDetail::ToolRequestAlreadyResolved { .. }
        | RejectionDetail::ToolRequestNotEarliestUndecided { .. }
        | RejectionDetail::ToolRequestNotInSession { .. }
        | RejectionDetail::ToolRequestNotDelegateDenied { .. }
        | RejectionDetail::ToolRequestNotTerminallyDenied { .. }
        | RejectionDetail::ToolDenialAlreadyOverridden { .. }
        | RejectionDetail::DelegationRequestNotInTurn { .. }
        | RejectionDetail::DelegationToolRequestNotExecutable { .. }
        | RejectionDetail::DelegationSpawnConflict { .. }
        | RejectionDetail::DelegatedChildIdentityCollision { .. }
        | RejectionDetail::DelegationRelationNotFound { .. }
        | RejectionDetail::DelegationAwaitConflict { .. }
        | RejectionDetail::DelegationMessageConflict { .. }
        | RejectionDetail::DelegationMessageIdentityCollision { .. }
        | RejectionDetail::DelegationEventOrdinalExhausted { .. }
        | RejectionDetail::DelegationDeliverySequenceExhausted { .. }
        | RejectionDetail::DefaultsVersionMismatch { .. }
        | RejectionDetail::UnknownModelAlias { .. }
        | RejectionDetail::AcceptancePositionExhausted { .. }
        | RejectionDetail::DefaultsVersionExhausted { .. }
        | RejectionDetail::ImportedConversationNotFound { .. }
        | RejectionDetail::ImportedFrontierPositionOutOfRange { .. } => false,
        RejectionDetail::BulkIngestAlreadyInProgress { .. }
        | RejectionDetail::BlobUploadAlreadyInProgress {}
        | RejectionDetail::BlobUploadNotInProgress {}
        | RejectionDetail::BlobUploadLengthOutOfRange { .. }
        | RejectionDetail::BlobUploadSizeExceeded { .. }
        | RejectionDetail::BlobUploadLengthMismatch { .. }
        | RejectionDetail::BlobUploadDigestMismatch { .. }
        | RejectionDetail::BlobReadLengthOutOfRange { .. }
        | RejectionDetail::BlobReadRangeOutOfBounds { .. } => false,
    };
    if valid {
        Ok(())
    } else {
        Err(FrameValidationError::ConversationImportShape)
    }
}

fn validate_blob_upload_detail(detail: RejectionDetail) -> Result<(), FrameValidationError> {
    let valid = match detail {
        RejectionDetail::BlobUploadAlreadyInProgress {}
        | RejectionDetail::BlobUploadNotInProgress {} => true,
        RejectionDetail::BlobUploadLengthOutOfRange {
            min_length_bytes,
            max_length_bytes,
            declared_length_bytes,
        } => {
            min_length_bytes.value() > 0
                && min_length_bytes.value() <= max_length_bytes.value()
                && (declared_length_bytes.value() < min_length_bytes.value()
                    || declared_length_bytes.value() > max_length_bytes.value())
        }
        RejectionDetail::BlobUploadSizeExceeded {
            expected_length_bytes,
            actual_length_bytes,
        } => {
            expected_length_bytes.value() > 0
                && actual_length_bytes.value() > expected_length_bytes.value()
        }
        RejectionDetail::BlobUploadLengthMismatch {
            expected_length_bytes,
            actual_length_bytes,
        } => expected_length_bytes.value() > 0 && expected_length_bytes != actual_length_bytes,
        RejectionDetail::BlobUploadDigestMismatch {
            expected_digest,
            actual_digest,
        } => expected_digest != actual_digest,
        _ => false,
    };
    if valid {
        Ok(())
    } else {
        Err(FrameValidationError::BlobUploadShape)
    }
}

fn validate_blob_read_detail(detail: RejectionDetail) -> Result<(), FrameValidationError> {
    let valid = match detail {
        RejectionDetail::BlobReadLengthOutOfRange {
            min_length_bytes,
            max_length_bytes,
            requested_length_bytes,
        } => {
            min_length_bytes.value() == 1
                && max_length_bytes.value() == MAX_BLOB_READ_BYTES as u64
                && (requested_length_bytes.value() < min_length_bytes.value()
                    || requested_length_bytes.value() > max_length_bytes.value())
        }
        RejectionDetail::BlobReadRangeOutOfBounds {
            offset_bytes,
            length_bytes,
            blob_length_bytes,
            ..
        } => {
            (1..=MAX_BLOB_READ_BYTES as u64).contains(&length_bytes.value())
                && blob_length_bytes.value() > 0
                && (offset_bytes
                    .value()
                    .checked_add(length_bytes.value())
                    .is_none_or(|end| end > blob_length_bytes.value()))
        }
        _ => false,
    };
    if valid {
        Ok(())
    } else {
        Err(FrameValidationError::BlobReadShape)
    }
}

/// Decodes and validates one complete client line including its final newline.
pub fn decode_client_line(line: &[u8]) -> Result<ClientFrame, FrameDecodeError> {
    let content = checked_line_content(line, false)?;
    let header = probe_header(content, "request", false)?;
    let frame: ClientFrame = serde_json::from_slice(content)
        .map_err(|_| FrameDecodeError::malformed(header.request_id))?;
    frame
        .validate()
        .map_err(|_| FrameDecodeError::malformed(header.request_id))?;
    Ok(frame)
}

/// Decodes and validates one complete server line including its final newline.
pub fn decode_server_line(line: &[u8]) -> Result<ServerFrame, FrameDecodeError> {
    let content = checked_line_content(line, true)?;
    let header = probe_header(content, "message", true)?;
    let frame: ServerFrame = serde_json::from_slice(content)
        .map_err(|_| FrameDecodeError::malformed(header.request_id))?;
    frame
        .validate()
        .map_err(|_| FrameDecodeError::malformed(header.request_id))?;
    Ok(frame)
}

/// Encodes one validated client frame with its final newline.
pub fn encode_client_line(frame: &ClientFrame) -> Result<Vec<u8>, FrameEncodeError> {
    frame.validate()?;
    encode_line(frame)
}

/// Encodes one validated server frame with its final newline.
pub fn encode_server_line(frame: &ServerFrame) -> Result<Vec<u8>, FrameEncodeError> {
    frame.validate()?;
    encode_line(frame)
}

fn encode_line<T: Serialize>(frame: &T) -> Result<Vec<u8>, FrameEncodeError> {
    let mut encoded = serde_json::to_vec(frame)?;
    encoded.push(b'\n');
    if encoded.len() > MAX_FRAME_BYTES {
        return Err(FrameEncodeError::OversizedFrame);
    }
    Ok(encoded)
}

#[cfg(test)]
mod tests;
