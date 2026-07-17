//! Baseline model-selection configuration.
//!
//! ADR-0027 (`docs/decisions/0027-input-delivery-lifecycle.md`) is the
//! normative specification. The first implementable effective configuration
//! is deliberately model-selection-only: one frozen direct or alias model
//! selection, provider-default parameters, and disabled known-provider-failure
//! retry and model fallback. Custom parameters, instructions, tool
//! enablement, placement constraints, per-turn resources, and
//! interpreting-policy selections are unavailable baseline capabilities, not
//! latent optional fields. The `Scope` section on [`EffectiveConfiguration`]
//! lists what these pure values deliberately omit.

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
///
/// # Scope
///
/// This and the surrounding configuration types are pure values. They omit
/// input acceptance transactions, command deduplication, selection of the
/// current alias definition from mutable state, exact provider/model target
/// resolution, and storage, wire, deployment-key, and display encodings.
/// Aggregate transitions and boundary code own those ADR-0027 requirements.
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

/// Identifies one immutable version of a session's model-selection defaults.
///
/// Session creation establishes version one; each explicit replacement
/// installs the next version. The version belongs to
/// [`OriginConfiguration`] provenance, not effective-value equality.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SessionConfigurationDefaultsVersion(u64);

impl SessionConfigurationDefaultsVersion {
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

/// One complete normalized model-selection default value.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct SessionConfigurationDefaults {
    model: ModelSelectionRequest,
}

impl SessionConfigurationDefaults {
    /// Creates a complete defaults value from its model-selection request.
    pub const fn new(model: ModelSelectionRequest) -> Self {
        Self { model }
    }

    /// Returns the default model-selection request.
    pub const fn model(&self) -> ModelSelectionRequest {
        self.model
    }
}

/// The current immutable version of a session's model-selection defaults.
///
/// Replacement installs a complete later version; it never mutates an
/// existing one. Whether an update affects only subsequently accepted origin
/// input is an aggregate acceptance rule, not a property of this value.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
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

    /// Installs a complete replacement as the next immutable version, or
    /// `None` when the version counter is exhausted.
    pub fn replace(self, defaults: SessionConfigurationDefaults) -> Option<Self> {
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
            request: ConfigurationRequest { model },
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
}

