use super::{
    error::HubModelConfigurationError,
    model_routing::ModelAdapter,
    toml_scalars::{reject_unknown_fields, required_string, required_uuid, validated_name},
};
use signalbox_domain::{
    AnthropicServiceTier, CodexCliServiceTier, FastMode, FastModeOverlay, FastModeSupport,
    ModelCapabilities, ModelSettingsOverlay, OpenAiServiceTier, ProviderModelIdentity,
    ReasoningLevel, ResolvedProviderTarget, ServiceTier, SettingOverlay, ValidatedModelSettings,
};
use signalbox_model_runtime::{
    AnthropicServiceTier as RuntimeAnthropicServiceTier,
    CodexCliServiceTier as RuntimeCodexCliServiceTier, FastMode as RuntimeFastMode,
    FastModeTarget as RuntimeFastModeTarget, ModelCapabilities as RuntimeModelCapabilities,
    ModelCapabilityCatalog as RuntimeModelCapabilityCatalog,
    ModelCapabilityDefinition as RuntimeModelCapabilityDefinition,
    ModelSettings as RuntimeModelSettings, OpenAiServiceTier as RuntimeOpenAiServiceTier,
    ReasoningLevel as RuntimeReasoningLevel, ResolvedTarget as RuntimeResolvedTarget,
    ServiceTier as RuntimeServiceTier,
};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    sync::Arc,
};
use toml_edit::{Item, Table};

pub(super) fn parse_model_settings_profiles(
    item: Option<&Item>,
) -> Result<HashMap<Arc<str>, ModelSettingsOverlay>, HubModelConfigurationError> {
    let Some(item) = item else {
        return Ok(HashMap::new());
    };
    let profiles = item
        .as_array_of_tables()
        .ok_or(HubModelConfigurationError::InvalidModelSettingsConfiguration)?;
    let mut parsed = HashMap::with_capacity(profiles.len());
    for profile in profiles {
        reject_unknown_fields(
            profile,
            &["name", "reasoning_level", "fast_mode", "service_tier"],
        )
        .map_err(|_| HubModelConfigurationError::InvalidModelSettingsConfiguration)?;
        let name = validated_name(required_string(profile, "name")?)?;
        let settings = parse_model_settings_overlay_table(profile)?;
        if parsed.insert(name, settings).is_some() {
            return Err(HubModelConfigurationError::InvalidModelSettingsConfiguration);
        }
    }
    Ok(parsed)
}

pub(super) fn parse_model_settings_overlay(
    item: Option<&Item>,
) -> Result<ModelSettingsOverlay, HubModelConfigurationError> {
    let Some(item) = item else {
        return Ok(ModelSettingsOverlay::inherit_all());
    };
    let table = item
        .as_table()
        .ok_or(HubModelConfigurationError::InvalidModelSettingsConfiguration)?;
    reject_unknown_fields(table, &["reasoning_level", "fast_mode", "service_tier"])
        .map_err(|_| HubModelConfigurationError::InvalidModelSettingsConfiguration)?;
    parse_model_settings_overlay_table(table)
}

fn parse_model_settings_overlay_table(
    table: &Table,
) -> Result<ModelSettingsOverlay, HubModelConfigurationError> {
    let reasoning_level = match table.get("reasoning_level") {
        None => SettingOverlay::Inherit,
        Some(item) => match item.as_str() {
            Some("provider_default") => SettingOverlay::ProviderDefault,
            Some(value) => SettingOverlay::Value(parse_configured_reasoning_level(value)?),
            None => return Err(HubModelConfigurationError::InvalidModelSettingsConfiguration),
        },
    };
    let fast_mode = match table.get("fast_mode").and_then(Item::as_str) {
        None if table.get("fast_mode").is_none() => FastModeOverlay::Inherit,
        Some("disabled") => FastModeOverlay::Value(FastMode::Disabled),
        Some("enabled") => FastModeOverlay::Value(FastMode::Enabled),
        _ => return Err(HubModelConfigurationError::InvalidModelSettingsConfiguration),
    };
    let service_tier = match table.get("service_tier") {
        None => SettingOverlay::Inherit,
        Some(item) if item.as_str() == Some("provider_default") => SettingOverlay::ProviderDefault,
        Some(item) => SettingOverlay::Value(parse_configured_service_tier(item)?),
    };
    Ok(ModelSettingsOverlay::new(
        reasoning_level,
        fast_mode,
        service_tier,
    ))
}

