//! Deployment-owned model mappings and credential delivery.

mod billing;
#[cfg(test)]
mod checked_in_example;
mod credential_files;
mod error;
mod model_routing;
mod model_settings;
mod numeric_bounds;
mod repository_watch;
mod startup;
#[cfg(test)]
pub(crate) mod tests;
mod toml_scalars;
mod tool_settings;

use crate::{
    blob_storage_configuration::BlobStorageConfiguration,
    credential_pools::{
        CredentialDelivery, CredentialPool, CredentialPoolTrigger, CredentialProfile,
        parse_credential_pools, parse_credential_profiles,
    },
};
use billing::{fold_reported_cost, parse_model_billing_rates};
pub use credential_files::FileCredentialAccess;
use credential_files::resolved_mcp_bridge_reference;
#[cfg(test)]
use credential_files::{absolute_search_entries, credential_bytes};
pub use error::{HubModelConfigurationError, UnknownSessionModel};
pub(crate) use model_routing::ModelCallInputUsage;
pub use model_routing::{
    ANTHROPIC_CREDENTIAL_REFERENCE, AvailabilityCause, BillingKind,
    CLAUDE_CLI_CREDENTIAL_REFERENCE, CODEX_CLI_CREDENTIAL_REFERENCE, ClaudeCliConfiguration,
    CodexCliConfiguration, DerivedModelCallCost, ModelAdapter, ModelBillingRates,
    OPENAI_CREDENTIAL_REFERENCE, ResolvedModelRoute,
};
use model_routing::{
    AdapterMapping, MIGRATED_ANTHROPIC_MODEL_FAMILY, runtime_pool_action, runtime_pool_exhaustion,
};
use model_settings::{
    RuntimeCapabilityProjection, parse_model_capabilities, parse_model_settings_overlay,
    parse_model_settings_profiles, parse_provider_compaction_capability,
    project_runtime_model_capabilities, record_reasoning_replay_family,
    validate_adapter_model_settings,
};
pub use numeric_bounds::NumericBoundsConfiguration;
#[cfg(test)]
use numeric_bounds::parse_numeric_bound_duration;
#[cfg(test)]
use repository_watch::DEFAULT_REPOSITORY_WATCH_WEBHOOK_BIND_ADDRESS;
use repository_watch::parse_repository_watch_configuration;
pub use repository_watch::{
    ConvergenceSweepConfiguration, RepositoryWatchConfiguration, RepositoryWatchWebhookMode,
    WatchedRepositoryConfiguration,
};
use signalbox_domain::{
    DirectModelSelection, FastMode, FastModeSupport, FrozenAliasDefinition, ModelAlias,
    ModelCapabilityCatalog, ModelCapabilityDefinition, ModelSelectionRequest, ModelSettingsOverlay,
    ModelSettingsPrecedence, ModelTargetCatalog, ModelTargetDefinition, ProviderModelIdentity,
    ResolvedProviderTarget, ToolApprovalPosture, ToolName, UnsupportedModelSetting,
    ValidatedModelSettings,
};
use signalbox_model_provider_runtime::{RuntimeModelCatalog, RuntimeModelDefinition};
use signalbox_model_runtime::{
    CredentialReference, ModelCapabilityCatalog as RuntimeModelCapabilityCatalog,
};
use signalbox_model_runtime_claude_cli::{
    ClaudeCliConfig, ClaudeCliConstructionError, ClaudeCliRuntime,
};
use signalbox_model_runtime_codex_cli::{
    CodexCliConfig, CodexCliConstructionError, CodexCliRuntime,
};
use signalbox_persistence::{
    ModelCredentialFamilyCatalog, SessionCredentialPin, SessionModelCredential,
    model_execution::{
        CredentialPoolRuntimeCatalog, CredentialPoolRuntimeMember, CredentialPoolRuntimePolicy,
        CredentialPoolRuntimeTieBreak, ToolContinuationUsageLimit,
    },
    process_read::ProcessModelCallInputTokenSemantics,
};
use signalbox_tools_web::WebFetchEgressPolicy;
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    str::FromStr,
    sync::Arc,
    time::Duration,
};
use toml_edit::DocumentMut;
use toml_scalars::{
    parse_positive_u32_inline_map, required_positive_u32, required_uuid, validate_alias_count,
    validate_model_count,
};
pub(crate) use toml_scalars::{reject_unknown_fields, required_string, validated_name};
pub use tool_settings::{
    DEFAULT_CONVERSATION_IMPORT_MAX_SOURCE_BYTES, DaemonToolConfiguration,
    MAX_COMPACTION_PROMPT_UTF8_BYTES, WorkspaceInstructionConfiguration,
};
use tool_settings::{
    parse_approval_judge, parse_daemon_tool_settings, parse_git_identity,
    parse_tool_approval_postures, parse_tool_mappings, parse_workspace_instruction_configuration,
};

#[derive(Clone)]
struct CheckedConfigurationSource(Arc<str>);

impl std::fmt::Debug for CheckedConfigurationSource {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("CheckedConfigurationSource(<redacted>)")
    }
}

