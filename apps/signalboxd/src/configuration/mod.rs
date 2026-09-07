//! Deployment-owned model mappings and credential delivery.

#[cfg(test)]
mod checked_in_example;
mod error;
mod numeric_bounds;
#[cfg(test)]
pub(crate) mod tests;
mod toml_scalars;

use crate::{
    blob_storage_configuration::BlobStorageConfiguration,
    credential_pools::{
        CredentialDelivery, CredentialPool, CredentialPoolAction, CredentialPoolExhaustion,
        CredentialPoolTrigger, CredentialProfile, parse_credential_pools,
        parse_credential_profiles,
    },
};
pub use error::{HubModelConfigurationError, UnknownSessionModel};
pub use numeric_bounds::NumericBoundsConfiguration;
#[cfg(test)]
use numeric_bounds::parse_numeric_bound_duration;
use rust_decimal::Decimal;
use signalbox_domain::{
    AnthropicServiceTier, BranchName, CheckConclusion, CodexCliServiceTier, DirectModelSelection,
    FastMode, FastModeOverlay, FastModeSupport, FrozenAliasDefinition, InstructionPath, LabelName,
    MergeableState, ModelAlias, ModelCapabilities, ModelCapabilityCatalog,
    ModelCapabilityDefinition, ModelSelectionRequest, ModelSettingsOverlay,
    ModelSettingsPrecedence, ModelTargetCatalog, ModelTargetDefinition, OpenAiServiceTier,
    ProviderModelIdentity, PullRequestNumber, ReasoningLevel, RepoWatchAuthorLogin,
    RepoWatchEventKindNameV1, RepoWatchLabelMatcher, RepoWatchLabelMatcherInput,
    RepoWatchMatcherV1, RepoWatchMatcherV1Input, RepoWatchPattern, RepoWatchRule,
    RepoWatchRuleActionV1, RepoWatchRuleId, RepoWatchRuleVersion, RepoWatchSingletonScope,
    RepositorySlug, ResolvedProviderTarget, ServiceTier, SessionTemplateName, SettingOverlay,
    ToolApprovalPosture, ToolName, UnsupportedModelSetting, ValidatedModelSettings,
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
    model_execution::{
        CredentialPoolRuntimeAction, CredentialPoolRuntimeCatalog, CredentialPoolRuntimeExhaustion,
        CredentialPoolRuntimeMember, CredentialPoolRuntimePolicy, ToolContinuationUsageLimit,
    },
    process_read::ProcessModelCallInputTokenSemantics,
};
use signalbox_process_protocol::MAX_RATE_VERSION_UTF8_BYTES;
use signalbox_tools_git::GitIdentity;
use signalbox_tools_github::{GITHUB_CREDENTIAL_REFERENCE, GitHubEgressPolicy};
use signalbox_tools_web::WebFetchEgressPolicy;
use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    fmt, fs, io,
    net::{Ipv4Addr, SocketAddr, SocketAddrV4},
    num::NonZeroU64,
    path::{Component, Path, PathBuf},
    str::FromStr,
    sync::Arc,
    time::Duration,
};
use toml_edit::{DocumentMut, Item, Table};
use toml_scalars::{
    parse_positive_u32_inline_map, required_positive_u32, required_uuid, validate_alias_count,
    validate_model_count,
};
pub(crate) use toml_scalars::{reject_unknown_fields, required_string, validated_name};

const fn runtime_pool_action(action: CredentialPoolAction) -> CredentialPoolRuntimeAction {
    match action {
        CredentialPoolAction::Stay => CredentialPoolRuntimeAction::Stay,
        CredentialPoolAction::SwitchNextTurn => CredentialPoolRuntimeAction::SwitchNextTurn,
        CredentialPoolAction::SwitchNow => CredentialPoolRuntimeAction::SwitchNow,
        CredentialPoolAction::AvoidNewSessions => CredentialPoolRuntimeAction::AvoidNewSessions,
        CredentialPoolAction::Quarantine => CredentialPoolRuntimeAction::Quarantine,
    }
}

const fn runtime_pool_exhaustion(
    exhaustion: CredentialPoolExhaustion,
) -> CredentialPoolRuntimeExhaustion {
    match exhaustion {
        CredentialPoolExhaustion::Park => CredentialPoolRuntimeExhaustion::Park,
        CredentialPoolExhaustion::Fail => CredentialPoolRuntimeExhaustion::Fail,
    }
}