fn parse_configured_reasoning_level(
    value: &str,
) -> Result<ReasoningLevel, HubModelConfigurationError> {
    match value {
        "none" => Ok(ReasoningLevel::None),
        "minimal" => Ok(ReasoningLevel::Minimal),
        "low" => Ok(ReasoningLevel::Low),
        "medium" => Ok(ReasoningLevel::Medium),
        "high" => Ok(ReasoningLevel::High),
        "xhigh" => Ok(ReasoningLevel::XHigh),
        "max" => Ok(ReasoningLevel::Max),
        "ultra" => Ok(ReasoningLevel::Ultra),
        _ => Err(HubModelConfigurationError::InvalidModelSettingsConfiguration),
    }
}

fn parse_configured_service_tier(item: &Item) -> Result<ServiceTier, HubModelConfigurationError> {
    let table = item
        .as_inline_table()
        .ok_or(HubModelConfigurationError::InvalidModelSettingsConfiguration)?;
    if table.len() != 2 || !table.contains_key("provider") || !table.contains_key("value") {
        return Err(HubModelConfigurationError::InvalidModelSettingsConfiguration);
    }
    let provider = table
        .get("provider")
        .and_then(toml_edit::Value::as_str)
        .ok_or(HubModelConfigurationError::InvalidModelSettingsConfiguration)?;
    let value = table
        .get("value")
        .and_then(toml_edit::Value::as_str)
        .ok_or(HubModelConfigurationError::InvalidModelSettingsConfiguration)?;
    match (provider, value) {
        ("anthropic", "auto") => Ok(ServiceTier::Anthropic(AnthropicServiceTier::Auto)),
        ("anthropic", "standard_only") => {
            Ok(ServiceTier::Anthropic(AnthropicServiceTier::StandardOnly))
        }
        ("open_ai", "auto") => Ok(ServiceTier::OpenAi(OpenAiServiceTier::Auto)),
        ("open_ai", "default") => Ok(ServiceTier::OpenAi(OpenAiServiceTier::Default)),
        ("open_ai", "flex") => Ok(ServiceTier::OpenAi(OpenAiServiceTier::Flex)),
        ("open_ai", "scale") => Ok(ServiceTier::OpenAi(OpenAiServiceTier::Scale)),
        ("open_ai", "priority") => Ok(ServiceTier::OpenAi(OpenAiServiceTier::Priority)),
        ("open_ai", "fast") => Ok(ServiceTier::OpenAi(OpenAiServiceTier::Fast)),
        ("codex_cli", "default") => Ok(ServiceTier::CodexCli(CodexCliServiceTier::Default)),
        ("codex_cli", "priority") => Ok(ServiceTier::CodexCli(CodexCliServiceTier::Priority)),
        ("codex_cli", "flex") => Ok(ServiceTier::CodexCli(CodexCliServiceTier::Flex)),
        _ => Err(HubModelConfigurationError::InvalidModelSettingsConfiguration),
    }
}

pub(super) struct RuntimeCapabilityProjection {
    pub(super) adapter: ModelAdapter,
    pub(super) provider_model: String,
    pub(super) capabilities: ModelCapabilities,
}

pub(super) fn parse_provider_compaction_capability(
    table: &Table,
    adapter: ModelAdapter,
) -> Result<bool, HubModelConfigurationError> {
    let supported = table
        .get("provider_compaction")
        .map(|item| {
            item.as_bool()
                .ok_or(HubModelConfigurationError::InvalidModelCapabilities)
        })
        .transpose()?
        .unwrap_or(false);
    if supported && adapter != ModelAdapter::Anthropic {
        return Err(HubModelConfigurationError::InvalidModelCapabilities);
    }
    Ok(supported)
}

pub(super) fn record_reasoning_replay_family(
    table: &Table,
    adapter: ModelAdapter,
    provider_model: &str,
    families: &mut BTreeMap<String, Option<String>>,
) -> Result<Option<String>, HubModelConfigurationError> {
    let family = table
        .get("reasoning_replay_family")
        .map(|item| {
            let value = item
                .as_str()
                .ok_or(HubModelConfigurationError::InvalidModelCapabilities)?;
            if adapter != ModelAdapter::OpenAi || value.is_empty() || value.trim() != value {
                return Err(HubModelConfigurationError::InvalidModelCapabilities);
            }
            Ok(value.to_owned())
        })
        .transpose()?;
    if let Some(previous) = families.insert(provider_model.to_owned(), family.clone())
        && previous != family
    {
        return Err(HubModelConfigurationError::InvalidModelCapabilities);
    }
    Ok(family)
}