/// Validated static model and alias definitions used by hub composition.
#[derive(Clone, Debug)]
pub struct HubModelConfiguration {
    source: CheckedConfigurationSource,
    numeric_bounds: NumericBoundsConfiguration,
    targets: ModelTargetCatalog,
    runtime_models: RuntimeModelCatalog,
    tool_continuation_usage_limits: Vec<ToolContinuationUsageLimit>,
    direct_selections: HashSet<DirectModelSelection>,
    aliases: HashMap<ModelAlias, FrozenAliasDefinition>,
    routes: HashMap<DirectModelSelection, ResolvedModelRoute>,
    credential_profiles: HashMap<Arc<str>, CredentialProfile>,
    credential_pools: HashMap<Arc<str>, CredentialPool>,
    model_capabilities: ModelCapabilityCatalog,
    runtime_model_capabilities: RuntimeModelCapabilityCatalog,
    model_settings_lower_layers: HashMap<DirectModelSelection, ModelSettingsLowerLayers>,
    billing_rates: HashMap<ResolvedProviderTarget, ModelBillingRates>,
    target_adapters: HashMap<ResolvedProviderTarget, ModelAdapter>,
    /// Pool name per target that can serve a call, selectable or serving-only.
    ///
    /// Selection keys on the target that actually serves the call, so a fast
    /// alternate target must be indexed here under its mapped family's pool;
    /// deriving this from selectable routes alone left those targets with no
    /// policy at all.
    target_credential_pools: HashMap<ResolvedProviderTarget, Arc<str>>,
    provider_model_adapters: HashMap<String, ModelAdapter>,
    session_credential_pin: SessionCredentialPin,
    fallback_credential_profile: Arc<str>,
    credential_families: ModelCredentialFamilyCatalog,
    codex_cli: Option<CodexCliConfiguration>,
    codex_cli_credential_profile: Option<Arc<str>>,
    claude_cli: Option<ClaudeCliConfiguration>,
    claude_cli_credential_profile: Option<Arc<str>>,
    compaction_prompt: Arc<str>,
    conversation_import_max_source_bytes: usize,
    web_fetch_egress_policy: WebFetchEgressPolicy,
    daemon_tools: Option<DaemonToolConfiguration>,
    tool_approval_postures: BTreeMap<ToolName, ToolApprovalPosture>,
    approval_judge_selection: Option<DirectModelSelection>,
    convergence: Option<signalbox_convergence::ConvergencePolicy>,
    repository_watch: Option<RepositoryWatchConfiguration>,
    blob_storage: Option<BlobStorageConfiguration>,
    workspace_instructions: WorkspaceInstructionConfiguration,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ModelSettingsLowerLayers {
    profile: ModelSettingsOverlay,
    global_default: ModelSettingsOverlay,
}

impl HubModelConfiguration {
    /// Reads and validates the versioned static TOML document.
    pub fn read(path: &Path) -> Result<Self, HubModelConfigurationError> {
        let content = fs::read_to_string(path).map_err(|_| HubModelConfigurationError::Read)?;
        Self::parse(&content)
    }

    /// Checks every startup-only section before opening the fenced database.
    pub fn startup_numeric_bounds(
        content: &str,
    ) -> Result<NumericBoundsConfiguration, HubModelConfigurationError> {
        let document = content
            .parse::<DocumentMut>()
            .map_err(|_| HubModelConfigurationError::InvalidDocument)?;
        Ok(startup::parse_startup(content, &document)?.numeric_bounds)
    }

