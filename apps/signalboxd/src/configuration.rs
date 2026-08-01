//! Deployment-owned model mappings and credential delivery.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    error::Error,
    fmt, fs, io,
    path::{Path, PathBuf},
    str::FromStr,
    sync::Arc,
};

use signalbox_domain::{
    DirectModelSelection, FrozenAliasDefinition, ModelAlias, ModelSelectionRequest,
    ModelTargetCatalog, ModelTargetDefinition, ProviderModelIdentity, ResolvedProviderTarget,
    ToolApprovalPosture, ToolName,
};
use signalbox_model_provider_runtime::{RuntimeModelCatalog, RuntimeModelDefinition};
use signalbox_model_runtime::{
    CredentialAccess, CredentialAccessError, CredentialAccessFailure, CredentialReference,
    CredentialValue,
};
use signalbox_model_runtime_codex_cli::{
    CodexCliConfig, CodexCliConstructionError, CodexCliRuntime,
};
use signalbox_persistence::{
    ModelCredentialFamilyCatalog, SessionCredentialPin, SessionModelCredential,
};
use signalbox_process_protocol::MAX_MODEL_ALIAS_CATALOG_ENTRIES;
use signalbox_tools_basic::WebFetchEgressPolicy;
use signalbox_tools_github::{GITHUB_CREDENTIAL_REFERENCE, GitHubEgressPolicy};
use toml_edit::{DocumentMut, Item, Table};
use uuid::Uuid;

/// Non-secret reference pinned into every Anthropic operation.
pub const ANTHROPIC_CREDENTIAL_REFERENCE: &str = "anthropic-primary";

/// Non-secret reference naming the deployment-selected ambient Codex login.
pub const CODEX_CLI_CREDENTIAL_REFERENCE: &str = "codex-subscription-primary";

const MIGRATED_ANTHROPIC_MODEL_FAMILY: &str = "anthropic";

/// Adapter implementations this daemon build can construct.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ModelAdapter {
    /// Anthropic's HTTP API adapter.
    Anthropic,
    /// The subscription-authenticated Codex CLI adapter.
    CodexCli,
}

impl ModelAdapter {
    fn parse(value: &str) -> Result<Self, HubModelConfigurationError> {
        match value {
            "anthropic" => Ok(Self::Anthropic),
            "codex_cli" => Ok(Self::CodexCli),
            _ => Err(HubModelConfigurationError::UnsupportedAdapter {
                adapter: Arc::from(value),
            }),
        }
    }

    fn credential_profile(self) -> &'static str {
        match self {
            Self::Anthropic => ANTHROPIC_CREDENTIAL_REFERENCE,
            Self::CodexCli => CODEX_CLI_CREDENTIAL_REFERENCE,
        }
    }
}

/// Validated deployment paths used to construct the Codex CLI adapter.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CodexCliConfiguration {
    executable: PathBuf,
    working_directory: PathBuf,
}

impl CodexCliConfiguration {
    /// Absolute Codex executable path.
    pub fn executable(&self) -> &Path {
        &self.executable
    }

    /// Absolute existing working directory used for CLI execution.
    pub fn working_directory(&self) -> &Path {
        &self.working_directory
    }
}

/// One model's fully resolved static delivery route.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedModelRoute {
    model_family: Arc<str>,
    adapter: ModelAdapter,
    credential_profile: Arc<str>,
    target: ResolvedProviderTarget,
}

impl ResolvedModelRoute {
    /// Configuration-owned model family key.
    pub fn model_family(&self) -> &str {
        &self.model_family
    }

    /// Build-provided adapter selected by the mapping table.
    pub const fn adapter(&self) -> ModelAdapter {
        self.adapter
    }

    /// Reports whether this route names the Anthropic HTTP adapter.
    pub const fn uses_anthropic_adapter(&self) -> bool {
        matches!(self.adapter, ModelAdapter::Anthropic)
    }

    /// Legacy credential family admitted only while a migration event is current.
    pub fn migration_credential_family(&self) -> Option<&'static str> {
        self.uses_anthropic_adapter()
            .then_some(MIGRATED_ANTHROPIC_MODEL_FAMILY)
    }

    /// Non-secret credential profile pinned for new sessions.
    pub fn credential_profile(&self) -> &str {
        &self.credential_profile
    }

    /// Exact provider target used by domain persistence.
    pub const fn target(&self) -> ResolvedProviderTarget {
        self.target
    }
}

#[derive(Clone, Debug)]
struct AdapterMapping {
    adapter: ModelAdapter,
    credential_profile: Arc<str>,
}

/// Validated deployment dependencies injected into daemon tool families.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DaemonToolConfiguration {
    workspace_root: PathBuf,
}

impl DaemonToolConfiguration {
    /// Absolute root pinned into both workspace tool families.
    pub fn workspace_root(&self) -> &Path {
        &self.workspace_root
    }

    /// Fixed public-GitHub-only egress policy selected by the tool registry.
    pub const fn github_egress_policy(&self) -> GitHubEgressPolicy {
        GitHubEgressPolicy::github_api_only()
    }

    /// Non-secret profile shared by both GitHub-backed tool adapters.
    pub const fn github_credential_profile(&self) -> &'static str {
        GITHUB_CREDENTIAL_REFERENCE
    }
}

/// Maximum exact deployment compaction-prompt bytes.
pub const MAX_COMPACTION_PROMPT_UTF8_BYTES: usize = 1_048_576;

/// Validated static model and alias definitions used by hub composition.
#[derive(Clone, Debug)]
pub struct HubModelConfiguration {
    targets: ModelTargetCatalog,
    runtime_models: RuntimeModelCatalog,
    direct_selections: HashSet<DirectModelSelection>,
    aliases: HashMap<ModelAlias, FrozenAliasDefinition>,
    routes: HashMap<DirectModelSelection, ResolvedModelRoute>,
    provider_model_adapters: HashMap<String, ModelAdapter>,
    session_credential_pin: SessionCredentialPin,
    credential_families: ModelCredentialFamilyCatalog,
    codex_cli: Option<CodexCliConfiguration>,
    compaction_prompt: Arc<str>,
    web_fetch_egress_policy: WebFetchEgressPolicy,
    daemon_tools: Option<DaemonToolConfiguration>,
    tool_approval_postures: BTreeMap<ToolName, ToolApprovalPosture>,
    approval_judge_selection: Option<DirectModelSelection>,
}

impl HubModelConfiguration {
    /// Reads and validates the versioned static TOML document.
    pub fn read(path: &Path) -> Result<Self, HubModelConfigurationError> {
        let content = fs::read_to_string(path).map_err(|_| HubModelConfigurationError::Read)?;
        Self::parse(&content)
    }

