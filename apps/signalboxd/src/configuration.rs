//! Deployment-owned model mappings and credential delivery.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    error::Error,
    fmt, fs, io,
    path::{Component, Path, PathBuf},
    str::FromStr,
    sync::Arc,
    time::Duration,
};

use rust_decimal::Decimal;
use signalbox_domain::{
    AnthropicServiceTier, BranchName, CheckConclusion, CodexCliServiceTier, DirectModelSelection,
    FastMode, FastModeOverlay, FastModeSupport, FrozenAliasDefinition, LabelName, MergeableState,
    ModelAlias, ModelCapabilities, ModelCapabilityCatalog, ModelCapabilityDefinition,
    ModelSelectionRequest, ModelSettingsOverlay, ModelSettingsPrecedence, ModelTargetCatalog,
    ModelTargetDefinition, OpenAiServiceTier, ProviderModelIdentity, ReasoningLevel,
    RepoWatchAuthorLogin, RepoWatchEventKindNameV1, RepoWatchLabelMatcher,
    RepoWatchLabelMatcherInput, RepoWatchMatcherV1, RepoWatchMatcherV1Input, RepoWatchPattern,
    RepoWatchRule, RepoWatchRuleActionV1, RepoWatchRuleId, RepoWatchSingletonScope,
    RepoWatchTemplateContextDeclaration, RepositorySlug, ResolvedProviderTarget, ServiceTier,
    SessionTemplateName, SettingOverlay, ToolApprovalPosture, ToolName, UnsupportedModelSetting,
    ValidatedModelSettings,
};
use signalbox_model_provider_runtime::{RuntimeModelCatalog, RuntimeModelDefinition};
use signalbox_model_runtime::{
    AnthropicServiceTier as RuntimeAnthropicServiceTier,
    CodexCliServiceTier as RuntimeCodexCliServiceTier, CredentialAccess, CredentialAccessError,
    CredentialAccessFailure, CredentialReference, CredentialValue, FastMode as RuntimeFastMode,
    FastModeTarget as RuntimeFastModeTarget, ModelCapabilities as RuntimeModelCapabilities,
    ModelCapabilityCatalog as RuntimeModelCapabilityCatalog,
    ModelCapabilityDefinition as RuntimeModelCapabilityDefinition,
    ModelSettings as RuntimeModelSettings, OpenAiServiceTier as RuntimeOpenAiServiceTier,
    ReasoningLevel as RuntimeReasoningLevel, ResolvedTarget as RuntimeResolvedTarget,
    ServiceTier as RuntimeServiceTier,
};
use signalbox_model_runtime_claude_cli::{
    ClaudeCliConfig, ClaudeCliConstructionError, ClaudeCliRuntime,
};
use signalbox_model_runtime_codex_cli::{
    CodexCliConfig, CodexCliConstructionError, CodexCliRuntime,
};
use signalbox_persistence::{
    ModelCredentialFamilyCatalog, SessionCredentialPin, SessionModelCredential,
    process_read::ProcessModelCallInputTokenSemantics,
};
use signalbox_process_protocol::{
    MAX_MODEL_ALIAS_CATALOG_ENTRIES, MAX_MODEL_CAPABILITY_CATALOG_ENTRIES,
    MAX_RATE_VERSION_UTF8_BYTES,
};
use signalbox_tools_git::GitIdentity;
use signalbox_tools_github::{GITHUB_CREDENTIAL_REFERENCE, GitHubEgressPolicy};
use signalbox_tools_web::WebFetchEgressPolicy;
use tokio::io::AsyncReadExt;
use toml_edit::{DocumentMut, Item, Table};
use uuid::Uuid;

/// Non-secret reference pinned into every Anthropic operation.
pub const ANTHROPIC_CREDENTIAL_REFERENCE: &str = "anthropic-primary";

/// Non-secret reference pinned into every OpenAI operation.
pub const OPENAI_CREDENTIAL_REFERENCE: &str = "openai-primary";

/// Non-secret reference naming the deployment-selected ambient Codex login.
pub const CODEX_CLI_CREDENTIAL_REFERENCE: &str = "codex-subscription-primary";

/// Non-secret reference naming the deployment-selected ambient Claude Code
/// login.
pub const CLAUDE_CLI_CREDENTIAL_REFERENCE: &str = "claude-subscription-primary";

const MIGRATED_ANTHROPIC_MODEL_FAMILY: &str = "anthropic";
const MAX_REPOSITORY_WATCH_RULES: usize = 128;
const MAX_REPOSITORY_WATCH_ACTIONS: usize = 32;

/// Adapter implementations this daemon build can construct.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ModelAdapter {
    /// Anthropic's HTTP API adapter.
    Anthropic,
    /// The Claude Code CLI adapter.
    ClaudeCli,
    /// The Codex CLI adapter.
    CodexCli,
    /// OpenAI's HTTP Chat Completions adapter.
    OpenAi,
}

impl ModelAdapter {
    fn parse(value: &str) -> Result<Self, HubModelConfigurationError> {
        match value {
            "anthropic" => Ok(Self::Anthropic),
            "claude_cli" => Ok(Self::ClaudeCli),
            "codex_cli" => Ok(Self::CodexCli),
            "openai" => Ok(Self::OpenAi),
            _ => Err(HubModelConfigurationError::UnsupportedAdapter {
                adapter: Arc::from(value),
            }),
        }
    }

    /// Reports whether this adapter's provider-stated input token count
    /// already contains the separately reported cache axes.
    ///
    /// Anthropic's Messages API and the Claude Code CLI both report input
    /// tokens exclusive of cache creation and cache reads, while the Codex
    /// CLI's total and OpenAI's `prompt_tokens` already contain them.
    pub(crate) const fn reports_cache_inclusive_input(self) -> bool {
        match self {
            Self::Anthropic | Self::ClaudeCli => false,
            Self::CodexCli | Self::OpenAi => true,
        }
    }
}

/// How one credential profile's authenticated calls are billed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BillingKind {
    /// Provider API usage is charged directly by metered token counts.
    ApiMetered,
    /// Authentication is subscription-backed; token rates are an equivalent.
    Subscription,
}

impl BillingKind {
    fn parse(value: &str) -> Result<Self, HubModelConfigurationError> {
        match value {
            "api_metered" => Ok(Self::ApiMetered),
            "subscription" => Ok(Self::Subscription),
            _ => Err(HubModelConfigurationError::InvalidBillingKind),
        }
    }
}

/// One model's versioned USD rates per million usage tokens.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelBillingRates {
    version: Arc<str>,
    input: Decimal,
    output: Decimal,
    cache_creation_input: Decimal,
    cache_read_input: Decimal,
}

impl ModelBillingRates {
    /// Exact deployment-owned rate version.
    pub fn version(&self) -> &str {
        &self.version
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ModelCallInputUsage {
    tokens: Option<u64>,
    semantics: Option<ProcessModelCallInputTokenSemantics>,
}

impl ModelCallInputUsage {
    #[cfg(test)]
    pub(crate) const fn new(
        tokens: Option<u64>,
        semantics: ProcessModelCallInputTokenSemantics,
    ) -> Self {
        Self {
            tokens,
            semantics: Some(semantics),
        }
    }

    pub(crate) const fn from_persisted(
        tokens: Option<u64>,
        semantics: Option<ProcessModelCallInputTokenSemantics>,
    ) -> Self {
        Self { tokens, semantics }
    }
}

/// One dollar figure derived from configured rates and exactly present axes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DerivedModelCallCost {
    amount_usd: Decimal,
    rate_version: Arc<str>,
    billing_kind: BillingKind,
}

impl DerivedModelCallCost {
    /// Exact decimal USD amount for the usage axes that were present.
    pub const fn amount_usd(&self) -> Decimal {
        self.amount_usd
    }

    /// Version of the configured rates used for this read-time derivation.
    pub fn rate_version(&self) -> &str {
        &self.rate_version
    }

    /// Billing kind of the credential profile pinned into the call.
    pub const fn billing_kind(&self) -> BillingKind {
        self.billing_kind
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

/// Validated deployment paths used to construct the Claude Code CLI adapter.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClaudeCliConfiguration {
    executable: PathBuf,
    mcp_bridge_executable: PathBuf,
    working_directory: PathBuf,
}

impl ClaudeCliConfiguration {
    /// Absolute Claude Code executable path.
    pub fn executable(&self) -> &Path {
        &self.executable
    }

    /// Absolute path of the adapter-owned MCP bridge executable.
    ///
    /// The bridge is a separate program the adapter spawns as Claude Code's
    /// only tool server, so the deployment names it exactly the way it names
    /// the CLI. Nothing is derived from the daemon's own image path.
    pub fn mcp_bridge_executable(&self) -> &Path {
        &self.mcp_bridge_executable
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
    git_identity: GitIdentity,
    exec_supervisor_executable: PathBuf,
}

impl DaemonToolConfiguration {
    /// Absolute root pinned into both workspace tool families.
    pub fn workspace_root(&self) -> &Path {
        &self.workspace_root
    }

    /// Explicit author and committer identity for daemon-local Git commits.
    pub const fn git_identity(&self) -> &GitIdentity {
        &self.git_identity
    }

    /// Absolute existing path of the separately packaged exec supervisor.
    pub fn exec_supervisor_executable(&self) -> &Path {
        &self.exec_supervisor_executable
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

/// Default maximum assembled source bytes for one conversation import.
pub const DEFAULT_CONVERSATION_IMPORT_MAX_SOURCE_BYTES: usize = 256 * 1024 * 1024;

const MAX_WATCHED_REPOSITORIES: usize = 128;
const MAX_SIGNAL_REVIEWERS: usize = 128;

/// One repository-specific version-one polling and credential configuration.
#[derive(Clone, Eq, PartialEq)]
pub struct WatchedRepositoryConfiguration {
    repository: RepositorySlug,
    poll_interval: Duration,
    credential_file: PathBuf,
}

impl WatchedRepositoryConfiguration {
    /// Returns the canonical repository identity authorized by this entry.
    pub const fn repository(&self) -> &RepositorySlug {
        &self.repository
    }

    /// Returns the positive interval between completed polling attempts.
    pub const fn poll_interval(&self) -> Duration {
        self.poll_interval
    }

    /// Returns the deployment-owned credential-file reference.
    pub fn credential_file(&self) -> &Path {
        &self.credential_file
    }

    /// Returns the non-secret request credential reference for this repository.
    pub fn credential_reference(&self) -> CredentialReference {
        CredentialReference::new(format!("repository-watch:{}", self.repository.as_str()))
    }
}

impl fmt::Debug for WatchedRepositoryConfiguration {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WatchedRepositoryConfiguration")
            .field("repository", &self.repository)
            .field("poll_interval", &self.poll_interval)
            .field("credential_file", &"[REDACTED REFERENCE]")
            .finish()
    }
}

/// Complete optional version-one repository-watch configuration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepositoryWatchConfiguration {
    signal_reviewers: Box<[RepoWatchAuthorLogin]>,
    repositories: Box<[WatchedRepositoryConfiguration]>,
    rules: Box<[RepoWatchRule]>,
}

impl RepositoryWatchConfiguration {
    /// Returns the exact canonical login set used for reaction ingestion.
    pub fn signal_reviewers(&self) -> &[RepoWatchAuthorLogin] {
        &self.signal_reviewers
    }

    /// Returns every independently credentialed repository task.
    pub fn repositories(&self) -> &[WatchedRepositoryConfiguration] {
        &self.repositories
    }

    /// Returns the validated structured rules in declaration order.
    pub fn rules(&self) -> &[RepoWatchRule] {
        &self.rules
    }

    /// Validates every rule against the immutable session-template catalog.
    pub fn validate_template_contexts(
        &self,
        declarations: &[RepoWatchTemplateContextDeclaration],
    ) -> Result<(), HubModelConfigurationError> {
        for rule in &self.rules {
            rule.validate_template_contexts(declarations)
                .map_err(
                    |error| HubModelConfigurationError::InvalidRepositoryWatchRule {
                        rule: rule.id().as_str().to_owned(),
                        reason: error.to_string(),
                    },
                )?;
        }
        Ok(())
    }
}

/// Validated static model and alias definitions used by hub composition.
#[derive(Clone, Debug)]
pub struct HubModelConfiguration {
    targets: ModelTargetCatalog,
    runtime_models: RuntimeModelCatalog,
    direct_selections: HashSet<DirectModelSelection>,
    aliases: HashMap<ModelAlias, FrozenAliasDefinition>,
    routes: HashMap<DirectModelSelection, ResolvedModelRoute>,
    model_capabilities: ModelCapabilityCatalog,
    runtime_model_capabilities: RuntimeModelCapabilityCatalog,
    model_settings_lower_layers: HashMap<DirectModelSelection, ModelSettingsLowerLayers>,
    billing_kinds: HashMap<Arc<str>, BillingKind>,
    billing_rates: HashMap<ResolvedProviderTarget, ModelBillingRates>,
    target_adapters: HashMap<ResolvedProviderTarget, ModelAdapter>,
    provider_model_adapters: HashMap<String, ModelAdapter>,
    session_credential_pin: SessionCredentialPin,
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
    repository_watch: Option<RepositoryWatchConfiguration>,
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

    /// Parses one complete versioned configuration document.
    pub fn parse(content: &str) -> Result<Self, HubModelConfigurationError> {
        let document = DocumentMut::from_str(content)
            .map_err(|_| HubModelConfigurationError::InvalidDocument)?;
        reject_unknown_fields(
            document.as_table(),
            &[
                "version",
                "credential_profiles",
                "adapter_mappings",
                "claude_cli",
                "codex_cli",
                "model_settings",
                "model_settings_profiles",
                "models",
                "serving_targets",
                "aliases",
                "compaction",
                "conversation_import",
                "web_fetch",
                "tool_mappings",
                "daemon_tools",
                "git_identity",
                "tool_approval_postures",
                "approval_judge",
                "repository_watch",
            ],
        )?;
        if document.get("version").and_then(|item| item.as_integer()) != Some(1) {
            return Err(HubModelConfigurationError::UnsupportedVersion);
        }
        let global_model_settings = parse_model_settings_overlay(document.get("model_settings"))?;
        let model_settings_profiles =
            parse_model_settings_profiles(document.get("model_settings_profiles"))?;
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
        let conversation_import_max_source_bytes = document
            .get("conversation_import")
            .map(|item| {
                let table = item
                    .as_table()
                    .ok_or(HubModelConfigurationError::InvalidConversationImportLimit)?;
                reject_unknown_fields(table, &["max_source_bytes"])
                    .map_err(|_| HubModelConfigurationError::InvalidConversationImportLimit)?;
                let value = table
                    .get("max_source_bytes")
                    .and_then(|item| item.as_integer())
                    .ok_or(HubModelConfigurationError::InvalidConversationImportLimit)?;
                let value = usize::try_from(value)
                    .map_err(|_| HubModelConfigurationError::InvalidConversationImportLimit)?;
                if value == 0 {
                    Err(HubModelConfigurationError::InvalidConversationImportLimit)
                } else {
                    Ok(value)
                }
            })
            .transpose()?
            .unwrap_or(DEFAULT_CONVERSATION_IMPORT_MAX_SOURCE_BYTES);
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
        let git_identity = parse_git_identity(document.get("git_identity"))?;
        let exec_supervisor_executable = parse_daemon_tool_settings(document.get("daemon_tools"))?;
        let daemon_tools = parse_tool_mappings(
            document.get("tool_mappings"),
            git_identity,
            exec_supervisor_executable,
        )?;
        let profile_tables = document
            .get("credential_profiles")
            .and_then(|item| item.as_array_of_tables())
            .ok_or(HubModelConfigurationError::MissingCredentialProfiles)?;
        if profile_tables.is_empty() {
            return Err(HubModelConfigurationError::MissingCredentialProfiles);
        }
        let mut billing_kinds = HashMap::with_capacity(profile_tables.len());
        for profile in profile_tables {
            reject_unknown_fields(profile, &["name", "billing_kind"])?;
            let name = validated_name(required_string(profile, "name")?)?;
            let billing_kind = BillingKind::parse(required_string(profile, "billing_kind")?)?;
            if billing_kinds
                .insert(Arc::clone(&name), billing_kind)
                .is_some()
            {
                return Err(HubModelConfigurationError::DuplicateCredentialProfile {
                    credential_profile: name,
                });
            }
        }
        let tool_approval_postures =
            parse_tool_approval_postures(document.get("tool_approval_postures"))?;
        let approval_judge_selection = parse_approval_judge(document.get("approval_judge"))?;
        let repository_watch = document
            .get("repository_watch")
            .map(parse_repository_watch_configuration)
            .transpose()?;
        let models = document
            .get("models")
            .and_then(|item| item.as_array_of_tables())
            .ok_or(HubModelConfigurationError::MissingModels)?;
        if models.is_empty() {
            return Err(HubModelConfigurationError::MissingModels);
        }
        validate_model_count(models.len())?;

        let mapping_tables = document
            .get("adapter_mappings")
            .and_then(|item| item.as_array_of_tables())
            .ok_or(HubModelConfigurationError::MissingAdapterMappings)?;
        if mapping_tables.is_empty() {
            return Err(HubModelConfigurationError::MissingAdapterMappings);
        }
        let mut mappings = HashMap::<Arc<str>, AdapterMapping>::new();
        let mut session_credentials = Vec::with_capacity(mapping_tables.len());
        let mut codex_cli_credential_profile = None;
        let mut claude_cli_credential_profile = None;
        for mapping in mapping_tables {
            reject_unknown_fields(mapping, &["model_family", "adapter", "credential_profile"])?;
            let family = validated_name(required_string(mapping, "model_family")?)?;
            let adapter = ModelAdapter::parse(required_string(mapping, "adapter")?)?;
            let credential_profile =
                validated_name(required_string(mapping, "credential_profile")?)?;
            if !billing_kinds.contains_key(&credential_profile)
                || (adapter == ModelAdapter::Anthropic
                    && credential_profile.as_ref() != ANTHROPIC_CREDENTIAL_REFERENCE)
                || (adapter == ModelAdapter::OpenAi
                    && credential_profile.as_ref() != OPENAI_CREDENTIAL_REFERENCE)
            {
                return Err(HubModelConfigurationError::UnknownCredentialProfile {
                    adapter,
                    credential_profile,
                });
            }
            if adapter == ModelAdapter::CodexCli {
                if codex_cli_credential_profile
                    .as_ref()
                    .is_some_and(|profile| profile != &credential_profile)
                {
                    return Err(HubModelConfigurationError::ConflictingCodexCredentialProfiles);
                }
                codex_cli_credential_profile = Some(Arc::clone(&credential_profile));
            }
            if adapter == ModelAdapter::ClaudeCli {
                if claude_cli_credential_profile
                    .as_ref()
                    .is_some_and(|profile| profile != &credential_profile)
                {
                    return Err(HubModelConfigurationError::ConflictingClaudeCredentialProfiles);
                }
                claude_cli_credential_profile = Some(Arc::clone(&credential_profile));
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
                CredentialReference::new(
                    codex_cli_credential_profile
                        .as_deref()
                        .unwrap_or(CODEX_CLI_CREDENTIAL_REFERENCE),
                ),
            ))
            .map_err(|_| HubModelConfigurationError::InvalidCodexCliConfiguration)?;
        }