    /// Parses one complete versioned configuration document.
    pub fn parse(content: &str) -> Result<Self, HubModelConfigurationError> {
        let document = DocumentMut::from_str(content)
            .map_err(|_| HubModelConfigurationError::InvalidDocument)?;
        let startup::ParsedStartup {
            numeric_bounds,
            global_model_settings,
            model_settings_profiles,
            compaction_prompt,
            conversation_import_max_source_bytes,
            blob_storage,
            web_fetch_egress_policy,
            daemon_tools,
            credential_profiles,
            credential_pools,
            tool_approval_postures,
            approval_judge_selection,
            convergence,
            workspace_instructions,
            mappings,
            session_credential_pin,
            fallback_credential_profile,
            codex_cli,
            codex_cli_credential_profile,
            claude_cli,
            claude_cli_credential_profile,
        } = startup::parse_startup(content, &document)?;
        let repository_watch = document
            .get("repository_watch")
            .map(|item| parse_repository_watch_configuration(item, &numeric_bounds))
            .transpose()?;
        let models = document
            .get("models")
            .and_then(|item| item.as_array_of_tables())
            .ok_or(HubModelConfigurationError::MissingModels)?;
        if models.is_empty() {
            return Err(HubModelConfigurationError::MissingModels);
        }
        validate_model_count(models.len())?;

        let mut domain_definitions = Vec::with_capacity(models.len());
        let mut runtime_definitions = Vec::with_capacity(models.len());
        let mut capability_definitions = Vec::with_capacity(models.len());
        let mut model_settings_lower_layers = HashMap::with_capacity(models.len());
        let mut direct_selections = HashSet::with_capacity(models.len());
        let mut routes = HashMap::with_capacity(models.len());
        let mut target_billing_rates = HashMap::with_capacity(models.len());
        let mut target_adapters = HashMap::with_capacity(models.len());
        let mut target_credential_pools = HashMap::with_capacity(models.len());
        let mut target_model_families = HashMap::with_capacity(models.len());
        let mut target_fast_targets = HashMap::new();
        let mut target_provider_models = HashMap::with_capacity(models.len());
        let mut selectable_targets = HashSet::with_capacity(models.len());
        let mut provider_model_adapters = HashMap::with_capacity(models.len());
        let mut runtime_capability_projections = Vec::with_capacity(models.len());
        let mut reasoning_families = std::collections::BTreeMap::new();
        for model in models {
            reject_unknown_fields(
                model,
                &[
                    "selection_id",
                    "target_id",
                    "model_family",
                    "provider_model",
                    "max_output_tokens",
                    "context_window_tokens",
                    "rate_version",
                    "input_usd_per_million_tokens",
                    "output_usd_per_million_tokens",
                    "cache_creation_input_usd_per_million_tokens",
                    "cache_read_input_usd_per_million_tokens",
                    "reasoning_levels",
                    "fast_mode",
                    "fast_target_id",
                    "service_tiers",
                    "settings_profile",
                    "provider_compaction",
                    "reasoning_replay_family",
                ],
            )?;
            let selection = DirectModelSelection::from_uuid(required_uuid(model, "selection_id")?);
            if !direct_selections.insert(selection) {
                return Err(HubModelConfigurationError::DuplicateSelection);
            }
            let model_family = validated_name(required_string(model, "model_family")?)?;
            let Some(mapping) = mappings.get(&model_family) else {
                return Err(HubModelConfigurationError::UnmappedModelFamily { model_family });
            };
            let provider_model = required_string(model, "provider_model")?;
            if provider_model.is_empty() || provider_model.trim() != provider_model {
                return Err(HubModelConfigurationError::InvalidProviderModel);
            }
            let max_output_tokens = required_positive_u32(model, "max_output_tokens")?;
            let context_window_tokens = required_positive_u32(model, "context_window_tokens")?;
            let provider_compaction = parse_provider_compaction_capability(model, mapping.adapter)?;
            record_reasoning_replay_family(
                model,
                mapping.adapter,
                provider_model,
                &mut reasoning_families,
            )?;
            let target = ResolvedProviderTarget::naming(ProviderModelIdentity::from_uuid(
                required_uuid(model, "target_id")?,
            ));
            selectable_targets.insert(target);
            let capabilities = parse_model_capabilities(model, mapping.adapter)?;
            let profile = match model.get("settings_profile") {
                None => ModelSettingsOverlay::inherit_all(),
                Some(_) => {
                    let profile_name = validated_name(required_string(model, "settings_profile")?)?;
                    model_settings_profiles
                        .get(&profile_name)
                        .copied()
                        .ok_or(HubModelConfigurationError::InvalidModelSettingsConfiguration)?
                }
            };
            capabilities
                .validate_explicit(selection, global_model_settings)
                .map_err(|_| HubModelConfigurationError::InvalidModelSettingsConfiguration)?;
            capabilities
                .validate_explicit(selection, profile)
                .map_err(|_| HubModelConfigurationError::InvalidModelSettingsConfiguration)?;
            let global_settings = capabilities
                .validate_precedence(
                    selection,
                    ModelSettingsPrecedence::new(
                        ModelSettingsOverlay::inherit_all(),
                        ModelSettingsOverlay::inherit_all(),
                        ModelSettingsOverlay::inherit_all(),
                        global_model_settings,
                    ),
                )
                .map_err(|_| HubModelConfigurationError::InvalidModelSettingsConfiguration)?;
            validate_adapter_model_settings(mapping.adapter, max_output_tokens, global_settings)?;
            let configured_settings = capabilities
                .validate_precedence(
                    selection,
                    ModelSettingsPrecedence::new(
                        ModelSettingsOverlay::inherit_all(),
                        ModelSettingsOverlay::inherit_all(),
                        profile,
                        global_model_settings,
                    ),
                )
                .map_err(|_| HubModelConfigurationError::InvalidModelSettingsConfiguration)?;
            validate_adapter_model_settings(
                mapping.adapter,
                max_output_tokens,
                configured_settings,
            )?;
            model_settings_lower_layers.insert(
                selection,
                ModelSettingsLowerLayers {
                    profile,
                    global_default: global_model_settings,
                },
            );
            let fast_target = match capabilities.fast_mode() {
                FastModeSupport::AlternateTarget(target) => Some(target),
                FastModeSupport::Unsupported | FastModeSupport::RequestControl => None,
            };
            let provider_model = provider_model.to_owned();
            let rates = parse_model_billing_rates(model)?;
            if let Some(previous) = target_billing_rates.insert(target, rates.clone())
                && previous != rates
            {
                return Err(HubModelConfigurationError::ConflictingTarget);
            }
            if let Some(previous) = target_adapters.insert(target, mapping.adapter)
                && previous != mapping.adapter
            {
                return Err(HubModelConfigurationError::ConflictingTarget);
            }
            if let Some(previous) = target_model_families.insert(target, Arc::clone(&model_family))
                && previous != model_family
            {
                return Err(HubModelConfigurationError::ConflictingTarget);
            }
            if let Some(previous) =
                target_credential_pools.insert(target, Arc::clone(&mapping.credential_pool))
                && previous != mapping.credential_pool
            {
                return Err(HubModelConfigurationError::ConflictingTarget);
            }
            if let Some(fast_target) = fast_target {
                target_fast_targets.insert(target, fast_target);
            }
            if let Some(previous) = target_provider_models.insert(target, provider_model.clone())
                && previous != provider_model
            {
                return Err(HubModelConfigurationError::ConflictingTarget);
            }
            if let Some(previous) =
                provider_model_adapters.insert(provider_model.clone(), mapping.adapter)
                && previous != mapping.adapter
            {
                return Err(HubModelConfigurationError::ConflictingProviderModelRoute);
            }
            routes.insert(
                selection,
                ResolvedModelRoute {
                    model_family,
                    adapter: mapping.adapter,
                    credential_pool: Arc::clone(&mapping.credential_pool),
                    credential_profile: Arc::clone(&mapping.credential_profile),
                    target,
                },
            );
            domain_definitions.push(ModelTargetDefinition::new(selection, target));
            capability_definitions.push(ModelCapabilityDefinition::new(
                selection,
                capabilities.clone(),
            ));
            runtime_capability_projections.push(RuntimeCapabilityProjection {
                adapter: mapping.adapter,
                provider_model: provider_model.clone(),
                capabilities,
            });
            let runtime_definition = RuntimeModelDefinition::try_new(
                target,
                provider_model,
                max_output_tokens,
                context_window_tokens,
            )
            .map_err(|_| HubModelConfigurationError::InvalidField)?;
            let runtime_definition = if provider_compaction {
                runtime_definition.with_provider_compaction()
            } else {
                runtime_definition
            };
            runtime_definitions.push(match fast_target {
                Some(target) => runtime_definition.with_fast_target(target),
                None => runtime_definition,
            });
        }

        if let Some(serving_targets) = document
            .get("serving_targets")
            .map(|item| {
                item.as_array_of_tables()
                    .ok_or(HubModelConfigurationError::InvalidModelCapabilities)
            })
            .transpose()?
        {
            for serving_target in serving_targets {
                reject_unknown_fields(
                    serving_target,
                    &[
                        "target_id",
                        "model_family",
                        "provider_model",
                        "max_output_tokens",
                        "context_window_tokens",
                        "provider_compaction",
                        "reasoning_replay_family",
                    ],
                )?;
                let target = ResolvedProviderTarget::naming(ProviderModelIdentity::from_uuid(
                    required_uuid(serving_target, "target_id")?,
                ));
                if target_provider_models.contains_key(&target) {
                    return Err(HubModelConfigurationError::ConflictingTarget);
                }
                let model_family =
                    validated_name(required_string(serving_target, "model_family")?)?;
                let Some(mapping) = mappings.get(&model_family) else {
                    return Err(HubModelConfigurationError::UnmappedModelFamily { model_family });
                };
                let provider_model = required_string(serving_target, "provider_model")?;
                if provider_model.is_empty() || provider_model.trim() != provider_model {
                    return Err(HubModelConfigurationError::InvalidProviderModel);
                }
                let provider_model = provider_model.to_owned();
                let provider_compaction =
                    parse_provider_compaction_capability(serving_target, mapping.adapter)?;
                record_reasoning_replay_family(
                    serving_target,
                    mapping.adapter,
                    &provider_model,
                    &mut reasoning_families,
                )?;
                let max_output_tokens = required_positive_u32(serving_target, "max_output_tokens")?;
                let context_window_tokens =
                    required_positive_u32(serving_target, "context_window_tokens")?;
                target_provider_models.insert(target, provider_model.clone());
                target_adapters.insert(target, mapping.adapter);
                target_model_families.insert(target, Arc::clone(&model_family));
                target_credential_pools.insert(target, Arc::clone(&mapping.credential_pool));
                if let Some(previous) =
                    provider_model_adapters.insert(provider_model.clone(), mapping.adapter)
                    && previous != mapping.adapter
                {
                    return Err(HubModelConfigurationError::ConflictingProviderModelRoute);
                }
                let runtime_definition = RuntimeModelDefinition::try_new(
                    target,
                    provider_model,
                    max_output_tokens,
                    context_window_tokens,
                )
                .map_err(|_| HubModelConfigurationError::InvalidField)?;
                runtime_definitions.push(if provider_compaction {
                    runtime_definition.with_provider_compaction()
                } else {
                    runtime_definition
                });
            }
        }

        if codex_cli.as_ref().is_some_and(|configuration| {
            configuration
                .model_context_window_overrides
                .keys()
                .any(|provider_model| {
                    provider_model_adapters.get(provider_model) != Some(&ModelAdapter::CodexCli)
                })
        }) {
            return Err(HubModelConfigurationError::InvalidCodexCliConfiguration);
        }

        if approval_judge_selection.is_some_and(|selection| !direct_selections.contains(&selection))
        {
            return Err(HubModelConfigurationError::DanglingApprovalJudgeSelection);
        }

        let mut aliases = HashMap::new();
        if let Some(alias_tables) = document
            .get("aliases")
            .map(|item| {
                item.as_array_of_tables()
                    .ok_or(HubModelConfigurationError::InvalidAliases)
            })
            .transpose()?
        {
            validate_alias_count(alias_tables.len())?;
            for alias in alias_tables {
                reject_unknown_fields(alias, &["alias_id", "selection_id"])?;
                let identity = ModelAlias::from_uuid(required_uuid(alias, "alias_id")?);
                let selected =
                    DirectModelSelection::from_uuid(required_uuid(alias, "selection_id")?);
                if !direct_selections.contains(&selected) {
                    return Err(HubModelConfigurationError::DanglingAlias);
                }
                if aliases
                    .insert(identity, FrozenAliasDefinition::selecting(selected))
                    .is_some()
                {
                    return Err(HubModelConfigurationError::DuplicateAlias);
                }
            }
        }

        let targets = ModelTargetCatalog::try_from_definitions(domain_definitions)
            .map_err(|_| HubModelConfigurationError::DuplicateSelection)?;
        let model_capabilities =
            ModelCapabilityCatalog::try_from_definitions(capability_definitions)
                .map_err(|_| HubModelConfigurationError::DuplicateSelection)?;
        let runtime_model_capabilities = project_runtime_model_capabilities(
            runtime_capability_projections,
            reasoning_families,
            &target_provider_models,
            &target_adapters,
            &selectable_targets,
        )?;
        let runtime_models = RuntimeModelCatalog::try_from_definitions(runtime_definitions)
            .map_err(|_| HubModelConfigurationError::ConflictingTarget)?;
        let mut tool_continuation_usage_limits = Vec::with_capacity(routes.len().saturating_mul(2));
        for route in routes.values() {
            let definition = runtime_models
                .resolve(route.target)
                .ok_or(HubModelConfigurationError::ConflictingTarget)?;
            for fast_mode in [FastMode::Disabled, FastMode::Enabled] {
                let effective = runtime_models
                    .effective_definition(definition, fast_mode)
                    .ok_or(HubModelConfigurationError::ConflictingTarget)?;
                let limit = ToolContinuationUsageLimit::new(
                    route.target,
                    fast_mode,
                    u64::from(effective.max_output_tokens()),
                    u64::from(effective.context_window_tokens()),
                );
                tool_continuation_usage_limits.push(if effective.provider_compaction_supported() {
                    limit.with_provider_compaction_replay()
                } else {
                    limit
                });
            }
        }
        let billing_rates = target_billing_rates
            .into_iter()
            .filter_map(|(target, rates)| rates.map(|rates| (target, rates)))
            .collect();
        let credential_families = ModelCredentialFamilyCatalog::try_new(
            target_model_families.into_iter().map(|(target, family)| {
                let migration_fallback = (target_adapters.get(&target)
                    == Some(&ModelAdapter::Anthropic))
                .then(|| Arc::<str>::from(MIGRATED_ANTHROPIC_MODEL_FAMILY));
                (target, family, migration_fallback)
            }),
        )
        .and_then(|catalog| catalog.with_fast_targets(target_fast_targets))
        .map_err(|_| HubModelConfigurationError::ConflictingTarget)?;
        Ok(Self {
            source: CheckedConfigurationSource(Arc::from(content)),
            numeric_bounds,
            targets,
            runtime_models,
            tool_continuation_usage_limits,
            direct_selections,
            aliases,
            routes,
            credential_profiles,
            credential_pools,
            model_capabilities,
            runtime_model_capabilities,
            model_settings_lower_layers,
            billing_rates,
            target_adapters,
            target_credential_pools,
            provider_model_adapters,
            session_credential_pin,
            fallback_credential_profile,
            credential_families,
            codex_cli,
            codex_cli_credential_profile,
            claude_cli,
            claude_cli_credential_profile,
            compaction_prompt,
            conversation_import_max_source_bytes,
            web_fetch_egress_policy,
            daemon_tools,
            tool_approval_postures,
            approval_judge_selection,
            convergence,
            repository_watch,
            blob_storage,
            workspace_instructions,
        })
    }