    /// Parses one complete versioned configuration document.
    pub fn parse(content: &str) -> Result<Self, HubModelConfigurationError> {
        let document = DocumentMut::from_str(content)
            .map_err(|_| HubModelConfigurationError::InvalidDocument)?;
        reject_unknown_fields(
            document.as_table(),
            &[
                "version",
                "adapter_mappings",
                "codex_cli",
                "models",
                "aliases",
                "compaction",
                "web_fetch",
                "tool_mappings",
                "tool_approval_postures",
                "approval_judge",
            ],
        )?;
        if document.get("version").and_then(|item| item.as_integer()) != Some(1) {
            return Err(HubModelConfigurationError::UnsupportedVersion);
        }
        let compaction = document
            .get("compaction")
            .and_then(|item| item.as_table())
            .ok_or(HubModelConfigurationError::MissingCompaction)?;
        reject_unknown_fields(compaction, &["prompt"])?;
        let compaction_prompt = required_string(compaction, "prompt")?;
        if compaction_prompt.is_empty()
            || compaction_prompt.contains('\0')
            || compaction_prompt.len() > MAX_COMPACTION_PROMPT_UTF8_BYTES
        {
            return Err(HubModelConfigurationError::InvalidCompactionPrompt);
        }
        let compaction_prompt: Arc<str> = Arc::from(compaction_prompt);
        let web_fetch_egress_policy = document
            .get("web_fetch")
            .map(|item| {
                let table = item
                    .as_table()
                    .ok_or(HubModelConfigurationError::InvalidWebFetchPolicy)?;
                reject_unknown_fields(table, &["allowed_origins"])
                    .map_err(|_| HubModelConfigurationError::InvalidWebFetchPolicy)?;
                let origins = table
                    .get("allowed_origins")
                    .and_then(|item| item.as_array())
                    .ok_or(HubModelConfigurationError::InvalidWebFetchPolicy)?;
                let origins = origins
                    .iter()
                    .map(|value| {
                        value
                            .as_str()
                            .map(str::to_owned)
                            .ok_or(HubModelConfigurationError::InvalidWebFetchPolicy)
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                WebFetchEgressPolicy::try_from_allowed_origins(origins)
                    .map_err(|_| HubModelConfigurationError::InvalidWebFetchPolicy)
            })
            .transpose()?
            .unwrap_or_default();
        let daemon_tools = parse_tool_mappings(document.get("tool_mappings"))?;
        let tool_approval_postures =
            parse_tool_approval_postures(document.get("tool_approval_postures"))?;
        let approval_judge_selection = parse_approval_judge(document.get("approval_judge"))?;
        let models = document
            .get("models")
            .and_then(|item| item.as_array_of_tables())
            .ok_or(HubModelConfigurationError::MissingModels)?;
        if models.is_empty() {
            return Err(HubModelConfigurationError::MissingModels);
        }

        let mapping_tables = document
            .get("adapter_mappings")
            .and_then(|item| item.as_array_of_tables())
            .ok_or(HubModelConfigurationError::MissingAdapterMappings)?;
        if mapping_tables.is_empty() {
            return Err(HubModelConfigurationError::MissingAdapterMappings);
        }
        let mut mappings = HashMap::<Arc<str>, AdapterMapping>::new();
        let mut session_credentials = Vec::with_capacity(mapping_tables.len());
        for mapping in mapping_tables {
            reject_unknown_fields(mapping, &["model_family", "adapter", "credential_profile"])?;
            let family = validated_name(required_string(mapping, "model_family")?)?;
            let adapter = ModelAdapter::parse(required_string(mapping, "adapter")?)?;
            let credential_profile =
                validated_name(required_string(mapping, "credential_profile")?)?;
            if credential_profile.as_ref() != adapter.credential_profile() {
                return Err(HubModelConfigurationError::UnknownCredentialProfile {
                    adapter,
                    credential_profile,
                });
            }
            let entry = AdapterMapping {
                adapter,
                credential_profile: Arc::clone(&credential_profile),
            };
            if mappings.contains_key(&family) {
                return Err(HubModelConfigurationError::DuplicateModelFamily {
                    model_family: family,
                });
            }
            mappings.insert(Arc::clone(&family), entry);
            session_credentials.push(SessionModelCredential::new(family, credential_profile));
        }
        let session_credential_pin = SessionCredentialPin::try_new(session_credentials)
            .map_err(|_| HubModelConfigurationError::InvalidField)?;

        let codex_cli = document
            .get("codex_cli")
            .map(|item| {
                let table = item
                    .as_table()
                    .ok_or(HubModelConfigurationError::InvalidCodexCliConfiguration)?;
                reject_unknown_fields(table, &["executable", "working_directory"])?;
                let executable = PathBuf::from(required_string(table, "executable")?);
                let working_directory = PathBuf::from(required_string(table, "working_directory")?);
                if !executable.is_absolute()
                    || !executable.is_file()
                    || !working_directory.is_absolute()
                    || !working_directory.is_dir()
                {
                    return Err(HubModelConfigurationError::InvalidCodexCliConfiguration);
                }
                Ok(CodexCliConfiguration {
                    executable,
                    working_directory,
                })
            })
            .transpose()?;
        if mappings
            .values()
            .any(|mapping| mapping.adapter == ModelAdapter::CodexCli)
            && codex_cli.is_none()
        {
            return Err(HubModelConfigurationError::MissingCodexCliConfiguration);
        }
        if let Some(configuration) = codex_cli.as_ref() {
            CodexCliRuntime::new(CodexCliConfig::new(
                configuration.executable.clone(),
                configuration.working_directory.clone(),
                CredentialReference::new(CODEX_CLI_CREDENTIAL_REFERENCE),
            ))
            .map_err(|_| HubModelConfigurationError::InvalidCodexCliConfiguration)?;
        }

        let mut domain_definitions = Vec::with_capacity(models.len());
        let mut runtime_definitions = Vec::with_capacity(models.len());
        let mut direct_selections = HashSet::with_capacity(models.len());
        let mut routes = HashMap::with_capacity(models.len());
        let mut provider_model_adapters = HashMap::with_capacity(models.len());
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
            let target = ResolvedProviderTarget::naming(ProviderModelIdentity::from_uuid(
                required_uuid(model, "target_id")?,
            ));
            if let Some(previous) =
                provider_model_adapters.insert(provider_model.to_owned(), mapping.adapter)
                && previous != mapping.adapter
            {
                return Err(HubModelConfigurationError::ConflictingProviderModelRoute);
            }
            routes.insert(
                selection,
                ResolvedModelRoute {
                    model_family,
                    adapter: mapping.adapter,
                    credential_profile: Arc::clone(&mapping.credential_profile),
                    target,
                },
            );
            domain_definitions.push(ModelTargetDefinition::new(selection, target));
            runtime_definitions.push(
                RuntimeModelDefinition::try_new(
                    target,
                    provider_model.to_owned(),
                    max_output_tokens,
                    context_window_tokens,
                )
                .map_err(|_| HubModelConfigurationError::InvalidField)?,
            );
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
        let runtime_models = RuntimeModelCatalog::try_from_definitions(runtime_definitions)
            .map_err(|_| HubModelConfigurationError::ConflictingTarget)?;
        let credential_families =
            ModelCredentialFamilyCatalog::try_new(routes.values().map(|route| {
                (
                    route.target,
                    Arc::<str>::from(route.model_family.as_ref()),
                    route
                        .uses_anthropic_adapter()
                        .then(|| Arc::<str>::from(MIGRATED_ANTHROPIC_MODEL_FAMILY)),
                )
            }))
            .map_err(|_| HubModelConfigurationError::ConflictingTarget)?;
        Ok(Self {
            targets,
            runtime_models,
            direct_selections,
            aliases,
            routes,
            provider_model_adapters,
            session_credential_pin,
            credential_families,
            codex_cli,
            compaction_prompt,
            web_fetch_egress_policy,
            daemon_tools,
            tool_approval_postures,
            approval_judge_selection,
        })
    }

    /// Returns the immutable domain target catalog used by persistence.
    pub fn target_catalog(&self) -> ModelTargetCatalog {
        self.targets.clone()
    }

    /// Returns the exact runtime delivery catalog used by the provider bridge.
    pub fn runtime_model_catalog(&self) -> RuntimeModelCatalog {
        self.runtime_models.clone()
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

    /// Returns the adapter selected for an exact provider-native model name.
    pub fn adapter_for_provider_model(&self, provider_model: &str) -> Option<ModelAdapter> {
        self.provider_model_adapters.get(provider_model).copied()
    }

    /// Returns the complete credential snapshot pinned into a new session.
    pub fn session_credential_pin(&self) -> SessionCredentialPin {
        self.session_credential_pin.clone()
    }

    /// Maps each exact target to the family key stored in session snapshots.
    pub fn credential_family_catalog(&self) -> ModelCredentialFamilyCatalog {
        self.credential_families.clone()
    }

    /// Returns validated Codex CLI paths when that adapter is configured.
    pub fn codex_cli(&self) -> Option<&CodexCliConfiguration> {
        self.codex_cli.as_ref()
    }

    pub(crate) fn codex_cli_runtime(
        &self,
    ) -> Result<Option<CodexCliRuntime>, CodexCliConstructionError> {
        self.codex_cli
            .as_ref()
            .map(|configuration| {
                CodexCliRuntime::new(CodexCliConfig::new(
                    configuration.executable.clone(),
                    configuration.working_directory.clone(),
                    CredentialReference::new(CODEX_CLI_CREDENTIAL_REFERENCE),
                ))
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

    /// Returns the exact configured compaction system prompt.
    pub fn compaction_prompt(&self) -> &str {
        &self.compaction_prompt
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

    /// Selects the configured judge or defaults to the judged direct model.
    pub fn approval_judge_selection(&self, judged: DirectModelSelection) -> DirectModelSelection {
        self.approval_judge_selection.unwrap_or(judged)
    }

    /// Returns explicitly configured daemon tool dependencies, when present.
    pub const fn daemon_tools(&self) -> Option<&DaemonToolConfiguration> {
        self.daemon_tools.as_ref()
    }

    /// Reports whether the configuration contains one direct selection key.
    pub fn contains_selection(&self, selection: DirectModelSelection) -> bool {
        self.direct_selections.contains(&selection)
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

fn parse_tool_approval_postures(
    item: Option<&Item>,
) -> Result<BTreeMap<ToolName, ToolApprovalPosture>, HubModelConfigurationError> {
    let Some(item) = item else {
        return Ok(BTreeMap::new());
    };
    let table = item
        .as_table()
        .ok_or(HubModelConfigurationError::InvalidToolApprovalPostures)?;
    let mut postures = BTreeMap::new();
    for (name, value) in table {
        let name = ToolName::try_new(name.to_owned())
            .map_err(|_| HubModelConfigurationError::InvalidToolApprovalPostures)?;
        let posture = match value.as_str() {
            Some("auto") => ToolApprovalPosture::Auto,
            Some("delegated") => ToolApprovalPosture::Delegated,
            Some("human") => ToolApprovalPosture::Human,
            _ => return Err(HubModelConfigurationError::InvalidToolApprovalPostures),
        };
        postures.insert(name, posture);
    }
    Ok(postures)
}

fn parse_approval_judge(
    item: Option<&Item>,
) -> Result<Option<DirectModelSelection>, HubModelConfigurationError> {
    let Some(item) = item else {
        return Ok(None);
    };
    let table = item
        .as_table()
        .ok_or(HubModelConfigurationError::InvalidApprovalJudge)?;
    reject_unknown_fields(table, &["selection_id"])
        .map_err(|_| HubModelConfigurationError::InvalidApprovalJudge)?;
    let selection = required_uuid(table, "selection_id")
        .map_err(|_| HubModelConfigurationError::InvalidApprovalJudge)?;
    Ok(Some(DirectModelSelection::from_uuid(selection)))
}

fn parse_tool_mappings(
    item: Option<&Item>,
) -> Result<Option<DaemonToolConfiguration>, HubModelConfigurationError> {
    let Some(item) = item else {
        return Ok(None);
    };
    let mappings = item
        .as_array_of_tables()
        .ok_or(HubModelConfigurationError::InvalidToolMappings)?;
    if mappings.is_empty() {
        return Err(HubModelConfigurationError::InvalidToolMappings);
    }
    let mut families = HashSet::with_capacity(mappings.len());
    let mut workspace_root = None;
    for mapping in mappings {
        reject_unknown_fields(
            mapping,
            &[
                "family",
                "adapter",
                "credential_profile",
                "egress_policy",
                "workspace_root",
            ],
        )?;
        let family = required_string(mapping, "family")?;
        if !families.insert(family.to_owned()) {
            return Err(HubModelConfigurationError::DuplicateToolFamily);
        }
        match family {
            "code_host" | "github" => validate_github_tool_mapping(mapping)?,
            "workspace" => {
                validate_workspace_tool_mapping(mapping)?;
                workspace_root = Some(PathBuf::from(required_string(mapping, "workspace_root")?));
            }
            "conversations" => validate_conversation_tool_mapping(mapping)?,
            _ => return Err(HubModelConfigurationError::InvalidToolMappings),
        }
    }
    if families
        != HashSet::from([
            String::from("code_host"),
            String::from("github"),
            String::from("workspace"),
            String::from("conversations"),
        ])
    {
        return Err(HubModelConfigurationError::InvalidToolMappings);
    }
    Ok(Some(DaemonToolConfiguration {
        workspace_root: workspace_root.ok_or(HubModelConfigurationError::InvalidToolMappings)?,
    }))
}

fn validate_github_tool_mapping(mapping: &Table) -> Result<(), HubModelConfigurationError> {
    if required_string(mapping, "adapter")? != "github"
        || required_string(mapping, "credential_profile")? != GITHUB_CREDENTIAL_REFERENCE
        || required_string(mapping, "egress_policy")? != "github_api_only"
        || mapping.get("workspace_root").is_some()
    {
        return Err(HubModelConfigurationError::InvalidToolMappings);
    }
    Ok(())
}

fn validate_workspace_tool_mapping(mapping: &Table) -> Result<(), HubModelConfigurationError> {
    let root = Path::new(required_string(mapping, "workspace_root")?);
    if required_string(mapping, "adapter")? != "local"
        || !root.is_absolute()
        || mapping.get("credential_profile").is_some()
        || mapping.get("egress_policy").is_some()
    {
        return Err(HubModelConfigurationError::InvalidToolMappings);
    }
    Ok(())
}

fn validate_conversation_tool_mapping(mapping: &Table) -> Result<(), HubModelConfigurationError> {
    if required_string(mapping, "adapter")? != "application"
        || mapping.get("credential_profile").is_some()
        || mapping.get("egress_policy").is_some()
        || mapping.get("workspace_root").is_some()
    {
        return Err(HubModelConfigurationError::InvalidToolMappings);
    }
    Ok(())
}

fn validate_alias_count(count: usize) -> Result<(), HubModelConfigurationError> {
    if count > MAX_MODEL_ALIAS_CATALOG_ENTRIES {
        Err(HubModelConfigurationError::TooManyAliases)
    } else {
        Ok(())
    }
}

fn validated_name(value: &str) -> Result<Arc<str>, HubModelConfigurationError> {
    if value.is_empty() || value.trim() != value || value.contains('\0') {
        Err(HubModelConfigurationError::InvalidField)
    } else {
        Ok(Arc::from(value))
    }
}

fn reject_unknown_fields(
    table: &Table,
    allowed: &[&str],
) -> Result<(), HubModelConfigurationError> {
    if table.iter().any(|(key, _)| !allowed.contains(&key)) {
        Err(HubModelConfigurationError::UnknownField)
    } else {
        Ok(())
    }
}

fn required_string<'a>(table: &'a Table, key: &str) -> Result<&'a str, HubModelConfigurationError> {
    table
        .get(key)
        .and_then(|item| item.as_str())
        .ok_or(HubModelConfigurationError::InvalidField)
}

fn required_uuid(table: &Table, key: &str) -> Result<Uuid, HubModelConfigurationError> {
    Uuid::parse_str(required_string(table, key)?)
        .map_err(|_| HubModelConfigurationError::InvalidIdentity)
}

fn required_positive_u32(table: &Table, key: &str) -> Result<u32, HubModelConfigurationError> {
    let value = table
        .get(key)
        .and_then(|item| item.as_integer())
        .ok_or(HubModelConfigurationError::InvalidField)?;
    let value = u32::try_from(value).map_err(|_| HubModelConfigurationError::InvalidLimit)?;
    if value == 0 {
        Err(HubModelConfigurationError::InvalidLimit)
    } else {
        Ok(value)
    }
}

/// Sanitized static-configuration failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HubModelConfigurationError {
    /// The configuration file could not be read as UTF-8 text.
    Read,
    /// The content was not a TOML document.
    InvalidDocument,
    /// The document version is absent or unsupported.
    UnsupportedVersion,
    /// No nonempty model-definition array exists.
    MissingModels,
    /// No nonempty static adapter mapping table exists.
    MissingAdapterMappings,
    /// The daemon tool mapping registry was incomplete or malformed.
    InvalidToolMappings,
    /// The per-tool approval posture table was malformed.
    InvalidToolApprovalPostures,
    /// The approval-judge selection table was malformed.
    InvalidApprovalJudge,
    /// The configured approval judge names no direct model selection.
    DanglingApprovalJudgeSelection,
    /// One daemon tool family appeared more than once.
    DuplicateToolFamily,
    /// The required compaction configuration table is absent.
    MissingCompaction,
    /// An unrecognized root or table field was present.
    UnknownField,
    /// A required field had the wrong TOML type or was absent.
    InvalidField,
    /// A configured identity was not a UUID.
    InvalidIdentity,
    /// A mapping named no adapter implementation provided by this build.
    UnsupportedAdapter {
        /// Exact adapter spelling from the rejected mapping.
        adapter: Arc<str>,
    },
    /// A mapping named no credential profile provided for its adapter.
    UnknownCredentialProfile {
        /// Build-provided adapter whose profile registry was checked.
        adapter: ModelAdapter,
        /// Exact profile spelling absent from that registry.
        credential_profile: Arc<str>,
    },
    /// One model family appeared more than once in the mapping table.
    DuplicateModelFamily {
        /// Exact repeated family key.
        model_family: Arc<str>,
    },
    /// A model named no entry in the static mapping table.
    UnmappedModelFamily {
        /// Exact family key absent from the table.
        model_family: Arc<str>,
    },
    /// One provider-native model spelling was routed to different adapters.
    ConflictingProviderModelRoute,
    /// A Codex mapping exists without its required process configuration.
    MissingCodexCliConfiguration,
    /// Codex paths were malformed, relative, or named no existing directory.
    InvalidCodexCliConfiguration,
    /// The provider-native model spelling was empty or padded.
    InvalidProviderModel,
    /// An output or context token limit was zero or outside `u32`.
    InvalidLimit,
    /// The compaction prompt was empty, oversized, or contained NUL.
    InvalidCompactionPrompt,
    /// The optional web-fetch table was malformed or named an invalid origin.
    InvalidWebFetchPolicy,
    /// One direct selection appeared more than once.
    DuplicateSelection,
    /// One target was assigned conflicting runtime meanings.
    ConflictingTarget,
    /// The aliases field was not an array of tables.
    InvalidAliases,
    /// The deployment alias catalog exceeded the process-protocol bound.
    TooManyAliases,
    /// One alias appeared more than once.
    DuplicateAlias,
    /// An alias selected no configured direct model.
    DanglingAlias,
}

impl fmt::Display for HubModelConfigurationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Read => "model configuration file could not be read",
            Self::InvalidDocument => "model configuration is not valid TOML",
            Self::UnsupportedVersion => "model configuration version is unsupported",
            Self::MissingModels => "model configuration has no model definitions",
            Self::MissingAdapterMappings => "model configuration has no adapter mappings",
            Self::InvalidToolApprovalPostures => {
                "model configuration contains invalid tool approval postures"
            }
            Self::InvalidApprovalJudge => {
                "model configuration contains invalid approval judge settings"
            }
            Self::DanglingApprovalJudgeSelection => {
                "model configuration contains a dangling approval judge selection"
            }
            Self::InvalidToolMappings => {
                "model configuration contains invalid daemon tool mappings"
            }
            Self::DuplicateToolFamily => "model configuration repeats a daemon tool family",
            Self::MissingCompaction => "model configuration has no compaction settings",
            Self::UnknownField => "model configuration contains an unknown field",
            Self::InvalidField => "model configuration has a missing or mistyped field",
            Self::InvalidIdentity => "model configuration contains an invalid identity",
            Self::UnsupportedAdapter { .. } => "model configuration names an unsupported adapter",
            Self::UnknownCredentialProfile { .. } => {
                "model configuration names an unknown adapter credential profile"
            }
            Self::DuplicateModelFamily { .. } => {
                "model configuration repeats a model family mapping"
            }
            Self::UnmappedModelFamily { .. } => {
                "model configuration names an unmapped model family"
            }
            Self::ConflictingProviderModelRoute => {
                "model configuration routes one provider model to conflicting adapters"
            }
            Self::MissingCodexCliConfiguration => {
                "model configuration maps Codex CLI without Codex CLI settings"
            }
            Self::InvalidCodexCliConfiguration => {
                "model configuration contains invalid Codex CLI settings"
            }
            Self::InvalidProviderModel => "model configuration contains an invalid provider model",
            Self::InvalidLimit => "model configuration contains an invalid token limit",
            Self::InvalidCompactionPrompt => {
                "model configuration contains an invalid compaction prompt"
            }
            Self::InvalidWebFetchPolicy => {
                "model configuration contains an invalid web_fetch egress policy"
            }
            Self::DuplicateSelection => "model configuration repeats a direct selection",
            Self::ConflictingTarget => "model configuration gives one target conflicting meaning",
            Self::InvalidAliases => "model aliases are not an array of tables",
            Self::TooManyAliases => "model configuration contains too many aliases",
            Self::DuplicateAlias => "model configuration repeats an alias",
            Self::DanglingAlias => "model configuration contains a dangling alias",
        })
    }
}

impl Error for HubModelConfigurationError {}

/// Typed session-admission rejection for a model absent from the static table.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UnknownSessionModel {
    /// Exact model request that no configured entry serves.
    pub selection: ModelSelectionRequest,
}

impl fmt::Display for UnknownSessionModel {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "session model is not configured: {:?}",
            self.selection
        )
    }
}