        let claude_cli = document
            .get("claude_cli")
            .map(|item| {
                let table = item
                    .as_table()
                    .ok_or(HubModelConfigurationError::InvalidClaudeCliConfiguration)?;
                reject_unknown_fields(
                    table,
                    &["executable", "mcp_bridge_executable", "working_directory"],
                )?;
                let executable = PathBuf::from(required_string(table, "executable")?);
                let mcp_bridge_executable = resolved_mcp_bridge_reference(
                    required_string(table, "mcp_bridge_executable")?,
                    std::env::var_os("PATH").as_deref(),
                )?;
                let working_directory = PathBuf::from(required_string(table, "working_directory")?);
                if !executable.is_absolute()
                    || !executable.is_file()
                    || !mcp_bridge_executable.is_absolute()
                    || !mcp_bridge_executable.is_file()
                    || !working_directory.is_absolute()
                    || !working_directory.is_dir()
                {
                    return Err(HubModelConfigurationError::InvalidClaudeCliConfiguration);
                }
                Ok(ClaudeCliConfiguration {
                    executable,
                    mcp_bridge_executable,
                    working_directory,
                })
            })
            .transpose()?;
        if mappings
            .values()
            .any(|mapping| mapping.adapter == ModelAdapter::ClaudeCli)
            && claude_cli.is_none()
        {
            return Err(HubModelConfigurationError::MissingClaudeCliConfiguration);
        }
        if let Some(configuration) = claude_cli.as_ref() {
            ClaudeCliRuntime::new(ClaudeCliConfig::new(
                configuration.executable.clone(),
                configuration.mcp_bridge_executable.clone(),
                configuration.working_directory.clone(),
                CredentialReference::new(
                    claude_cli_credential_profile
                        .as_deref()
                        .unwrap_or(CLAUDE_CLI_CREDENTIAL_REFERENCE),
                ),
            ))
            .map_err(|_| HubModelConfigurationError::InvalidClaudeCliConfiguration)?;
        }