    /// Parses a test catalog after adding the checked-in example's bound table.
    ///
    /// Test-only catalogs intentionally state only the behavior under test. The
    /// required deployment policy comes from the one source that owns today's
    /// values instead of being re-encoded across fixtures.
    #[doc(hidden)]
    #[cfg(test)]
    pub fn parse_test_fixture(content: &str) -> Result<Self, HubModelConfigurationError> {
        let example = include_str!("../../../../config/signalboxd.example.toml");
        let (_, numeric_bounds_and_after) = example
            .split_once("[numeric_bounds]")
            .ok_or(HubModelConfigurationError::InvalidDocument)?;
        let (numeric_bounds, _) = numeric_bounds_and_after
            .split_once("\n# Blob bytes live outside PostgreSQL.")
            .ok_or(HubModelConfigurationError::InvalidDocument)?;
        Self::parse(&format!("{content}\n[numeric_bounds]{numeric_bounds}\n"))
    }

    /// Returns the checked source document for durable reload snapshots.
    pub(crate) fn source(&self) -> &str {
        &self.source.0
    }

    /// Returns the immutable domain target catalog used by persistence.
    pub fn target_catalog(&self) -> ModelTargetCatalog {
        self.targets.clone()
    }

    /// Returns the complete per-direct-selection settings capability catalog.
    pub fn model_capability_catalog(&self) -> ModelCapabilityCatalog {
        self.model_capabilities.clone()
    }