pub(super) fn project_runtime_model_capabilities(
    projections: Vec<RuntimeCapabilityProjection>,
    reasoning_families: BTreeMap<String, Option<String>>,
    target_provider_models: &HashMap<ResolvedProviderTarget, String>,
    target_adapters: &HashMap<ResolvedProviderTarget, ModelAdapter>,
    selectable_targets: &HashSet<ResolvedProviderTarget>,
) -> Result<RuntimeModelCapabilityCatalog, HubModelConfigurationError> {
    let mut capabilities_by_provider_model = BTreeMap::new();
    for projection in projections {
        let capabilities = runtime_model_capabilities(
            projection.adapter,
            &projection.capabilities,
            target_provider_models,
            target_adapters,
            selectable_targets,
        )?;
        if let Some(previous) =
            capabilities_by_provider_model.insert(projection.provider_model, capabilities.clone())
            && previous != capabilities
        {
            return Err(HubModelConfigurationError::InvalidModelCapabilities);
        }
    }
    for (provider_model, family) in reasoning_families {
        let capabilities = capabilities_by_provider_model
            .remove(&provider_model)
            .unwrap_or_else(|| {
                RuntimeModelCapabilities::new(BTreeSet::new(), None, BTreeSet::new())
            });
        capabilities_by_provider_model.insert(
            provider_model,
            capabilities.with_reasoning_replay_family(family),
        );
    }
    RuntimeModelCapabilityCatalog::try_from_definitions(
        capabilities_by_provider_model
            .into_iter()
            .map(|(provider_model, capabilities)| {
                RuntimeModelCapabilityDefinition::new(
                    RuntimeResolvedTarget::new(provider_model),
                    capabilities,
                )
            }),
    )
    .map_err(|_| HubModelConfigurationError::InvalidModelCapabilities)
}

fn runtime_model_capabilities(
    adapter: ModelAdapter,
    capabilities: &ModelCapabilities,
    target_provider_models: &HashMap<ResolvedProviderTarget, String>,
    target_adapters: &HashMap<ResolvedProviderTarget, ModelAdapter>,
    selectable_targets: &HashSet<ResolvedProviderTarget>,
) -> Result<RuntimeModelCapabilities, HubModelConfigurationError> {
    let reasoning_levels = capabilities
        .reasoning_levels()
        .iter()
        .copied()
        .map(runtime_reasoning_level)
        .collect();
    let fast_mode = match capabilities.fast_mode() {
        FastModeSupport::Unsupported => None,
        FastModeSupport::RequestControl => Some(RuntimeFastModeTarget::SameTarget),
        FastModeSupport::AlternateTarget(target) => {
            if selectable_targets.contains(&target) {
                return Err(HubModelConfigurationError::InvalidModelCapabilities);
            }
            let provider_model = target_provider_models
                .get(&target)
                .ok_or(HubModelConfigurationError::InvalidModelCapabilities)?;
            if target_adapters.get(&target) != Some(&adapter) {
                return Err(HubModelConfigurationError::InvalidModelCapabilities);
            }
            Some(RuntimeFastModeTarget::Mapped(RuntimeResolvedTarget::new(
                provider_model.clone(),
            )))
        }
    };
    let service_tiers = capabilities
        .service_tiers()
        .iter()
        .copied()
        .map(runtime_service_tier)
        .collect();
    Ok(RuntimeModelCapabilities::new(
        reasoning_levels,
        fast_mode,
        service_tiers,
    ))
}

pub(super) const fn runtime_reasoning_level(value: ReasoningLevel) -> RuntimeReasoningLevel {
    match value {
        ReasoningLevel::None => RuntimeReasoningLevel::None,
        ReasoningLevel::Minimal => RuntimeReasoningLevel::Minimal,
        ReasoningLevel::Low => RuntimeReasoningLevel::Low,
        ReasoningLevel::Medium => RuntimeReasoningLevel::Medium,
        ReasoningLevel::High => RuntimeReasoningLevel::High,
        ReasoningLevel::XHigh => RuntimeReasoningLevel::XHigh,
        ReasoningLevel::Max => RuntimeReasoningLevel::Max,
        ReasoningLevel::Ultra => RuntimeReasoningLevel::Ultra,
    }
}

