//! Baseline model-selection configuration.
//!
//! The normative specification is
//! `docs/spec/configuration-and-credentials.md`. The first implementable
//! effective configuration includes one frozen direct or alias model
//! selection, validated model/session settings, provider-default legacy
//! parameters, and disabled known-provider-failure retry and model fallback.
//! Custom instructions, tool enablement, placement constraints,
//! per-turn resources, and interpreting-policy selections are unavailable
//! baseline capabilities, not latent optional fields. The `Scope` section
//! on [`EffectiveConfiguration`] lists what these pure values deliberately
//! omit. The optional bounded [`SessionSystemPrompt`] lives on the session
//! defaults value, not in per-turn effective configuration: turns freeze
//! only the epoch version and calls read the prompt through it
//! (`docs/spec/sessions-and-transcript.md`).

use core::fmt;

use crate::model_settings::apply_recorded_model_change_adjustments;
use crate::{
    AdjustedModelSettings, DangerousToolAutoApproval, ModelCapabilityCatalog,
    ModelChangeAdjustment, ModelSettingsOverlay, UnsupportedModelSetting, ValidatedModelSettings,
};

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
    /// Names one operator-configured model alias whose definition can change
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

impl FrozenModelSelection {
    /// Returns the exact direct selection whose model identity is frozen.
    pub const fn selected_direct(self) -> DirectModelSelection {
        match self {
            Self::Direct(selection) => selection,
            Self::FrozenAlias { definition, .. } => definition.selected(),
        }
    }
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
/// field: docs/spec/model-call-execution.md pins it as a separate turn
/// fact before the first model call is created.
///
/// # Scope
///
/// This and the surrounding configuration types are pure values. They omit
/// input acceptance transactions, command deduplication, selection of the
/// current alias definition from mutable state, exact provider/model target
/// resolution, and storage, wire, deployment-key, and display encodings.
/// Aggregate transitions and boundary code own those requirements.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct EffectiveConfiguration {
    model: FrozenModelSelection,
    parameters: ModelParameters,
    known_provider_failure_retry: KnownProviderFailureRetry,
    model_fallback: ModelFallback,
    dangerous_tool_auto_approval: DangerousToolAutoApproval,
    model_settings: ValidatedModelSettings,
}

impl EffectiveConfiguration {
    /// Constructs the complete baseline value for a frozen model selection.
    pub const fn baseline(model: FrozenModelSelection) -> Self {
        Self {
            model,
            parameters: ModelParameters::ProviderDefaults,
            known_provider_failure_retry: KnownProviderFailureRetry::Disabled,
            model_fallback: ModelFallback::Disabled,
            dangerous_tool_auto_approval: DangerousToolAutoApproval::Disabled,
            model_settings: ValidatedModelSettings::provider_defaults(),
        }
    }

    /// Constructs the complete value with an explicit dangerous tool posture.
    pub const fn with_dangerous_tool_auto_approval(
        model: FrozenModelSelection,
        dangerous_tool_auto_approval: DangerousToolAutoApproval,
    ) -> Self {
        Self {
            model,
            parameters: ModelParameters::ProviderDefaults,
            known_provider_failure_retry: KnownProviderFailureRetry::Disabled,
            model_fallback: ModelFallback::Disabled,
            dangerous_tool_auto_approval,
            model_settings: ValidatedModelSettings::provider_defaults(),
        }
    }

    /// Constructs the complete value with model settings validated for its
    /// frozen direct selection.
    pub fn with_model_settings(
        model: FrozenModelSelection,
        dangerous_tool_auto_approval: DangerousToolAutoApproval,
        model_settings: ValidatedModelSettings,
    ) -> Option<Self> {
        let validated_for = model_settings.validated_for();
        (validated_for.is_none() || validated_for == Some(model.selected_direct())).then(|| {
            Self::from_validated_model_settings(model, dangerous_tool_auto_approval, model_settings)
        })
    }

    const fn from_validated_model_settings(
        model: FrozenModelSelection,
        dangerous_tool_auto_approval: DangerousToolAutoApproval,
        model_settings: ValidatedModelSettings,
    ) -> Self {
        Self {
            model,
            parameters: ModelParameters::ProviderDefaults,
            known_provider_failure_retry: KnownProviderFailureRetry::Disabled,
            model_fallback: ModelFallback::Disabled,
            dangerous_tool_auto_approval,
            model_settings,
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

    /// Returns the dangerous blanket-auto posture frozen for this turn.
    pub const fn dangerous_tool_auto_approval(&self) -> DangerousToolAutoApproval {
        self.dangerous_tool_auto_approval
    }

    /// Returns the complete validated model settings frozen for this turn.
    pub const fn model_settings(&self) -> ValidatedModelSettings {
        self.model_settings
    }
}

/// Identifies one immutable version of a session's model-selection defaults.
///
/// Session creation establishes version one; each explicit replacement
/// installs the next version. The version belongs to
/// [`OriginConfiguration`] provenance, not effective-value equality.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SessionConfigurationDefaultsVersion(u64);

impl SessionConfigurationDefaultsVersion {
    /// Reconstitutes a version from its positive ordinal value.
    ///
    /// Returns `None` for zero, which is not a version. Storage and protocol
    /// boundaries remain responsible for decoding their own representations
    /// into a `u64` before calling this domain-owned check.
    pub const fn try_from_u64(value: u64) -> Option<Self> {
        if value == 0 { None } else { Some(Self(value)) }
    }

    /// Returns this version's positive ordinal value.
    pub const fn as_u64(self) -> u64 {
        self.0
    }

    /// Returns version one, established by session creation.
    pub const fn first() -> Self {
        Self(1)
    }

    /// Returns the version installed by the next complete replacement, or
    /// `None` when the ordinal counter is exhausted.
    pub const fn checked_next(self) -> Option<Self> {
        match self.0.checked_add(1) {
            Some(next) => Some(Self(next)),
            None => None,
        }
    }
}

/// One exact session-level system prompt.
///
/// Admission rejects empty text, any text containing U+0000 (which
/// PostgreSQL text cannot store). Admitted text is never trimmed, normalized,
/// case-folded, or otherwise rewritten; equality is the exact ordered
/// scalar sequence. Absence of a prompt is `Option::None` on the owning
/// defaults value, never an empty prompt.
#[derive(Clone, Eq, Hash, PartialEq)]
pub struct SessionSystemPrompt(String);

impl fmt::Debug for SessionSystemPrompt {
    /// Renders only the content length: prompt text never reaches logs or
    /// panic output through `{:?}` (mirroring `ImportedText`).
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SessionSystemPrompt")
            .field("utf8_len", &self.0.len())
            .finish_non_exhaustive()
    }
}

impl SessionSystemPrompt {
    /// Checks the admission rules without rewriting the value.
    pub fn try_new(value: String) -> Result<Self, SessionSystemPromptError> {
        let failure = if value.is_empty() {
            Some(SessionSystemPromptFailure::Empty)
        } else if value.contains('\0') {
            Some(SessionSystemPromptFailure::ContainsNull)
        } else {
            None
        };
        match failure {
            Some(failure) => Err(SessionSystemPromptError { value, failure }),
            None => Ok(Self(value)),
        }
    }

    /// Borrows the exact admitted prompt text.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Returns the exact admitted prompt text.
    pub fn into_string(self) -> String {
        self.0
    }
}

/// Why a session system prompt was not admitted.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionSystemPromptFailure {
    /// The prompt was empty; absence is `None`, never empty text.
    Empty,
    /// The prompt contained U+0000.
    ContainsNull,
}

/// Failed system-prompt construction retaining the rejected value.
#[derive(Clone, Eq, PartialEq)]
pub struct SessionSystemPromptError {
    value: String,
    failure: SessionSystemPromptFailure,
}