    /// Returns the exact provider-target capability catalog used at preparation.
    pub fn runtime_model_capability_catalog(&self) -> RuntimeModelCapabilityCatalog {
        self.runtime_model_capabilities.clone()
    }

    /// Returns the copied profile and global layers for a direct selection.
    pub fn model_settings_lower_layers(
        &self,
        selection: DirectModelSelection,
    ) -> Option<(ModelSettingsOverlay, ModelSettingsOverlay)> {
        self.model_settings_lower_layers
            .get(&selection)
            .map(|layers| (layers.profile, layers.global_default))
    }

    /// Resolves and validates one caller-owned session layer against its direct model.
    pub fn validate_session_model_settings(
        &self,
        selection: ModelSelectionRequest,
        session: ModelSettingsOverlay,
    ) -> Option<Result<ValidatedModelSettings, UnsupportedModelSetting>> {
        let direct = self.resolve_direct_selection(selection)?;
        let capabilities = self.model_capabilities.resolve(direct)?;
        let layers = self.model_settings_lower_layers.get(&direct)?;
        Some(capabilities.validate_precedence(
            direct,
            ModelSettingsPrecedence::new(
                ModelSettingsOverlay::inherit_all(),
                session,
                layers.profile,
                layers.global_default,
            ),
        ))
    }

    /// Returns the exact runtime delivery catalog used by the provider bridge.
    pub fn runtime_model_catalog(&self) -> RuntimeModelCatalog {
        self.runtime_models.clone()
    }

    /// Returns configured output reservations and context ceilings for every
    /// same-turn continuation mode.
    pub fn tool_continuation_usage_limits(&self) -> Vec<ToolContinuationUsageLimit> {
        self.tool_continuation_usage_limits.clone()
    }

    /// Returns the adapter route for one configured direct selection.
    pub fn resolve_direct_model(
        &self,
        selection: DirectModelSelection,
    ) -> Option<&ResolvedModelRoute> {
        self.routes.get(&selection)
    }

    /// Resolves one session request through the exact static catalog.
    pub fn resolve_session_model(
        &self,
        selection: ModelSelectionRequest,
    ) -> Result<&ResolvedModelRoute, UnknownSessionModel> {
        let direct = match selection {
            ModelSelectionRequest::Direct(direct) => direct,
            ModelSelectionRequest::Alias(alias) => self
                .aliases
                .get(&alias)
                .map(|definition| definition.selected())
                .ok_or(UnknownSessionModel { selection })?,
        };
        self.routes
            .get(&direct)
            .ok_or(UnknownSessionModel { selection })
    }