pub(super) const fn runtime_service_tier(value: ServiceTier) -> RuntimeServiceTier {
    match value {
        ServiceTier::Anthropic(value) => RuntimeServiceTier::Anthropic(match value {
            AnthropicServiceTier::Auto => RuntimeAnthropicServiceTier::Auto,
            AnthropicServiceTier::StandardOnly => RuntimeAnthropicServiceTier::StandardOnly,
        }),
        ServiceTier::OpenAi(value) => RuntimeServiceTier::OpenAi(match value {
            signalbox_domain::OpenAiServiceTier::Auto => RuntimeOpenAiServiceTier::Auto,
            signalbox_domain::OpenAiServiceTier::Default => RuntimeOpenAiServiceTier::Default,
            signalbox_domain::OpenAiServiceTier::Flex => RuntimeOpenAiServiceTier::Flex,
            signalbox_domain::OpenAiServiceTier::Scale => RuntimeOpenAiServiceTier::Scale,
            signalbox_domain::OpenAiServiceTier::Priority => RuntimeOpenAiServiceTier::Priority,
            signalbox_domain::OpenAiServiceTier::Fast => RuntimeOpenAiServiceTier::Fast,
        }),
        ServiceTier::CodexCli(value) => RuntimeServiceTier::CodexCli(match value {
            CodexCliServiceTier::Default => RuntimeCodexCliServiceTier::Default,
            CodexCliServiceTier::Priority => RuntimeCodexCliServiceTier::Priority,
            CodexCliServiceTier::Flex => RuntimeCodexCliServiceTier::Flex,
        }),
    }
}

pub(super) fn validate_adapter_model_settings(
    adapter: ModelAdapter,
    max_output_tokens: u32,
    settings: ValidatedModelSettings,
) -> Result<(), HubModelConfigurationError> {
    let effective = settings.effective();
    let mut runtime = RuntimeModelSettings::new(max_output_tokens);
    runtime.reasoning_level = effective.reasoning_level().map(runtime_reasoning_level);
    runtime.fast_mode = match effective.fast_mode() {
        FastMode::Disabled => RuntimeFastMode::Disabled,
        FastMode::Enabled => RuntimeFastMode::Enabled,
    };
    runtime.service_tier = effective.service_tier().map(runtime_service_tier);
    let supported = match adapter {
        ModelAdapter::Anthropic => {
            signalbox_model_runtime_anthropic::validate_model_settings(&runtime)
        }
        ModelAdapter::ClaudeCli => {
            signalbox_model_runtime_claude_cli::validate_model_settings(&runtime)
        }
        ModelAdapter::CodexCli => {
            signalbox_model_runtime_codex_cli::validate_model_settings(&runtime)
        }
        ModelAdapter::OpenAi => signalbox_model_runtime_openai::validate_model_settings(&runtime),
    };
    supported.map_err(|_| HubModelConfigurationError::InvalidModelSettingsConfiguration)
}

pub(super) fn parse_model_capabilities(
    model: &Table,
    adapter: ModelAdapter,
) -> Result<ModelCapabilities, HubModelConfigurationError> {
    let reasoning_levels = optional_string_array(model, "reasoning_levels")?
        .into_iter()
        .map(|value| parse_reasoning_level(adapter, value))
        .collect::<Result<BTreeSet<_>, _>>()?;
    let fast_mode = match (
        model.get("fast_mode").and_then(Item::as_str),
        model.get("fast_target_id"),
    ) {
        (None | Some("unsupported"), None) => FastModeSupport::Unsupported,
        (Some("request_control"), None) => FastModeSupport::RequestControl,
        (Some("alternate_target"), Some(_)) => {
            FastModeSupport::AlternateTarget(ResolvedProviderTarget::naming(
                ProviderModelIdentity::from_uuid(required_uuid(model, "fast_target_id")?),
            ))
        }
        _ => return Err(HubModelConfigurationError::InvalidModelCapabilities),
    };
    let service_tiers = optional_string_array(model, "service_tiers")?
        .into_iter()
        .map(|value| parse_service_tier(adapter, value))
        .collect::<Result<BTreeSet<_>, _>>()?;
    Ok(ModelCapabilities::new(
        reasoning_levels,
        fast_mode,
        service_tiers,
    ))
}