        let mut domain_definitions = Vec::with_capacity(models.len());
        let mut runtime_definitions = Vec::with_capacity(models.len());
        let mut capability_definitions = Vec::with_capacity(models.len());
        let mut model_settings_lower_layers = HashMap::with_capacity(models.len());
        let mut direct_selections = HashSet::with_capacity(models.len());
        let mut routes = HashMap::with_capacity(models.len());
        let mut target_billing_rates = HashMap::with_capacity(models.len());
        let mut target_adapters = HashMap::with_capacity(models.len());
        let mut target_provider_models = HashMap::with_capacity(models.len());
        let mut selectable_targets = HashSet::with_capacity(models.len());
        let mut provider_model_adapters = HashMap::with_capacity(models.len());
        let mut runtime_capability_projections = Vec::with_capacity(models.len());
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
                let max_output_tokens = required_positive_u32(serving_target, "max_output_tokens")?;
                let context_window_tokens =
                    required_positive_u32(serving_target, "context_window_tokens")?;
                target_provider_models.insert(target, provider_model.clone());
                target_adapters.insert(target, mapping.adapter);
                if let Some(previous) =
                    provider_model_adapters.insert(provider_model.clone(), mapping.adapter)
                    && previous != mapping.adapter
                {
                    return Err(HubModelConfigurationError::ConflictingProviderModelRoute);
                }
                runtime_definitions.push(
                    RuntimeModelDefinition::try_new(
                        target,
                        provider_model,
                        max_output_tokens,
                        context_window_tokens,
                    )
                    .map_err(|_| HubModelConfigurationError::InvalidField)?,
                );
            }
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
            &target_provider_models,
            &target_adapters,
            &selectable_targets,
        )?;
        let runtime_models = RuntimeModelCatalog::try_from_definitions(runtime_definitions)
            .map_err(|_| HubModelConfigurationError::ConflictingTarget)?;
        let billing_rates = target_billing_rates
            .into_iter()
            .filter_map(|(target, rates)| rates.map(|rates| (target, rates)))
            .collect();
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
            model_capabilities,
            runtime_model_capabilities,
            model_settings_lower_layers,
            billing_kinds,
            billing_rates,
            target_adapters,
            provider_model_adapters,
            session_credential_pin,
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
            repository_watch,
        })
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

    /// Returns targets whose provider-reported input count includes cache axes.
    pub fn cache_inclusive_input_targets(&self) -> HashSet<ResolvedProviderTarget> {
        self.target_adapters
            .iter()
            .filter_map(|(target, adapter)| {
                adapter.reports_cache_inclusive_input().then_some(*target)
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
        let billing_kind = *self.billing_kinds.get(credential_profile)?;
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
            (input_tokens, rates.input),
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
                let credential_profile = self
                    .codex_cli_credential_profile
                    .as_deref()
                    .unwrap_or(CODEX_CLI_CREDENTIAL_REFERENCE);
                let mut runtime_configuration = CodexCliConfig::new(
                    configuration.executable.clone(),
                    configuration.working_directory.clone(),
                    CredentialReference::new(credential_profile),
                );
                runtime_configuration.model_capabilities = self.runtime_model_capability_catalog();
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
                );
                runtime_configuration.model_capabilities = self.runtime_model_capability_catalog();
                ClaudeCliRuntime::new(runtime_configuration)
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

    /// Returns the maximum assembled source bytes for one conversation import.
    pub const fn conversation_import_max_source_bytes(&self) -> usize {
        self.conversation_import_max_source_bytes
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

    /// Reports whether the configuration contains one direct selection key.
    pub fn contains_selection(&self, selection: DirectModelSelection) -> bool {
        self.direct_selections.contains(&selection)
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

fn parse_repository_watch_configuration(
    item: &Item,
) -> Result<RepositoryWatchConfiguration, HubModelConfigurationError> {
    let table = item
        .as_table()
        .ok_or(HubModelConfigurationError::InvalidRepositoryWatchConfiguration)?;
    reject_unknown_fields(
        table,
        &["version", "signal_reviewers", "repositories", "rules"],
    )
    .map_err(|_| HubModelConfigurationError::InvalidRepositoryWatchConfiguration)?;
    if table.get("version").and_then(Item::as_integer) != Some(1) {
        return Err(HubModelConfigurationError::InvalidRepositoryWatchConfiguration);
    }
    let reviewer_values = table
        .get("signal_reviewers")
        .and_then(Item::as_array)
        .ok_or(HubModelConfigurationError::InvalidRepositoryWatchConfiguration)?;
    if reviewer_values.len() > MAX_SIGNAL_REVIEWERS {
        return Err(HubModelConfigurationError::InvalidRepositoryWatchConfiguration);
    }
    let mut signal_reviewers = Vec::with_capacity(reviewer_values.len());
    let mut reviewer_set = HashSet::with_capacity(reviewer_values.len());
    for value in reviewer_values {
        let login = value
            .as_str()
            .ok_or(HubModelConfigurationError::InvalidRepositoryWatchConfiguration)
            .and_then(|value| {
                RepoWatchAuthorLogin::try_new(value.to_owned())
                    .map_err(|_| HubModelConfigurationError::InvalidRepositoryWatchConfiguration)
            })?;
        if !reviewer_set.insert(login.clone()) {
            return Err(HubModelConfigurationError::DuplicateSignalReviewer);
        }
        signal_reviewers.push(login);
    }
    signal_reviewers.sort();

    let repository_tables = table
        .get("repositories")
        .and_then(Item::as_array_of_tables)
        .ok_or(HubModelConfigurationError::InvalidRepositoryWatchConfiguration)?;
    if repository_tables.is_empty() || repository_tables.len() > MAX_WATCHED_REPOSITORIES {
        return Err(HubModelConfigurationError::InvalidRepositoryWatchConfiguration);
    }
    let mut repositories = Vec::with_capacity(repository_tables.len());
    let mut repository_set = HashSet::with_capacity(repository_tables.len());
    let mut credential_file_references: Vec<PathBuf> = Vec::with_capacity(repository_tables.len());
    for repository in repository_tables {
        reject_unknown_fields(
            repository,
            &["repository", "poll_interval_seconds", "credential_file"],
        )
        .map_err(|_| HubModelConfigurationError::InvalidRepositoryWatchConfiguration)?;
        let repository_slug = RepositorySlug::try_new(
            required_string(repository, "repository")
                .map_err(|_| HubModelConfigurationError::InvalidRepositoryWatchConfiguration)?
                .to_owned(),
        )
        .map_err(|_| HubModelConfigurationError::InvalidRepositoryWatchConfiguration)?;
        if !repository_set.insert(repository_slug.clone()) {
            return Err(HubModelConfigurationError::DuplicateWatchedRepository);
        }
        let interval = repository
            .get("poll_interval_seconds")
            .and_then(Item::as_integer)
            .and_then(|value| u64::try_from(value).ok())
            .filter(|value| *value > 0)
            .ok_or(HubModelConfigurationError::InvalidRepositoryWatchConfiguration)?;
        let credential_file = PathBuf::from(
            required_string(repository, "credential_file")
                .map_err(|_| HubModelConfigurationError::InvalidRepositoryWatchConfiguration)?,
        );
        if !credential_file.is_absolute() {
            return Err(HubModelConfigurationError::InvalidRepositoryWatchConfiguration);
        }
        if credential_file
            .components()
            .any(|component| matches!(component, Component::ParentDir))
        {
            return Err(HubModelConfigurationError::InvalidRepositoryWatchConfiguration);
        }
        let resolved_credential_file = resolved_credential_file_reference(&credential_file)?;
        if credential_file_references.iter().any(|existing| {
            credential_file_references_conflict(existing, &resolved_credential_file)
        }) {
            return Err(HubModelConfigurationError::DuplicateRepositoryWatchCredentialFile);
        }
        credential_file_references.push(resolved_credential_file);
        repositories.push(WatchedRepositoryConfiguration {
            repository: repository_slug,
            poll_interval: Duration::from_secs(interval),
            credential_file,
        });
    }
    repositories.sort_by(|left, right| left.repository.cmp(&right.repository));
    let rules = parse_repository_watch_rules(table)?;
    Ok(RepositoryWatchConfiguration {
        signal_reviewers: signal_reviewers.into_boxed_slice(),
        repositories: repositories.into_boxed_slice(),
        rules: rules.into_boxed_slice(),
    })
}

fn parse_repository_watch_rules(
    table: &Table,
) -> Result<Vec<RepoWatchRule>, HubModelConfigurationError> {
    let Some(item) = table.get("rules") else {
        return Ok(Vec::new());
    };
    let tables = item
        .as_array_of_tables()
        .ok_or(HubModelConfigurationError::InvalidRepositoryWatchConfiguration)?;
    if tables.len() > MAX_REPOSITORY_WATCH_RULES {
        return Err(HubModelConfigurationError::InvalidRepositoryWatchConfiguration);
    }
    let mut rules = Vec::with_capacity(tables.len());
    let mut identities = HashSet::with_capacity(tables.len());
    for table in tables {
        reject_unknown_fields(
            table,
            &[
                "id",
                "version",
                "matcher",
                "actions",
                "singleton_per",
                "cooldown_seconds",
            ],
        )
        .map_err(|_| HubModelConfigurationError::InvalidRepositoryWatchConfiguration)?;
        if table.get("version").and_then(Item::as_integer) != Some(1) {
            return Err(HubModelConfigurationError::InvalidRepositoryWatchConfiguration);
        }
        let id = RepoWatchRuleId::try_new(
            required_string(table, "id")
                .map_err(|_| HubModelConfigurationError::InvalidRepositoryWatchConfiguration)?
                .to_owned(),
        )
        .map_err(|_| HubModelConfigurationError::InvalidRepositoryWatchConfiguration)?;
        if !identities.insert(id.clone()) {
            return Err(HubModelConfigurationError::InvalidRepositoryWatchConfiguration);
        }
        let matcher = table
            .get("matcher")
            .and_then(Item::as_table)
            .ok_or(HubModelConfigurationError::InvalidRepositoryWatchConfiguration)
            .and_then(parse_repository_watch_matcher)?;
        let actions = parse_repository_watch_actions(table)?;
        let singleton_per = match table.get("singleton_per").and_then(Item::as_str) {
            None | Some("pull_request") => RepoWatchSingletonScope::PullRequest,
            Some("stack") => RepoWatchSingletonScope::Stack,
            Some("rule") => RepoWatchSingletonScope::Rule,
            Some("repo") => RepoWatchSingletonScope::Repository,
            Some(_) => return Err(HubModelConfigurationError::InvalidRepositoryWatchConfiguration),
        };
        let cooldown = table
            .get("cooldown_seconds")
            .map(|item| {
                item.as_integer()
                    .and_then(|value| u64::try_from(value).ok())
                    .filter(|value| *value <= i64::MAX as u64)
                    .map(Duration::from_secs)
                    .ok_or(HubModelConfigurationError::InvalidRepositoryWatchConfiguration)
            })
            .transpose()?
            .unwrap_or(Duration::ZERO);
        let rule = RepoWatchRule::try_new(id.clone(), matcher, actions, singleton_per, cooldown)
            .map_err(
                |error| HubModelConfigurationError::InvalidRepositoryWatchRule {
                    rule: id.as_str().to_owned(),
                    reason: error.to_string(),
                },
            )?;
        rules.push(rule);
    }
    Ok(rules)
}

fn parse_repository_watch_matcher(
    table: &Table,
) -> Result<RepoWatchMatcherV1, HubModelConfigurationError> {
    reject_unknown_fields(
        table,
        &[
            "event_kinds",
            "repo",
            "base_branch",
            "head_branch_regex",
            "title_regex",
            "body_regex",
            "labels",
            "draft",
            "author",
            "mergeable_state",
            "conclusion",
        ],
    )
    .map_err(|_| HubModelConfigurationError::InvalidRepositoryWatchConfiguration)?;
    Ok(RepoWatchMatcherV1::new(RepoWatchMatcherV1Input {
        event_kinds: parse_event_kind_list(table.get("event_kinds"))?,
        repository: optional_repo_watch_string(table, "repo", RepositorySlug::try_new)?,
        base_branch: optional_repo_watch_string(table, "base_branch", BranchName::try_new)?,
        head_branch: optional_repo_watch_string(
            table,
            "head_branch_regex",
            RepoWatchPattern::try_new,
        )?,
        title: optional_repo_watch_string(table, "title_regex", RepoWatchPattern::try_new)?,
        body: optional_repo_watch_string(table, "body_regex", RepoWatchPattern::try_new)?,
        labels: parse_label_matcher(table.get("labels"))?,
        draft: table
            .get("draft")
            .map(|item| {
                item.as_bool()
                    .ok_or(HubModelConfigurationError::InvalidRepositoryWatchConfiguration)
            })
            .transpose()?,
        author: optional_repo_watch_string(table, "author", RepoWatchAuthorLogin::try_new)?,
        mergeable_state: parse_mergeable_state_list(table.get("mergeable_state"))?,
        conclusion: parse_conclusion_list(table.get("conclusion"))?,
    }))
}

fn optional_repo_watch_string<T>(
    table: &Table,
    key: &str,
    constructor: impl FnOnce(String) -> Result<T, signalbox_domain::RepoWatchTextError>,
) -> Result<Option<T>, HubModelConfigurationError> {
    table
        .get(key)
        .map(|item| {
            item.as_str()
                .ok_or(HubModelConfigurationError::InvalidRepositoryWatchConfiguration)
                .and_then(|value| {
                    constructor(value.to_owned()).map_err(|_| {
                        HubModelConfigurationError::InvalidRepositoryWatchConfiguration
                    })
                })
        })
        .transpose()
}

fn parse_repo_watch_any_of(
    item: Option<&Item>,
) -> Result<Option<&toml_edit::Array>, HubModelConfigurationError> {
    let Some(item) = item else {
        return Ok(None);
    };
    let table = item
        .as_table()
        .ok_or(HubModelConfigurationError::InvalidRepositoryWatchConfiguration)?;
    reject_unknown_fields(table, &["any_of"])
        .map_err(|_| HubModelConfigurationError::InvalidRepositoryWatchConfiguration)?;
    table
        .get("any_of")
        .and_then(Item::as_array)
        .map(Some)
        .ok_or(HubModelConfigurationError::InvalidRepositoryWatchConfiguration)
}

fn parse_event_kind_list(
    item: Option<&Item>,
) -> Result<Vec<RepoWatchEventKindNameV1>, HubModelConfigurationError> {
    parse_repo_watch_string_array(item, |value| match value {
        "pull_request_opened" => Some(RepoWatchEventKindNameV1::PullRequestOpened),
        "pull_request_closed" => Some(RepoWatchEventKindNameV1::PullRequestClosed),
        "pull_request_merged" => Some(RepoWatchEventKindNameV1::PullRequestMerged),
        "head_changed" => Some(RepoWatchEventKindNameV1::HeadChanged),
        "mergeable_state_changed" => Some(RepoWatchEventKindNameV1::MergeableStateChanged),
        "checks_completed" => Some(RepoWatchEventKindNameV1::ChecksCompleted),
        "check_run_completed" => Some(RepoWatchEventKindNameV1::CheckRunCompleted),
        "branch_workflow_run_completed" => {
            Some(RepoWatchEventKindNameV1::BranchWorkflowRunCompleted)
        }
        "review_submitted" => Some(RepoWatchEventKindNameV1::ReviewSubmitted),
        "thread_opened" => Some(RepoWatchEventKindNameV1::ThreadOpened),
        "thread_resolved" => Some(RepoWatchEventKindNameV1::ThreadResolved),
        "labeled" => Some(RepoWatchEventKindNameV1::Labeled),
        "unlabeled" => Some(RepoWatchEventKindNameV1::Unlabeled),
        "base_advanced" => Some(RepoWatchEventKindNameV1::BaseAdvanced),
        "reaction_changed" => Some(RepoWatchEventKindNameV1::ReactionChanged),
        _ => None,
    })
}

fn parse_label_matcher(
    item: Option<&Item>,
) -> Result<RepoWatchLabelMatcher, HubModelConfigurationError> {
    let Some(item) = item else {
        return Ok(RepoWatchLabelMatcher::default());
    };
    let table = item
        .as_table()
        .ok_or(HubModelConfigurationError::InvalidRepositoryWatchConfiguration)?;
    reject_unknown_fields(table, &["any_of", "all_of", "none_of"])
        .map_err(|_| HubModelConfigurationError::InvalidRepositoryWatchConfiguration)?;
    Ok(RepoWatchLabelMatcher::new(RepoWatchLabelMatcherInput {
        any_of: parse_repo_watch_text_array(table.get("any_of"), LabelName::try_new)?,
        all_of: parse_repo_watch_text_array(table.get("all_of"), LabelName::try_new)?,
        none_of: parse_repo_watch_text_array(table.get("none_of"), LabelName::try_new)?,
    }))
}

fn parse_mergeable_state_list(
    item: Option<&Item>,
) -> Result<Vec<MergeableState>, HubModelConfigurationError> {
    let array = parse_repo_watch_any_of(item)?;
    parse_repo_watch_array_values(array, |value| match value {
        "mergeable" => Some(MergeableState::Mergeable),
        "conflicting" => Some(MergeableState::Conflicting),
        "unknown" => Some(MergeableState::Unknown),
        _ => None,
    })
}

fn parse_conclusion_list(
    item: Option<&Item>,
) -> Result<Vec<CheckConclusion>, HubModelConfigurationError> {
    let array = parse_repo_watch_any_of(item)?;
    parse_repo_watch_array_values(array, |value| match value {
        "success" => Some(CheckConclusion::Success),
        "failure" => Some(CheckConclusion::Failure),
        "neutral" => Some(CheckConclusion::Neutral),
        "cancelled" => Some(CheckConclusion::Cancelled),
        "skipped" => Some(CheckConclusion::Skipped),
        "timed_out" => Some(CheckConclusion::TimedOut),
        "action_required" => Some(CheckConclusion::ActionRequired),
        "stale" => Some(CheckConclusion::Stale),
        "startup_failure" => Some(CheckConclusion::StartupFailure),
        _ => None,
    })
}

fn parse_repo_watch_text_array<T>(
    item: Option<&Item>,
    constructor: impl Fn(String) -> Result<T, signalbox_domain::RepoWatchTextError>,
) -> Result<Vec<T>, HubModelConfigurationError>
where
    T: Eq,
{
    parse_repo_watch_string_array(item, |value| constructor(value.to_owned()).ok())
}

fn parse_repo_watch_string_array<T>(
    item: Option<&Item>,
    parser: impl Fn(&str) -> Option<T>,
) -> Result<Vec<T>, HubModelConfigurationError>
where
    T: Eq,
{
    let Some(item) = item else {
        return Ok(Vec::new());
    };
    let array = item
        .as_array()
        .ok_or(HubModelConfigurationError::InvalidRepositoryWatchConfiguration)?;
    parse_repo_watch_array_values(Some(array), parser)
}

fn parse_repo_watch_array_values<T>(
    array: Option<&toml_edit::Array>,
    parser: impl Fn(&str) -> Option<T>,
) -> Result<Vec<T>, HubModelConfigurationError>
where
    T: Eq,
{
    let Some(array) = array else {
        return Ok(Vec::new());
    };
    let mut parsed = Vec::with_capacity(array.len());
    for value in array {
        let value = value
            .as_str()
            .and_then(&parser)
            .ok_or(HubModelConfigurationError::InvalidRepositoryWatchConfiguration)?;
        if parsed.contains(&value) {
            return Err(HubModelConfigurationError::InvalidRepositoryWatchConfiguration);
        }
        parsed.push(value);
    }
    Ok(parsed)
}

fn parse_repository_watch_actions(
    table: &Table,
) -> Result<Vec<RepoWatchRuleActionV1>, HubModelConfigurationError> {
    let actions = table
        .get("actions")
        .and_then(Item::as_array_of_tables)
        .ok_or(HubModelConfigurationError::InvalidRepositoryWatchConfiguration)?;
    if actions.is_empty() || actions.len() > MAX_REPOSITORY_WATCH_ACTIONS {
        return Err(HubModelConfigurationError::InvalidRepositoryWatchConfiguration);
    }
    actions
        .iter()
        .map(|action| {
            reject_unknown_fields(action, &["kind", "template"])
                .map_err(|_| HubModelConfigurationError::InvalidRepositoryWatchConfiguration)?;
            if required_string(action, "kind")
                .map_err(|_| HubModelConfigurationError::InvalidRepositoryWatchConfiguration)?
                != "dispatch_session"
            {
                return Err(HubModelConfigurationError::InvalidRepositoryWatchConfiguration);
            }
            let template = SessionTemplateName::try_new(
                required_string(action, "template")
                    .map_err(|_| HubModelConfigurationError::InvalidRepositoryWatchConfiguration)?
                    .to_owned(),
            )
            .map_err(|_| HubModelConfigurationError::InvalidRepositoryWatchConfiguration)?;
            Ok(RepoWatchRuleActionV1::DispatchSession { template })
        })
        .collect()
}

fn credential_file_references_conflict(left: &Path, right: &Path) -> bool {
    left == right || same_file_identity(left, right)
}

#[cfg(unix)]
fn same_file_identity(left: &Path, right: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;

    let (Ok(left), Ok(right)) = (fs::metadata(left), fs::metadata(right)) else {
        return false;
    };
    left.dev() == right.dev() && left.ino() == right.ino()
}

#[cfg(not(unix))]
fn same_file_identity(_left: &Path, _right: &Path) -> bool {
    false
}

fn resolved_credential_file_reference(path: &Path) -> Result<PathBuf, HubModelConfigurationError> {
    let mut resolved = normalize_absolute_reference(path)?;
    for _ in 0..40 {
        let mut prefix = PathBuf::new();
        let mut components = resolved.components();
        let mut replacement = None;
        while let Some(component) = components.next() {
            prefix.push(component.as_os_str());
            let metadata = match fs::symlink_metadata(&prefix) {
                Ok(metadata) => metadata,
                Err(_) => return Ok(resolved),
            };
            if !metadata.file_type().is_symlink() {
                continue;
            }
            let target = fs::read_link(&prefix)
                .map_err(|_| HubModelConfigurationError::InvalidRepositoryWatchConfiguration)?;
            let mut target = if target.is_absolute() {
                target
            } else {
                prefix
                    .parent()
                    .ok_or(HubModelConfigurationError::InvalidRepositoryWatchConfiguration)?
                    .join(target)
            };
            target.extend(components.map(|remaining| remaining.as_os_str()));
            replacement = Some(normalize_absolute_reference(&target)?);
            break;
        }
        let Some(replacement) = replacement else {
            return Ok(fs::canonicalize(&resolved).unwrap_or(resolved));
        };
        resolved = replacement;
    }
    Err(HubModelConfigurationError::InvalidRepositoryWatchConfiguration)
}

/// Resolves the configured Claude MCP bridge reference to one path.
///
/// The bridge is a program this workspace builds and a deployment installs, so
/// unlike the Claude executable it can be named the way an installed program is
/// named. Two spellings are admitted, told apart by whether the configured
/// value is a bare program name — a value equal to its own final path
/// component:
///
/// - a bare name is looked up in `search_path`, the daemon's own `PATH`, and
///   resolves to the first entry holding an executable regular file of that
///   name;
/// - any other value is a path, returned verbatim for the caller's
///   absolute-existing-file rule to judge, so a configured path never resolves
///   through `PATH` to a different program.
///
/// Only absolute search entries participate. A relative entry — including the
/// empty entry POSIX reads as the working directory — is skipped rather than
/// joined, because the resolved path is written into the MCP server
/// configuration Claude Code spawns from a working directory of its own.
fn resolved_mcp_bridge_reference(
    value: &str,
    search_path: Option<&std::ffi::OsStr>,
) -> Result<PathBuf, HubModelConfigurationError> {
    let reference = PathBuf::from(value);
    if reference.file_name() != Some(std::ffi::OsStr::new(value)) {
        return Ok(reference);
    }
    absolute_search_entries(search_path)
        .into_iter()
        .map(|entry| entry.join(value))
        .find(|candidate| is_executable_file(candidate))
        .ok_or(HubModelConfigurationError::UnresolvedClaudeMcpBridgeExecutable)
}

/// Absolute directories of one search path, in their configured order.
fn absolute_search_entries(search_path: Option<&std::ffi::OsStr>) -> Vec<PathBuf> {
    search_path
        .map(|value| {
            std::env::split_paths(value)
                .filter(|entry| entry.is_absolute())
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(unix)]
fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;

    fs::metadata(path)
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable_file(path: &Path) -> bool {
    path.is_file()
}

fn normalize_absolute_reference(path: &Path) -> Result<PathBuf, HubModelConfigurationError> {
    if !path.is_absolute() {
        return Err(HubModelConfigurationError::InvalidRepositoryWatchConfiguration);
    }
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::RootDir | Component::Prefix(_) | Component::Normal(_) => {
                normalized.push(component.as_os_str());
            }
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    return Err(HubModelConfigurationError::InvalidRepositoryWatchConfiguration);
                }
            }
        }
    }
    Ok(normalized)
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

fn parse_model_billing_rates(
    model: &Table,
) -> Result<Option<ModelBillingRates>, HubModelConfigurationError> {
    const RATE_FIELDS: [&str; 5] = [
        "rate_version",
        "input_usd_per_million_tokens",
        "output_usd_per_million_tokens",
        "cache_creation_input_usd_per_million_tokens",
        "cache_read_input_usd_per_million_tokens",
    ];
    if RATE_FIELDS.iter().all(|field| model.get(field).is_none()) {
        return Ok(None);
    }
    if RATE_FIELDS.iter().any(|field| model.get(field).is_none()) {
        return Err(HubModelConfigurationError::IncompleteBillingRates);
    }
    Ok(Some(ModelBillingRates {
        version: validated_rate_version(required_string(model, "rate_version")?)?,
        input: required_billing_rate(model, "input_usd_per_million_tokens")?,
        output: required_billing_rate(model, "output_usd_per_million_tokens")?,
        cache_creation_input: required_billing_rate(
            model,
            "cache_creation_input_usd_per_million_tokens",
        )?,
        cache_read_input: required_billing_rate(model, "cache_read_input_usd_per_million_tokens")?,
    }))
}

fn required_billing_rate(
    model: &Table,
    field: &str,
) -> Result<Decimal, HubModelConfigurationError> {
    let rate = Decimal::from_str_exact(required_string(model, field)?)
        .map_err(|_| HubModelConfigurationError::InvalidBillingRate)?;
    if rate.is_sign_negative() {
        Err(HubModelConfigurationError::InvalidBillingRate)
    } else {
        Ok(rate.normalize())
    }
}

fn validated_rate_version(value: &str) -> Result<Arc<str>, HubModelConfigurationError> {
    let version = validated_name(value)?;
    if version.len() > MAX_RATE_VERSION_UTF8_BYTES {
        Err(HubModelConfigurationError::InvalidBillingRate)
    } else {
        Ok(version)
    }
}

fn fold_reported_cost(axes: [(Option<u64>, Decimal); 4]) -> Option<Decimal> {
    const TOKENS_PER_MILLION: u64 = 1_000_000;
    let mut amount = Decimal::ZERO;
    let mut reported = false;
    for (tokens, rate) in axes {
        let Some(tokens) = tokens else {
            continue;
        };
        reported = true;
        let numerator = exact_rate_token_product(rate, tokens)?;
        let axis_cost = numerator.checked_div(Decimal::from(TOKENS_PER_MILLION))?;
        if axis_cost.checked_mul(Decimal::from(TOKENS_PER_MILLION))? != numerator {
            return None;
        }
        let next_amount = amount.checked_add(axis_cost)?;
        if next_amount.checked_sub(amount)? != axis_cost
            || next_amount.checked_sub(axis_cost)? != amount
        {
            return None;
        }
        amount = next_amount;
    }
    reported.then(|| amount.normalize())
}

fn exact_rate_token_product(rate: Decimal, tokens: u64) -> Option<Decimal> {
    let product = rate.checked_mul(Decimal::from(tokens))?;
    let scale_loss = rate.scale().checked_sub(product.scale())?;
    if scale_loss == 0 {
        return Some(product);
    }
    let mut rate_mantissa = u128::try_from(rate.mantissa()).ok()?;
    let mut token_mantissa = u128::from(tokens);
    for _ in 0..scale_loss {
        divide_product_factor(&mut rate_mantissa, &mut token_mantissa, 2)?;
        divide_product_factor(&mut rate_mantissa, &mut token_mantissa, 5)?;
    }
    let exact_mantissa = rate_mantissa.checked_mul(token_mantissa)?;
    (u128::try_from(product.mantissa()).ok()? == exact_mantissa).then_some(product)
}

fn divide_product_factor(left: &mut u128, right: &mut u128, factor: u128) -> Option<()> {
    if left.is_multiple_of(factor) {
        *left /= factor;
        Some(())
    } else if right.is_multiple_of(factor) {
        *right /= factor;
        Some(())
    } else {
        None
    }
}

fn parse_tool_mappings(
    item: Option<&Item>,
    git_identity: Option<GitIdentity>,
    exec_supervisor_executable: Option<PathBuf>,
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
        git_identity: git_identity
            .ok_or(HubModelConfigurationError::MissingGitIdentityConfiguration)?,
        exec_supervisor_executable: exec_supervisor_executable
            .ok_or(HubModelConfigurationError::MissingDaemonToolSettings)?,
    }))
}

fn parse_daemon_tool_settings(
    item: Option<&Item>,
) -> Result<Option<PathBuf>, HubModelConfigurationError> {
    let Some(item) = item else {
        return Ok(None);
    };
    let table = item
        .as_table()
        .ok_or(HubModelConfigurationError::InvalidDaemonToolSettings)?;
    reject_unknown_fields(table, &["exec_supervisor_executable"])
        .map_err(|_| HubModelConfigurationError::InvalidDaemonToolSettings)?;
    let executable = PathBuf::from(
        required_string(table, "exec_supervisor_executable")
            .map_err(|_| HubModelConfigurationError::InvalidDaemonToolSettings)?,
    );
    if !executable.is_absolute() {
        return Err(HubModelConfigurationError::InvalidDaemonToolSettings);
    }
    let executable = fs::canonicalize(executable)
        .map_err(|_| HubModelConfigurationError::InvalidDaemonToolSettings)?;
    if !executable.is_file() {
        return Err(HubModelConfigurationError::InvalidDaemonToolSettings);
    }
    Ok(Some(executable))
}