    /// Resolves a direct or current alias request to its selected direct key.
    pub fn resolve_direct_selection(
        &self,
        selection: ModelSelectionRequest,
    ) -> Option<DirectModelSelection> {
        match selection {
            ModelSelectionRequest::Direct(direct) => {
                self.direct_selections.contains(&direct).then_some(direct)
            }
            ModelSelectionRequest::Alias(alias) => self
                .aliases
                .get(&alias)
                .map(|definition| definition.selected()),
        }
    }

    /// Returns the adapter selected for an exact provider-native model name.
    pub fn adapter_for_provider_model(&self, provider_model: &str) -> Option<ModelAdapter> {
        self.provider_model_adapters.get(provider_model).copied()
    }

    /// Returns whether one configured target's reported input count includes cache axes.
    pub fn input_includes_cache_tokens(&self, target: ResolvedProviderTarget) -> bool {
        self.target_adapters
            .get(&target)
            .is_some_and(|adapter| adapter.reports_cache_inclusive_input())
    }

    /// Returns targets whose provider-reported input count includes cache axes.
    pub fn cache_inclusive_input_targets(&self) -> HashSet<ResolvedProviderTarget> {
        self.target_adapters
            .iter()
            .filter_map(|(target, adapter)| {
                adapter.reports_cache_inclusive_input().then_some(*target)
            })
            .collect()
    }

    /// Returns the exact targets whose adapters issue prospective count
    /// interactions before turn activation.
    pub fn provider_input_count_targets(&self) -> HashSet<ResolvedProviderTarget> {
        self.target_adapters
            .iter()
            .filter_map(|(target, adapter)| {
                (*adapter == ModelAdapter::Anthropic).then_some(*target)
            })
            .collect()
    }

    /// Returns the complete credential snapshot pinned into a new session.
    pub fn session_credential_pin(&self) -> SessionCredentialPin {
        self.session_credential_pin.clone()
    }

    /// Maps each exact target to the family key stored in session snapshots.
    pub fn credential_family_catalog(&self) -> ModelCredentialFamilyCatalog {
        self.credential_families.clone()
    }

    /// Projects admitted pool policy into the persistence-owned runtime form.
    pub fn credential_pool_runtime_catalog(&self) -> CredentialPoolRuntimeCatalog {
        self.target_credential_pools
            .iter()
            .filter_map(|(target, pool_name)| {
                let pool = self.credential_pools.get(pool_name)?;
                let mut members = pool.members().to_vec();
                members.sort_by_key(|member| member.priority());
                let members = members
                    .into_iter()
                    .map(|member| {
                        CredentialPoolRuntimeMember::new(member.profile(), member.priority())
                            .with_headroom_reserve(member.headroom_reserve_percent())
                    })
                    .collect::<Vec<_>>();
                Some((
                    *target,
                    CredentialPoolRuntimePolicy::new(
                        pool.name(),
                        members,
                        runtime_pool_exhaustion(pool.on_pool_exhausted()),
                        runtime_pool_action(pool.action(CredentialPoolTrigger::QuotaExhausted)),
                        runtime_pool_action(pool.action(CredentialPoolTrigger::RateLimited)),
                        runtime_pool_action(pool.action(CredentialPoolTrigger::Overloaded)),
                        runtime_pool_action(pool.action(CredentialPoolTrigger::CredentialRejected)),
                    )
                    .with_capacity_policy(
                        match pool.tie_break() {
                            crate::credential_pools::CredentialPoolTieBreak::LeastUsed => {
                                CredentialPoolRuntimeTieBreak::LeastUsed
                            }
                            _ => CredentialPoolRuntimeTieBreak::FirstListed,
                        },
                        pool.headroom_reserve_percent(),
                        runtime_pool_action(pool.action(CredentialPoolTrigger::HeadroomLow)),
                    ),
                ))
            })
            .collect()
    }

    /// Derives a labeled USD figure from exactly the token axes present.
    ///
    /// Absence means either this target has no declared rates, the historical
    /// credential profile has no declared billing kind, no token axis was
    /// reported, the historical input semantics are unknown, or exact decimal
    /// arithmetic could not represent the result.
    pub(crate) fn derive_model_call_cost(
        &self,
        target: ResolvedProviderTarget,
        credential_profile: &str,
        input: ModelCallInputUsage,
        output_tokens: Option<u64>,
        cache_creation_input_tokens: Option<u64>,
        cache_read_input_tokens: Option<u64>,
    ) -> Option<DerivedModelCallCost> {
        let rates = self.billing_rates.get(&target)?;
        let billing_kind = self
            .credential_profiles
            .get(credential_profile)?
            .billing_kind();
        let input_tokens = match input.semantics? {
            ProcessModelCallInputTokenSemantics::CacheInclusive => {
                match (
                    input.tokens,
                    cache_creation_input_tokens,
                    cache_read_input_tokens,
                ) {
                    (Some(total), Some(cache_creation), Some(cache_read)) => {
                        Some(total.checked_sub(cache_creation.checked_add(cache_read)?)?)
                    }
                    _ => None,
                }
            }
            ProcessModelCallInputTokenSemantics::CacheExclusive => input.tokens,
        };
        let amount_usd = fold_reported_cost([
            (input_tokens.map(u128::from), rates.input),
            (output_tokens.map(u128::from), rates.output),
            (
                cache_creation_input_tokens.map(u128::from),
                rates.cache_creation_input,
            ),
            (
                cache_read_input_tokens.map(u128::from),
                rates.cache_read_input,
            ),
        ])?;
        Some(DerivedModelCallCost {
            amount_usd,
            rate_version: Arc::clone(&rates.version),
            billing_kind,
        })
    }