impl fmt::Debug for SessionSystemPromptError {
    /// Renders the failure and only the rejected content's length, so `{:?}`
    /// never leaks the withheld text `Display` also omits.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SessionSystemPromptError")
            .field("utf8_len", &self.value.len())
            .field("failure", &self.failure)
            .finish_non_exhaustive()
    }
}

impl SessionSystemPromptError {
    /// Borrows the rejected text.
    pub fn value(&self) -> &str {
        &self.value
    }

    /// Returns the admission failure.
    pub const fn failure(&self) -> SessionSystemPromptFailure {
        self.failure
    }

    /// Returns the rejected text and failure.
    pub fn into_parts(self) -> (String, SessionSystemPromptFailure) {
        (self.value, self.failure)
    }
}

impl std::fmt::Display for SessionSystemPromptError {
    /// Renders the failure without the rejected content.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.failure {
            SessionSystemPromptFailure::Empty => {
                write!(f, "a session system prompt cannot be empty")
            }
            SessionSystemPromptFailure::ContainsNull => {
                write!(f, "a session system prompt cannot contain U+0000")
            }
        }
    }
}

impl std::error::Error for SessionSystemPromptError {}

/// One complete normalized model-selection default value.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct SessionConfigurationDefaults {
    model: ModelSelectionRequest,
    dangerous_tool_auto_approval: DangerousToolAutoApproval,
    system_prompt: Option<SessionSystemPrompt>,
    model_settings: ValidatedModelSettings,
}

impl SessionConfigurationDefaults {
    /// Creates a complete defaults value from its model-selection request.
    pub const fn new(model: ModelSelectionRequest) -> Self {
        Self {
            model,
            dangerous_tool_auto_approval: DangerousToolAutoApproval::Disabled,
            system_prompt: None,
            model_settings: ValidatedModelSettings::provider_defaults(),
        }
    }

    /// Creates complete defaults with an explicit dangerous tool posture.
    pub const fn with_dangerous_tool_auto_approval(
        model: ModelSelectionRequest,
        dangerous_tool_auto_approval: DangerousToolAutoApproval,
    ) -> Self {
        Self {
            model,
            dangerous_tool_auto_approval,
            system_prompt: None,
            model_settings: ValidatedModelSettings::provider_defaults(),
        }
    }

    /// Creates a complete defaults value stating every field explicitly.
    pub const fn complete(
        model: ModelSelectionRequest,
        dangerous_tool_auto_approval: DangerousToolAutoApproval,
        system_prompt: Option<SessionSystemPrompt>,
    ) -> Self {
        Self {
            model,
            dangerous_tool_auto_approval,
            system_prompt,
            model_settings: ValidatedModelSettings::provider_defaults(),
        }
    }

    /// Creates complete defaults including validated model settings.
    ///
    /// A direct model can carry only settings validated for that same direct
    /// selection. Provider defaults remain model-independent, while an alias
    /// retains the direct validation identity needed to detect a later
    /// retarget. A defaults epoch cannot carry a per-call contribution.
    pub fn complete_with_model_settings(
        model: ModelSelectionRequest,
        dangerous_tool_auto_approval: DangerousToolAutoApproval,
        system_prompt: Option<SessionSystemPrompt>,
        model_settings: ValidatedModelSettings,
    ) -> Option<Self> {
        let selection_matches = match (model, model_settings.validated_for()) {
            (ModelSelectionRequest::Direct(expected), Some(validated)) => expected == validated,
            (ModelSelectionRequest::Direct(_), None)
            | (ModelSelectionRequest::Alias(_), Some(_) | None) => true,
        };
        let per_call_inherits =
            model_settings.precedence().per_call() == ModelSettingsOverlay::inherit_all();
        if !selection_matches || !per_call_inherits {
            return None;
        }
        Some(Self {
            model,
            dangerous_tool_auto_approval,
            system_prompt,
            model_settings,
        })
    }

    /// Returns the default model-selection request.
    pub const fn model(&self) -> ModelSelectionRequest {
        self.model
    }

    /// Returns the dangerous blanket-auto default.
    pub const fn dangerous_tool_auto_approval(&self) -> DangerousToolAutoApproval {
        self.dangerous_tool_auto_approval
    }

    /// Borrows the optional session system prompt.
    pub const fn system_prompt(&self) -> Option<&SessionSystemPrompt> {
        self.system_prompt.as_ref()
    }

    /// Returns the complete settings snapshot installed in this defaults epoch.
    pub const fn model_settings(&self) -> ValidatedModelSettings {
        self.model_settings
    }
}

/// The current immutable version of a session's model-selection defaults.
///
/// Replacement installs a complete later version; it never mutates an
/// existing one. Whether an update affects only subsequently accepted origin
/// input is an aggregate acceptance rule, not a property of this value.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct VersionedSessionConfigurationDefaults {
    version: SessionConfigurationDefaultsVersion,
    defaults: SessionConfigurationDefaults,
}

impl VersionedSessionConfigurationDefaults {
    /// Establishes version one at session creation.
    pub const fn establish(defaults: SessionConfigurationDefaults) -> Self {
        Self {
            version: SessionConfigurationDefaultsVersion::first(),
            defaults,
        }
    }

    /// Assembles a checked stored version for an owning domain
    /// reconstitution seam.
    ///
    /// This remains crate-private so external boundaries cannot independently
    /// pair a version with a defaults value without the complete aggregate
    /// correlation required by docs/spec/persistence-protocol.md.
    pub(crate) const fn reconstitute(
        version: SessionConfigurationDefaultsVersion,
        defaults: SessionConfigurationDefaults,
    ) -> Self {
        Self { version, defaults }
    }

    /// Installs a complete replacement as the next immutable version, or
    /// `None` when the version counter is exhausted.
    pub fn replace(&self, defaults: SessionConfigurationDefaults) -> Option<Self> {
        Some(Self {
            version: self.version.checked_next()?,
            defaults,
        })
    }

    /// Returns the current version identity.
    pub const fn version(&self) -> SessionConfigurationDefaultsVersion {
        self.version
    }

    /// Borrows the current defaults value.
    pub const fn defaults(&self) -> &SessionConfigurationDefaults {
        &self.defaults
    }

    /// Derives one complete configuration request from the explicit model
    /// override or the named default.
    ///
    /// The caller's expected defaults version must still be current; a
    /// mismatch is an authoritative rejection that cannot silently adopt a
    /// newer version for the same caller payload. The result carries the
    /// exact version it was checked against.
    pub fn derive_request(
        &self,
        expected: SessionConfigurationDefaultsVersion,
        model: ModelSelectionOverride,
    ) -> Result<VersionCheckedConfigurationRequest, SessionDefaultsVersionMismatch> {
        self.derive_request_with_model_settings(
            expected,
            model,
            ModelSettingsOverlay::inherit_all(),
        )
    }

    /// Derives one request while preserving the caller's per-call settings
    /// contribution for capability resolution after alias freezing.
    pub fn derive_request_with_model_settings(
        &self,
        expected: SessionConfigurationDefaultsVersion,
        model: ModelSelectionOverride,
        per_call_model_settings: ModelSettingsOverlay,
    ) -> Result<VersionCheckedConfigurationRequest, SessionDefaultsVersionMismatch> {
        if expected != self.version {
            return Err(SessionDefaultsVersionMismatch {
                expected,
                current: self.version,
            });
        }

        let model = match model {
            ModelSelectionOverride::UseSessionDefault => self.defaults.model(),
            ModelSelectionOverride::ReplaceWith(request) => request,
        };

        Ok(VersionCheckedConfigurationRequest {
            request: ConfigurationRequest {
                model,
                dangerous_tool_auto_approval: self.defaults.dangerous_tool_auto_approval(),
                model_settings: self.defaults.model_settings(),
                per_call_model_settings,
            },
            session_defaults_version: self.version,
        })
    }
}