fn parse_git_identity(
    item: Option<&Item>,
) -> Result<Option<GitIdentity>, HubModelConfigurationError> {
    let Some(item) = item else {
        return Ok(None);
    };
    let table = item
        .as_table()
        .ok_or(HubModelConfigurationError::InvalidGitIdentityConfiguration)?;
    reject_unknown_fields(table, &["author_name", "author_email"])
        .map_err(|_| HubModelConfigurationError::InvalidGitIdentityConfiguration)?;
    let author_name = required_string(table, "author_name")
        .map_err(|_| HubModelConfigurationError::InvalidGitIdentityConfiguration)?;
    let author_email = required_string(table, "author_email")
        .map_err(|_| HubModelConfigurationError::InvalidGitIdentityConfiguration)?;
    GitIdentity::try_new(author_name, author_email)
        .map(Some)
        .map_err(|_| HubModelConfigurationError::InvalidGitIdentityConfiguration)
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

fn validate_model_count(count: usize) -> Result<(), HubModelConfigurationError> {
    if count > MAX_MODEL_CAPABILITY_CATALOG_ENTRIES {
        Err(HubModelConfigurationError::TooManyModels)
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

fn parse_model_settings_profiles(
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

fn parse_model_settings_overlay(
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

struct RuntimeCapabilityProjection {
    adapter: ModelAdapter,
    provider_model: String,
    capabilities: ModelCapabilities,
}

fn project_runtime_model_capabilities(
    projections: Vec<RuntimeCapabilityProjection>,
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

const fn runtime_reasoning_level(value: ReasoningLevel) -> RuntimeReasoningLevel {
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

const fn runtime_service_tier(value: ServiceTier) -> RuntimeServiceTier {
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

fn validate_adapter_model_settings(
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

fn parse_model_capabilities(
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
    /// No nonempty credential-profile billing registry exists.
    MissingCredentialProfiles,
    /// One credential profile appeared more than once.
    DuplicateCredentialProfile {
        /// Exact repeated profile name.
        credential_profile: Arc<str>,
    },
    /// A credential profile declared no supported billing kind.
    InvalidBillingKind,
    /// The daemon tool mapping registry was incomplete or malformed.
    InvalidToolMappings,
    /// Mapped daemon tools were configured without the required Git identity.
    MissingGitIdentityConfiguration,
    /// The daemon Git identity table was malformed or unsafe.
    InvalidGitIdentityConfiguration,
    /// Mapped daemon tools were configured without their process settings.
    MissingDaemonToolSettings,
    /// The daemon tool process-settings table was malformed or unsafe.
    InvalidDaemonToolSettings,
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
    /// Codex model families selected more than one credential profile.
    ConflictingCodexCredentialProfiles,
    /// A Codex mapping exists without its required process configuration.
    MissingCodexCliConfiguration,
    /// Codex paths were malformed, relative, or named no existing directory.
    InvalidCodexCliConfiguration,
    /// Claude model families selected more than one credential profile.
    ConflictingClaudeCredentialProfiles,
    /// A Claude mapping exists without its required process configuration.
    MissingClaudeCliConfiguration,
    /// Claude paths were malformed, relative, or named no existing directory.
    InvalidClaudeCliConfiguration,
    /// The named Claude MCP bridge executable is on no absolute PATH entry.
    UnresolvedClaudeMcpBridgeExecutable,
    /// The provider-native model spelling was empty or padded.
    InvalidProviderModel,
    /// Only part of a model's five-field versioned rate set was declared.
    IncompleteBillingRates,
    /// A billing rate was not a bounded nonnegative decimal string.
    InvalidBillingRate,
    /// An output or context token limit was zero or outside `u32`.
    InvalidLimit,
    /// The compaction prompt was empty, oversized, or contained NUL.
    InvalidCompactionPrompt,
    /// The optional conversation-import byte bound was absent, zero, or invalid.
    InvalidConversationImportLimit,
    /// The optional web-fetch table was malformed or named an invalid origin.
    InvalidWebFetchPolicy,
    /// The optional version-one repository-watch section was malformed.
    InvalidRepositoryWatchConfiguration,
    /// One structured repository-watch rule failed closed validation.
    InvalidRepositoryWatchRule {
        /// Stable operator-assigned rule identity.
        rule: String,
        /// Safe domain or template-validation diagnostic.
        reason: String,
    },
    /// Two repository-watch entries normalized to the same repository.
    DuplicateWatchedRepository,
    /// Two signal-reviewer spellings normalized to the same login.
    DuplicateSignalReviewer,
    /// Two watched repositories named the same credential-file reference.
    DuplicateRepositoryWatchCredentialFile,
    /// One direct selection appeared more than once.
    DuplicateSelection,
    /// One per-model settings capability record was malformed.
    InvalidModelCapabilities,
    /// A global, named-profile, or model-profile settings declaration was malformed.
    InvalidModelSettingsConfiguration,
    /// The model catalog exceeded the process-protocol capability bound.
    TooManyModels,
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
        if let Self::InvalidRepositoryWatchRule { rule, reason } = self {
            return write!(
                formatter,
                "model configuration contains invalid repository-watch rule `{rule}`: {reason}"
            );
        }
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
            Self::MissingCredentialProfiles => {
                "model configuration has no credential profile billing registry"
            }
            Self::DuplicateCredentialProfile { .. } => {
                "model configuration repeats a credential profile"
            }
            Self::InvalidBillingKind => {
                "model configuration contains an invalid credential billing kind"
            }
            Self::InvalidToolMappings => {
                "model configuration contains invalid daemon tool mappings"
            }
            Self::MissingGitIdentityConfiguration => {
                "model configuration maps daemon tools without Git identity settings"
            }
            Self::InvalidGitIdentityConfiguration => {
                "model configuration contains invalid Git identity settings"
            }
            Self::MissingDaemonToolSettings => {
                "model configuration maps daemon tools without process settings"
            }
            Self::InvalidDaemonToolSettings => {
                "model configuration contains invalid daemon tool process settings"
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
            Self::ConflictingCodexCredentialProfiles => {
                "model configuration routes Codex through conflicting credential profiles"
            }
            Self::MissingCodexCliConfiguration => {
                "model configuration maps Codex CLI without Codex CLI settings"
            }
            Self::InvalidCodexCliConfiguration => {
                "model configuration contains invalid Codex CLI settings"
            }
            Self::ConflictingClaudeCredentialProfiles => {
                "model configuration routes Claude CLI through conflicting credential profiles"
            }
            Self::MissingClaudeCliConfiguration => {
                "model configuration maps Claude CLI without Claude CLI settings"
            }
            Self::InvalidClaudeCliConfiguration => {
                "model configuration contains invalid Claude CLI settings"
            }
            Self::UnresolvedClaudeMcpBridgeExecutable => {
                "model configuration names an unresolvable Claude MCP bridge executable"
            }
            Self::InvalidProviderModel => "model configuration contains an invalid provider model",
            Self::IncompleteBillingRates => {
                "model configuration contains an incomplete model billing rate set"
            }
            Self::InvalidBillingRate => {
                "model configuration contains an invalid model billing rate"
            }
            Self::InvalidLimit => "model configuration contains an invalid token limit",
            Self::InvalidCompactionPrompt => {
                "model configuration contains an invalid compaction prompt"
            }
            Self::InvalidConversationImportLimit => {
                "model configuration contains an invalid conversation import byte limit"
            }
            Self::InvalidWebFetchPolicy => {
                "model configuration contains an invalid web_fetch egress policy"
            }
            Self::InvalidRepositoryWatchConfiguration => {
                "model configuration contains invalid repository-watch settings"
            }
            Self::InvalidRepositoryWatchRule { .. } => {
                "model configuration contains an invalid repository-watch rule"
            }
            Self::DuplicateWatchedRepository => "model configuration repeats a watched repository",
            Self::DuplicateSignalReviewer => {
                "model configuration repeats a repository-watch signal reviewer"
            }
            Self::DuplicateRepositoryWatchCredentialFile => {
                "model configuration repeats a repository-watch credential-file reference"
            }
            Self::DuplicateSelection => "model configuration repeats a direct selection",
            Self::InvalidModelCapabilities => {
                "model configuration contains invalid model capabilities"
            }
            Self::InvalidModelSettingsConfiguration => {
                "model configuration contains invalid model settings layers"
            }
            Self::TooManyModels => "model configuration contains too many models",
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
    maximum_bytes: Option<usize>,
}

impl FileCredentialAccess {
    /// Binds one non-secret credential reference to one deployment file.
    pub fn new(path: PathBuf, reference: CredentialReference) -> Self {
        Self {
            path: Arc::new(path),
            reference,
            maximum_bytes: None,
        }
    }

    pub(crate) fn new_bounded(
        path: PathBuf,
        reference: CredentialReference,
        maximum_bytes: usize,
    ) -> Self {
        Self {
            path: Arc::new(path),
            reference,
            maximum_bytes: Some(maximum_bytes),
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
        let file_bytes = match self.maximum_bytes {
            Some(maximum_bytes) => {
                read_bounded_credential_file(self.path.as_ref(), maximum_bytes).await
            }
            None => tokio::fs::read(self.path.as_ref()).await,
        };
        match file_bytes {
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

async fn read_bounded_credential_file(path: &Path, maximum_bytes: usize) -> io::Result<Vec<u8>> {
    let file = tokio::fs::File::open(path).await?;
    let read_limit = u64::try_from(maximum_bytes)
        .unwrap_or(u64::MAX)
        .saturating_add(1);
    let mut bytes = Vec::with_capacity(maximum_bytes.min(8 * 1_024));
    file.take(read_limit).read_to_end(&mut bytes).await?;
    if bytes.len() > maximum_bytes {
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "credential file exceeds its accepted byte bound",
        ))
    } else {
        Ok(bytes)
    }
}

#[cfg(test)]
mod tests {
    use std::{
        path::{Path, PathBuf},
        sync::Arc,
        time::Duration,
    };

    use rust_decimal::Decimal;
    use signalbox_domain::{
        AnthropicServiceTier, DirectModelSelection, FastMode, FastModeOverlay, MergeableState,
        ModelAlias, ModelSelectionRequest, ModelSettingSource, ModelSettingsOverlay,
        ReasoningLevel, RepoWatchDispatchContextShape, RepoWatchEventKindNameV1,
        RepoWatchSingletonScope, RepoWatchTemplateContextDeclaration, ServiceTier,
        SessionTemplateName, SettingOverlay, ToolApprovalPosture,
    };
    use signalbox_model_runtime::{CredentialAccess, CredentialAccessFailure, CredentialReference};
    use signalbox_persistence::process_read::ProcessModelCallInputTokenSemantics;
    use signalbox_tools_basic::{CURRENT_TIME_NAME, ECHO_NAME};
    use signalbox_tools_web::{WEB_FETCH_NAME, WebFetchEgressPolicy};
    use uuid::Uuid;

    use super::{
        ANTHROPIC_CREDENTIAL_REFERENCE, BillingKind, DEFAULT_CONVERSATION_IMPORT_MAX_SOURCE_BYTES,
        FileCredentialAccess, HubModelConfiguration, HubModelConfigurationError,
        MAX_COMPACTION_PROMPT_UTF8_BYTES, MIGRATED_ANTHROPIC_MODEL_FAMILY, ModelAdapter,
        ModelCallInputUsage, UnknownSessionModel, absolute_search_entries, credential_bytes,
        resolved_mcp_bridge_reference, validate_alias_count, validate_model_count,
    };

    const CODEX_SUBSCRIPTION_PROFILE: &str = "codex-subscription-primary";
    const WATCH_REPOSITORY: &str = "namespace/project";
    const SECOND_WATCH_REPOSITORY: &str = "namespace/second";
    const WATCH_CREDENTIAL_FILE: &str = "/run/credentials/repository-watch-token";
    const SECOND_WATCH_CREDENTIAL_FILE: &str = "/run/credentials/second-watch-token";
    const PARENT_COMPONENT_WATCH_CREDENTIAL_FILE: &str =
        "/run/credentials/alias/../repository-watch-token";
    const WATCH_CREDENTIAL_REFERENCE: &str = "repository-watch:namespace/project";
    const WATCH_INTERVAL_SECONDS: u64 = 90;
    const SECOND_WATCH_INTERVAL_SECONDS: u64 = 120;
    const SIGNAL_REVIEWER: &str = "signal-reviewer";
    const SECOND_SIGNAL_REVIEWER: &str = "review-bot[bot]";
    const GIT_AUTHOR_NAME: &str = "Signalbox Daemon";
    const GIT_AUTHOR_EMAIL: &str = "signalbox@example.test";
    const EXEC_SUPERVISOR_EXECUTABLE: &str = "/bin/sh";
    const PROVIDER_WATCH_REPOSITORY: &str = "Namespace/Project";
    const PROVIDER_SECOND_WATCH_REPOSITORY: &str = "Namespace/Second";
    const PROVIDER_SIGNAL_REVIEWER: &str = "Signal-Reviewer";
    const PROVIDER_SECOND_SIGNAL_REVIEWER: &str = "Review-Bot[bot]";
    const DUPLICATE_PROVIDER_WATCH_REPOSITORY: &str = "NAMESPACE/PROJECT";
    const DUPLICATE_PROVIDER_SIGNAL_REVIEWER: &str = "SIGNAL-REVIEWER";
    const RELATIVE_WATCH_CREDENTIAL_FILE: &str = "relative/watch-token";
    const WATCH_RULE_ID: &str = "merge-forward-on-conflict";
    const WATCH_TEMPLATE: &str = "merge-forward";
    const CONFIGURATION: &str = r#"
version = 1

[[credential_profiles]]
name = "anthropic-primary"
billing_kind = "api_metered"

[[credential_profiles]]
name = "codex-subscription-primary"
billing_kind = "subscription"

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

[daemon_tools]
exec_supervisor_executable = "/bin/sh"

[git_identity]
author_name = "Signalbox Daemon"
author_email = "signalbox@example.test"

[[models]]
selection_id = "10000000-0000-4000-8000-000000000001"
target_id = "20000000-0000-4000-8000-000000000001"
model_family = "anthropic"
provider_model = "claude-example"
max_output_tokens = 256
context_window_tokens = 200000
rate_version = "fixture-rates-v1"
input_usd_per_million_tokens = "3"
output_usd_per_million_tokens = "15"
cache_creation_input_usd_per_million_tokens = "3.75"
cache_read_input_usd_per_million_tokens = "0.30"

[[aliases]]
alias_id = "30000000-0000-4000-8000-000000000001"
selection_id = "10000000-0000-4000-8000-000000000001"
"#;

    const OPENAI_PROFILE: &str = "openai-primary";
    const OPENAI_MAPPING_AND_MODEL: &str = r#"
[[credential_profiles]]
name = "openai-primary"
billing_kind = "api_metered"

[[adapter_mappings]]
model_family = "openai"
adapter = "openai"
credential_profile = "openai-primary"

[[models]]
selection_id = "10000000-0000-4000-8000-00000000000e"
target_id = "20000000-0000-4000-8000-00000000000e"
model_family = "openai"
provider_model = "gpt-example"
max_output_tokens = 256
context_window_tokens = 200000
reasoning_levels = ["minimal", "medium", "xhigh"]
fast_mode = "request_control"
service_tiers = ["flex", "priority"]
"#;

    const CLAUDE_SUBSCRIPTION_PROFILE: &str = "claude-subscription-primary";
    const CLAUDE_MODEL_ENTRY: &str = r#"
[[models]]
selection_id = "10000000-0000-4000-8000-00000000000c"
target_id = "20000000-0000-4000-8000-00000000000c"
model_family = "claude_code"
provider_model = "claude-cli-example"
max_output_tokens = 256
context_window_tokens = 200000
reasoning_levels = ["high"]
"#;

    const CLAUDE_MCP_BRIDGE_NAME: &str = "signalbox-claude-mcp-bridge";

    /// A bridge name no installation holds, so a fixture naming it fails
    /// resolution because of the fixture and not the developer's own `PATH`.
    const ABSENT_MCP_BRIDGE_NAME: &str = "signalbox-synthetic-absent-mcp-bridge";

    fn synthetic_search_directory(root: &Path, name: &str) -> PathBuf {
        let directory = root.join(name);
        std::fs::create_dir(&directory).expect("fixture search entry is creatable");
        directory
    }

    fn synthetic_search_path(entries: &[&Path]) -> std::ffi::OsString {
        std::env::join_paths(entries.iter().copied()).expect("fixture search entries join")
    }

    fn synthetic_executable(directory: &Path, name: &str) -> PathBuf {
        let path = synthetic_file(directory, name);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
                .expect("fixture program is markable executable");
        }
        path
    }

    #[cfg(unix)]
    fn synthetic_unexecutable_file(directory: &Path, name: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;

        let path = synthetic_file(directory, name);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))
            .expect("fixture file is markable unexecutable");
        path
    }

    fn synthetic_file(directory: &Path, name: &str) -> PathBuf {
        let path = directory.join(name);
        std::fs::write(&path, b"").expect("fixture file is writable");
        path
    }

    fn configuration_with_claude_paths(
        executable: &Path,
        mcp_bridge_executable: &Path,
        working_directory: &Path,
    ) -> String {
        format!(
            r#"{CONFIGURATION}
[[credential_profiles]]
name = "{CLAUDE_SUBSCRIPTION_PROFILE}"
billing_kind = "subscription"

[[adapter_mappings]]
model_family = "claude_code"
adapter = "claude_cli"
credential_profile = "{CLAUDE_SUBSCRIPTION_PROFILE}"

[claude_cli]
executable = "{}"
mcp_bridge_executable = "{}"
working_directory = "{}"
"#,
            executable.display(),
            mcp_bridge_executable.display(),
            working_directory.display(),
        )
    }

    fn configuration_with_codex_paths(executable: &Path, working_directory: &Path) -> String {
        format!(
            r#"{CONFIGURATION}
[[adapter_mappings]]
model_family = "codex"
adapter = "codex_cli"
credential_profile = "{CODEX_SUBSCRIPTION_PROFILE}"

[codex_cli]
executable = "{}"
working_directory = "{}"
"#,
            executable.display(),
            working_directory.display(),
        )
    }

    fn configuration_with_api_metered_codex_model(
        executable: &Path,
        working_directory: &Path,
    ) -> String {
        format!(
            r#"{CONFIGURATION}
[[credential_profiles]]
name = "codex-api-primary"
billing_kind = "api_metered"

[[adapter_mappings]]
model_family = "codex-api"
adapter = "codex_cli"
credential_profile = "codex-api-primary"

[codex_cli]
executable = "{}"
working_directory = "{}"

[[models]]
selection_id = "10000000-0000-4000-8000-000000000002"
target_id = "20000000-0000-4000-8000-000000000002"
model_family = "codex-api"
provider_model = "gpt-example"
max_output_tokens = 256
context_window_tokens = 200000
rate_version = "fixture-codex-rates-v1"
input_usd_per_million_tokens = "1"
output_usd_per_million_tokens = "2"
cache_creation_input_usd_per_million_tokens = "3"
cache_read_input_usd_per_million_tokens = "4"
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

    fn configuration_with_repository_watch() -> String {
        format!(
            r#"{CONFIGURATION}

[repository_watch]
version = 1
signal_reviewers = ["{PROVIDER_SIGNAL_REVIEWER}", "{PROVIDER_SECOND_SIGNAL_REVIEWER}"]

[[repository_watch.repositories]]
repository = "{PROVIDER_WATCH_REPOSITORY}"
poll_interval_seconds = {WATCH_INTERVAL_SECONDS}
credential_file = "{WATCH_CREDENTIAL_FILE}"

[[repository_watch.repositories]]
repository = "{PROVIDER_SECOND_WATCH_REPOSITORY}"
poll_interval_seconds = {SECOND_WATCH_INTERVAL_SECONDS}
credential_file = "{SECOND_WATCH_CREDENTIAL_FILE}"
"#,
        )
    }

    fn configuration_with_repository_watch_rule() -> String {
        format!(
            r#"{}

[[repository_watch.rules]]
id = "{WATCH_RULE_ID}"
version = 1
singleton_per = "pull_request"
cooldown_seconds = 30

[repository_watch.rules.matcher]
event_kinds = ["mergeable_state_changed"]
repo = "{PROVIDER_WATCH_REPOSITORY}"
base_branch = "main"
head_branch_regex = "^stack/.+$"
title_regex = "^.*$"
body_regex = "^.*$"
draft = false
author = "{PROVIDER_SIGNAL_REVIEWER}"

[repository_watch.rules.matcher.labels]
any_of = ["stack"]
all_of = ["owned"]
none_of = ["hold"]

[repository_watch.rules.matcher.mergeable_state]
any_of = ["conflicting"]

[repository_watch.rules.matcher.conclusion]
any_of = []

[[repository_watch.rules.actions]]
kind = "dispatch_session"
template = "{WATCH_TEMPLATE}"
"#,
            configuration_with_repository_watch()
        )
    }

    fn watch_interval_fixture() -> Duration {
        Duration::from_secs(WATCH_INTERVAL_SECONDS)
    }

    fn judged_direct_selection_fixture() -> DirectModelSelection {
        DirectModelSelection::from_uuid(Uuid::from_u128(2))
    }

    fn configured_judge_selection_fixture() -> DirectModelSelection {
        DirectModelSelection::from_uuid(
            Uuid::parse_str("10000000-0000-4000-8000-000000000001")
                .expect("configured judge fixture UUID is valid"),
        )
    }

    #[test]
    fn configured_tool_postures_are_typed() {
        let configured = HubModelConfiguration::parse(&format!(
            r#"{CONFIGURATION}

[tool_approval_postures]
{echo} = "auto"
{current_time} = "delegated"
{web_fetch} = "human"
"#,
            echo = ECHO_NAME,
            current_time = CURRENT_TIME_NAME,
            web_fetch = WEB_FETCH_NAME
        ))
        .expect("posture settings are valid");
        let postures = configured.tool_approval_postures().collect::<Vec<_>>();

        assert_eq!(postures[0].0.as_str(), CURRENT_TIME_NAME);
        assert_eq!(postures[0].1, ToolApprovalPosture::Delegated);
        assert_eq!(postures[1].0.as_str(), ECHO_NAME);
        assert_eq!(postures[1].1, ToolApprovalPosture::Auto);
        assert_eq!(postures[2].0.as_str(), WEB_FETCH_NAME);
        assert_eq!(postures[2].1, ToolApprovalPosture::Human);
    }

    #[test]
    fn configured_judge_selection_is_typed() {
        let configured = HubModelConfiguration::parse(&format!(
            r#"{CONFIGURATION}

[approval_judge]
selection_id = "10000000-0000-4000-8000-000000000001"
"#
        ))
        .expect("judge setting is valid");
        let judged = judged_direct_selection_fixture();

        assert_eq!(
            configured.approval_judge_selection(judged),
            configured_judge_selection_fixture()
        );
    }

    #[test]
    fn absent_tool_postures_preserve_legacy_policy() {
        let configured =
            HubModelConfiguration::parse(CONFIGURATION).expect("fixture configuration is valid");

        assert_eq!(configured.tool_approval_postures().count(), 0);
    }

    #[test]
    fn absent_judge_selection_preserves_the_judged_model() {
        let configured =
            HubModelConfiguration::parse(CONFIGURATION).expect("fixture configuration is valid");
        let judged = judged_direct_selection_fixture();

        assert_eq!(configured.approval_judge_selection(judged), judged);
    }

    #[test]
    fn absent_repository_watch_configuration_starts_no_watch_tasks() {
        let configured =
            HubModelConfiguration::parse(CONFIGURATION).expect("fixture configuration is valid");

        assert_eq!(configured.repository_watch(), None);
    }

    #[test]
    fn repository_watch_normalizes_signal_reviewer_logins() {
        let configured = HubModelConfiguration::parse(&configuration_with_repository_watch())
            .expect("repository-watch fixture is valid");
        let repository_watch = configured
            .repository_watch()
            .expect("fixture configures repository watch");

        assert_eq!(
            repository_watch.signal_reviewers()[0].as_str(),
            SECOND_SIGNAL_REVIEWER
        );
        assert_eq!(
            repository_watch.signal_reviewers()[1].as_str(),
            SIGNAL_REVIEWER
        );
    }

    #[test]
    fn repository_watch_builds_a_canonical_repository_inventory() {
        let configured = HubModelConfiguration::parse(&configuration_with_repository_watch())
            .expect("repository-watch fixture is valid");
        let repositories = configured
            .repository_watch()
            .expect("fixture configures repository watch")
            .repositories();

        assert_eq!(repositories[0].repository().as_str(), WATCH_REPOSITORY);
        assert_eq!(
            repositories[1].repository().as_str(),
            SECOND_WATCH_REPOSITORY
        );
    }

    #[test]
    fn repository_watch_preserves_each_repository_interval() {
        let configured = HubModelConfiguration::parse(&configuration_with_repository_watch())
            .expect("repository-watch fixture is valid");
        let watched = &configured
            .repository_watch()
            .expect("fixture configures repository watch")
            .repositories()[0];

        assert_eq!(watched.poll_interval(), watch_interval_fixture());
    }

    #[test]
    fn repository_watch_preserves_each_credential_file_reference() {
        let configured = HubModelConfiguration::parse(&configuration_with_repository_watch())
            .expect("repository-watch fixture is valid");
        let repositories = configured
            .repository_watch()
            .expect("fixture configures repository watch")
            .repositories();

        assert_eq!(
            repositories[0].credential_file(),
            Path::new(WATCH_CREDENTIAL_FILE)
        );
        assert_eq!(
            repositories[1].credential_file(),
            Path::new(SECOND_WATCH_CREDENTIAL_FILE)
        );
    }

    #[test]
    fn repository_watch_derives_a_repository_scoped_credential_reference() {
        let configured = HubModelConfiguration::parse(&configuration_with_repository_watch())
            .expect("repository-watch fixture is valid");
        let watched = &configured
            .repository_watch()
            .expect("fixture configures repository watch")
            .repositories()[0];

        assert_eq!(
            watched.credential_reference().as_str(),
            WATCH_CREDENTIAL_REFERENCE
        );
    }

    #[test]
    fn repository_watch_debug_redacts_the_credential_file_reference() {
        let configured = HubModelConfiguration::parse(&configuration_with_repository_watch())
            .expect("repository-watch fixture is valid");
        let watched = &configured
            .repository_watch()
            .expect("fixture configures repository watch")
            .repositories()[0];
        let debug = format!("{watched:?}");

        assert!(!debug.contains(WATCH_CREDENTIAL_FILE));
        assert!(debug.contains("[REDACTED REFERENCE]"));
    }

    #[test]
    fn repository_watch_parses_the_conflict_only_live_rule() {
        let configured = HubModelConfiguration::parse(&configuration_with_repository_watch_rule())
            .expect("repository-watch rule fixture is valid");
        let rule = &configured
            .repository_watch()
            .expect("fixture configures repository watch")
            .rules()[0];

        assert_eq!(rule.id().as_str(), WATCH_RULE_ID);
        assert_eq!(rule.version().get(), 1);
        assert_eq!(rule.singleton_per(), RepoWatchSingletonScope::PullRequest);
        assert_eq!(rule.cooldown(), Duration::from_secs(30));
        assert_eq!(
            rule.matcher().event_kinds(),
            [RepoWatchEventKindNameV1::MergeableStateChanged]
        );
        assert_eq!(
            rule.matcher().mergeable_state(),
            [MergeableState::Conflicting]
        );
        assert_eq!(rule.actions()[0].template().as_str(), WATCH_TEMPLATE);
    }

    #[test]
    fn repository_watch_rule_accepts_its_declared_template_context() {
        let configured = HubModelConfiguration::parse(&configuration_with_repository_watch_rule())
            .expect("repository-watch rule fixture is valid");
        let template = SessionTemplateName::try_new(String::from(WATCH_TEMPLATE))
            .expect("template fixture name is valid");
        let declaration = RepoWatchTemplateContextDeclaration::try_new(
            template,
            vec![RepoWatchDispatchContextShape::PullRequest],
        )
        .expect("template declaration is nonempty");

        assert_eq!(
            configured
                .repository_watch()
                .expect("fixture configures repository watch")
                .validate_template_contexts(&[declaration]),
            Ok(())
        );
    }

    #[test]
    fn repository_watch_rule_rejects_a_template_context_mismatch() {
        let configured = HubModelConfiguration::parse(&configuration_with_repository_watch_rule())
            .expect("repository-watch rule fixture is valid");
        let template = SessionTemplateName::try_new(String::from(WATCH_TEMPLATE))
            .expect("template fixture name is valid");
        let declaration = RepoWatchTemplateContextDeclaration::try_new(
            template,
            vec![RepoWatchDispatchContextShape::Branch],
        )
        .expect("template declaration is nonempty");
        let error = configured
            .repository_watch()
            .expect("fixture configures repository watch")
            .validate_template_contexts(&[declaration])
            .expect_err("pull-request rule cannot target branch-only template");

        assert!(error.to_string().contains(WATCH_RULE_ID));
        assert!(error.to_string().contains(WATCH_TEMPLATE));
    }

    #[test]
    fn repository_watch_rejects_a_missing_credential_file_reference() {
        let configured = configuration_with_repository_watch().replace(
            &format!("credential_file = \"{WATCH_CREDENTIAL_FILE}\""),
            "credential_path_was_omitted = true",
        );

        assert_eq!(
            HubModelConfiguration::parse(&configured).err(),
            Some(HubModelConfigurationError::InvalidRepositoryWatchConfiguration)
        );
    }

    #[test]
    fn repository_watch_rejects_a_relative_credential_file_reference() {
        let configured = configuration_with_repository_watch()
            .replace(WATCH_CREDENTIAL_FILE, RELATIVE_WATCH_CREDENTIAL_FILE);

        assert_eq!(
            HubModelConfiguration::parse(&configured).err(),
            Some(HubModelConfigurationError::InvalidRepositoryWatchConfiguration)
        );
    }

    #[test]
    fn repository_watch_rejects_a_zero_poll_interval() {
        let configured = configuration_with_repository_watch().replace(
            &format!("poll_interval_seconds = {WATCH_INTERVAL_SECONDS}"),
            "poll_interval_seconds = 0",
        );

        assert_eq!(
            HubModelConfiguration::parse(&configured).err(),
            Some(HubModelConfigurationError::InvalidRepositoryWatchConfiguration)
        );
    }

    #[test]
    fn repository_watch_rejects_duplicate_canonical_repositories() {
        let configured = configuration_with_repository_watch().replace(
            &format!("repository = \"{PROVIDER_SECOND_WATCH_REPOSITORY}\""),
            &format!("repository = \"{DUPLICATE_PROVIDER_WATCH_REPOSITORY}\""),
        );

        assert_eq!(
            HubModelConfiguration::parse(&configured).err(),
            Some(HubModelConfigurationError::DuplicateWatchedRepository)
        );
    }

    #[test]
    fn repository_watch_rejects_duplicate_canonical_signal_reviewers() {
        let configured = configuration_with_repository_watch().replace(
            &format!(
                "signal_reviewers = [\"{PROVIDER_SIGNAL_REVIEWER}\", \"{PROVIDER_SECOND_SIGNAL_REVIEWER}\"]"
            ),
            &format!(
                "signal_reviewers = [\"{PROVIDER_SIGNAL_REVIEWER}\", \"{DUPLICATE_PROVIDER_SIGNAL_REVIEWER}\"]"
            ),
        );

        assert_eq!(
            HubModelConfiguration::parse(&configured).err(),
            Some(HubModelConfigurationError::DuplicateSignalReviewer)
        );
    }

    #[test]
    fn repository_watch_rejects_a_shared_credential_file_reference() {
        let configured = configuration_with_repository_watch()
            .replace(SECOND_WATCH_CREDENTIAL_FILE, WATCH_CREDENTIAL_FILE);

        assert_eq!(
            HubModelConfiguration::parse(&configured).err(),
            Some(HubModelConfigurationError::DuplicateRepositoryWatchCredentialFile)
        );
    }

    #[test]
    fn repository_watch_rejects_a_shared_credential_file_alias() {
        let directory = tempfile::tempdir().expect("the credential fixture directory exists");
        let credential = directory.path().join("watch-token");
        std::fs::write(&credential, []).expect("the credential fixture exists");
        let alias = directory.path().join("watch-token-alias");
        std::os::unix::fs::symlink(&credential, &alias).expect("the credential alias exists");
        let configured = configuration_with_repository_watch()
            .replace(WATCH_CREDENTIAL_FILE, &credential.display().to_string())
            .replace(SECOND_WATCH_CREDENTIAL_FILE, &alias.display().to_string());

        assert_eq!(
            HubModelConfiguration::parse(&configured).err(),
            Some(HubModelConfigurationError::DuplicateRepositoryWatchCredentialFile)
        );
    }

    #[cfg(unix)]
    #[test]
    fn repository_watch_rejects_a_shared_hard_linked_credential_file() {
        let directory = tempfile::tempdir().expect("the credential fixture directory exists");
        let credential = directory.path().join("watch-token");
        std::fs::write(&credential, []).expect("the credential fixture exists");
        let hard_link = directory.path().join("watch-token-hard-link");
        std::fs::hard_link(&credential, &hard_link).expect("the credential hard link exists");
        let configured = configuration_with_repository_watch()
            .replace(WATCH_CREDENTIAL_FILE, &credential.display().to_string())
            .replace(
                SECOND_WATCH_CREDENTIAL_FILE,
                &hard_link.display().to_string(),
            );

        assert_eq!(
            HubModelConfiguration::parse(&configured).err(),
            Some(HubModelConfigurationError::DuplicateRepositoryWatchCredentialFile)
        );
    }

    #[test]
    fn repository_watch_rejects_a_dangling_shared_credential_file_alias() {
        let directory = tempfile::tempdir().expect("the credential fixture directory exists");
        let credential = directory.path().join("watch-token");
        let alias = directory.path().join("watch-token-alias");
        std::os::unix::fs::symlink(&credential, &alias).expect("the credential alias exists");
        let configured = configuration_with_repository_watch()
            .replace(WATCH_CREDENTIAL_FILE, &credential.display().to_string())
            .replace(SECOND_WATCH_CREDENTIAL_FILE, &alias.display().to_string());

        assert_eq!(
            HubModelConfiguration::parse(&configured).err(),
            Some(HubModelConfigurationError::DuplicateRepositoryWatchCredentialFile)
        );
    }

    #[test]
    fn repository_watch_rejects_a_dangling_intermediate_credential_alias() {
        let directory = tempfile::tempdir().expect("the credential fixture directory exists");
        let target_directory = directory.path().join("pending-target");
        let alias_directory = directory.path().join("pending-alias");
        std::os::unix::fs::symlink(&target_directory, &alias_directory)
            .expect("the intermediate credential alias exists");
        let credential = target_directory.join("watch-token");
        let alias = alias_directory.join("watch-token");
        let configured = configuration_with_repository_watch()
            .replace(WATCH_CREDENTIAL_FILE, &credential.display().to_string())
            .replace(SECOND_WATCH_CREDENTIAL_FILE, &alias.display().to_string());

        assert_eq!(
            HubModelConfiguration::parse(&configured).err(),
            Some(HubModelConfigurationError::DuplicateRepositoryWatchCredentialFile)
        );
    }

    #[test]
    fn repository_watch_defers_an_unreadable_credential_file_reference() {
        use std::os::unix::fs::PermissionsExt as _;

        let directory = tempfile::tempdir().expect("the credential fixture directory exists");
        let credential = directory.path().join("watch-token");
        let configured = configuration_with_repository_watch()
            .replace(WATCH_CREDENTIAL_FILE, &credential.display().to_string());
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o000))
            .expect("the credential fixture directory becomes unreadable");

        let parsed = HubModelConfiguration::parse(&configured);

        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700))
            .expect("the credential fixture directory becomes removable");
        parsed.expect("credential readability is deferred until request preparation");
    }

    #[test]
    fn repository_watch_rejects_parent_components_in_credential_paths() {
        let configured = configuration_with_repository_watch().replace(
            SECOND_WATCH_CREDENTIAL_FILE,
            PARENT_COMPONENT_WATCH_CREDENTIAL_FILE,
        );

        assert_eq!(
            HubModelConfiguration::parse(&configured).err(),
            Some(HubModelConfigurationError::InvalidRepositoryWatchConfiguration)
        );
    }

    #[test]
    fn repository_watch_rejects_unknown_repository_fields() {
        let configured = configuration_with_repository_watch().replace(
            &format!("credential_file = \"{WATCH_CREDENTIAL_FILE}\""),
            &format!(
                "credential_file = \"{WATCH_CREDENTIAL_FILE}\"\nwebhook_secret_file = \"/not-v1\""
            ),
        );

        assert_eq!(
            HubModelConfiguration::parse(&configured).err(),
            Some(HubModelConfigurationError::InvalidRepositoryWatchConfiguration)
        );
    }

    #[test]
    fn repository_watch_rejects_an_unsupported_version() {
        let configured = configuration_with_repository_watch().replace(
            "[repository_watch]\nversion = 1",
            "[repository_watch]\nversion = 2",
        );

        assert_eq!(
            HubModelConfiguration::parse(&configured).err(),
            Some(HubModelConfigurationError::InvalidRepositoryWatchConfiguration)
        );
    }

    #[test]
    fn tool_approval_postures_reject_a_non_table_shape() {
        let configured = CONFIGURATION.replacen(
            "version = 1",
            "version = 1\ntool_approval_postures = \"delegated\"",
            1,
        );

        assert_eq!(
            HubModelConfiguration::parse(&configured).err(),
            Some(HubModelConfigurationError::InvalidToolApprovalPostures)
        );
    }

    #[test]
    fn tool_approval_postures_reject_an_invalid_tool_name_key() {
        let configured = format!("{CONFIGURATION}\n[tool_approval_postures]\n\"\" = \"auto\"\n");

        assert_eq!(
            HubModelConfiguration::parse(&configured).err(),
            Some(HubModelConfigurationError::InvalidToolApprovalPostures)
        );
    }

    #[test]
    fn tool_approval_postures_reject_an_unknown_posture() {
        let configured = format!("{CONFIGURATION}\n[tool_approval_postures]\necho = \"ask\"\n");

        assert_eq!(
            HubModelConfiguration::parse(&configured).err(),
            Some(HubModelConfigurationError::InvalidToolApprovalPostures)
        );
    }

    #[test]
    fn tool_approval_postures_reject_a_non_string_posture() {
        let configured = format!("{CONFIGURATION}\n[tool_approval_postures]\necho = true\n");

        assert_eq!(
            HubModelConfiguration::parse(&configured).err(),
            Some(HubModelConfigurationError::InvalidToolApprovalPostures)
        );
    }

    #[test]
    fn approval_judge_rejects_a_non_table_shape() {
        let configured =
            CONFIGURATION.replacen("version = 1", "version = 1\napproval_judge = \"same\"", 1);

        assert_eq!(
            HubModelConfiguration::parse(&configured).err(),
            Some(HubModelConfigurationError::InvalidApprovalJudge)
        );
    }

    #[test]
    fn approval_judge_rejects_an_unknown_field() {
        let configured = format!(
            "{CONFIGURATION}\n[approval_judge]\nselection_id = \"10000000-0000-4000-8000-000000000001\"\nextra = true\n"
        );

        assert_eq!(
            HubModelConfiguration::parse(&configured).err(),
            Some(HubModelConfigurationError::InvalidApprovalJudge)
        );
    }

    #[test]
    fn approval_judge_rejects_a_malformed_selection_identity() {
        let configured =
            format!("{CONFIGURATION}\n[approval_judge]\nselection_id = \"not-a-uuid\"\n");

        assert_eq!(
            HubModelConfiguration::parse(&configured).err(),
            Some(HubModelConfigurationError::InvalidApprovalJudge)
        );
    }

    #[test]
    fn approval_judge_rejects_an_unconfigured_direct_selection() {
        let configured = format!(
            "{CONFIGURATION}\n[approval_judge]\nselection_id = \"10000000-0000-4000-8000-000000000002\"\n"
        );

        assert_eq!(
            HubModelConfiguration::parse(&configured).err(),
            Some(HubModelConfigurationError::DanglingApprovalJudgeSelection)
        );
    }

    #[test]
    fn conversation_import_bound_defaults_to_256_mib() {
        let configuration = HubModelConfiguration::parse(CONFIGURATION)
            .expect("the canonical configuration is valid");

        assert_eq!(
            configuration.conversation_import_max_source_bytes(),
            DEFAULT_CONVERSATION_IMPORT_MAX_SOURCE_BYTES
        );
    }

    #[test]
    fn conversation_import_bound_accepts_an_explicit_positive_byte_count() {
        let max_source_bytes = 1_048_576;
        let configured = CONFIGURATION.replace(
            "[compaction]",
            &format!(
                "[conversation_import]\nmax_source_bytes = {max_source_bytes}\n\n[compaction]"
            ),
        );
        let configuration =
            HubModelConfiguration::parse(&configured).expect("the explicit import bound is valid");

        assert_eq!(
            configuration.conversation_import_max_source_bytes(),
            max_source_bytes
        );
    }

    #[test]
    fn conversation_import_bound_rejects_zero() {
        let configured = CONFIGURATION.replace(
            "[compaction]",
            "[conversation_import]\nmax_source_bytes = 0\n\n[compaction]",
        );

        assert_eq!(
            HubModelConfiguration::parse(&configured).err(),
            Some(HubModelConfigurationError::InvalidConversationImportLimit)
        );
    }

    #[test]
    fn conversation_import_bound_rejects_unknown_fields() {
        let configured = CONFIGURATION.replace(
            "[compaction]",
            "[conversation_import]\nmax_source_bytes = 1048576\nextra = 1\n\n[compaction]",
        );

        assert_eq!(
            HubModelConfiguration::parse(&configured).err(),
            Some(HubModelConfigurationError::InvalidConversationImportLimit)
        );
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
        let expected_exec_supervisor = std::fs::canonicalize(EXEC_SUPERVISOR_EXECUTABLE)
            .expect("fixture supervisor path has a canonical target");
        assert_eq!(
            daemon_tools.workspace_root(),
            Path::new("/srv/signalbox/workspace")
        );
        assert_eq!(daemon_tools.github_credential_profile(), "github-primary");
        assert_eq!(daemon_tools.git_identity().name(), GIT_AUTHOR_NAME);
        assert_eq!(daemon_tools.git_identity().email(), GIT_AUTHOR_EMAIL);
        assert_eq!(
            daemon_tools.exec_supervisor_executable(),
            expected_exec_supervisor
        );
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

    fn configured_target(
        configuration: &HubModelConfiguration,
    ) -> signalbox_domain::ResolvedProviderTarget {
        let selection = DirectModelSelection::from_uuid(
            Uuid::parse_str("10000000-0000-4000-8000-000000000001").expect("fixture UUID is valid"),
        );
        configuration
            .resolve_direct_model(selection)
            .expect("fixture selection has a route")
            .target()
    }

    #[test]
    fn configured_rates_fold_only_reported_axes_with_version_provenance() {
        let configuration =
            HubModelConfiguration::parse(CONFIGURATION).expect("fixture configuration is valid");
        let cost = configuration
            .derive_model_call_cost(
                configured_target(&configuration),
                "anthropic-primary",
                ModelCallInputUsage::new(
                    Some(1_000_000),
                    ProcessModelCallInputTokenSemantics::CacheExclusive,
                ),
                Some(2),
                None,
                Some(10),
            )
            .expect("rated reported axes derive a cost");

        assert_eq!(cost.amount_usd().to_string(), "3.000033");
        assert_eq!(cost.rate_version(), "fixture-rates-v1");
        assert_eq!(cost.billing_kind(), BillingKind::ApiMetered);
    }

    #[test]
    fn historical_unknown_input_semantics_yield_no_dollar_figure() {
        let configuration =
            HubModelConfiguration::parse(CONFIGURATION).expect("fixture configuration is valid");

        assert_eq!(
            configuration.derive_model_call_cost(
                configured_target(&configuration),
                "anthropic-primary",
                ModelCallInputUsage::from_persisted(Some(1_000_000), None),
                Some(2),
                Some(3),
                Some(4),
            ),
            None
        );
    }

    #[test]
    fn inexact_rate_arithmetic_yields_no_dollar_figure() {
        let configuration = HubModelConfiguration::parse(&CONFIGURATION.replace(
            "input_usd_per_million_tokens = \"3\"",
            "input_usd_per_million_tokens = \"0.0000000000000000000000000001\"",
        ))
        .expect("the representable high-precision rate is valid configuration");

        assert_eq!(
            configuration.derive_model_call_cost(
                configured_target(&configuration),
                "anthropic-primary",
                ModelCallInputUsage::new(
                    Some(1),
                    ProcessModelCallInputTokenSemantics::CacheExclusive
                ),
                None,
                None,
                None,
            ),
            None
        );
    }

    #[test]
    fn rounded_rate_multiplication_yields_no_dollar_figure() {
        let configuration = HubModelConfiguration::parse(&CONFIGURATION.replace(
            "input_usd_per_million_tokens = \"3\"",
            "input_usd_per_million_tokens = \"1.2345678901234567890123456789\"",
        ))
        .expect("the representable high-precision rate is valid configuration");

        assert_eq!(
            configuration.derive_model_call_cost(
                configured_target(&configuration),
                "anthropic-primary",
                ModelCallInputUsage::new(
                    Some(u64::MAX),
                    ProcessModelCallInputTokenSemantics::CacheExclusive
                ),
                None,
                None,
                None,
            ),
            None
        );
    }

    #[test]
    fn cost_sum_that_loses_an_earlier_axis_yields_no_dollar_figure() {
        let configuration = HubModelConfiguration::parse(
            &CONFIGURATION
                .replace(
                    "input_usd_per_million_tokens = \"3\"",
                    "input_usd_per_million_tokens = \"0.0000000000000000000000000001\"",
                )
                .replace(
                    "output_usd_per_million_tokens = \"15\"",
                    "output_usd_per_million_tokens = \"10000000000000000000000000000\"",
                ),
        )
        .expect("both extreme rates are representable configuration values");

        assert_eq!(
            configuration.derive_model_call_cost(
                configured_target(&configuration),
                "anthropic-primary",
                ModelCallInputUsage::new(
                    Some(1_000_000),
                    ProcessModelCallInputTokenSemantics::CacheExclusive
                ),
                Some(1),
                None,
                None,
            ),
            None
        );
    }

    #[test]
    fn exact_rate_multiplication_may_reduce_scale() {
        let configuration = HubModelConfiguration::parse(&CONFIGURATION.replace(
            "input_usd_per_million_tokens = \"3\"",
            "input_usd_per_million_tokens = \"7922816251426433759354395033.5\"",
        ))
        .expect("the representable high-precision rate is valid configuration");

        let cost = configuration
            .derive_model_call_cost(
                configured_target(&configuration),
                "anthropic-primary",
                ModelCallInputUsage::new(
                    Some(10),
                    ProcessModelCallInputTokenSemantics::CacheExclusive,
                ),
                None,
                None,
                None,
            )
            .expect("exact multiplication may reduce decimal scale");
        let expected = Decimal::MAX
            .checked_div(Decimal::from(1_000_000_u64))
            .expect("fixture quotient is representable");

        assert_eq!(cost.amount_usd(), expected);
    }

    #[test]
    fn an_unrated_model_yields_no_dollar_figure() {
        let unrated = CONFIGURATION
            .lines()
            .filter(|line| {
                !line.starts_with("rate_version") && !line.contains("usd_per_million_tokens")
            })
            .collect::<Vec<_>>()
            .join("\n");
        let configuration =
            HubModelConfiguration::parse(&unrated).expect("an unrated model remains valid");

        assert_eq!(
            configuration.derive_model_call_cost(
                configured_target(&configuration),
                "anthropic-primary",
                ModelCallInputUsage::new(
                    Some(1),
                    ProcessModelCallInputTokenSemantics::CacheExclusive
                ),
                Some(1),
                Some(1),
                Some(1),
            ),
            None
        );
    }

    #[test]
    fn one_target_cannot_mix_rated_and_unrated_model_entries() {
        let conflicting = format!(
            r#"{CONFIGURATION}
[[models]]
selection_id = "10000000-0000-4000-8000-000000000002"
target_id = "20000000-0000-4000-8000-000000000001"
model_family = "anthropic"
provider_model = "claude-example"
max_output_tokens = 256
context_window_tokens = 200000
"#
        );

        assert_eq!(
            HubModelConfiguration::parse(&conflicting).err(),
            Some(HubModelConfigurationError::ConflictingTarget)
        );
    }

    #[test]
    fn a_partial_model_rate_set_is_rejected() {
        let partial =
            CONFIGURATION.replace("cache_read_input_usd_per_million_tokens = \"0.30\"\n", "");

        assert_eq!(
            HubModelConfiguration::parse(&partial).err(),
            Some(HubModelConfigurationError::IncompleteBillingRates)
        );
    }

    #[test]
    fn configuration_rejects_a_rate_that_requires_rounding() {
        let too_precise = CONFIGURATION.replace(
            "input_usd_per_million_tokens = \"3\"",
            "input_usd_per_million_tokens = \"0.00000000000000000000000000001\"",
        );

        assert_eq!(
            HubModelConfiguration::parse(&too_precise).err(),
            Some(HubModelConfigurationError::InvalidBillingRate)
        );
    }

    #[test]
    fn cost_label_follows_the_credential_profile() {
        let configuration =
            HubModelConfiguration::parse(CONFIGURATION).expect("fixture configuration is valid");
        let cost = configuration
            .derive_model_call_cost(
                configured_target(&configuration),
                "codex-subscription-primary",
                ModelCallInputUsage::new(
                    Some(1),
                    ProcessModelCallInputTokenSemantics::CacheExclusive,
                ),
                None,
                None,
                None,
            )
            .expect("the historical subscription profile is declared");

        assert_eq!(cost.billing_kind(), BillingKind::Subscription);
    }

    #[test]
    fn codex_cli_on_an_api_metered_profile_derives_real_cost() {
        let temporary = tempfile::tempdir().expect("fixture directory is available");
        let executable = std::env::current_exe().expect("the test executable has a path");
        let configuration = HubModelConfiguration::parse(
            &configuration_with_api_metered_codex_model(&executable, temporary.path()),
        )
        .expect("the API-metered Codex fixture is valid");
        let selection = DirectModelSelection::from_uuid(
            Uuid::parse_str("10000000-0000-4000-8000-000000000002").expect("fixture UUID is valid"),
        );
        let route = configuration
            .resolve_direct_model(selection)
            .expect("the Codex fixture has a route");
        let cost = configuration
            .derive_model_call_cost(
                route.target(),
                route.credential_profile(),
                ModelCallInputUsage::new(
                    Some(1),
                    ProcessModelCallInputTokenSemantics::CacheInclusive,
                ),
                None,
                Some(0),
                Some(0),
            )
            .expect("the API-metered Codex fixture has rates");

        assert_eq!(route.adapter(), ModelAdapter::CodexCli);
        assert_eq!(route.credential_profile(), "codex-api-primary");
        assert_eq!(cost.billing_kind(), BillingKind::ApiMetered);
    }

    #[test]
    fn codex_cache_breakdowns_are_not_charged_twice_as_input() {
        let temporary = tempfile::tempdir().expect("fixture directory is available");
        let executable = std::env::current_exe().expect("the test executable has a path");
        let configuration = HubModelConfiguration::parse(
            &configuration_with_api_metered_codex_model(&executable, temporary.path()),
        )
        .expect("the API-metered Codex fixture is valid");
        let selection = DirectModelSelection::from_uuid(
            Uuid::parse_str("10000000-0000-4000-8000-000000000002").expect("fixture UUID is valid"),
        );
        let route = configuration
            .resolve_direct_model(selection)
            .expect("the Codex fixture has a route");
        let cost = configuration
            .derive_model_call_cost(
                route.target(),
                route.credential_profile(),
                ModelCallInputUsage::new(
                    Some(1_000_000),
                    ProcessModelCallInputTokenSemantics::CacheInclusive,
                ),
                None,
                Some(100_000),
                Some(200_000),
            )
            .expect("the consistent Codex breakdown derives a cost");

        assert_eq!(cost.amount_usd().to_string(), "1.8");
    }

    #[test]
    fn codex_unreported_cache_axis_suppresses_only_ordinary_input_cost() {
        let temporary = tempfile::tempdir().expect("fixture directory is available");
        let executable = std::env::current_exe().expect("the test executable has a path");
        let configuration = HubModelConfiguration::parse(
            &configuration_with_api_metered_codex_model(&executable, temporary.path()),
        )
        .expect("the API-metered Codex fixture is valid");
        let selection = DirectModelSelection::from_uuid(
            Uuid::parse_str("10000000-0000-4000-8000-000000000002").expect("fixture UUID is valid"),
        );
        let route = configuration
            .resolve_direct_model(selection)
            .expect("the Codex fixture has a route");
        let partial_breakdown = configuration
            .derive_model_call_cost(
                route.target(),
                route.credential_profile(),
                ModelCallInputUsage::new(
                    Some(1_000_000),
                    ProcessModelCallInputTokenSemantics::CacheInclusive,
                ),
                None,
                Some(100_000),
                None,
            )
            .expect("the independently reported cache axis derives a cost");
        let cache_axis_only = configuration
            .derive_model_call_cost(
                route.target(),
                route.credential_profile(),
                ModelCallInputUsage::new(None, ProcessModelCallInputTokenSemantics::CacheInclusive),
                None,
                Some(100_000),
                None,
            )
            .expect("the independently reported cache axis derives the reference cost");

        assert_eq!(partial_breakdown, cache_axis_only);
    }

    #[test]
    fn historical_input_semantics_survive_an_adapter_route_change() {
        let temporary = tempfile::tempdir().expect("fixture directory is available");
        let executable = std::env::current_exe().expect("the test executable has a path");
        let configuration = HubModelConfiguration::parse(
            &configuration_with_api_metered_codex_model(&executable, temporary.path()),
        )
        .expect("the API-metered Codex fixture is valid");
        let selection = DirectModelSelection::from_uuid(
            Uuid::parse_str("10000000-0000-4000-8000-000000000002").expect("fixture UUID is valid"),
        );
        let route = configuration
            .resolve_direct_model(selection)
            .expect("the Codex fixture has a route");
        let cost = configuration
            .derive_model_call_cost(
                route.target(),
                route.credential_profile(),
                ModelCallInputUsage::new(
                    Some(1_000_000),
                    ProcessModelCallInputTokenSemantics::CacheExclusive,
                ),
                None,
                Some(100_000),
                Some(200_000),
            )
            .expect("historically exclusive input axes derive a cost");

        assert_eq!(route.adapter(), ModelAdapter::CodexCli);
        assert_eq!(cost.amount_usd().to_string(), "2.1");
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
    fn tool_mapping_registry_requires_git_identity() {
        let missing = CONFIGURATION.replace(
            "[git_identity]\nauthor_name = \"Signalbox Daemon\"\nauthor_email = \"signalbox@example.test\"\n\n",
            "",
        );

        assert_eq!(
            HubModelConfiguration::parse(&missing).err(),
            Some(HubModelConfigurationError::MissingGitIdentityConfiguration)
        );
    }

    #[test]
    fn git_identity_rejects_an_unsafe_value() {
        let invalid = CONFIGURATION.replace(
            "author_email = \"signalbox@example.test\"",
            "author_email = \"signalbox@example.test>\"",
        );

        assert_eq!(
            HubModelConfiguration::parse(&invalid).err(),
            Some(HubModelConfigurationError::InvalidGitIdentityConfiguration)
        );
    }

    #[test]
    fn git_identity_rejects_an_unknown_field() {
        let unknown = CONFIGURATION.replace(
            "author_email = \"signalbox@example.test\"",
            "author_email = \"signalbox@example.test\"\ncommitter_name = \"Ambient User\"",
        );

        assert_eq!(
            HubModelConfiguration::parse(&unknown).err(),
            Some(HubModelConfigurationError::InvalidGitIdentityConfiguration)
        );
    }

    #[test]
    fn tool_mapping_registry_requires_daemon_tool_process_settings() {
        let missing = CONFIGURATION.replace(
            &format!(
                "[daemon_tools]\nexec_supervisor_executable = \"{EXEC_SUPERVISOR_EXECUTABLE}\"\n\n"
            ),
            "",
        );

        assert_eq!(
            HubModelConfiguration::parse(&missing).err(),
            Some(HubModelConfigurationError::MissingDaemonToolSettings)
        );
    }

    #[test]
    fn daemon_tool_process_settings_reject_a_relative_supervisor() {
        let relative = CONFIGURATION.replace(
            &format!("exec_supervisor_executable = \"{EXEC_SUPERVISOR_EXECUTABLE}\""),
            "exec_supervisor_executable = \"relative/signalbox-exec-supervisor\"",
        );

        assert_eq!(
            HubModelConfiguration::parse(&relative).err(),
            Some(HubModelConfigurationError::InvalidDaemonToolSettings)
        );
    }

    #[test]
    fn daemon_tool_process_settings_reject_a_missing_supervisor() {
        let temporary = tempfile::tempdir().expect("fixture directory is available");
        let missing_supervisor = temporary.path().join("missing-supervisor");
        let missing = CONFIGURATION.replace(
            EXEC_SUPERVISOR_EXECUTABLE,
            missing_supervisor
                .to_str()
                .expect("fixture path is UTF-8 representable"),
        );

        assert_eq!(
            HubModelConfiguration::parse(&missing).err(),
            Some(HubModelConfigurationError::InvalidDaemonToolSettings)
        );
    }

    #[cfg(unix)]
    #[test]
    fn daemon_tool_process_settings_canonicalize_a_supervisor_symlink() {
        let temporary = tempfile::tempdir().expect("fixture directory is available");
        let supervisor_link = temporary.path().join("signalbox-exec-supervisor");
        std::os::unix::fs::symlink(EXEC_SUPERVISOR_EXECUTABLE, &supervisor_link)
            .expect("fixture supervisor symlink is created");
        let linked = CONFIGURATION.replace(
            EXEC_SUPERVISOR_EXECUTABLE,
            supervisor_link
                .to_str()
                .expect("fixture path is UTF-8 representable"),
        );
        let expected = std::fs::canonicalize(&supervisor_link)
            .expect("fixture supervisor symlink has a canonical target");

        let configuration =
            HubModelConfiguration::parse(&linked).expect("an absolute supervisor symlink is valid");

        assert_eq!(
            configuration
                .daemon_tools()
                .expect("mapped fixture has daemon tool settings")
                .exec_supervisor_executable(),
            expected
        );
    }

    #[test]
    fn daemon_tool_process_settings_reject_an_unknown_field() {
        let unknown = CONFIGURATION.replace(
            &format!("exec_supervisor_executable = \"{EXEC_SUPERVISOR_EXECUTABLE}\""),
            &format!(
                "exec_supervisor_executable = \"{EXEC_SUPERVISOR_EXECUTABLE}\"\nderive_from_daemon = true"
            ),
        );

        assert_eq!(
            HubModelConfiguration::parse(&unknown).err(),
            Some(HubModelConfigurationError::InvalidDaemonToolSettings)
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
    fn unused_codex_mapping_retains_its_declared_credential_profile() {
        let temporary = tempfile::tempdir().expect("fixture directory is available");
        let executable = std::env::current_exe().expect("the test executable has a path");
        let configuration = HubModelConfiguration::parse(&configuration_with_codex_paths(
            &executable,
            temporary.path(),
        ))
        .expect("the unused Codex mapping is valid configuration");

        assert_eq!(
            configuration.codex_cli_credential_profile.as_deref(),
            Some(CODEX_SUBSCRIPTION_PROFILE)
        );
        assert!(
            configuration
                .codex_cli_runtime()
                .expect("the stored profile constructs the runtime")
                .is_some()
        );
    }

    #[test]
    fn configuration_rejects_a_missing_claude_executable() {
        let temporary = tempfile::tempdir().expect("fixture directory is available");
        let bridge = std::env::current_exe().expect("the test executable has a path");
        let missing_executable = temporary.path().join("missing-claude");
        let configuration =
            configuration_with_claude_paths(&missing_executable, &bridge, temporary.path());

        assert_eq!(
            HubModelConfiguration::parse(&configuration).err(),
            Some(HubModelConfigurationError::InvalidClaudeCliConfiguration)
        );
    }

    #[test]
    fn configuration_rejects_a_claude_executable_that_is_not_a_file() {
        let temporary = tempfile::tempdir().expect("fixture directory is available");
        let bridge = std::env::current_exe().expect("the test executable has a path");
        let configuration =
            configuration_with_claude_paths(temporary.path(), &bridge, temporary.path());

        assert_eq!(
            HubModelConfiguration::parse(&configuration).err(),
            Some(HubModelConfigurationError::InvalidClaudeCliConfiguration)
        );
    }

    /// The MCP bridge is a second deployment-named program, so its path is
    /// validated exactly as strictly as the CLI's rather than being derived.
    #[test]
    fn configuration_rejects_a_missing_claude_mcp_bridge_executable() {
        let temporary = tempfile::tempdir().expect("fixture directory is available");
        let executable = std::env::current_exe().expect("the test executable has a path");
        let missing_bridge = temporary.path().join("missing-bridge");
        let configuration =
            configuration_with_claude_paths(&executable, &missing_bridge, temporary.path());

        assert_eq!(
            HubModelConfiguration::parse(&configuration).err(),
            Some(HubModelConfigurationError::InvalidClaudeCliConfiguration)
        );
    }

    /// A bare name is the second admitted spelling, so a name the daemon's own
    /// search path does not hold fails startup as its own diagnosis rather
    /// than as a malformed path.
    #[test]
    fn configuration_rejects_a_claude_mcp_bridge_name_no_search_entry_holds() {
        let temporary = tempfile::tempdir().expect("fixture directory is available");
        let executable = std::env::current_exe().expect("the test executable has a path");
        let configuration = configuration_with_claude_paths(
            &executable,
            Path::new(ABSENT_MCP_BRIDGE_NAME),
            temporary.path(),
        );

        assert_eq!(
            HubModelConfiguration::parse(&configuration).err(),
            Some(HubModelConfigurationError::UnresolvedClaudeMcpBridgeExecutable)
        );
    }

    /// A relative value carries a path separator, so it is a path and keeps
    /// the absolute-path rule instead of being looked up as a program name.
    #[test]
    fn configuration_rejects_a_relative_claude_mcp_bridge_path() {
        let temporary = tempfile::tempdir().expect("fixture directory is available");
        let executable = std::env::current_exe().expect("the test executable has a path");
        let configuration = configuration_with_claude_paths(
            &executable,
            Path::new("bin/signalbox-claude-mcp-bridge"),
            temporary.path(),
        );

        assert_eq!(
            HubModelConfiguration::parse(&configuration).err(),
            Some(HubModelConfigurationError::InvalidClaudeCliConfiguration)
        );
    }

    #[test]
    fn mcp_bridge_name_resolves_through_the_first_search_entry_holding_it() {
        let temporary = tempfile::tempdir().expect("fixture directory is available");
        let earlier = synthetic_search_directory(temporary.path(), "earlier");
        let later = synthetic_search_directory(temporary.path(), "later");
        let expected = synthetic_executable(&earlier, CLAUDE_MCP_BRIDGE_NAME);
        let shadowed = synthetic_executable(&later, CLAUDE_MCP_BRIDGE_NAME);

        let resolved = resolved_mcp_bridge_reference(
            CLAUDE_MCP_BRIDGE_NAME,
            Some(&synthetic_search_path(&[&earlier, &later])),
        )
        .expect("the fixture search path holds the bridge");

        assert_eq!(resolved, expected);
        assert_ne!(resolved, shadowed);
    }

    /// Resolution matches what executing the name would do: a same-named file
    /// no one can execute shadows nothing.
    #[cfg(unix)]
    #[test]
    fn mcp_bridge_name_skips_a_search_entry_whose_file_is_not_executable() {
        let temporary = tempfile::tempdir().expect("fixture directory is available");
        let earlier = synthetic_search_directory(temporary.path(), "earlier");
        let later = synthetic_search_directory(temporary.path(), "later");
        let unexecutable = synthetic_unexecutable_file(&earlier, CLAUDE_MCP_BRIDGE_NAME);
        let expected = synthetic_executable(&later, CLAUDE_MCP_BRIDGE_NAME);

        let resolved = resolved_mcp_bridge_reference(
            CLAUDE_MCP_BRIDGE_NAME,
            Some(&synthetic_search_path(&[&earlier, &later])),
        )
        .expect("the later search entry holds an executable bridge");

        assert_eq!(resolved, expected);
        assert_ne!(resolved, unexecutable);
    }

    #[test]
    fn mcp_bridge_name_without_a_search_path_resolves_to_nothing() {
        assert_eq!(
            resolved_mcp_bridge_reference(CLAUDE_MCP_BRIDGE_NAME, None),
            Err(HubModelConfigurationError::UnresolvedClaudeMcpBridgeExecutable)
        );
    }

    /// A configured path is the operator's exact choice, so a same-named
    /// program on the search path never displaces it.
    #[test]
    fn mcp_bridge_path_is_used_verbatim_over_a_search_entry_holding_the_name() {
        let temporary = tempfile::tempdir().expect("fixture directory is available");
        let entry = synthetic_search_directory(temporary.path(), "entry");
        let shadowing = synthetic_executable(&entry, CLAUDE_MCP_BRIDGE_NAME);
        let configured = temporary
            .path()
            .join("install")
            .join(CLAUDE_MCP_BRIDGE_NAME);

        let resolved = resolved_mcp_bridge_reference(
            configured
                .to_str()
                .expect("the fixture install path is UTF-8"),
            Some(&synthetic_search_path(&[&entry])),
        )
        .expect("a configured path needs no search entry");

        assert_eq!(resolved, configured);
        assert_ne!(resolved, shadowing);
    }

    /// The resolved path is written into a configuration another process
    /// reads from a working directory of its own, so an entry that only means
    /// something relative to this process is not a place to look.
    #[cfg(unix)]
    #[test]
    fn search_entries_drop_the_relative_and_empty_ones() {
        let search_path = std::ffi::OsString::from(":relative/bin:/absolute/bin");

        assert_eq!(
            absolute_search_entries(Some(&search_path)),
            vec![PathBuf::from("/absolute/bin")]
        );
    }

    #[test]
    fn configuration_rejects_a_claude_mapping_without_process_settings() {
        let temporary = tempfile::tempdir().expect("fixture directory is available");
        let executable = std::env::current_exe().expect("the test executable has a path");
        let complete = configuration_with_claude_paths(&executable, &executable, temporary.path());
        let start = complete
            .find("[claude_cli]")
            .expect("the fixture declares Claude process settings");
        let without_process_settings = &complete[..start];

        assert_eq!(
            HubModelConfiguration::parse(without_process_settings).err(),
            Some(HubModelConfigurationError::MissingClaudeCliConfiguration)
        );
    }

    #[test]
    fn unused_claude_mapping_retains_its_declared_credential_profile() {
        let temporary = tempfile::tempdir().expect("fixture directory is available");
        let executable = std::env::current_exe().expect("the test executable has a path");
        let configuration = HubModelConfiguration::parse(&configuration_with_claude_paths(
            &executable,
            &executable,
            temporary.path(),
        ))
        .expect("the unused Claude mapping is valid configuration");

        assert_eq!(
            configuration.claude_cli_credential_profile.as_deref(),
            Some(CLAUDE_SUBSCRIPTION_PROFILE)
        );
        assert_eq!(
            configuration
                .claude_cli()
                .expect("the fixture declares Claude process settings")
                .mcp_bridge_executable(),
            executable.as_path()
        );
        assert!(
            configuration
                .claude_cli_runtime()
                .expect("the stored profile constructs the runtime")
                .is_some()
        );
    }

    /// Claude Code exposes no service tier, so a configured tier fails startup
    /// instead of reaching preparation as an unenforceable request control.
    #[test]
    fn configuration_rejects_a_service_tier_on_a_claude_model() {
        let temporary = tempfile::tempdir().expect("fixture directory is available");
        let executable = std::env::current_exe().expect("the test executable has a path");
        let configuration = format!(
            "{}{}",
            configuration_with_claude_paths(&executable, &executable, temporary.path()),
            CLAUDE_MODEL_ENTRY.replace(
                "reasoning_levels = [\"high\"]",
                "reasoning_levels = [\"high\"]\nservice_tiers = [\"auto\"]",
            ),
        );

        assert_eq!(
            HubModelConfiguration::parse(&configuration).err(),
            Some(HubModelConfigurationError::InvalidModelCapabilities)
        );
    }

    /// Claude Code reports input tokens exclusive of the cache axes it reports
    /// separately, exactly as the Anthropic API does.
    #[test]
    fn configured_claude_models_route_to_the_claude_adapter_with_cache_exclusive_input() {
        let temporary = tempfile::tempdir().expect("fixture directory is available");
        let executable = std::env::current_exe().expect("the test executable has a path");
        let configuration = HubModelConfiguration::parse(&format!(
            "{}{CLAUDE_MODEL_ENTRY}",
            configuration_with_claude_paths(&executable, &executable, temporary.path()),
        ))
        .expect("the Claude mapping, process settings, and model are valid");
        let selection = DirectModelSelection::from_uuid(
            Uuid::parse_str("10000000-0000-4000-8000-00000000000c").expect("fixture UUID is valid"),
        );

        let route = configuration
            .resolve_direct_model(selection)
            .expect("the Claude selection has an adapter route");

        assert_eq!(route.adapter(), ModelAdapter::ClaudeCli);
        assert_eq!(route.credential_profile(), CLAUDE_SUBSCRIPTION_PROFILE);
        assert_eq!(
            configuration.adapter_for_provider_model("claude-cli-example"),
            Some(ModelAdapter::ClaudeCli)
        );
        assert!(
            !configuration
                .cache_inclusive_input_targets()
                .contains(&route.target())
        );
    }

    /// OpenAI is an API-key adapter, so it mirrors Anthropic: the mapping is
    /// pinned to the one profile the daemon binds its credential file to, and
    /// `prompt_tokens` already contains the cache axes reported beside it.
    #[test]
    fn configured_openai_models_route_through_the_pinned_api_key_profile() {
        let configuration =
            HubModelConfiguration::parse(&format!("{CONFIGURATION}{OPENAI_MAPPING_AND_MODEL}"))
                .expect("the OpenAI mapping, profile, and model are valid");
        let selection = DirectModelSelection::from_uuid(
            Uuid::parse_str("10000000-0000-4000-8000-00000000000e").expect("fixture UUID is valid"),
        );

        let route = configuration
            .resolve_direct_model(selection)
            .expect("the OpenAI selection has an adapter route");

        assert_eq!(route.adapter(), ModelAdapter::OpenAi);
        assert_eq!(route.credential_profile(), OPENAI_PROFILE);
        assert!(configuration.uses_openai_adapter());
        assert_eq!(
            configuration.adapter_for_provider_model("gpt-example"),
            Some(ModelAdapter::OpenAi)
        );
        assert!(
            configuration
                .cache_inclusive_input_targets()
                .contains(&route.target())
        );
    }

    #[test]
    fn configuration_rejects_an_openai_mapping_naming_another_profile() {
        let other_profile = "openai-secondary";
        let configuration = format!("{CONFIGURATION}{OPENAI_MAPPING_AND_MODEL}").replace(
            "credential_profile = \"openai-primary\"",
            &format!("credential_profile = \"{other_profile}\""),
        );

        assert_eq!(
            HubModelConfiguration::parse(&configuration).err(),
            Some(HubModelConfigurationError::UnknownCredentialProfile {
                adapter: ModelAdapter::OpenAi,
                credential_profile: Arc::from(other_profile),
            })
        );
    }

    /// `ultra` is the Codex effort value, so it is unsupported here even though
    /// every lower level maps onto the OpenAI wire control.
    #[test]
    fn configuration_rejects_an_openai_reasoning_level_the_adapter_cannot_enforce() {
        let configuration = format!("{CONFIGURATION}{OPENAI_MAPPING_AND_MODEL}").replace(
            "reasoning_levels = [\"minimal\", \"medium\", \"xhigh\"]",
            "reasoning_levels = [\"ultra\"]",
        );

        assert_eq!(
            HubModelConfiguration::parse(&configuration).err(),
            Some(HubModelConfigurationError::InvalidModelCapabilities)
        );
    }

    /// Service-tier spellings are provider-tagged, so an Anthropic-only value
    /// cannot be read as OpenAI's despite the shared word.
    #[test]
    fn configuration_rejects_another_providers_service_tier_on_an_openai_model() {
        let configuration = format!("{CONFIGURATION}{OPENAI_MAPPING_AND_MODEL}").replace(
            "service_tiers = [\"flex\", \"priority\"]",
            "service_tiers = [\"standard_only\"]",
        );

        assert_eq!(
            HubModelConfiguration::parse(&configuration).err(),
            Some(HubModelConfigurationError::InvalidModelCapabilities)
        );
    }

    /// Fast mode maps an absent tier onto `fast`, so a simultaneous explicit
    /// non-fast tier is an adapter-level conflict caught before startup ends.
    #[test]
    fn configuration_rejects_openai_fast_mode_beside_a_conflicting_configured_tier() {
        let configuration = format!(
            "{CONFIGURATION}{OPENAI_MAPPING_AND_MODEL}\n[model_settings]\nfast_mode = \"enabled\"\nservice_tier = {{ provider = \"open_ai\", value = \"flex\" }}\n"
        );

        assert_eq!(
            HubModelConfiguration::parse(&configuration).err(),
            Some(HubModelConfigurationError::InvalidModelSettingsConfiguration)
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

    #[test]
    fn configuration_enforces_the_protocol_model_capability_catalog_capacity() {
        assert_eq!(
            validate_model_count(signalbox_process_protocol::MAX_MODEL_CAPABILITY_CATALOG_ENTRIES),
            Ok(())
        );
        assert_eq!(
            validate_model_count(
                signalbox_process_protocol::MAX_MODEL_CAPABILITY_CATALOG_ENTRIES + 1
            ),
            Err(HubModelConfigurationError::TooManyModels)
        );
    }

    #[test]
    fn configuration_rejects_reasoning_levels_the_selected_adapter_cannot_map() {
        let configuration = CONFIGURATION.replace(
            "context_window_tokens = 200000",
            "context_window_tokens = 200000\nreasoning_levels = [\"ultra\"]",
        );

        assert_eq!(
            HubModelConfiguration::parse(&configuration).err(),
            Some(HubModelConfigurationError::InvalidModelCapabilities)
        );
    }

    #[test]
    fn configuration_copies_named_profile_and_global_settings_layers_per_model() {
        let configuration = CONFIGURATION
            .replace(
                "version = 1",
                "version = 1\n\n[model_settings]\nreasoning_level = \"low\"\n\n[[model_settings_profiles]]\nname = \"deliberate\"\nreasoning_level = \"high\"\nfast_mode = \"enabled\"\nservice_tier = { provider = \"anthropic\", value = \"standard_only\" }",
            )
            .replace(
                "context_window_tokens = 200000",
                "context_window_tokens = 200000\nreasoning_levels = [\"low\", \"high\"]\nfast_mode = \"request_control\"\nservice_tiers = [\"standard_only\"]\nsettings_profile = \"deliberate\"",
            );
        let configured = HubModelConfiguration::parse(&configuration)
            .expect("the selected model supports its copied lower layers");
        let (profile, global_default) = configured
            .model_settings_lower_layers(configured_judge_selection_fixture())
            .expect("the direct model has copied lower settings layers");
        let validated = configured
            .validate_session_model_settings(
                ModelSelectionRequest::Direct(configured_judge_selection_fixture()),
                ModelSettingsOverlay::inherit_all(),
            )
            .expect("the direct model is configured")
            .expect("the inherited settings chain is supported");

        assert_eq!(
            profile.reasoning_level(),
            SettingOverlay::Value(ReasoningLevel::High)
        );
        assert_eq!(
            global_default.reasoning_level(),
            SettingOverlay::Value(ReasoningLevel::Low)
        );
        assert_eq!(
            profile.fast_mode(),
            FastModeOverlay::Value(FastMode::Enabled)
        );
        assert_eq!(
            profile.service_tier(),
            SettingOverlay::Value(ServiceTier::Anthropic(AnthropicServiceTier::StandardOnly))
        );
        assert_eq!(
            validated.effective().reasoning_level(),
            Some(ReasoningLevel::High)
        );
        assert_eq!(
            validated.resolved().reasoning_source(),
            Some(ModelSettingSource::Profile)
        );
    }

    #[test]
    fn configuration_rejects_a_lower_layer_unsupported_by_the_selected_model() {
        let configuration = CONFIGURATION.replace(
            "version = 1",
            "version = 1\n\n[model_settings]\nreasoning_level = \"low\"",
        );

        assert_eq!(
            HubModelConfiguration::parse(&configuration).err(),
            Some(HubModelConfigurationError::InvalidModelSettingsConfiguration)
        );
    }

    /// S37 / INV-051: every explicit lower layer is validated even when a
    /// higher-precedence layer masks it in the effective configuration.
    #[test]
    fn s37_inv051_configuration_rejects_an_unsupported_global_value_masked_by_a_profile() {
        let profile_configuration = CONFIGURATION
            .replace(
                "version = 1",
                "version = 1\n\n[[model_settings_profiles]]\nname = \"provider-defaults\"\nreasoning_level = \"provider_default\"",
            )
            .replace(
                "context_window_tokens = 200000",
                "context_window_tokens = 200000\nsettings_profile = \"provider-defaults\"",
            );
        HubModelConfiguration::parse(&profile_configuration)
            .expect("the selected profile is supported without the masked global layer");
        let configuration = profile_configuration.replace(
            "version = 1",
            "version = 1\n\n[model_settings]\nreasoning_level = \"low\"",
        );

        assert_eq!(
            HubModelConfiguration::parse(&configuration).err(),
            Some(HubModelConfigurationError::InvalidModelSettingsConfiguration)
        );
    }

    /// S37 / INV-051: an explicit unsupported selected-profile value is
    /// rejected even when the global layer is valid.
    #[test]
    fn s37_inv051_configuration_rejects_an_unsupported_selected_profile_value() {
        let configuration = CONFIGURATION
            .replace(
                "version = 1",
                "version = 1\n\n[[model_settings_profiles]]\nname = \"unsupported\"\nreasoning_level = \"low\"",
            )
            .replace(
                "context_window_tokens = 200000",
                "context_window_tokens = 200000\nsettings_profile = \"unsupported\"",
            );

        assert_eq!(
            HubModelConfiguration::parse(&configuration).err(),
            Some(HubModelConfigurationError::InvalidModelSettingsConfiguration)
        );
    }

    /// S37 / INV-051: a selected profile cannot combine individually
    /// supported controls that its adapter cannot enforce together.
    #[test]
    fn s37_inv051_configuration_rejects_an_adapter_incompatible_selected_profile() {
        let configuration = CONFIGURATION
            .replace(
                "version = 1",
                "version = 1\n\n[[model_settings_profiles]]\nname = \"incompatible\"\nfast_mode = \"enabled\"\nservice_tier = { provider = \"anthropic\", value = \"auto\" }",
            )
            .replace(
                "context_window_tokens = 200000",
                "context_window_tokens = 200000\nfast_mode = \"request_control\"\nservice_tiers = [\"auto\"]\nsettings_profile = \"incompatible\"",
            );

        assert_eq!(
            HubModelConfiguration::parse(&configuration).err(),
            Some(HubModelConfigurationError::InvalidModelSettingsConfiguration)
        );
    }

    /// S37 / INV-051: an adapter-incompatible global combination remains
    /// invalid when a selected profile masks it with a supported combination.
    #[test]
    fn s37_inv051_configuration_rejects_a_masked_adapter_incompatible_global_layer() {
        let configuration = CONFIGURATION
            .replace(
                "version = 1",
                "version = 1\n\n[model_settings]\nfast_mode = \"enabled\"\nservice_tier = { provider = \"anthropic\", value = \"auto\" }\n\n[[model_settings_profiles]]\nname = \"standard-tier\"\nservice_tier = { provider = \"anthropic\", value = \"standard_only\" }",
            )
            .replace(
                "context_window_tokens = 200000",
                "context_window_tokens = 200000\nfast_mode = \"request_control\"\nservice_tiers = [\"auto\", \"standard_only\"]\nsettings_profile = \"standard-tier\"",
            );

        assert_eq!(
            HubModelConfiguration::parse(&configuration).err(),
            Some(HubModelConfigurationError::InvalidModelSettingsConfiguration)
        );
    }

    #[test]
    fn configuration_rejects_a_selectable_model_as_an_alternate_fast_target() {
        let configuration = format!(
            r#"{}

[[models]]
selection_id = "10000000-0000-4000-8000-000000000002"
target_id = "20000000-0000-4000-8000-000000000002"
model_family = "anthropic"
provider_model = "synthetic-selectable-fast-target"
max_output_tokens = 256
context_window_tokens = 200000
"#,
            CONFIGURATION.replace(
                "context_window_tokens = 200000",
                "context_window_tokens = 200000\nfast_mode = \"alternate_target\"\nfast_target_id = \"20000000-0000-4000-8000-000000000002\"",
            )
        );

        assert_eq!(
            HubModelConfiguration::parse(&configuration).err(),
            Some(HubModelConfigurationError::InvalidModelCapabilities)
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

    #[tokio::test]
    async fn bounded_file_credentials_reject_before_accumulating_past_the_limit() {
        const ACCEPTED_BYTES: usize = 8;
        let temporary = tempfile::tempdir().expect("fixture directory is available");
        let path = temporary.path().join("bounded-credential");
        std::fs::write(&path, vec![b'x'; ACCEPTED_BYTES + 1])
            .expect("oversized credential fixture is writable");
        let source = FileCredentialAccess::new_bounded(
            path,
            CredentialReference::new(ANTHROPIC_CREDENTIAL_REFERENCE),
            ACCEPTED_BYTES,
        );

        assert_eq!(
            source
                .resolve(&source.credential_reference())
                .await
                .expect_err("oversized credential is rejected")
                .failure,
            CredentialAccessFailure::Unreadable
        );
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

    use super::{
        EXAMPLE_EXEC_SUPERVISOR, HubModelConfiguration, ModelAdapter,
        checked_in_example_configuration,
    };

    fn example_path() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config/signalboxd.example.toml")
    }

    /// The checked-in operator example is the one configuration document a
    /// deployment is invited to copy. Its installation-specific supervisor
    /// path is replaced by an existing fixture file before the same fail-closed
    /// loader validates every other byte.
    #[test]
    fn the_example_catalog_parses_and_validates() {
        checked_in_example_configuration()
            .expect("the checked-in example catalog is a valid version 1 document");
    }

    /// Whether one line is commented-out configuration rather than active TOML.
    fn is_inactive(line: &str) -> bool {
        line == "#" || line.starts_with("# ")
    }

    /// Removes exactly one comment level, the edit a reader makes by hand.
    fn uncomment(line: &str) -> &str {
        if let Some(remainder) = line.strip_prefix("# ") {
            return remainder;
        }
        if let Some(remainder) = line.strip_prefix('#') {
            return remainder;
        }
        line
    }

    /// Activates every inactive block the example ships, the transformation an
    /// operator performs when adopting a family the shipped document leaves
    /// commented out.
    ///
    /// An inactive block is a run of consecutive commented lines whose first
    /// line uncomments to a TOML table header. A run that opens with prose is
    /// documentation, not configuration, and stays commented; prose written
    /// inside an inactive block is doubly commented so that one level of
    /// removal leaves it a comment.
    fn activated(document: &str) -> String {
        let lines: Vec<&str> = document.lines().collect();
        let mut activated = String::with_capacity(document.len());
        let mut index = 0;
        while index < lines.len() {
            let mut end = index;
            while end < lines.len() && is_inactive(lines[end]) {
                end += 1;
            }
            let opens_a_table = end > index && uncomment(lines[index]).starts_with('[');
            while index < end {
                activated.push_str(if opens_a_table {
                    uncomment(lines[index])
                } else {
                    lines[index]
                });
                activated.push('\n');
                index += 1;
            }
            if index < lines.len() && !is_inactive(lines[index]) {
                activated.push_str(lines[index]);
                activated.push('\n');
                index += 1;
            }
        }
        activated
    }

    /// Binds the example's documented placeholder paths to paths that exist,
    /// which the wrapped-CLI process tables require of a real deployment.
    ///
    /// The bridge is not a placeholder: the example names it the way a
    /// deployment that installs it on the daemon's own search path does, which
    /// this test process is not, so the whole assignment is rebound to a path.
    fn with_existing_paths(document: &str, executable: &Path, working_directory: &Path) -> String {
        let executable = executable.to_string_lossy();
        let working_directory = working_directory.to_string_lossy();
        document
            .replace(CLAUDE_EXECUTABLE_PLACEHOLDER, &executable)
            .replace(
                &bridge_assignment(CLAUDE_BRIDGE_NAME),
                &bridge_assignment(&executable),
            )
            .replace(CODEX_EXECUTABLE_PLACEHOLDER, &executable)
            .replace(EXAMPLE_EXEC_SUPERVISOR, &executable)
            .replace(WORKING_DIRECTORY_PLACEHOLDER, &working_directory)
    }

    fn bridge_assignment(value: &str) -> String {
        format!("mcp_bridge_executable = \"{value}\"")
    }

    const CLAUDE_EXECUTABLE_PLACEHOLDER: &str = "/absolute/path/to/claude";
    const CLAUDE_BRIDGE_NAME: &str = "signalbox-claude-mcp-bridge";
    const CODEX_EXECUTABLE_PLACEHOLDER: &str = "/absolute/path/to/codex";
    const WORKING_DIRECTORY_PLACEHOLDER: &str = "/absolute/path/to/workspace";

    #[test]
    fn activation_uncomments_a_block_that_opens_a_table() {
        let activated = activated("# [codex_cli]\n# executable = \"codex\"\n");

        assert_eq!(activated, "[codex_cli]\nexecutable = \"codex\"\n");
    }

    #[test]
    fn activation_leaves_a_block_that_opens_with_prose_commented() {
        let activated = activated("# To serve it, add:\n# [codex_cli]\n");

        assert_eq!(activated, "# To serve it, add:\n# [codex_cli]\n");
    }

    #[test]
    fn activation_leaves_prose_inside_an_activated_block_commented() {
        let activated = activated("# [[models]]\n# # Hard cap: 128000.\n");

        assert_eq!(activated, "[[models]]\n# Hard cap: 128000.\n");
    }

    #[test]
    fn activation_leaves_active_lines_unchanged() {
        let activated = activated("version = 1\n\n[compaction]\n");

        assert_eq!(activated, "version = 1\n\n[compaction]\n");
    }

    /// A commented model row that activation walks past would be catalog data
    /// no test ever loads, so the example is written so that activation
    /// consumes every one of them.
    #[test]
    fn activation_leaves_no_commented_model_row_behind() {
        let document =
            std::fs::read_to_string(example_path()).expect("the checked-in example is readable");

        let activated = activated(&document);

        assert!(!activated.contains("\n# [[models]]"));
    }

    /// Every model row the example ships for a family it leaves commented out
    /// is catalog data an operator is invited to adopt by uncommenting it, so
    /// the whole activated document must satisfy the same fail-closed loader —
    /// including the adapter-specific reasoning levels and service tiers each
    /// row declares, and the one-adapter-per-provider-model rule that a
    /// spelling offered on two surfaces would violate.
    #[test]
    fn every_inactive_example_block_activates_into_a_valid_catalog() {
        let temporary = tempfile::tempdir().expect("fixture directory is available");
        let executable = std::env::current_exe().expect("the test executable has a path");
        let document =
            std::fs::read_to_string(example_path()).expect("the checked-in example is readable");

        let activated = with_existing_paths(&activated(&document), &executable, temporary.path());

        let configuration = HubModelConfiguration::parse(&activated)
            .expect("the example's commented families activate into a valid version 1 document");

        assert_eq!(
            configuration.adapter_for_provider_model("claude-fable-5"),
            Some(ModelAdapter::Anthropic)
        );
        assert_eq!(
            configuration.adapter_for_provider_model("gpt-5.6"),
            Some(ModelAdapter::OpenAi)
        );
        assert_eq!(
            configuration.adapter_for_provider_model("gpt-5.6-sol"),
            Some(ModelAdapter::CodexCli)
        );
        assert_eq!(
            configuration.adapter_for_provider_model("fable"),
            Some(ModelAdapter::ClaudeCli)
        );
    }
}