    /// Derives a labeled USD figure from widened aggregate token totals.
    ///
    /// Every reported axis must price exactly; when any reported axis cannot
    /// be represented by exact decimal arithmetic, the whole derivation is
    /// absent rather than an understated partial total.
    pub(crate) fn derive_usage_aggregate_cost(
        &self,
        target: ResolvedProviderTarget,
        profile: &str,
        semantics: ProcessModelCallInputTokenSemantics,
        token_axes: [Option<u128>; 4],
    ) -> Option<DerivedModelCallCost> {
        let [
            input_tokens,
            output_tokens,
            cache_creation_input_tokens,
            cache_read_input_tokens,
        ] = token_axes;
        let rates = self.billing_rates.get(&target)?;
        let billing_kind = self.credential_profiles.get(profile)?.billing_kind();
        let ordinary_input_tokens = match semantics {
            ProcessModelCallInputTokenSemantics::CacheInclusive => match (
                input_tokens,
                cache_creation_input_tokens,
                cache_read_input_tokens,
            ) {
                (Some(total), Some(cache_creation), Some(cache_read)) => {
                    Some(total.checked_sub(cache_creation.checked_add(cache_read)?)?)
                }
                _ => None,
            },
            ProcessModelCallInputTokenSemantics::CacheExclusive => input_tokens,
        };
        let amount_usd = fold_reported_cost([
            (ordinary_input_tokens, rates.input),
            (output_tokens, rates.output),
            (cache_creation_input_tokens, rates.cache_creation_input),
            (cache_read_input_tokens, rates.cache_read_input),
        ])?;
        Some(DerivedModelCallCost {
            amount_usd,
            rate_version: Arc::clone(&rates.version),
            billing_kind,
        })
    }

    /// Iterates every file-delivered profile for one adapter and its path.
    ///
    /// The catalog is deliberately not narrowed to currently preferred pool
    /// members: an existing session may still pin any declared profile after a
    /// configuration edit or restart. It is narrowed to the declared adapter
    /// so a historical reference cannot deliver one provider's secret to
    /// another provider after an adapter remap.
    pub fn file_credential_profiles(
        &self,
        adapter: ModelAdapter,
    ) -> impl Iterator<Item = (&str, &Path)> {
        self.credential_profiles
            .values()
            .filter(move |profile| profile.adapter() == adapter)
            .filter_map(|profile| {
                profile
                    .delivery()
                    .path()
                    .map(|path| (profile.name(), path.as_path()))
            })
    }

    /// Returns a configuration-owned compatibility fallback for persistence
    /// paths that predate per-family session credential snapshots.
    pub fn fallback_credential_profile(&self) -> &str {
        &self.fallback_credential_profile
    }

    /// Returns one declared credential profile by its exact name.
    pub fn credential_profile(&self, name: &str) -> Option<&CredentialProfile> {
        self.credential_profiles.get(name)
    }

    /// Returns one declared credential pool by its exact name.
    pub fn credential_pool(&self, name: &str) -> Option<&CredentialPool> {
        self.credential_pools.get(name)
    }

    /// Returns validated Codex CLI paths when that adapter is configured.
    pub fn codex_cli(&self) -> Option<&CodexCliConfiguration> {
        self.codex_cli.as_ref()
    }

    pub(crate) fn codex_cli_runtime(
        &self,
        model_exchange_timeout: Option<Duration>,
        post_kill_reap_bound: Option<Duration>,
    ) -> Result<Option<CodexCliRuntime>, CodexCliConstructionError> {
        self.codex_cli
            .as_ref()
            .map(|configuration| {
                let credential_profile = self
                    .codex_cli_credential_profile
                    .as_deref()
                    .unwrap_or(CODEX_CLI_CREDENTIAL_REFERENCE);
                let mut runtime_configuration = CodexCliConfig::new(
                    configuration.executable.clone(),
                    configuration.working_directory.clone(),
                    CredentialReference::new(credential_profile),
                    post_kill_reap_bound,
                );
                runtime_configuration.exchange_timeout = model_exchange_timeout;
                runtime_configuration = runtime_configuration.with_credential_homes(
                    self.credential_profiles.values().filter_map(|profile| {
                        let CredentialDelivery::CodexHome { path, .. } = profile.delivery() else {
                            return None;
                        };
                        Some((CredentialReference::new(profile.name()), path.to_path_buf()))
                    }),
                );
                runtime_configuration.model_capabilities = self.runtime_model_capability_catalog();
                runtime_configuration.model_context_window_overrides =
                    configuration.model_context_window_overrides.clone();
                CodexCliRuntime::new(runtime_configuration)
            })
            .transpose()
    }

    /// Returns validated Claude Code CLI paths when that adapter is configured.
    pub fn claude_cli(&self) -> Option<&ClaudeCliConfiguration> {
        self.claude_cli.as_ref()
    }