/// Non-secret reference the process binds its Anthropic key file to when no
/// configured route names that adapter, so a deployment serving Codex alone
/// still has one durable default. A configured Anthropic route supplies its own
/// profile name instead, which this build never compares against this value.
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
/// One provider-availability cause a pool trigger can react to.
///
/// Only these three carry proof that the request was not accepted, so only they
/// can authorize an availability successor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AvailabilityCause {
    /// The account's quota for the period is spent.
    QuotaExhausted,
    /// The provider rate limited this request.
    RateLimited,
    /// The provider reported itself overloaded.
    Overloaded,
}

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
    pub(crate) fn parse(value: &str) -> Result<Self, HubModelConfigurationError> {
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

    /// Reports whether this adapter's contract admits one credential delivery.
    ///
    /// This is a permission rather than an asserted subset: a pair is admitted
    /// exactly where a delivery contract defines how the secret reaches that
    /// adapter's provider, and startup rejects every pair no contract defines.
    /// Direct HTTP adapters authenticate each request themselves, so only
    /// `file` is defined for them; the CLI adapters additionally own an
    /// external login, and the Codex profile-specific deliveries are defined
    /// here even though no surface supplies them yet — which `delivers` below,
    /// not this predicate, is what decides.
    pub(crate) fn admits_delivery(self, delivery: &str) -> bool {
        match self {
            Self::Anthropic | Self::OpenAi => matches!(delivery, "file"),
            Self::ClaudeCli => matches!(delivery, "ambient" | "file"),
            Self::CodexCli => matches!(delivery, "ambient" | "file" | "codex_home" | "oauth"),
        }
    }

    /// Reports whether this build supplies a surface for one delivery.
    ///
    /// A delivery the grammar admits but no surface honors is a startup
    /// failure rather than an inert setting, on the same principle as the
    /// capacity-dependent pool keys.
    pub(crate) fn delivers(self, delivery: &str) -> bool {
        match self {
            Self::Anthropic | Self::OpenAi => matches!(delivery, "file"),
            Self::ClaudeCli => matches!(delivery, "ambient" | "file"),
            Self::CodexCli => matches!(delivery, "ambient" | "codex_home"),
        }
    }

    /// Reports whether this adapter observes remaining provider capacity.
    ///
    /// Neither composed runtime does. Listing the variants rather than
    /// answering `false` outright makes a later adapter state its own answer.
    pub(crate) const fn reports_remaining_capacity(self) -> bool {
        match self {
            Self::Anthropic | Self::ClaudeCli | Self::CodexCli | Self::OpenAi => false,
        }
    }

    /// Reports whether this adapter can supply the typed proof that a provider
    /// rejected a request before accepting it for one exact availability cause,
    /// which is what authorizes an availability successor.
    ///
    /// Only a decoded native error envelope carries that proof, and each adapter
    /// names native tokens for only some causes
    /// (`docs/spec/runtime-substrate.md`); a status-derived fallback carries
    /// none. Anthropic maps `rate_limit_error` and `overloaded_error` but has no
    /// quota token, and OpenAI maps `rate_limit_exceeded`/`rate_limit_error` and
    /// `insufficient_quota` but reaches overload only by status. Codex
    /// classifies the narrower cause from rendered failure prose only after its
    /// machine-readable JSONL lifecycle closes the request as `turn.failed`;
    /// that envelope proves non-acceptance. Claude Code exposes no equivalent
    /// proof. Listing every pair
    /// rather than matching on a group makes a later adapter state its own
    /// answer.
    pub(crate) const fn proves_non_acceptance(self, cause: AvailabilityCause) -> bool {
        match (self, cause) {
            (Self::Anthropic, AvailabilityCause::RateLimited | AvailabilityCause::Overloaded) => {
                true
            }
            (Self::Anthropic, AvailabilityCause::QuotaExhausted) => false,
            (Self::OpenAi, AvailabilityCause::RateLimited | AvailabilityCause::QuotaExhausted) => {
                true
            }
            (Self::OpenAi, AvailabilityCause::Overloaded) => false,
            (Self::CodexCli, _) => true,
            (Self::ClaudeCli, _) => false,
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
    pub(crate) fn parse(value: &str) -> Result<Self, HubModelConfigurationError> {
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

/// Validated deployment settings used to construct the Codex CLI adapter.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CodexCliConfiguration {
    executable: PathBuf,
    working_directory: PathBuf,
    model_context_window_overrides: HashMap<String, u32>,
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
    credential_pool: Arc<str>,
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

    /// Non-secret credential pool whose members may authenticate this family.
    pub fn credential_pool(&self) -> &str {
        &self.credential_pool
    }

    /// Non-secret credential profile pinned for new sessions: the pool member
    /// preparation prefers while no member is excluded.
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
    credential_pool: Arc<str>,
    credential_profile: Arc<str>,
}

/// Validated deployment dependencies injected into daemon tool families.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DaemonToolConfiguration {
    workspace_root: PathBuf,
    git_identity: GitIdentity,
    exec_supervisor_executable: PathBuf,
    cargo_registry_cache: Option<PathBuf>,
}

/// Explicit non-workspace instruction roots registered by deployment configuration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceInstructionConfiguration {
    roots: Box<[InstructionPath]>,
}

impl WorkspaceInstructionConfiguration {
    /// Returns explicit roots in deterministic configuration order.
    pub fn roots(&self) -> &[InstructionPath] {
        &self.roots
    }
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

    /// Optional host Cargo registry pinned read-only into sandboxed execution.
    pub fn cargo_registry_cache(&self) -> Option<&Path> {
        self.cargo_registry_cache.as_deref()
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

/// Loopback-only reference address selected when the webhook listener table
/// omits `bind_address`.
pub const DEFAULT_REPOSITORY_WATCH_WEBHOOK_BIND_ADDRESS: SocketAddr =
    SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 3333));

/// One deployment-owned local HTTP listener for authenticated GitHub hooks.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepositoryWatchWebhookConfiguration {
    bind_address: SocketAddr,
    path: Arc<str>,
}