/// The caller's per-input model-selection choice.
///
/// `UseSessionDefault` and `ReplaceWith(X)` remain structurally distinct
/// even when the current default is `X`, because canonical construction
/// cannot consult mutable aggregate state before command lookup.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ModelSelectionOverride {
    /// Resolve against the session default named by the expected version.
    UseSessionDefault,
    /// Replace the default with an explicit request.
    ReplaceWith(ModelSelectionRequest),
}

/// One complete derived configuration request for an origin input.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ConfigurationRequest {
    model: ModelSelectionRequest,
    dangerous_tool_auto_approval: DangerousToolAutoApproval,
    model_settings: ValidatedModelSettings,
    per_call_model_settings: ModelSettingsOverlay,
}

impl ConfigurationRequest {
    /// Returns the requested model selection.
    pub const fn model(&self) -> ModelSelectionRequest {
        self.model
    }

    /// Returns the dangerous blanket-auto posture derived with this request.
    pub const fn dangerous_tool_auto_approval(&self) -> DangerousToolAutoApproval {
        self.dangerous_tool_auto_approval
    }

    /// Returns the complete model settings derived with this request.
    pub const fn model_settings(&self) -> ValidatedModelSettings {
        self.model_settings
    }

    /// Returns the caller's provenance-preserving per-call contribution.
    pub const fn per_call_model_settings(&self) -> ModelSettingsOverlay {
        self.per_call_model_settings
    }
}

/// A derived configuration request bound to the exact defaults version it
/// was checked against.
///
/// It is constructible only by
/// [`VersionedSessionConfigurationDefaults::derive_request`], so frozen
/// origin provenance can never claim a defaults version that did not
/// validate its request.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct VersionCheckedConfigurationRequest {
    request: ConfigurationRequest,
    session_defaults_version: SessionConfigurationDefaultsVersion,
}

impl VersionCheckedConfigurationRequest {
    /// Borrows the derived configuration request.
    pub const fn request(&self) -> &ConfigurationRequest {
        &self.request
    }

    /// Returns the exact defaults version the request was checked against.
    pub const fn session_defaults_version(&self) -> SessionConfigurationDefaultsVersion {
        self.session_defaults_version
    }
}

/// Reports a caller-expected defaults version that is no longer current.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SessionDefaultsVersionMismatch {
    expected: SessionConfigurationDefaultsVersion,
    current: SessionConfigurationDefaultsVersion,
}

impl SessionDefaultsVersionMismatch {
    /// Returns the version the caller expected to be current.
    pub const fn expected(&self) -> SessionConfigurationDefaultsVersion {
        self.expected
    }

    /// Returns the version that was current instead.
    pub const fn current(&self) -> SessionConfigurationDefaultsVersion {
        self.current
    }
}

/// The complete configuration provenance frozen for one explicitly
/// configured origin turn.
///
/// It is constructible only by consuming a
/// [`VersionCheckedConfigurationRequest`], so the stored request, defaults
/// version, and effective value can neither be cross-wired nor bypass the
/// defaults-version check that produced the request.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct OriginConfiguration {
    requested: ConfigurationRequest,
    session_defaults_version: SessionConfigurationDefaultsVersion,
    effective: EffectiveConfiguration,
    model_settings_adjusted_from: Option<DirectModelSelection>,
    model_settings_adjustments: Box<[ModelChangeAdjustment]>,
}

impl OriginConfiguration {
    /// Freezes provenance by consuming the derived, version-checked request.
    ///
    /// `select_definition` supplies the current immutable definition when the
    /// request names an alias; returning `None` reports the alias as unknown
    /// and freezes nothing. A direct request never invokes it. Settings that
    /// require validation for a different selected model need the
    /// settings-aware path and fail as missing capability evidence here.
    pub fn freeze(
        checked: VersionCheckedConfigurationRequest,
        select_definition: impl FnOnce(ModelAlias) -> Option<FrozenAliasDefinition>,
    ) -> Result<Self, OriginModelSettingsError> {
        let VersionCheckedConfigurationRequest {
            request: requested,
            session_defaults_version,
        } = checked;

        let model = match requested.model() {
            ModelSelectionRequest::Direct(selection) => FrozenModelSelection::Direct(selection),
            ModelSelectionRequest::Alias(alias) => match select_definition(alias) {
                Some(definition) => FrozenModelSelection::FrozenAlias { alias, definition },
                None => {
                    return Err(OriginModelSettingsError::UnknownAlias(UnknownModelAlias {
                        alias,
                    }));
                }
            },
        };
        let selection = model.selected_direct();
        let needs_capabilities = requested.per_call_model_settings()
            != ModelSettingsOverlay::inherit_all()
            || requested
                .model_settings()
                .validated_for()
                .is_some_and(|validated| validated != selection);
        if needs_capabilities {
            return Err(OriginModelSettingsError::MissingCapabilities { selection });
        }

        Ok(Self {
            requested,
            session_defaults_version,
            effective: EffectiveConfiguration::from_validated_model_settings(
                model,
                requested.dangerous_tool_auto_approval(),
                requested.model_settings(),
            ),
            model_settings_adjusted_from: None,
            model_settings_adjustments: Box::new([]),
        })
    }

    /// Freezes selection and resolves per-call settings against the exact
    /// direct model capability record selected for this origin.
    pub fn freeze_with_model_settings(
        checked: VersionCheckedConfigurationRequest,
        select_definition: impl FnOnce(ModelAlias) -> Option<FrozenAliasDefinition>,
        capabilities: &ModelCapabilityCatalog,
    ) -> Result<Self, OriginModelSettingsError> {
        let VersionCheckedConfigurationRequest {
            mut request,
            session_defaults_version,
        } = checked;
        let model = match request.model() {
            ModelSelectionRequest::Direct(selection) => FrozenModelSelection::Direct(selection),
            ModelSelectionRequest::Alias(alias) => match select_definition(alias) {
                Some(definition) => FrozenModelSelection::FrozenAlias { alias, definition },
                None => {
                    return Err(OriginModelSettingsError::UnknownAlias(UnknownModelAlias {
                        alias,
                    }));
                }
            },
        };
        let selection = model.selected_direct();
        let Some(model_capabilities) = capabilities.resolve(selection) else {
            return Err(OriginModelSettingsError::MissingCapabilities { selection });
        };
        let caller_overlay = request.per_call_model_settings;
        let precedence = request
            .model_settings
            .precedence()
            .with_per_call(caller_overlay);
        let prior_validation = request.model_settings.validated_for();
        let model_changed = prior_validation.is_some_and(|validated| validated != selection);
        let (model_settings, adjustments) = if model_changed {
            model_capabilities
                .validate_model_change(selection, precedence, caller_overlay)
                .map(AdjustedModelSettings::into_parts)
                .map_err(OriginModelSettingsError::Unsupported)?
        } else {
            (
                model_capabilities
                    .validate_precedence(selection, precedence)
                    .map_err(OriginModelSettingsError::Unsupported)?,
                Vec::new().into_boxed_slice(),
            )
        };
        let model_settings_adjusted_from = (!adjustments.is_empty())
            .then_some(prior_validation)
            .flatten();
        request.model_settings = model_settings;
        Ok(Self {
            requested: request,
            session_defaults_version,
            effective: EffectiveConfiguration::from_validated_model_settings(
                model,
                request.dangerous_tool_auto_approval(),
                model_settings,
            ),
            model_settings_adjusted_from,
            model_settings_adjustments: adjustments,
        })
    }