    pub(crate) fn claude_cli_runtime(
        &self,
        model_exchange_timeout: Option<Duration>,
        post_kill_reap_bound: Option<Duration>,
        native_message_limit: Option<usize>,
    ) -> Result<Option<ClaudeCliRuntime>, ClaudeCliConstructionError> {
        self.claude_cli
            .as_ref()
            .map(|configuration| {
                let credential_profile = self
                    .claude_cli_credential_profile
                    .as_deref()
                    .unwrap_or(CLAUDE_CLI_CREDENTIAL_REFERENCE);
                let mut runtime_configuration = ClaudeCliConfig::new(
                    configuration.executable.clone(),
                    configuration.mcp_bridge_executable.clone(),
                    configuration.working_directory.clone(),
                    CredentialReference::new(credential_profile),
                    post_kill_reap_bound,
                    native_message_limit,
                );
                runtime_configuration.exchange_timeout = model_exchange_timeout;
                runtime_configuration.model_capabilities = self.runtime_model_capability_catalog();
                let credentials = FileCredentialAccess::from_files(
                    self.file_credential_profiles(ModelAdapter::ClaudeCli).map(
                        |(reference, path)| {
                            (CredentialReference::new(reference), path.to_path_buf())
                        },
                    ),
                );
                let ambient_reference = self
                    .credential_profiles
                    .values()
                    .find(|profile| {
                        profile.adapter() == ModelAdapter::ClaudeCli
                            && matches!(profile.delivery(), CredentialDelivery::Ambient)
                    })
                    .map(|profile| CredentialReference::new(profile.name()));
                let file_env_key = self
                    .credential_profiles
                    .values()
                    .filter(|profile| profile.adapter() == ModelAdapter::ClaudeCli)
                    .find_map(|profile| profile.delivery().env_key());
                match file_env_key {
                    Some(file_env_key) => ClaudeCliRuntime::new_with_credential_catalog(
                        runtime_configuration,
                        credentials,
                        ambient_reference,
                        file_env_key,
                    ),
                    None => ClaudeCliRuntime::new(runtime_configuration),
                }
            })
            .transpose()
    }

    pub(crate) fn adapter_routes(&self) -> HashMap<String, ModelAdapter> {
        self.provider_model_adapters.clone()
    }

    fn uses_adapter(&self, adapter: ModelAdapter) -> bool {
        self.provider_model_adapters
            .values()
            .any(|configured| *configured == adapter)
    }

    /// Reports whether at least one configured route requires Anthropic.
    pub fn uses_anthropic_adapter(&self) -> bool {
        self.uses_adapter(ModelAdapter::Anthropic)
    }

    /// Reports whether at least one configured route requires OpenAI.
    pub fn uses_openai_adapter(&self) -> bool {
        self.uses_adapter(ModelAdapter::OpenAi)
    }

    /// Returns the exact configured compaction system prompt.
    pub fn compaction_prompt(&self) -> &str {
        &self.compaction_prompt
    }

    /// Returns every required deployment-owned numeric-bound policy.
    pub const fn numeric_bounds(&self) -> &NumericBoundsConfiguration {
        &self.numeric_bounds
    }

    /// Returns the maximum assembled source bytes for one conversation import.
    pub const fn conversation_import_max_source_bytes(&self) -> usize {
        self.conversation_import_max_source_bytes
    }

    /// Returns the validated blob-store registry and write routes, when enabled.
    pub const fn blob_storage(&self) -> Option<&BlobStorageConfiguration> {
        self.blob_storage.as_ref()
    }

    /// Returns the exact deployment-owned automatic web-fetch egress policy.
    pub fn web_fetch_egress_policy(&self) -> WebFetchEgressPolicy {
        self.web_fetch_egress_policy.clone()
    }

    /// Iterates explicit per-tool posture overrides in canonical name order.
    pub fn tool_approval_postures(&self) -> impl Iterator<Item = (ToolName, ToolApprovalPosture)> {
        self.tool_approval_postures
            .iter()
            .map(|(name, posture)| (name.clone(), *posture))
    }

    /// Resolves the selection reserved for the committed daemon judge wiring.
    pub fn approval_judge_selection(&self, judged: DirectModelSelection) -> DirectModelSelection {
        self.approval_judge_selection.unwrap_or(judged)
    }

    /// Returns the explicit approval-judge selection, leaving the producing
    /// call to supply the default when configuration omits the table.
    pub const fn configured_approval_judge_selection(&self) -> Option<DirectModelSelection> {
        self.approval_judge_selection
    }

    /// Returns explicitly configured daemon tool dependencies, when present.
    pub const fn daemon_tools(&self) -> Option<&DaemonToolConfiguration> {
        self.daemon_tools.as_ref()
    }

    /// Returns explicit roots whose content is discoverable but not eligible by default.
    pub const fn workspace_instructions(&self) -> &WorkspaceInstructionConfiguration {
        &self.workspace_instructions
    }

    /// Reports whether the configuration contains one direct selection key.
    pub fn contains_selection(&self, selection: DirectModelSelection) -> bool {
        self.direct_selections.contains(&selection)
    }

    /// Shared pull-request convergence policy for the daemon and code-host tools.
    pub const fn convergence(&self) -> Option<&signalbox_convergence::ConvergencePolicy> {
        self.convergence.as_ref()
    }

    /// Returns the complete watch configuration, or absence when no task starts.
    pub const fn repository_watch(&self) -> Option<&RepositoryWatchConfiguration> {
        self.repository_watch.as_ref()
    }

    /// Resolves one configured alias to the immutable definition frozen at
    /// acceptance time.
    pub fn resolve_alias(&self, alias: ModelAlias) -> Option<FrozenAliasDefinition> {
        self.aliases.get(&alias).copied()
    }

    /// Iterates the complete deployment-owned alias catalog.
    pub fn model_aliases(&self) -> impl Iterator<Item = (ModelAlias, DirectModelSelection)> + '_ {
        self.aliases
            .iter()
            .map(|(alias, definition)| (*alias, definition.selected()))
    }
}

#[cfg(test)]
const EXAMPLE_EXEC_SUPERVISOR: &str = "/usr/local/bin/signalbox-exec-supervisor";

#[cfg(test)]
pub(crate) fn checked_in_example_configuration()
-> Result<HubModelConfiguration, HubModelConfigurationError> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config/signalboxd.example.toml");
    let content = fs::read_to_string(path).map_err(|_| HubModelConfigurationError::Read)?;
    let executable = std::env::current_exe().map_err(|_| HubModelConfigurationError::Read)?;
    HubModelConfiguration::parse(&content.replace(
        EXAMPLE_EXEC_SUPERVISOR,
        executable.to_string_lossy().as_ref(),
    ))
}