impl Error for UnknownSessionModel {}

/// Line-termination bytes a credential file may end with. `gh auth token`,
/// `op read`, `pass`, and a shell redirect all terminate the line they write,
/// so these bytes are how the file ends rather than part of the secret.
const CREDENTIAL_LINE_TERMINATORS: [u8; 2] = *b"\n\r";

/// Narrows the bytes a credential file holds to the credential value itself by
/// dropping only trailing line termination.
///
/// Every other byte is retained exactly, including interior and leading
/// whitespace: only the terminator a writing tool appends is unambiguously not
/// the secret. A file holding nothing but terminators narrows to an empty
/// value, which the adapter boundary then refuses as unusable exactly as an
/// empty file already was.
fn credential_bytes(file_bytes: &[u8]) -> &[u8] {
    let end = file_bytes
        .iter()
        .rposition(|byte| !CREDENTIAL_LINE_TERMINATORS.contains(byte))
        .map_or(0, |last_value_byte| last_value_byte.saturating_add(1));
    &file_bytes[..end]
}

/// Credential source that rereads one deployment-owned secret file for every
/// request preparation so rotation is visible without restarting signalboxd.
#[derive(Clone)]
pub struct FileCredentialAccess {
    path: Arc<PathBuf>,
    reference: CredentialReference,
}