fn optional_string_array<'a>(
    table: &'a Table,
    key: &str,
) -> Result<Vec<&'a str>, HubModelConfigurationError> {
    let Some(item) = table.get(key) else {
        return Ok(Vec::new());
    };
    let array = item
        .as_array()
        .ok_or(HubModelConfigurationError::InvalidModelCapabilities)?;
    let values = array
        .iter()
        .map(|value| {
            value
                .as_str()
                .ok_or(HubModelConfigurationError::InvalidModelCapabilities)
        })
        .collect::<Result<Vec<_>, _>>()?;
    let unique = values.iter().copied().collect::<BTreeSet<_>>();
    if unique.len() != values.len() {
        return Err(HubModelConfigurationError::InvalidModelCapabilities);
    }
    Ok(values)
}

fn parse_reasoning_level(
    adapter: ModelAdapter,
    value: &str,
) -> Result<ReasoningLevel, HubModelConfigurationError> {
    match (adapter, value) {
        (ModelAdapter::CodexCli | ModelAdapter::OpenAi, "none") => Ok(ReasoningLevel::None),
        (ModelAdapter::CodexCli | ModelAdapter::OpenAi, "minimal") => Ok(ReasoningLevel::Minimal),
        (
            ModelAdapter::Anthropic
            | ModelAdapter::ClaudeCli
            | ModelAdapter::CodexCli
            | ModelAdapter::OpenAi,
            "low",
        ) => Ok(ReasoningLevel::Low),
        (
            ModelAdapter::Anthropic
            | ModelAdapter::ClaudeCli
            | ModelAdapter::CodexCli
            | ModelAdapter::OpenAi,
            "medium",
        ) => Ok(ReasoningLevel::Medium),
        (
            ModelAdapter::Anthropic
            | ModelAdapter::ClaudeCli
            | ModelAdapter::CodexCli
            | ModelAdapter::OpenAi,
            "high",
        ) => Ok(ReasoningLevel::High),
        (
            ModelAdapter::Anthropic
            | ModelAdapter::ClaudeCli
            | ModelAdapter::CodexCli
            | ModelAdapter::OpenAi,
            "xhigh",
        ) => Ok(ReasoningLevel::XHigh),
        (
            ModelAdapter::Anthropic
            | ModelAdapter::ClaudeCli
            | ModelAdapter::CodexCli
            | ModelAdapter::OpenAi,
            "max",
        ) => Ok(ReasoningLevel::Max),
        (ModelAdapter::CodexCli, "ultra") => Ok(ReasoningLevel::Ultra),
        _ => Err(HubModelConfigurationError::InvalidModelCapabilities),
    }
}

fn parse_service_tier(
    adapter: ModelAdapter,
    value: &str,
) -> Result<ServiceTier, HubModelConfigurationError> {
    match (adapter, value) {
        (ModelAdapter::Anthropic, "auto") => Ok(ServiceTier::Anthropic(AnthropicServiceTier::Auto)),
        (ModelAdapter::Anthropic, "standard_only") => {
            Ok(ServiceTier::Anthropic(AnthropicServiceTier::StandardOnly))
        }
        (ModelAdapter::CodexCli, "default") => {
            Ok(ServiceTier::CodexCli(CodexCliServiceTier::Default))
        }
        (ModelAdapter::CodexCli, "priority") => {
            Ok(ServiceTier::CodexCli(CodexCliServiceTier::Priority))
        }
        (ModelAdapter::CodexCli, "flex") => Ok(ServiceTier::CodexCli(CodexCliServiceTier::Flex)),
        (ModelAdapter::OpenAi, "auto") => Ok(ServiceTier::OpenAi(OpenAiServiceTier::Auto)),
        (ModelAdapter::OpenAi, "default") => Ok(ServiceTier::OpenAi(OpenAiServiceTier::Default)),
        (ModelAdapter::OpenAi, "flex") => Ok(ServiceTier::OpenAi(OpenAiServiceTier::Flex)),
        (ModelAdapter::OpenAi, "scale") => Ok(ServiceTier::OpenAi(OpenAiServiceTier::Scale)),
        (ModelAdapter::OpenAi, "priority") => Ok(ServiceTier::OpenAi(OpenAiServiceTier::Priority)),
        (ModelAdapter::OpenAi, "fast") => Ok(ServiceTier::OpenAi(OpenAiServiceTier::Fast)),
        _ => Err(HubModelConfigurationError::InvalidModelCapabilities),
    }
}