    /// Reconstitutes stored settings-aware provenance without consulting a
    /// deployment capability catalog that may have changed since acceptance.
    ///
    /// The stored adjustment sequence is accepted only when it rewrites an
    /// inherited value, preserves the fixed knob order, and reproduces the
    /// complete stored precedence and source evidence exactly.
    pub fn reconstitute_with_model_settings(
        checked: VersionCheckedConfigurationRequest,
        frozen_model: FrozenModelSelection,
        stored_settings: ValidatedModelSettings,
        adjustments: Vec<ModelChangeAdjustment>,
    ) -> Option<Self> {
        let VersionCheckedConfigurationRequest {
            mut request,
            session_defaults_version,
        } = checked;
        if !frozen_selection_matches_request(frozen_model, request.model()) {
            return None;
        }
        let selected = frozen_model.selected_direct();
        if stored_settings
            .validated_for()
            .is_some_and(|validated| validated != selected)
        {
            return None;
        }
        let prior_validation = request.model_settings.validated_for();
        let model_changed = prior_validation.is_some_and(|validated| validated != selected);
        if !adjustments.is_empty() && !model_changed {
            return None;
        }
        let precedence = request
            .model_settings
            .precedence()
            .with_per_call(request.per_call_model_settings);
        let prior = precedence.resolve();
        let adjusted = apply_recorded_model_change_adjustments(prior, &adjustments)?;
        let expected_precedence = precedence.with_effective_adjustment(prior, adjusted);
        if stored_settings.precedence() != expected_precedence
            || stored_settings.resolved() != expected_precedence.resolve()
        {
            return None;
        }
        request.model_settings = stored_settings;
        let model_settings_adjusted_from = (!adjustments.is_empty())
            .then_some(prior_validation)
            .flatten();
        Some(Self {
            requested: request,
            session_defaults_version,
            effective: EffectiveConfiguration::from_validated_model_settings(
                frozen_model,
                request.dangerous_tool_auto_approval(),
                stored_settings,
            ),
            model_settings_adjusted_from,
            model_settings_adjustments: adjustments.into_boxed_slice(),
        })
    }

    /// Borrows the derived configuration request.
    pub const fn requested(&self) -> &ConfigurationRequest {
        &self.requested
    }

    /// Returns the exact defaults version the request was accepted under.
    pub const fn session_defaults_version(&self) -> SessionConfigurationDefaultsVersion {
        self.session_defaults_version
    }

    /// Borrows the complete frozen effective value.
    pub const fn effective(&self) -> &EffectiveConfiguration {
        &self.effective
    }

    /// Returns the prior direct validation identity that caused adjustments.
    pub const fn model_settings_adjusted_from(&self) -> Option<DirectModelSelection> {
        self.model_settings_adjusted_from
    }

    /// Borrows ordered automatic model-change adjustments.
    pub fn model_settings_adjustments(&self) -> &[ModelChangeAdjustment] {
        &self.model_settings_adjustments
    }
}

fn frozen_selection_matches_request(
    frozen: FrozenModelSelection,
    requested: ModelSelectionRequest,
) -> bool {
    match (frozen, requested) {
        (FrozenModelSelection::Direct(stored), ModelSelectionRequest::Direct(requested)) => {
            stored == requested
        }
        (
            FrozenModelSelection::FrozenAlias { alias: stored, .. },
            ModelSelectionRequest::Alias(requested),
        ) => stored == requested,
        (FrozenModelSelection::Direct(_) | FrozenModelSelection::FrozenAlias { .. }, _) => false,
    }
}

/// Why settings-aware origin freezing could not produce durable provenance.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OriginModelSettingsError {
    /// The requested alias has no current immutable definition.
    UnknownAlias(UnknownModelAlias),
    /// The selected direct model has no capability record.
    MissingCapabilities {
        /// Selected direct model.
        selection: DirectModelSelection,
    },
    /// A caller-owned setting is unsupported by the selected model.
    Unsupported(UnsupportedModelSetting),
}

impl fmt::Display for OriginModelSettingsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownAlias(error) => write!(
                formatter,
                "model alias {} has no current definition",
                error.alias().into_uuid()
            ),
            Self::MissingCapabilities { selection } => write!(
                formatter,
                "model selection {} has no capability record",
                selection.into_uuid()
            ),
            Self::Unsupported(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for OriginModelSettingsError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Unsupported(error) => Some(error),
            Self::UnknownAlias(_) | Self::MissingCapabilities { .. } => None,
        }
    }
}

/// Checked stored facts for reconstituting one explicit turn origin.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OriginConfigurationReconstitutionInput {
    defaults_version: SessionConfigurationDefaultsVersion,
    defaults: SessionConfigurationDefaults,
    requested_model: ModelSelectionRequest,
    frozen_model: FrozenModelSelection,
}

impl OriginConfigurationReconstitutionInput {
    /// Binds the immutable defaults epoch to its stored requested and frozen selections.
    pub const fn new(
        defaults_version: SessionConfigurationDefaultsVersion,
        defaults: SessionConfigurationDefaults,
        requested_model: ModelSelectionRequest,
        frozen_model: FrozenModelSelection,
    ) -> Self {
        Self {
            defaults_version,
            defaults,
            requested_model,
            frozen_model,
        }
    }

    /// Reconstitutes only when the stored request and frozen selection derive
    /// from the exact supplied defaults epoch.
    pub fn reconstitute(self) -> Option<OriginConfiguration> {
        let versioned = VersionedSessionConfigurationDefaults::reconstitute(
            self.defaults_version,
            self.defaults,
        );
        let model_override = if versioned.defaults().model() == self.requested_model {
            ModelSelectionOverride::UseSessionDefault
        } else {
            ModelSelectionOverride::ReplaceWith(self.requested_model)
        };
        let checked = versioned
            .derive_request(self.defaults_version, model_override)
            .ok()?;
        if checked.request().model() != self.requested_model {
            return None;
        }
        let frozen_model = self.frozen_model;
        let origin = OriginConfiguration::freeze(checked, |alias| match frozen_model {
            FrozenModelSelection::FrozenAlias {
                alias: stored_alias,
                definition,
            } if stored_alias == alias => Some(definition),
            FrozenModelSelection::Direct(_) | FrozenModelSelection::FrozenAlias { .. } => None,
        })
        .ok()?;
        if origin.effective().model() != &frozen_model {
            return None;
        }
        Some(origin)
    }
}

/// Reports an alias request whose current definition could not be selected.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UnknownModelAlias {
    alias: ModelAlias,
}

impl UnknownModelAlias {
    /// Returns the alias with no selectable definition.
    pub const fn alias(&self) -> ModelAlias {
        self.alias
    }
}