impl ConfigurationRequest {
    /// Returns the requested model selection.
    pub const fn model(&self) -> ModelSelectionRequest {
        self.model
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
}

impl OriginConfiguration {
    /// Freezes provenance by consuming the derived, version-checked request.
    ///
    /// `select_definition` supplies the current immutable definition when the
    /// request names an alias; returning `None` reports the alias as unknown
    /// and freezes nothing. A direct request never invokes it.
    pub fn freeze(
        checked: VersionCheckedConfigurationRequest,
        select_definition: impl FnOnce(ModelAlias) -> Option<FrozenAliasDefinition>,
    ) -> Result<Self, UnknownModelAlias> {
        let VersionCheckedConfigurationRequest {
            request: requested,
            session_defaults_version,
        } = checked;

        let model = match requested.model() {
            ModelSelectionRequest::Direct(selection) => FrozenModelSelection::Direct(selection),
            ModelSelectionRequest::Alias(alias) => match select_definition(alias) {
                Some(definition) => FrozenModelSelection::FrozenAlias { alias, definition },
                None => return Err(UnknownModelAlias { alias }),
            },
        };

        Ok(Self {
            requested,
            session_defaults_version,
            effective: EffectiveConfiguration::baseline(model),
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
    use super::{
        ConfigurationRequest, DirectModelSelection, EffectiveConfiguration, FrozenAliasDefinition,
        FrozenModelSelection, KnownProviderFailureRetry, ModelAlias, ModelFallback,
        ModelParameters, ModelSelectionOverride, ModelSelectionRequest, OriginConfiguration,
        SessionConfigurationDefaults, SessionConfigurationDefaultsVersion,
        SessionDefaultsVersionMismatch, TurnConfigurationProvenance,
        VersionCheckedConfigurationRequest, VersionedSessionConfigurationDefaults,
    };
    use crate::{SteeringBinding, TurnId};
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

    fn defaults(value: u128) -> SessionConfigurationDefaults {
        SessionConfigurationDefaults::new(ModelSelectionRequest::Direct(direct(value)))
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

    #[test]
    fn replacement_at_an_exhausted_version_is_reported_rather_than_panicking() {
        let exhausted = VersionedSessionConfigurationDefaults {
            version: SessionConfigurationDefaultsVersion(u64::MAX),
            defaults: defaults(1),
        };

        assert_eq!(exhausted.replace(defaults(2)), None);
    }

    #[test]
    fn session_creation_establishes_defaults_version_one() {
        let established = VersionedSessionConfigurationDefaults::establish(defaults(1));

        assert_eq!(
            established.version(),
            SessionConfigurationDefaultsVersion::first()
        );
        assert_eq!(established.defaults(), &defaults(1));
    }

    /// INV-008: session model-selection defaults are versioned; a
    /// replacement installs a complete later immutable version.
    #[test]
    fn replacement_installs_the_next_complete_version() {
        let established = VersionedSessionConfigurationDefaults::establish(defaults(1));
        let replaced = established
            .replace(defaults(2))
            .expect("an unexhausted version counter installs the next version");

        assert_eq!(
            replaced.version(),
            SessionConfigurationDefaultsVersion::first()
                .checked_next()
                .expect("the second version is representable")
        );
        assert_ne!(replaced.version(), established.version());
        assert_eq!(replaced.defaults(), &defaults(2));
    }

    #[test]
    fn use_session_default_derives_the_named_default() {
        let current = VersionedSessionConfigurationDefaults::establish(defaults(1));

        let checked = current
            .derive_request(current.version(), ModelSelectionOverride::UseSessionDefault)
            .expect("current expected version derives a request");

        assert_eq!(
            checked.request().model(),
            ModelSelectionRequest::Direct(direct(1))
        );
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

    /// INV-012: `UseSessionDefault` and `ReplaceWith(X)` remain structurally
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
        let current = VersionedSessionConfigurationDefaults::establish(defaults(1))
            .replace(defaults(2))
            .expect("an unexhausted version counter installs the next version");
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
        value: u128,
        current: &VersionedSessionConfigurationDefaults,
    ) -> VersionCheckedConfigurationRequest {
        current
            .derive_request(
                current.version(),
                ModelSelectionOverride::ReplaceWith(ModelSelectionRequest::Direct(direct(value))),
            )
            .expect("current expected version derives a request")
    }

    fn freeze_direct_request(
        value: u128,
        current: &VersionedSessionConfigurationDefaults,
    ) -> OriginConfiguration {
        OriginConfiguration::freeze(checked_direct_request(value, current), |_| None)
            .expect("a direct request freezes without an alias definition")
    }

    /// INV-008: an explicitly configured origin atomically records its
    /// version-checked request, exact defaults version, and effective value.
    #[test]
    fn origin_configuration_freezes_the_derived_direct_request_coherently() {
        let current = VersionedSessionConfigurationDefaults::establish(defaults(1));
        let checked = current
            .derive_request(current.version(), ModelSelectionOverride::UseSessionDefault)
            .expect("current expected version derives a request");

        let origin = OriginConfiguration::freeze(checked, |_| None)
            .expect("a direct request freezes without an alias definition");

        assert_eq!(origin.requested(), checked.request());
        assert_eq!(origin.session_defaults_version(), current.version());
        assert_eq!(
            origin.effective(),
            &EffectiveConfiguration::baseline(FrozenModelSelection::Direct(direct(1)))
        );
    }

    #[test]
    fn origin_configuration_freezes_an_alias_request_with_the_selected_definition() {
        let current = VersionedSessionConfigurationDefaults::establish(defaults(1));
        let definition = FrozenAliasDefinition::selecting(direct(1));
        let checked = current
            .derive_request(
                current.version(),
                ModelSelectionOverride::ReplaceWith(ModelSelectionRequest::Alias(alias(2))),
            )
            .expect("current expected version derives a request");

        let origin = OriginConfiguration::freeze(checked, |requested| {
            assert_eq!(requested, alias(2));
            Some(definition)
        })
        .expect("a selectable alias definition freezes the request");

        assert_eq!(
            origin.requested().model(),
            ModelSelectionRequest::Alias(alias(2))
        );
        assert_eq!(origin.session_defaults_version(), current.version());
        assert_eq!(
            origin.effective(),
            &EffectiveConfiguration::baseline(FrozenModelSelection::FrozenAlias {
                alias: alias(2),
                definition,
            })
        );
    }

    #[test]
    fn an_alias_request_without_a_selectable_definition_freezes_nothing() {
        let current = VersionedSessionConfigurationDefaults::establish(defaults(1));
        let checked = current
            .derive_request(
                current.version(),
                ModelSelectionOverride::ReplaceWith(ModelSelectionRequest::Alias(alias(1))),
            )
            .expect("current expected version derives a request");

        let error = OriginConfiguration::freeze(checked, |_| None)
            .expect_err("an unknown alias cannot freeze provenance");

        assert_eq!(error.alias(), alias(1));
    }

    /// INV-008: the defaults version belongs to provenance, not
    /// effective-value equality.
    #[test]
    fn defaults_version_is_provenance_rather_than_effective_equality() {
        let established = VersionedSessionConfigurationDefaults::establish(defaults(1));
        let replaced = established
            .replace(defaults(1))
            .expect("an unexhausted version counter installs the next version");

        let first = freeze_direct_request(1, &established);
        let later = freeze_direct_request(1, &replaced);

        assert_eq!(first.effective(), later.effective());
        assert_ne!(
            first.session_defaults_version(),
            later.session_defaults_version()
        );
        assert_ne!(first, later);
    }

    /// INV-008: an explicit origin records request, defaults version, and
    /// effective value; reclassified steering carries only its source-turn
    /// binding.
    #[test]
    fn provenance_variants_carry_an_origin_record_or_only_the_binding() {
        let current = VersionedSessionConfigurationDefaults::establish(defaults(1));
        let origin = freeze_direct_request(1, &current);
        let binding = SteeringBinding::new(TurnId::from_uuid(Uuid::from_u128(2)));

        let explicit = TurnConfigurationProvenance::ExplicitOrigin(origin.clone());
        let inherited = TurnConfigurationProvenance::InheritedForReclassifiedSteering(binding);

        assert_ne!(
            explicit,
            TurnConfigurationProvenance::ExplicitOrigin(freeze_direct_request(3, &current))
        );
        match inherited {
            TurnConfigurationProvenance::InheritedForReclassifiedSteering(carried) => {
                assert_eq!(carried, binding);
            }
            TurnConfigurationProvenance::ExplicitOrigin(_) => {
                panic!("reclassified steering carries only its binding");
            }
        }
    }

    #[test]
    fn configuration_request_exposes_its_model_selection() {
        let request = ConfigurationRequest {
            model: ModelSelectionRequest::Direct(direct(1)),
        };

        assert_eq!(request.model(), ModelSelectionRequest::Direct(direct(1)));
    }
}