impl RepositoryWatchWebhookConfiguration {
    /// Returns the exact local socket address the daemon must bind.
    pub const fn bind_address(&self) -> SocketAddr {
        self.bind_address
    }

    /// Returns the exact absolute local request path the listener admits.
    pub fn path(&self) -> &str {
        &self.path
    }
}

/// One watched repository's authenticated webhook association.
#[derive(Clone, Eq, PartialEq)]
pub struct WatchedRepositoryWebhookConfiguration {
    hook_id: NonZeroU64,
    secret_file: PathBuf,
    mode: RepositoryWatchWebhookMode,
}

impl WatchedRepositoryWebhookConfiguration {
    /// Returns the positive GitHub hook identity selecting this repository.
    pub const fn hook_id(&self) -> NonZeroU64 {
        self.hook_id
    }

    /// Returns the deployment-owned webhook-secret file reference.
    pub fn secret_file(&self) -> &Path {
        &self.secret_file
    }

    /// Returns whether authenticated deliveries only acknowledge or also wake ingestion.
    pub const fn mode(&self) -> RepositoryWatchWebhookMode {
        self.mode
    }
}

impl fmt::Debug for WatchedRepositoryWebhookConfiguration {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WatchedRepositoryWebhookConfiguration")
            .field("hook_id", &self.hook_id)
            .field("secret_file", &"[REDACTED REFERENCE]")
            .field("mode", &self.mode)
            .finish()
    }
}

/// Per-repository rollout mode for authenticated webhook deliveries.
///
/// Shadow authenticates and acknowledges without waking ingestion. Primary
/// wakes the repository task to fetch a complete provider observation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RepositoryWatchWebhookMode {
    Shadow,
    Primary,
}

/// One repository-specific polling and credential configuration.
#[derive(Clone, Eq, PartialEq)]
pub struct WatchedRepositoryConfiguration {
    repository: RepositorySlug,
    poll_interval: Duration,
    credential_file: PathBuf,
    webhook: Option<WatchedRepositoryWebhookConfiguration>,
    convergence_pull_requests: Box<[PullRequestNumber]>,
}

impl WatchedRepositoryConfiguration {
    /// Returns the canonical repository identity authorized by this entry.
    pub const fn repository(&self) -> &RepositorySlug {
        &self.repository
    }

    /// Returns the positive start-to-start interval between scheduled polls.
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

    /// Returns this repository's authenticated webhook association, if enabled.
    pub const fn webhook(&self) -> Option<&WatchedRepositoryWebhookConfiguration> {
        self.webhook.as_ref()
    }

    /// Returns the explicit operator-owned convergence throttle for this repository.
    pub fn convergence_pull_requests(&self) -> &[PullRequestNumber] {
        &self.convergence_pull_requests
    }

    /// Returns the non-secret reference used to resolve this repository's
    /// webhook secret, if webhook delivery is enabled for it.
    pub fn webhook_secret_reference(&self) -> Option<CredentialReference> {
        self.webhook.as_ref().map(|_| {
            CredentialReference::new(format!(
                "repository-watch-webhook:{}",
                self.repository.as_str()
            ))
        })
    }
}

impl fmt::Debug for WatchedRepositoryConfiguration {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WatchedRepositoryConfiguration")
            .field("repository", &self.repository)
            .field("poll_interval", &self.poll_interval)
            .field("credential_file", &"[REDACTED REFERENCE]")
            .field("webhook", &self.webhook)
            .field("convergence_pull_requests", &self.convergence_pull_requests)
            .finish()
    }
}

/// Complete optional repository-watch configuration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepositoryWatchConfiguration {
    enabled: bool,
    signal_reviewers: Box<[RepoWatchAuthorLogin]>,
    repositories: Box<[WatchedRepositoryConfiguration]>,
    rules: Box<[RepoWatchRule]>,
    webhook: Option<RepositoryWatchWebhookConfiguration>,
    convergence_sweep: Option<ConvergenceSweepConfiguration>,
}

/// Daemon-native convergence sweep policy, enabled only with explicit targets.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConvergenceSweepConfiguration {
    template: SessionTemplateName,
    interval: Duration,
    cool_off: Duration,
}

impl ConvergenceSweepConfiguration {
    /// Returns the fenced session template used for review-response work.
    pub const fn template(&self) -> &SessionTemplateName {
        &self.template
    }
    /// Returns the census interval, never above its hard ceiling.
    pub const fn interval(&self) -> Duration {
        self.interval
    }
    /// Returns the per-pull-request dispatch cool-off, never above its hard ceiling.
    pub const fn cool_off(&self) -> Duration {
        self.cool_off
    }
}

impl RepositoryWatchConfiguration {
    /// Returns whether repository polling, webhook wakes, and dispatch are enabled.
    pub const fn enabled(&self) -> bool {
        self.enabled
    }

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

    /// Returns the configured local webhook listener, or absence when webhook
    /// intake is disabled.
    pub const fn webhook(&self) -> Option<&RepositoryWatchWebhookConfiguration> {
        self.webhook.as_ref()
    }

    /// Returns enabled convergence reconciliation policy, if explicitly configured.
    pub const fn convergence_sweep(&self) -> Option<&ConvergenceSweepConfiguration> {
        if self.enabled {
            self.convergence_sweep.as_ref()
        } else {
            None
        }
    }