impl FileCredentialAccess {
    /// Binds one non-secret credential reference to one deployment file.
    pub fn new(path: PathBuf, reference: CredentialReference) -> Self {
        Self {
            path: Arc::new(path),
            reference,
        }
    }

    /// Returns the non-secret reference accepted by this source.
    pub fn credential_reference(&self) -> CredentialReference {
        self.reference.clone()
    }
}

impl fmt::Debug for FileCredentialAccess {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FileCredentialAccess")
            .field("path", &"[credential file]")
            .field("reference", &self.reference)
            .finish()
    }
}

impl CredentialAccess for FileCredentialAccess {
    async fn resolve(
        &self,
        reference: &CredentialReference,
    ) -> Result<CredentialValue, CredentialAccessError> {
        if reference != &self.reference {
            return Err(CredentialAccessError::new(
                reference.clone(),
                CredentialAccessFailure::Unmapped,
            ));
        }
        match tokio::fs::read(self.path.as_ref()).await {
            Ok(file_bytes) => Ok(CredentialValue::new(credential_bytes(&file_bytes))),
            Err(error) => Err(CredentialAccessError::new(
                reference.clone(),
                if error.kind() == io::ErrorKind::NotFound {
                    CredentialAccessFailure::Unavailable
                } else {
                    CredentialAccessFailure::Unreadable
                },
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        path::{Path, PathBuf},
        sync::Arc,
    };

    use signalbox_domain::{
        DirectModelSelection, ModelAlias, ModelSelectionRequest, ToolApprovalPosture,
    };
    use signalbox_model_runtime::{CredentialAccess, CredentialAccessFailure, CredentialReference};
    use signalbox_tools_basic::{
        CURRENT_TIME_NAME, ECHO_NAME, WEB_FETCH_NAME, WebFetchEgressPolicy,
    };
    use uuid::Uuid;

    use super::{
        ANTHROPIC_CREDENTIAL_REFERENCE, FileCredentialAccess, HubModelConfiguration,
        HubModelConfigurationError, MAX_COMPACTION_PROMPT_UTF8_BYTES,
        MIGRATED_ANTHROPIC_MODEL_FAMILY, ModelAdapter, UnknownSessionModel, credential_bytes,
        validate_alias_count,
    };

    const CONFIGURATION: &str = r#"
version = 1

[[adapter_mappings]]
model_family = "anthropic"
adapter = "anthropic"
credential_profile = "anthropic-primary"

[compaction]
prompt = "Summarize the prior conversation faithfully for continuation."

[web_fetch]
allowed_origins = ["https://example.com"]

[[tool_mappings]]
family = "code_host"
adapter = "github"
credential_profile = "github-primary"
egress_policy = "github_api_only"

[[tool_mappings]]
family = "github"
adapter = "github"
credential_profile = "github-primary"
egress_policy = "github_api_only"

[[tool_mappings]]
family = "workspace"
adapter = "local"
workspace_root = "/srv/signalbox/workspace"

[[tool_mappings]]
family = "conversations"
adapter = "application"

[[models]]
selection_id = "10000000-0000-4000-8000-000000000001"
target_id = "20000000-0000-4000-8000-000000000001"
model_family = "anthropic"
provider_model = "claude-example"
max_output_tokens = 256
context_window_tokens = 200000

[[aliases]]
alias_id = "30000000-0000-4000-8000-000000000001"
selection_id = "10000000-0000-4000-8000-000000000001"
"#;

    fn configuration_with_codex_paths(executable: &Path, working_directory: &Path) -> String {
        format!(
            r#"{CONFIGURATION}
[[adapter_mappings]]
model_family = "codex"
adapter = "codex_cli"
credential_profile = "codex-subscription-primary"

[codex_cli]
executable = "{}"
working_directory = "{}"
"#,
            executable.display(),
            working_directory.display(),
        )
    }

    fn configuration_without_tool_mappings() -> String {
        let start = CONFIGURATION
            .find("[[tool_mappings]]")
            .expect("fixture has tool mappings");
        let end = CONFIGURATION
            .find("[[models]]")
            .expect("fixture has model definitions");
        format!("{}{}", &CONFIGURATION[..start], &CONFIGURATION[end..])
    }

    #[test]
    fn configured_tool_postures_and_judge_selection_are_typed() {
        let configured = HubModelConfiguration::parse(&format!(
            r#"{CONFIGURATION}

[tool_approval_postures]
{echo} = "auto"
{current_time} = "delegated"
{web_fetch} = "human"

[approval_judge]
selection_id = "10000000-0000-4000-8000-000000000001"
"#,
            echo = ECHO_NAME,
            current_time = CURRENT_TIME_NAME,
            web_fetch = WEB_FETCH_NAME
        ))
        .expect("posture and judge settings are valid");
        let postures = configured.tool_approval_postures().collect::<Vec<_>>();
        let selected = configured
            .model_aliases()
            .next()
            .expect("fixture has one alias")
            .1;
        let judged = DirectModelSelection::from_uuid(Uuid::from_u128(2));

        assert_eq!(postures[0].0.as_str(), CURRENT_TIME_NAME);
        assert_eq!(postures[0].1, ToolApprovalPosture::Delegated);
        assert_eq!(postures[1].0.as_str(), ECHO_NAME);
        assert_eq!(postures[1].1, ToolApprovalPosture::Auto);
        assert_eq!(postures[2].0.as_str(), WEB_FETCH_NAME);
        assert_eq!(postures[2].1, ToolApprovalPosture::Human);
        assert_eq!(configured.approval_judge_selection(judged), selected);
    }

    #[test]
    fn absent_approval_settings_preserve_legacy_policy_and_same_model_judge() {
        let configured =
            HubModelConfiguration::parse(CONFIGURATION).expect("fixture configuration is valid");
        let judged = DirectModelSelection::from_uuid(Uuid::from_u128(2));
        assert_eq!(configured.tool_approval_postures().count(), 0);
        assert_eq!(configured.approval_judge_selection(judged), judged);
    }

    #[test]
    fn static_configuration_builds_correlated_domain_runtime_and_alias_mappings() {
        let configuration =
            HubModelConfiguration::parse(CONFIGURATION).expect("fixture configuration is valid");
        let selection = DirectModelSelection::from_uuid(
            Uuid::parse_str("10000000-0000-4000-8000-000000000001").expect("fixture UUID is valid"),
        );
        let alias = ModelAlias::from_uuid(
            Uuid::parse_str("30000000-0000-4000-8000-000000000001").expect("fixture UUID is valid"),
        );
        assert!(configuration.contains_selection(selection));
        assert_eq!(
            configuration.web_fetch_egress_policy(),
            WebFetchEgressPolicy::try_from_allowed_origins([String::from("https://example.com")])
                .expect("fixture egress origin is valid")
        );
        let daemon_tools = configuration
            .daemon_tools()
            .expect("fixture tool mappings are complete");
        assert_eq!(
            daemon_tools.workspace_root(),
            Path::new("/srv/signalbox/workspace")
        );
        assert_eq!(daemon_tools.github_credential_profile(), "github-primary");
        assert_eq!(
            daemon_tools.github_egress_policy().admitted_origin(),
            "https://api.github.com"
        );
        assert_eq!(
            configuration
                .resolve_alias(alias)
                .expect("fixture alias resolves")
                .selected(),
            selection
        );
        assert_eq!(
            configuration.model_aliases().collect::<Vec<_>>(),
            vec![(alias, selection)]
        );
        assert!(
            configuration
                .target_catalog()
                .resolve(signalbox_domain::FrozenModelSelection::Direct(selection))
                .is_ok()
        );
        let route = configuration
            .resolve_direct_model(selection)
            .expect("fixture selection has an adapter route");
        assert_eq!(route.adapter(), ModelAdapter::Anthropic);
        assert_eq!(
            route.migration_credential_family(),
            Some(MIGRATED_ANTHROPIC_MODEL_FAMILY)
        );
    }

    #[test]
    fn absent_tool_mappings_preserve_the_base_daemon_catalog() {
        let configuration = HubModelConfiguration::parse(&configuration_without_tool_mappings())
            .expect("model configuration without tool mappings remains parseable");

        assert_eq!(configuration.daemon_tools(), None);
    }

    #[test]
    fn tool_mapping_registry_rejects_a_duplicate_family() {
        let duplicate = format!(
            "{CONFIGURATION}\n[[tool_mappings]]\nfamily = \"github\"\nadapter = \"github\"\ncredential_profile = \"github-primary\"\negress_policy = \"github_api_only\"\n"
        );

        assert_eq!(
            HubModelConfiguration::parse(&duplicate).err(),
            Some(HubModelConfigurationError::DuplicateToolFamily)
        );
    }

    #[test]
    fn tool_mapping_registry_rejects_an_unpinned_workspace_root() {
        let relative = CONFIGURATION.replace(
            "workspace_root = \"/srv/signalbox/workspace\"",
            "workspace_root = \"relative/workspace\"",
        );

        assert_eq!(
            HubModelConfiguration::parse(&relative).err(),
            Some(HubModelConfigurationError::InvalidToolMappings)
        );
    }

    #[test]
    fn configuration_rejects_an_adapter_the_build_does_not_provide() {
        let unsupported_adapter_name = "openai_http";
        let unsupported_adapter = CONFIGURATION.replace(
            "adapter = \"anthropic\"",
            &format!("adapter = \"{unsupported_adapter_name}\""),
        );

        assert_eq!(
            HubModelConfiguration::parse(&unsupported_adapter).err(),
            Some(HubModelConfigurationError::UnsupportedAdapter {
                adapter: Arc::from(unsupported_adapter_name),
            })
        );
    }

    #[test]
    fn configuration_rejects_a_profile_the_adapter_does_not_provide() {
        let unknown_profile_name = "unknown-profile";
        let unknown_profile = CONFIGURATION.replace(
            "credential_profile = \"anthropic-primary\"",
            &format!("credential_profile = \"{unknown_profile_name}\""),
        );

        assert_eq!(
            HubModelConfiguration::parse(&unknown_profile).err(),
            Some(HubModelConfigurationError::UnknownCredentialProfile {
                adapter: ModelAdapter::Anthropic,
                credential_profile: Arc::from(unknown_profile_name),
            })
        );
    }

    #[test]
    fn configuration_rejects_models_with_no_family_mapping() {
        let unmapped_family = "codex";
        let unmapped = CONFIGURATION.replace(
            "model_family = \"anthropic\"\nprovider_model",
            &format!("model_family = \"{unmapped_family}\"\nprovider_model"),
        );

        assert_eq!(
            HubModelConfiguration::parse(&unmapped).err(),
            Some(HubModelConfigurationError::UnmappedModelFamily {
                model_family: Arc::from(unmapped_family),
            })
        );
    }

    #[test]
    fn configuration_rejects_a_missing_codex_executable() {
        let temporary = tempfile::tempdir().expect("fixture directory is available");
        let missing_executable = temporary.path().join("missing-codex");
        let configuration = configuration_with_codex_paths(&missing_executable, temporary.path());

        assert_eq!(
            HubModelConfiguration::parse(&configuration).err(),
            Some(HubModelConfigurationError::InvalidCodexCliConfiguration)
        );
    }

    #[test]
    fn configuration_rejects_a_codex_executable_that_is_not_a_file() {
        let temporary = tempfile::tempdir().expect("fixture directory is available");
        let configuration = configuration_with_codex_paths(temporary.path(), temporary.path());

        assert_eq!(
            HubModelConfiguration::parse(&configuration).err(),
            Some(HubModelConfigurationError::InvalidCodexCliConfiguration)
        );
    }

    #[test]
    fn unknown_session_model_rejection_names_the_requested_model() {
        let configuration =
            HubModelConfiguration::parse(CONFIGURATION).expect("fixture configuration is valid");
        let selection = DirectModelSelection::from_uuid(Uuid::from_u128(99));
        let request = ModelSelectionRequest::Direct(selection);

        assert_eq!(
            configuration.resolve_session_model(request),
            Err(UnknownSessionModel { selection: request })
        );
        assert!(
            UnknownSessionModel { selection: request }
                .to_string()
                .contains(&format!("{request:?}"))
        );
    }

    #[test]
    fn configuration_rejects_unknown_fields_and_dangling_aliases() {
        assert_eq!(
            HubModelConfiguration::parse(&CONFIGURATION.replace(
                "max_output_tokens = 256",
                "max_output_tokens = 256\nretry = true",
            ))
            .err(),
            Some(HubModelConfigurationError::UnknownField)
        );
        let dangling = CONFIGURATION.rsplit_once("[[aliases]]").map_or_else(
            || String::from(CONFIGURATION),
            |(prefix, _)| {
                format!(
                    "{prefix}[[aliases]]\nalias_id = \"30000000-0000-4000-8000-000000000001\"\nselection_id = \"10000000-0000-4000-8000-000000000009\"\n"
                )
            },
        );
        assert_eq!(
            HubModelConfiguration::parse(&dangling).err(),
            Some(HubModelConfigurationError::DanglingAlias)
        );
    }

    #[test]
    fn configuration_rejects_each_malformed_web_fetch_policy_shape() {
        let unknown_field = CONFIGURATION.replace(
            r#"allowed_origins = ["https://example.com"]"#,
            r#"allowed_origins = ["https://example.com"]
extra = true"#,
        );
        let non_string_origin = CONFIGURATION.replace(
            r#"allowed_origins = ["https://example.com"]"#,
            "allowed_origins = [17]",
        );
        let non_origin_url = CONFIGURATION.replace(
            r#"allowed_origins = ["https://example.com"]"#,
            r#"allowed_origins = ["https://example.com/path"]"#,
        );

        assert_eq!(
            HubModelConfiguration::parse(&unknown_field).err(),
            Some(HubModelConfigurationError::InvalidWebFetchPolicy)
        );
        assert_eq!(
            HubModelConfiguration::parse(&non_string_origin).err(),
            Some(HubModelConfigurationError::InvalidWebFetchPolicy)
        );
        assert_eq!(
            HubModelConfiguration::parse(&non_origin_url).err(),
            Some(HubModelConfigurationError::InvalidWebFetchPolicy)
        );
    }

    #[test]
    fn configuration_requires_a_positive_viable_declared_context_window() {
        let missing = CONFIGURATION.replace("\ncontext_window_tokens = 200000", "");
        assert_eq!(
            HubModelConfiguration::parse(&missing).err(),
            Some(HubModelConfigurationError::InvalidField)
        );

        let zero = CONFIGURATION.replace(
            "context_window_tokens = 200000",
            "context_window_tokens = 0",
        );
        assert_eq!(
            HubModelConfiguration::parse(&zero).err(),
            Some(HubModelConfigurationError::InvalidLimit)
        );

        let impossible_reservation =
            CONFIGURATION.replace("max_output_tokens = 256", "max_output_tokens = 200001");
        assert_eq!(
            HubModelConfiguration::parse(&impossible_reservation).err(),
            Some(HubModelConfigurationError::InvalidField)
        );
    }

    #[test]
    fn configuration_requires_one_bounded_exact_compaction_prompt() {
        let missing_table = CONFIGURATION.replace(
            "[compaction]\nprompt = \"Summarize the prior conversation faithfully for continuation.\"\n\n",
            "",
        );
        assert_eq!(
            HubModelConfiguration::parse(&missing_table).err(),
            Some(HubModelConfigurationError::MissingCompaction)
        );

        let missing_prompt = CONFIGURATION.replace(
            "prompt = \"Summarize the prior conversation faithfully for continuation.\"\n",
            "",
        );
        assert_eq!(
            HubModelConfiguration::parse(&missing_prompt).err(),
            Some(HubModelConfigurationError::InvalidField)
        );

        let empty = CONFIGURATION.replace(
            "Summarize the prior conversation faithfully for continuation.",
            "",
        );
        assert_eq!(
            HubModelConfiguration::parse(&empty).err(),
            Some(HubModelConfigurationError::InvalidCompactionPrompt)
        );

        let nul = CONFIGURATION.replace(
            "Summarize the prior conversation faithfully for continuation.",
            "contains\\u0000nul",
        );
        assert_eq!(
            HubModelConfiguration::parse(&nul).err(),
            Some(HubModelConfigurationError::InvalidCompactionPrompt)
        );

        let oversized_prompt = "x".repeat(MAX_COMPACTION_PROMPT_UTF8_BYTES + 1);
        let oversized = CONFIGURATION.replace(
            "Summarize the prior conversation faithfully for continuation.",
            &oversized_prompt,
        );
        assert_eq!(
            HubModelConfiguration::parse(&oversized).err(),
            Some(HubModelConfigurationError::InvalidCompactionPrompt)
        );
    }

    #[test]
    fn configuration_enforces_the_protocol_alias_catalog_capacity() {
        assert_eq!(
            validate_alias_count(signalbox_process_protocol::MAX_MODEL_ALIAS_CATALOG_ENTRIES),
            Ok(())
        );
        assert_eq!(
            validate_alias_count(signalbox_process_protocol::MAX_MODEL_ALIAS_CATALOG_ENTRIES + 1),
            Err(HubModelConfigurationError::TooManyAliases)
        );
    }

    /// INV-035: credential references stay scoped while paths and values stay
    /// out of errors and debug output.
    #[tokio::test]
    async fn file_credentials_are_reference_scoped_and_paths_are_redacted() {
        let source = FileCredentialAccess::new(
            PathBuf::from("/definitely/not/a/credential"),
            CredentialReference::new(ANTHROPIC_CREDENTIAL_REFERENCE),
        );
        assert_eq!(
            source
                .resolve(&CredentialReference::new("another-reference"))
                .await
                .expect_err("foreign references are rejected")
                .failure,
            CredentialAccessFailure::Unmapped
        );
        assert_eq!(
            source
                .resolve(&source.credential_reference())
                .await
                .expect_err("fixture path does not exist")
                .failure,
            CredentialAccessFailure::Unavailable
        );
        assert!(!format!("{source:?}").contains("definitely"));
    }

    /// INV-035: each operation preparation observes the file as it exists at
    /// that request, so atomic deployment replacement rotates the key without
    /// caching secret bytes in hub composition.
    #[tokio::test]
    async fn inv035_file_credentials_are_reread_for_rotation() {
        let path = std::env::temp_dir().join(format!("signalbox-credential-{}", Uuid::now_v7()));
        std::fs::write(&path, b"first-test-value").expect("fixture file is writable");
        let source = FileCredentialAccess::new(
            path.clone(),
            CredentialReference::new(ANTHROPIC_CREDENTIAL_REFERENCE),
        );
        let reference = source.credential_reference();
        assert_eq!(
            source
                .resolve(&reference)
                .await
                .expect("first fixture value resolves")
                .expose_bytes(),
            b"first-test-value"
        );
        std::fs::write(&path, b"rotated-test-value").expect("fixture file can be replaced");
        assert_eq!(
            source
                .resolve(&reference)
                .await
                .expect("rotated fixture value resolves")
                .expose_bytes(),
            b"rotated-test-value"
        );
        std::fs::remove_file(path).expect("fixture file is removable");
    }

    /// A credential-printing tool terminates the line it writes, so the
    /// terminator is how the file ends rather than part of the secret.
    #[test]
    fn credential_file_trailing_line_feed_is_not_part_of_the_value() {
        assert_eq!(
            credential_bytes(b"synthetic-token-value\n"),
            b"synthetic-token-value"
        );
    }

    /// A file written with CRLF line endings ends the same way, so both
    /// terminator bytes fall outside the value.
    #[test]
    fn credential_file_trailing_carriage_return_line_feed_is_not_part_of_the_value() {
        assert_eq!(
            credential_bytes(b"synthetic-token-value\r\n"),
            b"synthetic-token-value"
        );
    }

    /// Only trailing termination is dropped: a value carrying interior line
    /// termination is still delivered whole, so narrowing can never truncate a
    /// credential at its first line.
    #[test]
    fn credential_file_interior_line_termination_is_retained() {
        assert_eq!(
            credential_bytes(b"synthetic\ntoken\nvalue\n"),
            b"synthetic\ntoken\nvalue"
        );
    }

    /// A file holding nothing but termination narrows to an empty value, which
    /// the adapter boundary refuses exactly as it already refuses an empty
    /// file — narrowing never invents a credential.
    #[test]
    fn credential_file_of_only_line_termination_narrows_to_an_empty_value() {
        assert_eq!(credential_bytes(b"\r\n\n"), b"");
    }

    /// The narrowing is wired into the file read itself, so every adapter that
    /// resolves a reference receives the bare secret rather than the bytes the
    /// writing tool happened to leave behind.
    #[tokio::test]
    async fn file_credentials_resolve_a_terminated_file_to_the_bare_value() {
        let path = std::env::temp_dir().join(format!("signalbox-credential-{}", Uuid::now_v7()));
        std::fs::write(&path, b"synthetic-token-value\n").expect("fixture file is writable");
        let source = FileCredentialAccess::new(
            path.clone(),
            CredentialReference::new(ANTHROPIC_CREDENTIAL_REFERENCE),
        );

        let resolved = source
            .resolve(&source.credential_reference())
            .await
            .expect("fixture value resolves");

        assert_eq!(resolved.expose_bytes(), b"synthetic-token-value");
        std::fs::remove_file(path).expect("fixture file is removable");
    }
}

#[cfg(test)]
mod checked_in_example {
    use std::path::{Path, PathBuf};

    use super::HubModelConfiguration;

    fn example_path() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config/signalboxd.example.toml")
    }

    /// The checked-in operator example is the one configuration document a
    /// deployment is invited to copy, so it must satisfy the same fail-closed
    /// loader every real catalog does.
    #[test]
    fn the_example_catalog_parses_and_validates() {
        HubModelConfiguration::read(&example_path())
            .expect("the checked-in example catalog is a valid version 1 document");
    }
}