/// How one turn's effective configuration is explained.
///
/// A reclassified-steering origin carries only its source-turn binding; the
/// variant has no configuration or request field, so a different inherited
/// value cannot be supplied. The new origin's effective configuration is set
/// equal to the referenced source turn's canonical value by the aggregate
/// reclassification transition.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum TurnConfigurationProvenance {
    /// The origin recorded its request, defaults version, and effective value.
    ExplicitOrigin(OriginConfiguration),
    /// The origin inherits the canonical value of the bound source turn.
    InheritedForReclassifiedSteering(crate::SteeringBinding),
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::{
        ConfigurationRequest, DirectModelSelection, EffectiveConfiguration, FrozenAliasDefinition,
        FrozenModelSelection, KnownProviderFailureRetry, ModelAlias, ModelFallback,
        ModelParameters, ModelSelectionOverride, ModelSelectionRequest, OriginConfiguration,
        OriginConfigurationReconstitutionInput, OriginModelSettingsError,
        SessionConfigurationDefaults, SessionConfigurationDefaultsVersion,
        SessionDefaultsVersionMismatch, SessionSystemPrompt, SessionSystemPromptFailure,
        TurnConfigurationProvenance, UnknownModelAlias, VersionCheckedConfigurationRequest,
        VersionedSessionConfigurationDefaults,
    };
    use crate::test_support::{alias, direct, turn_id};
    use crate::{
        DangerousToolAutoApproval, FastModeOverlay, FastModeSupport, ModelCapabilities,
        ModelCapabilityCatalog, ModelCapabilityDefinition, ModelChangeAdjustment,
        ModelSettingsOverlay, ModelSettingsPrecedence, ReasoningLevel, SettingOverlay,
        SteeringBinding, ValidatedModelSettings,
    };
    use uuid::Uuid;

    /// S34: the domain retains exact prompt text independently of
    /// deployment policy; empty and U+0000-bearing text is rejected with the
    /// value retained unchanged.
    #[test]
    fn s34_system_prompt_retains_large_exact_utf8_text() {
        let exact = "y".repeat(2 * 1024 * 1024) + "\u{221a}";
        let admitted =
            SessionSystemPrompt::try_new(exact.clone()).expect("large exact text is admitted");
        assert_eq!(admitted.as_str(), exact);
        assert_eq!(admitted.clone().into_string(), exact);

        let empty = SessionSystemPrompt::try_new(String::new())
            .expect_err("absence is None, never empty text");
        assert_eq!(empty.failure(), SessionSystemPromptFailure::Empty);

        let with_null = SessionSystemPrompt::try_new(String::from("a\u{0}b"))
            .expect_err("PostgreSQL text cannot store U+0000");
        assert_eq!(
            with_null.failure(),
            SessionSystemPromptFailure::ContainsNull
        );
        assert_eq!(with_null.into_parts().0, "a\u{0}b");
    }

    /// S34: the complete defaults value carries the optional prompt
    /// in structural equality, so an epoch differing only in its prompt is a
    /// different replacement payload.
    #[test]
    fn s34_defaults_equality_covers_the_system_prompt() {
        let model = ModelSelectionRequest::Direct(direct(1));
        let prompt = SessionSystemPrompt::try_new(String::from("exact session instructions"))
            .expect("test prompt is admissible");
        let promptless = SessionConfigurationDefaults::complete(
            model,
            DangerousToolAutoApproval::Disabled,
            None,
        );
        let prompted = SessionConfigurationDefaults::complete(
            model,
            DangerousToolAutoApproval::Disabled,
            Some(prompt.clone()),
        );

        assert_eq!(promptless, SessionConfigurationDefaults::new(model));
        assert_ne!(prompted, promptless);
        assert_eq!(prompted.system_prompt(), Some(&prompt));
        assert_eq!(promptless.system_prompt(), None);
        assert_eq!(prompted.model(), model);
    }

    /// a direct default cannot carry settings admitted for
    /// a different direct selection.
    #[test]
    fn defaults_reject_crosswired_direct_settings() {
        let validated_selection = direct(1);
        let configured_selection = direct(2);
        let settings = ModelCapabilities::new(
            BTreeSet::from([ReasoningLevel::High]),
            FastModeSupport::Unsupported,
            BTreeSet::new(),
        )
        .validate_precedence(
            validated_selection,
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
        .expect("the fixture level is supported");

        let defaults = SessionConfigurationDefaults::complete_with_model_settings(
            ModelSelectionRequest::Direct(configured_selection),
            DangerousToolAutoApproval::Disabled,
            None,
            settings,
        );

        assert_eq!(defaults, None);
    }

    /// a durable defaults epoch cannot retain a per-call
    /// contribution that belongs only to one origin request.
    #[test]
    fn defaults_reject_per_call_settings_provenance() {
        let selection = direct(1);
        let settings = ModelCapabilities::new(
            BTreeSet::from([ReasoningLevel::High]),
            FastModeSupport::Unsupported,
            BTreeSet::new(),
        )
        .validate_precedence(
            selection,
            ModelSettingsPrecedence::new(
                ModelSettingsOverlay::new(
                    SettingOverlay::Value(ReasoningLevel::High),
                    FastModeOverlay::Inherit,
                    SettingOverlay::Inherit,
                ),
                ModelSettingsOverlay::inherit_all(),
                ModelSettingsOverlay::inherit_all(),
                ModelSettingsOverlay::inherit_all(),
            ),
        )
        .expect("the fixture level is supported");

        let defaults = SessionConfigurationDefaults::complete_with_model_settings(
            ModelSelectionRequest::Direct(selection),
            DangerousToolAutoApproval::Disabled,
            None,
            settings,
        );

        assert_eq!(defaults, None);
    }

    #[test]
    fn selection_keys_expose_their_uuid_values() {
        let selection_uuid = Uuid::from_u128(1);
        let other_selection_uuid = Uuid::from_u128(2);
        let alias_uuid = Uuid::from_u128(3);
        let other_alias_uuid = Uuid::from_u128(4);
        let selection = DirectModelSelection::from_uuid(selection_uuid);
        let other_selection = DirectModelSelection::from_uuid(other_selection_uuid);
        let model_alias = ModelAlias::from_uuid(alias_uuid);
        let other_alias = ModelAlias::from_uuid(other_alias_uuid);

        assert_ne!(selection, other_selection);
        assert_eq!(selection.as_uuid(), &selection_uuid);
        assert_eq!(model_alias.into_uuid(), alias_uuid);
        assert_ne!(model_alias, other_alias);
    }

    #[test]
    fn frozen_alias_definition_selects_exactly_one_direct_selection() {
        let selected = direct(1);
        let other = direct(2);
        let definition = FrozenAliasDefinition::selecting(selected);
        let other_definition = FrozenAliasDefinition::selecting(other);

        assert_eq!(definition.selected(), selected);
        assert_ne!(definition, other_definition);
    }

    /// S37: alias retargeting resolves the caller overlay
    /// against the new direct capability and retains its automatic adjustment.
    #[test]
    fn s37_alias_retarget_freezes_adjusted_origin_settings() {
        let prior_selection = direct(1);
        let installed_selection = direct(2);
        let prior_capabilities = ModelCapabilities::new(
            BTreeSet::from([ReasoningLevel::Minimal]),
            FastModeSupport::Unsupported,
            BTreeSet::new(),
        );
        let prior_settings = prior_capabilities
            .validate_precedence(
                prior_selection,
                ModelSettingsPrecedence::new(
                    ModelSettingsOverlay::inherit_all(),
                    ModelSettingsOverlay::new(
                        SettingOverlay::Value(ReasoningLevel::Minimal),
                        FastModeOverlay::Inherit,
                        SettingOverlay::Inherit,
                    ),
                    ModelSettingsOverlay::inherit_all(),
                    ModelSettingsOverlay::inherit_all(),
                ),
            )
            .expect("the prior model supports the stored level");
        let defaults = SessionConfigurationDefaults::complete_with_model_settings(
            ModelSelectionRequest::Alias(alias(1)),
            DangerousToolAutoApproval::Disabled,
            None,
            prior_settings,
        )
        .expect("an alias retains its prior direct validation identity");
        let versioned = VersionedSessionConfigurationDefaults::establish(defaults);
        let checked = versioned
            .derive_request_with_model_settings(
                versioned.version(),
                ModelSelectionOverride::UseSessionDefault,
                ModelSettingsOverlay::inherit_all(),
            )
            .expect("the fixture names the current defaults epoch");
        let installed_capabilities = ModelCapabilities::new(
            BTreeSet::from([ReasoningLevel::Medium]),
            FastModeSupport::Unsupported,
            BTreeSet::new(),
        );
        let catalog =
            ModelCapabilityCatalog::try_from_definitions([ModelCapabilityDefinition::new(
                installed_selection,
                installed_capabilities,
            )])
            .expect("the fixture catalog has one selection");

        let origin = OriginConfiguration::freeze_with_model_settings(
            checked,
            |_| Some(FrozenAliasDefinition::selecting(installed_selection)),
            &catalog,
        )
        .expect("inherited incompatibility adjusts during alias freezing");

        assert_eq!(
            origin
                .effective()
                .model_settings()
                .effective()
                .reasoning_level(),
            Some(ReasoningLevel::Medium)
        );
        assert_eq!(
            origin.model_settings_adjustments(),
            [ModelChangeAdjustment::ReasoningLevelClamped {
                from: ReasoningLevel::Minimal,
                to: ReasoningLevel::Medium,
            }]
        );
        assert_eq!(origin.model_settings_adjusted_from(), Some(prior_selection));
        assert_eq!(
            origin.requested().per_call_model_settings(),
            ModelSettingsOverlay::inherit_all()
        );
        let reconstituted = OriginConfiguration::reconstitute_with_model_settings(
            checked,
            FrozenModelSelection::FrozenAlias {
                alias: alias(1),
                definition: FrozenAliasDefinition::selecting(installed_selection),
            },
            origin.effective().model_settings(),
            origin.model_settings_adjustments().to_vec(),
        )
        .expect("stored resolution evidence reconstructs without the live catalog");
        assert_eq!(reconstituted, origin);
    }

    /// adjustment evidence is impossible when the stored
    /// settings were already validated for the unchanged direct selection.
    #[test]
    fn origin_rejects_adjustment_without_a_model_change() {
        let selection = direct(1);
        let capabilities = ModelCapabilities::new(
            BTreeSet::from([ReasoningLevel::Low, ReasoningLevel::High]),
            FastModeSupport::Unsupported,
            BTreeSet::new(),
        );
        let high_precedence = ModelSettingsPrecedence::new(
            ModelSettingsOverlay::inherit_all(),
            ModelSettingsOverlay::new(
                SettingOverlay::Value(ReasoningLevel::High),
                FastModeOverlay::Inherit,
                SettingOverlay::Inherit,
            ),
            ModelSettingsOverlay::inherit_all(),
            ModelSettingsOverlay::inherit_all(),
        );
        let high = capabilities
            .validate_precedence(selection, high_precedence)
            .expect("the fixture level is supported");
        let defaults = SessionConfigurationDefaults::complete_with_model_settings(
            ModelSelectionRequest::Direct(selection),
            DangerousToolAutoApproval::Disabled,
            None,
            high,
        )
        .expect("the settings were validated for the direct default");
        let versioned = VersionedSessionConfigurationDefaults::establish(defaults);
        let checked = versioned
            .derive_request_with_model_settings(
                versioned.version(),
                ModelSelectionOverride::UseSessionDefault,
                ModelSettingsOverlay::inherit_all(),
            )
            .expect("the fixture names the current defaults epoch");
        let low_precedence = high_precedence.with_effective_adjustment(
            high_precedence.resolve(),
            crate::EffectiveModelSettings::new(
                Some(ReasoningLevel::Low),
                crate::FastMode::Disabled,
                None,
            ),
        );
        let low = capabilities
            .validate_precedence(selection, low_precedence)
            .expect("the fixture level is supported");

        let reconstituted = OriginConfiguration::reconstitute_with_model_settings(
            checked,
            FrozenModelSelection::Direct(selection),
            low,
            vec![ModelChangeAdjustment::ReasoningLevelClamped {
                from: ReasoningLevel::High,
                to: ReasoningLevel::Low,
            }],
        );

        assert_eq!(reconstituted, None);
    }

    /// model-independent provider defaults remain valid
    /// durable evidence when an origin is reconstituted.
    #[test]
    fn origin_reconstitutes_provider_default_settings() {
        let selection = direct(1);
        let defaults = SessionConfigurationDefaults::new(ModelSelectionRequest::Direct(selection));
        let versioned = VersionedSessionConfigurationDefaults::establish(defaults);
        let checked = versioned
            .derive_request_with_model_settings(
                versioned.version(),
                ModelSelectionOverride::UseSessionDefault,
                ModelSettingsOverlay::inherit_all(),
            )
            .expect("the fixture names the current defaults epoch");
        let expected = OriginConfiguration::freeze(checked, |_| None)
            .expect("a direct model does not require alias resolution");

        let reconstituted = OriginConfiguration::reconstitute_with_model_settings(
            checked,
            FrozenModelSelection::Direct(selection),
            ValidatedModelSettings::provider_defaults(),
            Vec::new(),
        );

        assert_eq!(reconstituted, Some(expected));
    }

    /// comparison uses constructible semantic values; a direct
    /// request and an alias request remain distinct.
    #[test]
    fn direct_and_alias_requests_remain_semantically_distinct() {
        assert_ne!(
            ModelSelectionRequest::Direct(direct(1)),
            ModelSelectionRequest::Alias(alias(1))
        );
    }

    /// direct and alias selections remain semantically unequal even
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

    /// alias provenance is part of the frozen selection's semantic
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
        let selection = FrozenModelSelection::Direct(direct(1));
        let configuration = EffectiveConfiguration::baseline(selection);

        assert_eq!(configuration.model(), &selection);
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

    /// a complete effective configuration rejects settings
    /// validated for another frozen direct model while retaining canonical,
    /// model-independent provider defaults.
    #[test]
    fn effective_configuration_rejects_crosswired_model_settings() {
        let validated_selection = direct(1);
        let frozen_selection = FrozenModelSelection::Direct(direct(2));
        let level = ReasoningLevel::High;
        let settings = ModelCapabilities::new(
            BTreeSet::from([level]),
            FastModeSupport::Unsupported,
            BTreeSet::new(),
        )
        .validate_precedence(
            validated_selection,
            ModelSettingsPrecedence::new(
                ModelSettingsOverlay::inherit_all(),
                ModelSettingsOverlay::new(
                    SettingOverlay::Value(level),
                    FastModeOverlay::Inherit,
                    SettingOverlay::Inherit,
                ),
                ModelSettingsOverlay::inherit_all(),
                ModelSettingsOverlay::inherit_all(),
            ),
        )
        .expect("the fixture setting is supported by its validating model");

        let crosswired = EffectiveConfiguration::with_model_settings(
            frozen_selection,
            DangerousToolAutoApproval::Disabled,
            settings,
        );
        let provider_defaults = EffectiveConfiguration::with_model_settings(
            frozen_selection,
            DangerousToolAutoApproval::Disabled,
            ValidatedModelSettings::provider_defaults(),
        );

        assert_eq!(crosswired, None);
        assert_eq!(
            provider_defaults,
            Some(EffectiveConfiguration::baseline(frozen_selection))
        );
    }

    /// configuration equality is structural semantic value equality
    /// over the frozen model selection and the unit policy values.
    #[test]
    fn effective_configuration_equality_is_structural_over_the_frozen_selection() {
        let selected = direct(2);
        let selection = FrozenModelSelection::FrozenAlias {
            alias: alias(1),
            definition: FrozenAliasDefinition::selecting(selected),
        };
        let configuration = EffectiveConfiguration::baseline(selection);
        let same_configuration = EffectiveConfiguration::baseline(selection);
        let direct_configuration =
            EffectiveConfiguration::baseline(FrozenModelSelection::Direct(selected));

        assert_eq!(configuration, same_configuration);
        assert_ne!(configuration, direct_configuration);
    }

    fn defaults(value: u128) -> SessionConfigurationDefaults {
        SessionConfigurationDefaults::new(ModelSelectionRequest::Direct(direct(value)))
    }

    fn canonical_defaults() -> SessionConfigurationDefaults {
        defaults(1)
    }

    /// Canonical current defaults for tests whose behavior does not depend on
    /// the configured direct selection.
    fn current_defaults() -> VersionedSessionConfigurationDefaults {
        VersionedSessionConfigurationDefaults::establish(canonical_defaults())
    }

    /// Canonical version-two defaults for tests whose behavior depends only
    /// on having advanced beyond the first version.
    fn second_version_defaults() -> VersionedSessionConfigurationDefaults {
        current_defaults()
            .replace(canonical_defaults())
            .expect("an unexhausted version counter installs the next version")
    }

    #[test]
    fn defaults_version_successor_is_checked_instead_of_panicking_at_exhaustion() {
        let first = SessionConfigurationDefaultsVersion::first();
        let second = first
            .checked_next()
            .expect("the second version is representable");

        assert!(first < second);
        assert_eq!(
            SessionConfigurationDefaultsVersion(u64::MAX).checked_next(),
            None
        );
    }

    /// reconstitution accepts the complete positive `u64` domain and
    /// rejects the zero sentinel without admitting a storage representation.
    #[test]
    fn defaults_version_checked_u64_boundary() {
        assert_eq!(SessionConfigurationDefaultsVersion::try_from_u64(0), None);
        assert_eq!(
            SessionConfigurationDefaultsVersion::try_from_u64(1),
            Some(SessionConfigurationDefaultsVersion::first())
        );

        let maximum =
            SessionConfigurationDefaultsVersion::try_from_u64(u64::MAX).expect("positive maximum");
        assert_eq!(maximum.as_u64(), u64::MAX);
    }

    #[test]
    fn replacement_at_an_exhausted_version_is_reported_rather_than_panicking() {
        let exhausted = VersionedSessionConfigurationDefaults {
            version: SessionConfigurationDefaultsVersion(u64::MAX),
            defaults: canonical_defaults(),
        };

        assert_eq!(exhausted.replace(canonical_defaults()), None);
    }

    #[test]
    fn session_creation_establishes_defaults_version_one() {
        let initial = defaults(1);
        let established = VersionedSessionConfigurationDefaults::establish(initial.clone());

        assert_eq!(
            established.version(),
            SessionConfigurationDefaultsVersion::first()
        );
        assert_eq!(established.defaults(), &initial);
    }

    /// session model-selection defaults are versioned; a
    /// replacement installs a complete later immutable version.
    #[test]
    fn replacement_installs_the_next_complete_version() {
        let initial = defaults(1);
        let replacement = defaults(2);
        let established_version = SessionConfigurationDefaultsVersion::first();
        let replacement_version = SessionConfigurationDefaultsVersion(2);
        let established = VersionedSessionConfigurationDefaults::establish(initial);
        let replaced = established
            .replace(replacement.clone())
            .expect("an unexhausted version counter installs the next version");

        assert_eq!(established.version(), established_version);
        assert_eq!(replaced.version(), replacement_version);
        assert_ne!(replaced.version(), established.version());
        assert_eq!(replaced.defaults(), &replacement);
    }

    #[test]
    fn use_session_default_derives_the_named_default() {
        let named_default = ModelSelectionRequest::Direct(direct(1));
        let current = VersionedSessionConfigurationDefaults::establish(
            SessionConfigurationDefaults::new(named_default),
        );

        let checked = current
            .derive_request(current.version(), ModelSelectionOverride::UseSessionDefault)
            .expect("current expected version derives a request");

        assert_eq!(checked.request().model(), named_default);
        assert_eq!(checked.session_defaults_version(), current.version());
    }

    #[test]
    fn replace_with_derives_the_explicit_request() {
        let current = VersionedSessionConfigurationDefaults::establish(defaults(1));
        let explicit = ModelSelectionRequest::Alias(alias(2));

        let checked = current
            .derive_request(
                current.version(),
                ModelSelectionOverride::ReplaceWith(explicit),
            )
            .expect("current expected version derives a request");

        assert_eq!(checked.request().model(), explicit);
        assert_eq!(checked.session_defaults_version(), current.version());
    }

    /// `UseSessionDefault` and `ReplaceWith(X)` remain structurally
    /// distinct comparison payloads even when the current default is `X`.
    #[test]
    fn override_payloads_stay_distinct_even_when_they_derive_equal_requests() {
        let current = VersionedSessionConfigurationDefaults::establish(defaults(1));
        let use_default = ModelSelectionOverride::UseSessionDefault;
        let replace_with_default = ModelSelectionOverride::ReplaceWith(current.defaults().model());

        assert_ne!(use_default, replace_with_default);
        assert_eq!(
            current.derive_request(current.version(), use_default),
            current.derive_request(current.version(), replace_with_default)
        );
    }

    #[test]
    fn stale_expected_version_is_an_authoritative_rejection() {
        let current = second_version_defaults();
        let stale = SessionConfigurationDefaultsVersion::first();

        let error = current
            .derive_request(stale, ModelSelectionOverride::UseSessionDefault)
            .expect_err("a stale expected version cannot derive a request");

        assert_eq!(error.expected(), stale);
        assert_eq!(error.current(), current.version());
        assert_eq!(
            error,
            SessionDefaultsVersionMismatch {
                expected: stale,
                current: current.version(),
            }
        );
    }

    fn checked_direct_request(
        selection: DirectModelSelection,
        current: &VersionedSessionConfigurationDefaults,
    ) -> VersionCheckedConfigurationRequest {
        current
            .derive_request(
                current.version(),
                ModelSelectionOverride::ReplaceWith(ModelSelectionRequest::Direct(selection)),
            )
            .expect("current expected version derives a request")
    }

    fn freeze_direct_request(
        selection: DirectModelSelection,
        current: &VersionedSessionConfigurationDefaults,
    ) -> OriginConfiguration {
        OriginConfiguration::freeze(checked_direct_request(selection, current), |_| None)
            .expect("a direct request freezes without an alias definition")
    }

    /// an explicitly configured origin atomically records its
    /// version-checked request, exact defaults version, and effective value.
    #[test]
    fn origin_configuration_freezes_the_derived_direct_request_coherently() {
        let selection = direct(1);
        let named_default = ModelSelectionRequest::Direct(selection);
        let current = VersionedSessionConfigurationDefaults::establish(
            SessionConfigurationDefaults::new(named_default),
        );
        let current_version = current.version();
        let expected_effective =
            EffectiveConfiguration::baseline(FrozenModelSelection::Direct(selection));
        let checked = current
            .derive_request(current_version, ModelSelectionOverride::UseSessionDefault)
            .expect("current expected version derives a request");

        let origin = OriginConfiguration::freeze(checked, |_| None)
            .expect("a direct request freezes without an alias definition");

        assert_eq!(origin.requested(), checked.request());
        assert_eq!(origin.session_defaults_version(), current_version);
        assert_eq!(origin.effective(), &expected_effective);
    }

    #[test]
    fn stored_origin_configuration_reconstitutes_from_its_exact_defaults_epoch() {
        let current = current_defaults();
        let defaults_version = current.version();
        let defaults = current.defaults().clone();
        let requested = defaults.model();
        let frozen = FrozenModelSelection::Direct(direct(1));
        let expected = OriginConfiguration::freeze(
            current
                .derive_request(defaults_version, ModelSelectionOverride::UseSessionDefault)
                .expect("the exact defaults version derives"),
            |_| None,
        )
        .expect("the direct default freezes");

        let reconstituted = OriginConfigurationReconstitutionInput::new(
            defaults_version,
            defaults,
            requested,
            frozen,
        )
        .reconstitute();

        assert_eq!(reconstituted, Some(expected));
    }

    #[test]
    fn stored_origin_configuration_rejects_a_frozen_model_not_derived_from_defaults() {
        let current = current_defaults();
        let reconstituted = OriginConfigurationReconstitutionInput::new(
            current.version(),
            current.defaults().clone(),
            current.defaults().model(),
            FrozenModelSelection::Direct(direct(2)),
        )
        .reconstitute();

        assert_eq!(reconstituted, None);
    }

    #[test]
    fn stored_origin_configuration_restores_an_explicit_model_override() {
        let current = current_defaults();
        let requested = ModelSelectionRequest::Direct(direct(2));
        let frozen = FrozenModelSelection::Direct(direct(2));
        let expected = OriginConfiguration::freeze(
            current
                .derive_request(
                    current.version(),
                    ModelSelectionOverride::ReplaceWith(requested),
                )
                .expect("the exact defaults version derives an override"),
            |_| None,
        )
        .expect("the direct override freezes");

        let reconstituted = OriginConfigurationReconstitutionInput::new(
            current.version(),
            current.defaults().clone(),
            requested,
            frozen,
        )
        .reconstitute();

        assert_eq!(reconstituted, Some(expected));
    }

    #[test]
    fn origin_configuration_freezes_an_alias_request_with_the_selected_definition() {
        let current = current_defaults();
        let current_version = current.version();
        let requested_alias = alias(2);
        let request = ModelSelectionRequest::Alias(requested_alias);
        let selected = direct(3);
        let definition = FrozenAliasDefinition::selecting(selected);
        let frozen_selection = FrozenModelSelection::FrozenAlias {
            alias: requested_alias,
            definition,
        };
        let expected_effective = EffectiveConfiguration::baseline(frozen_selection);
        let checked = current
            .derive_request(
                current_version,
                ModelSelectionOverride::ReplaceWith(request),
            )
            .expect("current expected version derives a request");

        let origin = OriginConfiguration::freeze(checked, |requested| {
            assert_eq!(requested, requested_alias);
            Some(definition)
        })
        .expect("a selectable alias definition freezes the request");

        assert_eq!(origin.requested().model(), request);
        assert_eq!(origin.session_defaults_version(), current_version);
        assert_eq!(origin.effective(), &expected_effective);
    }

    #[test]
    fn an_alias_request_without_a_selectable_definition_freezes_nothing() {
        let current = current_defaults();
        let requested_alias = alias(1);
        let request = ModelSelectionRequest::Alias(requested_alias);
        let checked = current
            .derive_request(
                current.version(),
                ModelSelectionOverride::ReplaceWith(request),
            )
            .expect("current expected version derives a request");

        let error = OriginConfiguration::freeze(checked, |_| None)
            .expect_err("an unknown alias cannot freeze provenance");

        assert_eq!(
            error,
            OriginModelSettingsError::UnknownAlias(UnknownModelAlias {
                alias: requested_alias,
            })
        );
    }

    /// S37: the catalog-free origin path cannot preserve
    /// settings admitted for an alias's prior direct target after retargeting.
    #[test]
    fn s37_legacy_freeze_rejects_alias_retarget_settings() {
        let prior_selection = direct(1);
        let installed_selection = direct(2);
        let requested_alias = alias(1);
        let settings = ModelCapabilities::new(
            BTreeSet::from([ReasoningLevel::High]),
            FastModeSupport::Unsupported,
            BTreeSet::new(),
        )
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
        .expect("the prior direct target supports the fixture setting");
        let defaults = SessionConfigurationDefaults::complete_with_model_settings(
            ModelSelectionRequest::Alias(requested_alias),
            DangerousToolAutoApproval::Disabled,
            None,
            settings,
        )
        .expect("alias defaults retain their validation identity");
        let versioned = VersionedSessionConfigurationDefaults::establish(defaults);
        let checked = versioned
            .derive_request(
                versioned.version(),
                ModelSelectionOverride::UseSessionDefault,
            )
            .expect("the fixture names the current defaults epoch");

        let error = OriginConfiguration::freeze(checked, |_| {
            Some(FrozenAliasDefinition::selecting(installed_selection))
        })
        .expect_err("retargeted settings require exact capability evidence");

        assert_eq!(
            error,
            OriginModelSettingsError::MissingCapabilities {
                selection: installed_selection,
            }
        );
    }

    /// S37: legacy reconstitution rejects an alias whose
    /// frozen target differs from the stored settings validation identity.
    #[test]
    fn s37_legacy_reconstitution_rejects_alias_retarget_settings() {
        let prior_selection = direct(1);
        let installed_selection = direct(2);
        let requested_alias = alias(1);
        let settings = ModelCapabilities::new(
            BTreeSet::from([ReasoningLevel::High]),
            FastModeSupport::Unsupported,
            BTreeSet::new(),
        )
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
        .expect("the prior direct target supports the fixture setting");
        let requested_model = ModelSelectionRequest::Alias(requested_alias);
        let defaults = SessionConfigurationDefaults::complete_with_model_settings(
            requested_model,
            DangerousToolAutoApproval::Disabled,
            None,
            settings,
        )
        .expect("alias defaults retain their validation identity");
        let versioned = VersionedSessionConfigurationDefaults::establish(defaults);

        let reconstituted = OriginConfigurationReconstitutionInput::new(
            versioned.version(),
            versioned.defaults().clone(),
            requested_model,
            FrozenModelSelection::FrozenAlias {
                alias: requested_alias,
                definition: FrozenAliasDefinition::selecting(installed_selection),
            },
        )
        .reconstitute();

        assert_eq!(reconstituted, None);
    }

    /// the defaults version belongs to provenance, not
    /// effective-value equality.
    #[test]
    fn defaults_version_is_provenance_rather_than_effective_equality() {
        let established = current_defaults();
        let replaced = established
            .replace(canonical_defaults())
            .expect("an unexhausted version counter installs the next version");
        let selection = direct(1);

        let first = freeze_direct_request(selection, &established);
        let later = freeze_direct_request(selection, &replaced);

        assert_eq!(first.effective(), later.effective());
        assert_ne!(
            first.session_defaults_version(),
            later.session_defaults_version()
        );
        assert_ne!(first, later);
    }

    /// an explicit origin records request, defaults version, and
    /// effective value; reclassified steering carries only its source-turn
    /// binding.
    #[test]
    fn provenance_variants_carry_an_origin_record_or_only_the_binding() {
        let current = current_defaults();
        let origin = freeze_direct_request(direct(1), &current);
        let other_origin = freeze_direct_request(direct(3), &current);
        let binding = SteeringBinding::new(turn_id(2));

        let explicit = TurnConfigurationProvenance::ExplicitOrigin(origin);
        let inherited = TurnConfigurationProvenance::InheritedForReclassifiedSteering(binding);

        assert_ne!(
            explicit,
            TurnConfigurationProvenance::ExplicitOrigin(other_origin)
        );
        let TurnConfigurationProvenance::InheritedForReclassifiedSteering(carried) = inherited
        else {
            panic!("reclassified steering carries only its binding");
        };
        assert_eq!(carried, binding);
    }

    #[test]
    fn configuration_request_exposes_its_model_selection() {
        let model = ModelSelectionRequest::Direct(direct(1));
        let request = ConfigurationRequest {
            model,
            dangerous_tool_auto_approval: DangerousToolAutoApproval::Disabled,
            model_settings: ValidatedModelSettings::provider_defaults(),
            per_call_model_settings: ModelSettingsOverlay::inherit_all(),
        };

        assert_eq!(request.model(), model);
    }
}