    /// Validates the convergence template against the immutable session-template catalog.
    pub fn validate_convergence_template<'a>(
        &self,
        templates: impl Iterator<Item = &'a SessionTemplateName>,
    ) -> Result<(), HubModelConfigurationError> {
        let Some(policy) = self.convergence_sweep() else {
            return Ok(());
        };
        if templates.into_iter().any(|name| name == policy.template()) {
            Ok(())
        } else {
            Err(
                HubModelConfigurationError::UnknownConvergenceSweepTemplate {
                    template: policy.template().as_str().to_owned(),
                },
            )
        }
    }
}

/// Validated static model and alias definitions used by hub composition.
#[derive(Clone, Debug)]
pub struct HubModelConfiguration {
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

    /// Parses one complete versioned configuration document.
    pub fn parse(content: &str) -> Result<Self, HubModelConfigurationError> {
        let document = DocumentMut::from_str(content)
            .map_err(|_| HubModelConfigurationError::InvalidDocument)?;
        reject_unknown_fields(
            document.as_table(),
            &[
                "version",
                "numeric_bounds",
                "credential_profiles",
                "credential_pools",
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
                "convergence",
                "repository_watch",
                "blob_storage",
                "workspace_instructions",
            ],
        )?;
        if document.get("version").and_then(|item| item.as_integer()) != Some(1) {
            return Err(HubModelConfigurationError::UnsupportedVersion);
        }
        let numeric_bounds = NumericBoundsConfiguration::parse(document.get("numeric_bounds"))?;
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
        let minimum_blob_bytes = u64::try_from(conversation_import_max_source_bytes)
            .map_err(|_| HubModelConfigurationError::InvalidBlobStorageConfiguration)?;
        let blob_storage =
            BlobStorageConfiguration::parse(document.get("blob_storage"), minimum_blob_bytes)
                .map_err(|_| HubModelConfigurationError::InvalidBlobStorageConfiguration)?;
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
        let credential_profiles = parse_credential_profiles(document.get("credential_profiles"))?;
        let credential_pools =
            parse_credential_pools(document.get("credential_pools"), &credential_profiles)?;
        let tool_approval_postures =
            parse_tool_approval_postures(document.get("tool_approval_postures"))?;
        let approval_judge_selection = parse_approval_judge(document.get("approval_judge"))?;
        #[derive(serde::Deserialize)]
        struct ConvergenceSection {
            convergence: Option<signalbox_convergence::ConvergencePolicy>,
        }
        let convergence = toml::from_str::<ConvergenceSection>(content)
            .map_err(|_| HubModelConfigurationError::InvalidDocument)?
            .convergence;
        if let Some(policy) = &convergence {
            policy
                .validate()
                .map_err(|_| HubModelConfigurationError::InvalidDocument)?;
        }
        let repository_watch = document
            .get("repository_watch")
            .map(|item| parse_repository_watch_configuration(item, &numeric_bounds))
            .transpose()?;
        let workspace_instructions =
            parse_workspace_instruction_configuration(document.get("workspace_instructions"))?;
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
            reject_unknown_fields(mapping, &["model_family", "adapter", "credential_pool"])?;
            let family = validated_name(required_string(mapping, "model_family")?)?;
            let adapter = ModelAdapter::parse(required_string(mapping, "adapter")?)?;
            let credential_pool = validated_name(required_string(mapping, "credential_pool")?)?;
            let Some(pool) = credential_pools.get(&credential_pool) else {
                return Err(HubModelConfigurationError::UnknownCredentialPool {
                    model_family: family,
                    credential_pool,
                });
            };
            if pool.adapter() != adapter {
                return Err(HubModelConfigurationError::ConflictingPoolAdapters {
                    credential_pool,
                });
            }
            let credential_profile = pool
                .preferred_member()
                .map(|member| Arc::<str>::from(member.profile()))
                .ok_or_else(|| HubModelConfigurationError::EmptyCredentialPool {
                    credential_pool: Arc::clone(&credential_pool),
                })?;
            let adapter_profile = match adapter {
                ModelAdapter::CodexCli => &mut codex_cli_credential_profile,
                ModelAdapter::ClaudeCli => &mut claude_cli_credential_profile,
                ModelAdapter::Anthropic | ModelAdapter::OpenAi => {
                    // Direct HTTP runtimes resolve the operation's pinned
                    // profile from the complete file-access catalog.
                    let entry = AdapterMapping {
                        adapter,
                        credential_pool,
                        credential_profile: Arc::clone(&credential_profile),
                    };
                    if mappings.contains_key(&family) {
                        return Err(HubModelConfigurationError::DuplicateModelFamily {
                            model_family: family,
                        });
                    }
                    mappings.insert(Arc::clone(&family), entry);
                    session_credentials
                        .push(SessionModelCredential::new(family, credential_profile));
                    continue;
                }
            };
            // CLI runtimes receive their complete adapter-scoped delivery
            // catalogs. The retained value is only the default for an ambient
            // operation that pins no catalog member.
            adapter_profile.get_or_insert_with(|| Arc::clone(&credential_profile));
            let entry = AdapterMapping {
                adapter,
                credential_pool,
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
        let fallback_credential_profile = session_credentials
            .first()
            .map(|credential| Arc::from(credential.credential_reference()))
            .ok_or(HubModelConfigurationError::InvalidField)?;
        let session_credential_pin = SessionCredentialPin::try_new(session_credentials)
            .map_err(|_| HubModelConfigurationError::InvalidField)?;

        let codex_cli = document
            .get("codex_cli")
            .map(|item| {
                let table = item
                    .as_table()
                    .ok_or(HubModelConfigurationError::InvalidCodexCliConfiguration)?;
                reject_unknown_fields(
                    table,
                    &[
                        "executable",
                        "working_directory",
                        "model_context_window_overrides",
                    ],
                )?;
                let executable = PathBuf::from(required_string(table, "executable")?);
                let working_directory = PathBuf::from(required_string(table, "working_directory")?);
                let model_context_window_overrides =
                    parse_positive_u32_inline_map(table.get("model_context_window_overrides"))?;
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
                    model_context_window_overrides,
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
            let mut runtime_configuration = CodexCliConfig::new(
                configuration.executable.clone(),
                configuration.working_directory.clone(),
                CredentialReference::new(
                    codex_cli_credential_profile
                        .as_deref()
                        .unwrap_or(CODEX_CLI_CREDENTIAL_REFERENCE),
                ),
                None,
            );
            runtime_configuration.model_context_window_overrides =
                configuration.model_context_window_overrides.clone();
            CodexCliRuntime::new(runtime_configuration)
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
                None,
                None,
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
        let mut target_credential_pools = HashMap::with_capacity(models.len());
        let mut target_model_families = HashMap::with_capacity(models.len());
        let mut target_fast_targets = HashMap::new();
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
                    "provider_compaction",
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

fn parse_workspace_instruction_configuration(
    item: Option<&Item>,
) -> Result<WorkspaceInstructionConfiguration, HubModelConfigurationError> {
    let Some(item) = item else {
        return Ok(WorkspaceInstructionConfiguration {
            roots: Box::new([]),
        });
    };
    let table = item
        .as_table()
        .ok_or(HubModelConfigurationError::InvalidWorkspaceInstructionConfiguration)?;
    reject_unknown_fields(table, &["version", "registered_roots"])
        .map_err(|_| HubModelConfigurationError::InvalidWorkspaceInstructionConfiguration)?;
    if table.get("version").and_then(Item::as_integer) != Some(1) {
        return Err(HubModelConfigurationError::InvalidWorkspaceInstructionConfiguration);
    }
    let values = table
        .get("registered_roots")
        .and_then(Item::as_array)
        .ok_or(HubModelConfigurationError::InvalidWorkspaceInstructionConfiguration)?;
    if values.len() > 64 {
        return Err(HubModelConfigurationError::InvalidWorkspaceInstructionConfiguration);
    }
    let mut roots = Vec::with_capacity(values.len());
    let mut unique = BTreeSet::new();
    for value in values {
        let value = value
            .as_str()
            .ok_or(HubModelConfigurationError::InvalidWorkspaceInstructionConfiguration)?;
        let root = InstructionPath::try_new(value.to_owned())
            .map_err(|_| HubModelConfigurationError::InvalidWorkspaceInstructionConfiguration)?;
        if !unique.insert(root.clone()) {
            return Err(HubModelConfigurationError::InvalidWorkspaceInstructionConfiguration);
        }
        roots.push(root);
    }
    Ok(WorkspaceInstructionConfiguration {
        roots: roots.into_boxed_slice(),
    })
}

fn parse_repository_watch_configuration(
    item: &Item,
    numeric_bounds: &NumericBoundsConfiguration,
) -> Result<RepositoryWatchConfiguration, HubModelConfigurationError> {
    let table = item
        .as_table()
        .ok_or(HubModelConfigurationError::InvalidRepositoryWatchConfiguration)?;
    reject_unknown_fields(
        table,
        &[
            "version",
            "enabled",
            "signal_reviewers",
            "repositories",
            "rules",
            "webhook",
            "convergence_sweep",
        ],
    )
    .map_err(|_| HubModelConfigurationError::InvalidRepositoryWatchConfiguration)?;
    if table.get("version").and_then(Item::as_integer) != Some(1) {
        return Err(HubModelConfigurationError::InvalidRepositoryWatchConfiguration);
    }
    let enabled = table
        .get("enabled")
        .map(|value| {
            value
                .as_bool()
                .ok_or(HubModelConfigurationError::InvalidRepositoryWatchConfiguration)
        })
        .transpose()?
        .unwrap_or(true);
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

    let webhook = parse_repository_watch_webhook_configuration(table.get("webhook"))?;
    let convergence_sweep = parse_convergence_sweep_configuration(
        table.get("convergence_sweep"),
        numeric_bounds
            .duration("max_convergence_sweep_interval")
            .flatten(),
        numeric_bounds
            .duration("max_convergence_sweep_cool_off")
            .flatten(),
    )?;

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
    let mut webhook_hook_ids = HashSet::with_capacity(repository_tables.len());
    let mut webhook_repository_count = 0_usize;
    for repository in repository_tables {
        reject_unknown_fields(
            repository,
            &[
                "repository",
                "poll_interval_seconds",
                "credential_file",
                "webhook_hook_id",
                "webhook_secret_file",
                "webhook_mode",
                "convergence_pull_requests",
            ],
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
        let repository_webhook = match (
            repository.get("webhook_hook_id"),
            repository.get("webhook_secret_file"),
            repository.get("webhook_mode"),
        ) {
            (None, None, None) => None,
            (Some(hook_id), Some(secret_file), mode) => {
                let hook_id = hook_id
                    .as_integer()
                    .and_then(|value| u64::try_from(value).ok())
                    .and_then(NonZeroU64::new)
                    .ok_or(HubModelConfigurationError::InvalidRepositoryWatchConfiguration)?;
                if !webhook_hook_ids.insert(hook_id) {
                    return Err(HubModelConfigurationError::DuplicateRepositoryWatchWebhookHookId);
                }
                let secret_file = secret_file
                    .as_str()
                    .map(PathBuf::from)
                    .ok_or(HubModelConfigurationError::InvalidRepositoryWatchConfiguration)?;
                if !secret_file.is_absolute()
                    || secret_file
                        .components()
                        .any(|component| matches!(component, Component::ParentDir))
                {
                    return Err(HubModelConfigurationError::InvalidRepositoryWatchConfiguration);
                }
                let resolved_secret_file = resolved_credential_file_reference(&secret_file)?;
                if credential_file_references.iter().any(|existing| {
                    credential_file_references_conflict(existing, &resolved_secret_file)
                }) {
                    return Err(HubModelConfigurationError::DuplicateRepositoryWatchCredentialFile);
                }
                credential_file_references.push(resolved_secret_file);
                // Only an absent key defaults. A present item of any other TOML
                // type is malformed configuration rather than an omission, so it
                // is refused instead of silently selecting the shadow rollout
                // mode a deployment did not ask for.
                let mode = match mode {
                    None => RepositoryWatchWebhookMode::Shadow,
                    Some(item) => match item.as_str() {
                        Some("shadow") => RepositoryWatchWebhookMode::Shadow,
                        Some("primary") => RepositoryWatchWebhookMode::Primary,
                        Some(_) | None => {
                            return Err(
                                HubModelConfigurationError::InvalidRepositoryWatchConfiguration,
                            );
                        }
                    },
                };
                webhook_repository_count += 1;
                Some(WatchedRepositoryWebhookConfiguration {
                    hook_id,
                    secret_file,
                    mode,
                })
            }
            (Some(_), None, _) | (None, Some(_), _) | (None, None, Some(_)) => {
                return Err(HubModelConfigurationError::InvalidRepositoryWatchConfiguration);
            }
        };
        let convergence_pull_requests =
            parse_convergence_pull_requests(repository.get("convergence_pull_requests"))?;
        repositories.push(WatchedRepositoryConfiguration {
            repository: repository_slug,
            poll_interval: Duration::from_secs(interval),
            credential_file,
            webhook: repository_webhook,
            convergence_pull_requests: convergence_pull_requests.into_boxed_slice(),
        });
    }
    if webhook.is_some() != (webhook_repository_count > 0) {
        return Err(HubModelConfigurationError::InvalidRepositoryWatchConfiguration);
    }
    repositories.sort_by(|left, right| left.repository.cmp(&right.repository));
    let rules = parse_repository_watch_rules(table)?;
    let convergence_target_count = repositories
        .iter()
        .map(|repository| repository.convergence_pull_requests.len())
        .sum::<usize>();
    let convergence_target_limit = numeric_bounds
        .integer("max_convergence_sweep_targets")
        .flatten()
        .and_then(|value| usize::try_from(value).ok());
    if convergence_target_limit.is_some_and(|limit| convergence_target_count > limit)
        || (convergence_target_count == 0) != convergence_sweep.is_none()
    {
        return Err(HubModelConfigurationError::InvalidRepositoryWatchConfiguration);
    }
    Ok(RepositoryWatchConfiguration {
        enabled,
        signal_reviewers: signal_reviewers.into_boxed_slice(),
        repositories: repositories.into_boxed_slice(),
        rules: rules.into_boxed_slice(),
        webhook,
        convergence_sweep,
    })
}

fn parse_convergence_sweep_configuration(
    item: Option<&Item>,
    interval_ceiling: Option<Duration>,
    cool_off_ceiling: Option<Duration>,
) -> Result<Option<ConvergenceSweepConfiguration>, HubModelConfigurationError> {
    let Some(item) = item else {
        return Ok(None);
    };
    let table = item
        .as_table()
        .ok_or(HubModelConfigurationError::InvalidRepositoryWatchConfiguration)?;
    reject_unknown_fields(table, &["template", "interval_seconds", "cool_off_seconds"])
        .map_err(|_| HubModelConfigurationError::InvalidRepositoryWatchConfiguration)?;
    let template = SessionTemplateName::try_new(required_string(table, "template")?.to_owned())
        .map_err(|_| HubModelConfigurationError::InvalidRepositoryWatchConfiguration)?;
    let interval = bounded_positive_duration(table, "interval_seconds", interval_ceiling)?;
    let cool_off = bounded_positive_duration(table, "cool_off_seconds", cool_off_ceiling)?;
    Ok(Some(ConvergenceSweepConfiguration {
        template,
        interval,
        cool_off,
    }))
}

fn bounded_positive_duration(
    table: &Table,
    field: &str,
    ceiling: Option<Duration>,
) -> Result<Duration, HubModelConfigurationError> {
    table
        .get(field)
        .and_then(Item::as_integer)
        .and_then(|value| u64::try_from(value).ok())
        .filter(|value| *value > 0)
        .map(Duration::from_secs)
        .filter(|value| ceiling.is_none_or(|ceiling| *value <= ceiling))
        .ok_or(HubModelConfigurationError::InvalidRepositoryWatchConfiguration)
}

fn parse_convergence_pull_requests(
    item: Option<&Item>,
) -> Result<Vec<PullRequestNumber>, HubModelConfigurationError> {
    let Some(item) = item else {
        return Ok(Vec::new());
    };
    let values = item
        .as_array()
        .ok_or(HubModelConfigurationError::InvalidRepositoryWatchConfiguration)?;
    let mut parsed = Vec::with_capacity(values.len());
    for value in values {
        let number = value
            .as_integer()
            .and_then(|value| u64::try_from(value).ok())
            .and_then(NonZeroU64::new)
            .filter(|value| value.get() <= i32::MAX as u64)
            .map(PullRequestNumber::new)
            .ok_or(HubModelConfigurationError::InvalidRepositoryWatchConfiguration)?;
        if parsed.contains(&number) {
            return Err(HubModelConfigurationError::InvalidRepositoryWatchConfiguration);
        }
        parsed.push(number);
    }
    parsed.sort();
    Ok(parsed)
}

fn parse_repository_watch_webhook_configuration(
    item: Option<&Item>,
) -> Result<Option<RepositoryWatchWebhookConfiguration>, HubModelConfigurationError> {
    let Some(item) = item else {
        return Ok(None);
    };
    let table = item
        .as_table()
        .ok_or(HubModelConfigurationError::InvalidRepositoryWatchConfiguration)?;
    reject_unknown_fields(table, &["bind_address", "path"])
        .map_err(|_| HubModelConfigurationError::InvalidRepositoryWatchConfiguration)?;
    let bind_address = table
        .get("bind_address")
        .map(|item| {
            item.as_str()
                .and_then(|value| value.parse::<SocketAddr>().ok())
                .ok_or(HubModelConfigurationError::InvalidRepositoryWatchConfiguration)
        })
        .transpose()?
        .unwrap_or(DEFAULT_REPOSITORY_WATCH_WEBHOOK_BIND_ADDRESS);
    let path = required_string(table, "path")
        .map_err(|_| HubModelConfigurationError::InvalidRepositoryWatchConfiguration)?;
    if !valid_repository_watch_webhook_path(path) {
        return Err(HubModelConfigurationError::InvalidRepositoryWatchConfiguration);
    }
    Ok(Some(RepositoryWatchWebhookConfiguration {
        bind_address,
        path: Arc::from(path),
    }))
}

/// Whether the configured path names exactly one literal request path.
///
/// Configuration promises one exact path, but `Router::route` reads its argument
/// as a route pattern: Axum 0.8 treats `{name}` and `{*name}` as captures that
/// match many paths, and it panics on the legacy `:name` and `*name` forms. Both
/// are rejected here rather than at listener start.
fn valid_repository_watch_webhook_path(path: &str) -> bool {
    path.starts_with('/')
        && path.bytes().all(|byte| byte.is_ascii_graphic())
        && !path.contains(['?', '#'])
        && !path.contains(REPOSITORY_WATCH_WEBHOOK_ROUTE_METACHARACTERS)
}

/// Characters Axum reads as routing syntax rather than as literal path bytes.
const REPOSITORY_WATCH_WEBHOOK_ROUTE_METACHARACTERS: [char; 4] = ['*', ':', '{', '}'];

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
        let id = RepoWatchRuleId::try_new(
            required_string(table, "id")
                .map_err(|_| HubModelConfigurationError::InvalidRepositoryWatchConfiguration)?
                .to_owned(),
        )
        .map_err(|_| HubModelConfigurationError::InvalidRepositoryWatchConfiguration)?;
        if !identities.insert(id.clone()) {
            return Err(HubModelConfigurationError::InvalidRepositoryWatchConfiguration);
        }
        let version = table
            .get("version")
            .and_then(Item::as_integer)
            .and_then(|value| u64::try_from(value).ok())
            .and_then(NonZeroU64::new)
            .and_then(RepoWatchRuleVersion::new)
            .ok_or_else(|| HubModelConfigurationError::InvalidRepositoryWatchRule {
                rule: id.as_str().to_owned(),
                reason: String::from(
                    "field `version` must be a positive integer within signed 64-bit range",
                ),
            })?;
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
        let rule = RepoWatchRule::try_new(
            id.clone(),
            version,
            matcher,
            actions,
            singleton_per,
            cooldown,
        )
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
/// - a bare name is looked up in `search_path`, the daemon's own `PATH`, and resolves to the first
///   entry holding a regular file of that name this process can execute;
/// - any other value is a path, returned verbatim for the caller's absolute-existing-file rule to
///   judge, so a configured path never resolves through `PATH` to a different program.
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

/// Whether this process could execute `path` as a program.
///
/// Both halves are load-bearing. The metadata check rejects anything that is
/// not a regular file, because execute access on a directory means the right
/// to traverse it. The access check asks the kernel about the daemon's own
/// effective credentials rather than reading permission bits, so a file some
/// other user may execute — mode `0o700` owned by another UID, or one an ACL
/// denies — does not satisfy a search entry and shadow a bridge the daemon can
/// actually run in a later one.
#[cfg(unix)]
fn is_executable_file(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|metadata| metadata.is_file())
        && rustix::fs::accessat(
            rustix::fs::CWD,
            path,
            rustix::fs::Access::EXEC_OK,
            rustix::fs::AtFlags::EACCESS,
        )
        .is_ok()
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

fn fold_reported_cost(axes: [(Option<u128>, Decimal); 4]) -> Option<Decimal> {
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

fn exact_rate_token_product(rate: Decimal, tokens: u128) -> Option<Decimal> {
    if tokens > u128::try_from(Decimal::MAX.mantissa()).ok()? {
        return None;
    }
    let product = rate.checked_mul(Decimal::from(tokens))?;
    let scale_loss = rate.scale().checked_sub(product.scale())?;
    if scale_loss == 0 {
        return Some(product);
    }
    let mut rate_mantissa = u128::try_from(rate.mantissa()).ok()?;
    let mut token_mantissa = tokens;
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
    daemon_tool_settings: Option<DaemonToolSettings>,
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
    let settings =
        daemon_tool_settings.ok_or(HubModelConfigurationError::MissingDaemonToolSettings)?;
    Ok(Some(DaemonToolConfiguration {
        workspace_root: workspace_root.ok_or(HubModelConfigurationError::InvalidToolMappings)?,
        git_identity: git_identity
            .ok_or(HubModelConfigurationError::MissingGitIdentityConfiguration)?,
        exec_supervisor_executable: settings.exec_supervisor_executable,
        cargo_registry_cache: settings.cargo_registry_cache,
    }))
}

#[derive(Clone, Debug)]
struct DaemonToolSettings {
    exec_supervisor_executable: PathBuf,
    cargo_registry_cache: Option<PathBuf>,
}

fn parse_daemon_tool_settings(
    item: Option<&Item>,
) -> Result<Option<DaemonToolSettings>, HubModelConfigurationError> {
    let Some(item) = item else {
        return Ok(None);
    };
    let table = item
        .as_table()
        .ok_or(HubModelConfigurationError::InvalidDaemonToolSettings)?;
    reject_unknown_fields(
        table,
        &["exec_supervisor_executable", "cargo_registry_cache"],
    )
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
    let cargo_registry_cache = table
        .get("cargo_registry_cache")
        .map(|_| {
            let path = PathBuf::from(
                required_string(table, "cargo_registry_cache")
                    .map_err(|_| HubModelConfigurationError::InvalidDaemonToolSettings)?,
            );
            if !path.is_absolute() {
                return Err(HubModelConfigurationError::InvalidDaemonToolSettings);
            }
            let canonical = fs::canonicalize(path)
                .map_err(|_| HubModelConfigurationError::InvalidDaemonToolSettings)?;
            if !canonical.is_dir() {
                return Err(HubModelConfigurationError::InvalidDaemonToolSettings);
            }
            Ok(canonical)
        })
        .transpose()?;
    Ok(Some(DaemonToolSettings {
        exec_supervisor_executable: executable,
        cargo_registry_cache,
    }))
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
    let root_value = required_string(mapping, "workspace_root")?;
    let root = Path::new(root_value);
    if required_string(mapping, "adapter")? != "local"
        || !root.is_absolute()
        || InstructionPath::try_new(root_value.to_owned()).is_err()
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

fn parse_provider_compaction_capability(
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
    paths: Arc<HashMap<CredentialReference, PathBuf>>,
}

impl FileCredentialAccess {
    /// Binds one non-secret credential reference to one deployment file.
    pub fn new(path: PathBuf, reference: CredentialReference) -> Self {
        Self::from_files([(reference, path)])
    }

    /// Binds a complete set of non-secret credential references to deployment
    /// files. Each resolution selects and rereads only its mapped path.
    pub fn from_files(files: impl IntoIterator<Item = (CredentialReference, PathBuf)>) -> Self {
        Self {
            paths: Arc::new(files.into_iter().collect()),
        }
    }

    /// Returns the non-secret reference accepted by this source.
    pub fn credential_reference(&self) -> Option<CredentialReference> {
        (self.paths.len() == 1)
            .then(|| self.paths.keys().next().cloned())
            .flatten()
    }
}

impl fmt::Debug for FileCredentialAccess {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FileCredentialAccess")
            .field("paths", &"[credential file map]")
            .field("reference_count", &self.paths.len())
            .finish()
    }
}

impl CredentialAccess for FileCredentialAccess {
    async fn resolve(
        &self,
        reference: &CredentialReference,
    ) -> Result<CredentialValue, CredentialAccessError> {
        let path = self.paths.get(reference).ok_or_else(|| {
            CredentialAccessError::new(reference.clone(), CredentialAccessFailure::Unmapped)
        })?;
        let file_bytes = tokio::fs::read(path).await;
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
