//! Deployment-owned model mappings and credential delivery.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    error::Error,
    fmt, fs, io,
    net::{Ipv4Addr, SocketAddr, SocketAddrV4},
    num::NonZeroU64,
    path::{Component, Path, PathBuf},
    str::FromStr,
    sync::Arc,
    time::Duration,
};

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
    model_execution::{
        CredentialPoolRuntimeAction, CredentialPoolRuntimeCatalog, CredentialPoolRuntimeExhaustion,
        CredentialPoolRuntimeMember, CredentialPoolRuntimePolicy, ToolContinuationUsageLimit,
    },
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

use crate::blob_storage_configuration::BlobStorageConfiguration;
use crate::credential_pools::{
    CredentialDelivery, CredentialPool, CredentialPoolAction, CredentialPoolExhaustion,
    CredentialPoolTrigger, CredentialProfile, parse_credential_pools, parse_credential_profiles,
};

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
    /// `insufficient_quota` but reaches overload only by status. Neither CLI
    /// adapter supplies a machine-readable availability cause. Listing every pair
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
            (Self::ClaudeCli | Self::CodexCli, _) => false,
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

    /// Returns whether authenticated deliveries only project or also write.
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
/// Shadow projects a delivery against an in-memory baseline and writes only
/// parity rows; the durable cursor stays the poller's. Primary applies the
/// delivery to the durable cursor and writes ordinary webhook-produced events.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RepositoryWatchWebhookMode {
    Shadow,
    Primary,
}

/// One repository-specific version-one polling and credential configuration.
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

/// Complete optional version-one repository-watch configuration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepositoryWatchConfiguration {
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
        self.convergence_sweep.as_ref()
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum NumericBoundKind {
    Integer,
    Duration,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum NumericBoundValue {
    Integer(u64),
    Duration(Duration),
    Unbounded,
}

/// Validated deployment policy for every non-structural numeric bound.
#[derive(Clone, Debug)]
pub struct NumericBoundsConfiguration {
    values: HashMap<&'static str, NumericBoundValue>,
}

const REQUIRED_NUMERIC_BOUNDS: &[(&str, NumericBoundKind)] = &[
    (
        "repository_reconciliation_quantum",
        NumericBoundKind::Integer,
    ),
    ("webhook_drain_work_budget", NumericBoundKind::Duration),
    ("fenced_pool_min_connections", NumericBoundKind::Integer),
    (
        "fenced_pool_floor_reconciliation_interval",
        NumericBoundKind::Duration,
    ),
    (
        "fenced_pool_floor_reconciliation_attempt_bound",
        NumericBoundKind::Duration,
    ),
    ("max_concurrent_snapshot_readers", NumericBoundKind::Integer),
    ("max_blob_replica_count", NumericBoundKind::Integer),
    ("max_session_metadata_tags", NumericBoundKind::Integer),
    ("max_session_metadata_attributes", NumericBoundKind::Integer),
    (
        "max_session_metadata_required_tags",
        NumericBoundKind::Integer,
    ),
    ("max_system_prompt_utf8_bytes", NumericBoundKind::Integer),
    (
        "max_imported_text_preview_utf8_bytes",
        NumericBoundKind::Integer,
    ),
    (
        "max_review_orchestration_concerns",
        NumericBoundKind::Integer,
    ),
    (
        "max_imported_conversation_display_title_scalars",
        NumericBoundKind::Integer,
    ),
    (
        "graceful_shutdown_cleanup_window",
        NumericBoundKind::Duration,
    ),
    ("model_exchange_timeout", NumericBoundKind::Duration),
    ("codex_cli_version_probe_bound", NumericBoundKind::Duration),
    ("expired_pass_recovery_attempts", NumericBoundKind::Integer),
    (
        "expired_pass_recovery_attempt_bound",
        NumericBoundKind::Duration,
    ),
    (
        "expired_pass_recovery_lock_retry_delay",
        NumericBoundKind::Duration,
    ),
    (
        "expired_pass_recovery_conservative_retry_delay",
        NumericBoundKind::Duration,
    ),
    (
        "convergence_sweep_request_timeout",
        NumericBoundKind::Duration,
    ),
    (
        "max_convergence_sweep_connection_pages",
        NumericBoundKind::Integer,
    ),
    (
        "max_concurrent_convergence_sweep_targets",
        NumericBoundKind::Integer,
    ),
    (
        "max_convergence_sweep_request_attempts",
        NumericBoundKind::Integer,
    ),
    (
        "convergence_sweep_request_retry_delay",
        NumericBoundKind::Duration,
    ),
    (
        "convergence_sweep_retry_backoff_base",
        NumericBoundKind::Duration,
    ),
    (
        "convergence_sweep_retry_backoff_cap",
        NumericBoundKind::Duration,
    ),
    (
        "terminalizations_per_liveness_scan",
        NumericBoundKind::Integer,
    ),
    (
        "turn_liveness_recovery_attempt_bound",
        NumericBoundKind::Duration,
    ),
    (
        "automatic_reconciliations_per_liveness_scan",
        NumericBoundKind::Integer,
    ),
    (
        "automatic_reconciliation_attempt_bound",
        NumericBoundKind::Duration,
    ),
    ("max_convergence_sweep_targets", NumericBoundKind::Integer),
    ("max_convergence_sweep_interval", NumericBoundKind::Duration),
    ("max_convergence_sweep_cool_off", NumericBoundKind::Duration),
    ("automatic_resume_base_backoff", NumericBoundKind::Duration),
    ("automatic_resume_backoff_cap", NumericBoundKind::Duration),
    ("automatic_resume_attempt_budget", NumericBoundKind::Integer),
    (
        "automatic_resume_attempt_ceiling",
        NumericBoundKind::Integer,
    ),
    (
        "automatic_resume_startup_retry_delay",
        NumericBoundKind::Duration,
    ),
    ("post_kill_reap_bound", NumericBoundKind::Duration),
    ("stale_active_turn_bound", NumericBoundKind::Duration),
    ("turn_liveness_scan_interval", NumericBoundKind::Duration),
    (
        "automatic_reconciliation_base_backoff",
        NumericBoundKind::Duration,
    ),
    (
        "automatic_reconciliation_backoff_cap",
        NumericBoundKind::Duration,
    ),
    (
        "automatic_reconciliation_attempt_budget",
        NumericBoundKind::Integer,
    ),
    ("terminal_input_channel_capacity", NumericBoundKind::Integer),
    ("max_message_utf8_bytes", NumericBoundKind::Integer),
    ("min_metadata_page_size", NumericBoundKind::Integer),
    ("max_metadata_page_size", NumericBoundKind::Integer),
    ("max_review_findings_per_run", NumericBoundKind::Integer),
    (
        "max_automatic_tool_rounds_per_turn",
        NumericBoundKind::Integer,
    ),
    (
        "max_same_credential_attempts_per_turn",
        NumericBoundKind::Integer,
    ),
    ("max_required_tags", NumericBoundKind::Integer),
    ("reconciliation_sweep_interval", NumericBoundKind::Duration),
    ("nudge_buffer_capacity", NumericBoundKind::Integer),
    ("scheduler_pass_admission_cap", NumericBoundKind::Integer),
    ("scheduler_pass_occupancy_bound", NumericBoundKind::Duration),
    ("max_native_message_bytes", NumericBoundKind::Integer),
    ("terminalization_lock_wait", NumericBoundKind::Duration),
    ("terminalization_acquire_wait", NumericBoundKind::Duration),
    (
        "terminalization_write_lock_wait",
        NumericBoundKind::Duration,
    ),
    (
        "disposable_postgres_state_ceiling_bytes",
        NumericBoundKind::Integer,
    ),
    ("diagnostic_model_identity_limit", NumericBoundKind::Integer),
    ("code_host_request_timeout", NumericBoundKind::Duration),
    ("max_job_log_bytes", NumericBoundKind::Integer),
    ("max_stack_comparisons_in_flight", NumericBoundKind::Integer),
    ("max_code_host_result_text_bytes", NumericBoundKind::Integer),
    ("max_code_host_result_items", NumericBoundKind::Integer),
    (
        "max_repository_file_content_bytes",
        NumericBoundKind::Integer,
    ),
    ("session_admission_deadline", NumericBoundKind::Duration),
    ("session_active_stall_deadline", NumericBoundKind::Duration),
    ("session_waiting_deadline", NumericBoundKind::Duration),
    (
        "session_lifecycle_metric_scan_interval",
        NumericBoundKind::Duration,
    ),
];

impl NumericBoundsConfiguration {
    fn parse(item: Option<&Item>) -> Result<Self, HubModelConfigurationError> {
        let table = item.and_then(Item::as_table);
        let missing = REQUIRED_NUMERIC_BOUNDS
            .iter()
            .filter_map(|(name, _)| {
                table
                    .is_none_or(|table| !table.contains_key(name))
                    .then_some(*name)
            })
            .collect::<Vec<_>>();
        if !missing.is_empty() {
            return Err(HubModelConfigurationError::MissingNumericBounds { fields: missing });
        }
        let table = table.ok_or_else(|| HubModelConfigurationError::MissingNumericBounds {
            fields: REQUIRED_NUMERIC_BOUNDS
                .iter()
                .map(|(name, _)| *name)
                .collect(),
        })?;
        let allowed_fields = REQUIRED_NUMERIC_BOUNDS
            .iter()
            .map(|(name, _)| *name)
            .collect::<Vec<_>>();
        reject_unknown_fields(table, &allowed_fields)?;
        let mut values = HashMap::with_capacity(REQUIRED_NUMERIC_BOUNDS.len());
        for (name, kind) in REQUIRED_NUMERIC_BOUNDS {
            let item = table.get(name).ok_or_else(|| {
                HubModelConfigurationError::MissingNumericBounds {
                    fields: vec![*name],
                }
            })?;
            let value = if item.as_str() == Some("none") {
                NumericBoundValue::Unbounded
            } else {
                match kind {
                    NumericBoundKind::Integer => item
                        .as_integer()
                        .and_then(|value| u64::try_from(value).ok())
                        .filter(|value| usize::try_from(*value).is_ok())
                        .map(NumericBoundValue::Integer),
                    NumericBoundKind::Duration => item
                        .as_str()
                        .and_then(parse_numeric_bound_duration)
                        .map(NumericBoundValue::Duration),
                }
                .ok_or(HubModelConfigurationError::InvalidNumericBound { field: name })?
            };
            values.insert(*name, value);
        }
        Ok(Self { values })
    }

    /// Returns one integer policy, with inner `None` denoting configured `"none"`.
    ///
    /// Callers use field names from the checked-in schema inventory.
    pub fn integer(&self, field: &'static str) -> Option<Option<u64>> {
        match self.values.get(field) {
            Some(NumericBoundValue::Integer(value)) => Some(Some(*value)),
            Some(NumericBoundValue::Unbounded) => Some(None),
            _ => None,
        }
    }

    /// Returns one duration policy, with inner `None` denoting configured `"none"`.
    ///
    /// Callers use field names from the checked-in schema inventory.
    pub fn duration(&self, field: &'static str) -> Option<Option<Duration>> {
        match self.values.get(field) {
            Some(NumericBoundValue::Duration(value)) => Some(Some(*value)),
            Some(NumericBoundValue::Unbounded) => Some(None),
            _ => None,
        }
    }
}

fn parse_numeric_bound_duration(value: &str) -> Option<Duration> {
    value
        .strip_suffix("ms")
        .and_then(|amount| amount.parse::<u64>().ok())
        .map(Duration::from_millis)
        .or_else(|| {
            value
                .strip_suffix('s')
                .and_then(|amount| amount.parse::<u64>().ok())
                .map(Duration::from_secs)
        })
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
                target_model_families.insert(target, Arc::clone(&model_family));
                target_credential_pools.insert(target, Arc::clone(&mapping.credential_pool));
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
                tool_continuation_usage_limits.push(ToolContinuationUsageLimit::new(
                    route.target,
                    fast_mode,
                    u64::from(effective.max_output_tokens()),
                    u64::from(effective.context_window_tokens()),
                ));
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
        let example = include_str!("../../../config/signalboxd.example.toml");
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
/// - a bare name is looked up in `search_path`, the daemon's own `PATH`, and
///   resolves to the first entry holding a regular file of that name this
///   process can execute;
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

pub(crate) fn validated_name(value: &str) -> Result<Arc<str>, HubModelConfigurationError> {
    if value.is_empty() || value.trim() != value || value.contains('\0') {
        Err(HubModelConfigurationError::InvalidField)
    } else {
        Ok(Arc::from(value))
    }
}

pub(crate) fn reject_unknown_fields(
    table: &Table,
    allowed: &[&str],
) -> Result<(), HubModelConfigurationError> {
    if table.iter().any(|(key, _)| !allowed.contains(&key)) {
        Err(HubModelConfigurationError::UnknownField)
    } else {
        Ok(())
    }
}

pub(crate) fn required_string<'a>(
    table: &'a Table,
    key: &str,
) -> Result<&'a str, HubModelConfigurationError> {
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

fn parse_positive_u32_inline_map(
    item: Option<&Item>,
) -> Result<HashMap<String, u32>, HubModelConfigurationError> {
    let Some(item) = item else {
        return Ok(HashMap::new());
    };
    let table = item
        .as_inline_table()
        .ok_or(HubModelConfigurationError::InvalidCodexCliConfiguration)?;
    table
        .iter()
        .map(|(target, value)| {
            validated_name(target)
                .map_err(|_| HubModelConfigurationError::InvalidCodexCliConfiguration)?;
            let value = value
                .as_integer()
                .and_then(|value| u32::try_from(value).ok())
                .filter(|value| *value > 0)
                .ok_or(HubModelConfigurationError::InvalidCodexCliConfiguration)?;
            Ok((target.to_string(), value))
        })
        .collect()
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
    /// One or more required deployment numeric-bound fields were absent.
    MissingNumericBounds {
        /// Every absent field, in schema order.
        fields: Vec<&'static str>,
    },
    /// One required deployment numeric bound had the wrong type or spelling.
    InvalidNumericBound {
        /// The rejected field.
        field: &'static str,
    },
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
    /// A profile's `billing_kind` contradicts the authentication its delivery
    /// establishes.
    ///
    /// Both spellings are carried because a refusal naming only the profile
    /// leaves an operator to rediscover which of its two fields to edit.
    DisagreeingCredentialBillingKind {
        /// Exact profile name whose two fields disagree.
        credential_profile: Arc<str>,
        /// Exact delivery spelling that fixes the authentication kind.
        delivery: Arc<str>,
        /// Exact billing kind the profile declared alongside it.
        billing_kind: Arc<str>,
    },
    /// A credential profile named no delivery, or its delivery's own fields
    /// were absent or malformed.
    InvalidCredentialDelivery,
    /// One member's Codex home failed path/directory admission.
    InvalidCredentialHome {
        /// Non-secret profile reference identifying the failed member.
        credential_profile: Arc<str>,
        /// Closed startup failure class; never path or auth material.
        failure: crate::credential_pools::CredentialHomeAdmissionFailure,
    },
    /// A credential profile named a delivery its adapter does not admit.
    UnsupportedCredentialDelivery {
        /// Build-provided adapter whose admitted deliveries were checked.
        adapter: ModelAdapter,
        /// Exact delivery spelling that adapter does not admit.
        delivery: Arc<str>,
    },
    /// A credential profile named a delivery the grammar admits but no surface
    /// in this build supplies.
    UndeliveredCredentialDelivery {
        /// Exact delivery spelling no composed surface honors.
        delivery: Arc<str>,
    },
    /// No nonempty credential pool array exists.
    MissingCredentialPools,
    /// One credential pool appeared more than once.
    DuplicateCredentialPool {
        /// Exact repeated pool name.
        credential_pool: Arc<str>,
    },
    /// A credential pool declared no members.
    EmptyCredentialPool {
        /// Exact pool name that admitted no member.
        credential_pool: Arc<str>,
    },
    /// One profile appeared twice among a pool's members.
    DuplicatePoolMember {
        /// Exact pool name carrying the repetition.
        credential_pool: Arc<str>,
        /// Exact repeated profile name.
        credential_profile: Arc<str>,
    },
    /// A pool member named no declared credential profile.
    UnknownPoolMemberProfile {
        /// Exact pool name carrying the member.
        credential_pool: Arc<str>,
        /// Exact profile spelling absent from the profile registry.
        credential_profile: Arc<str>,
    },
    /// An adapter mapping named no declared credential pool.
    UnknownCredentialPool {
        /// Exact family key whose mapping named the pool.
        model_family: Arc<str>,
        /// Exact pool spelling absent from the pool registry.
        credential_pool: Arc<str>,
    },
    /// A pool's members carried different adapters, or a mapping's adapter
    /// disagreed with its pool's.
    ConflictingPoolAdapters {
        /// Exact pool name carrying the disagreement.
        credential_pool: Arc<str>,
    },
    /// A pool member's priority was absent, zero, or outside `u32`.
    InvalidMemberPriority {
        /// Exact pool name carrying the member.
        credential_pool: Arc<str>,
    },
    /// A pool named no supported tie-break or exhaustion behavior.
    InvalidCredentialPoolPolicy,
    /// A pool trigger named no supported action.
    UnknownCredentialPoolAction,
    /// A pool trigger carried an action that cause does not admit.
    InadmissibleCredentialPoolAction {
        /// Exact trigger key whose configured action it does not admit.
        trigger: Arc<str>,
    },
    /// A headroom reserve was outside zero through ninety-nine percent.
    InvalidHeadroomReserve,
    /// A pool's selection depends on remaining capacity its adapter does not
    /// report, so the setting could never take effect.
    UnobservedCapacityPolicy {
        /// Exact pool name carrying the unobservable setting.
        credential_pool: Arc<str>,
    },
    /// A pool configures `switch_now` for an adapter that cannot prove a
    /// provider did not accept the request, so the substitution could never
    /// take effect.
    UnprovableSubstitutionPolicy {
        /// Exact pool name carrying the unusable action.
        credential_pool: Arc<str>,
    },
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
    /// One adapter's model families resolved to more than one credential
    /// profile, which this build's single runtime per adapter cannot serve.
    ConflictingAdapterCredentialProfiles {
        /// Build-provided adapter whose families disagreed.
        adapter: ModelAdapter,
    },
    /// A Codex mapping exists without its required process configuration.
    MissingCodexCliConfiguration,
    /// Codex paths were malformed, relative, or named no existing directory.
    InvalidCodexCliConfiguration,
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
    /// The optional blob-store registry or its routes were malformed.
    InvalidBlobStorageConfiguration,
    /// The optional web-fetch table was malformed or named an invalid origin.
    InvalidWebFetchPolicy,
    /// The optional version-one repository-watch section was malformed.
    InvalidRepositoryWatchConfiguration,
    /// The optional version-one workspace-instruction section was malformed.
    InvalidWorkspaceInstructionConfiguration,
    /// The convergence sweep names no loaded session template.
    UnknownConvergenceSweepTemplate {
        /// Exact missing template name.
        template: String,
    },
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
    /// Two repository-watch polling credentials or webhook secrets resolve to
    /// the same file reference.
    DuplicateRepositoryWatchCredentialFile,
    /// Two webhook-enabled repositories named the same positive GitHub hook ID.
    DuplicateRepositoryWatchWebhookHookId,
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
        if let Self::MissingNumericBounds { fields } = self {
            return write!(
                formatter,
                "model configuration is missing required numeric bounds: {}",
                fields.join(", ")
            );
        }
        if let Self::InvalidNumericBound { field } = self {
            return write!(
                formatter,
                "model configuration contains invalid numeric bound `{field}`"
            );
        }
        if let Self::InvalidRepositoryWatchRule { rule, reason } = self {
            return write!(
                formatter,
                "model configuration contains invalid repository-watch rule `{rule}`: {reason}"
            );
        }
        if let Self::UnknownConvergenceSweepTemplate { template } = self {
            return write!(
                formatter,
                "model configuration names unknown convergence template `{template}`"
            );
        }
        // Startup telemetry formats this value, so the failing member and the
        // closed admission cause must both survive. The path never appears, as
        // `configuration-and-credentials.md` requires.
        if let Self::InvalidCredentialHome {
            credential_profile,
            failure,
        } = self
        {
            return write!(
                formatter,
                "model configuration credential profile `{credential_profile}` names an unavailable Codex credential home: {}",
                failure.cause()
            );
        }
        formatter.write_str(match self {
            Self::Read => "model configuration file could not be read",
            Self::InvalidDocument => "model configuration is not valid TOML",
            Self::UnsupportedVersion => "model configuration version is unsupported",
            Self::MissingNumericBounds { .. } => {
                "model configuration is missing required numeric bounds"
            }
            Self::InvalidNumericBound { .. } => {
                "model configuration contains an invalid numeric bound"
            }
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
            Self::DisagreeingCredentialBillingKind { .. } => {
                "model configuration declares a billing kind its credential delivery cannot authenticate"
            }
            Self::InvalidCredentialDelivery => {
                "model configuration contains an invalid credential delivery"
            }
            Self::InvalidCredentialHome { .. } => {
                "model configuration contains an unavailable Codex credential home"
            }
            Self::UnsupportedCredentialDelivery { .. } => {
                "model configuration names a credential delivery its adapter does not admit"
            }
            Self::UndeliveredCredentialDelivery { .. } => {
                "model configuration names a credential delivery this build does not supply"
            }
            Self::MissingCredentialPools => "model configuration has no credential pools",
            Self::DuplicateCredentialPool { .. } => "model configuration repeats a credential pool",
            Self::EmptyCredentialPool { .. } => {
                "model configuration contains a credential pool with no members"
            }
            Self::DuplicatePoolMember { .. } => {
                "model configuration repeats a credential pool member"
            }
            Self::UnknownPoolMemberProfile { .. } => {
                "model configuration pools an undeclared credential profile"
            }
            Self::UnknownCredentialPool { .. } => {
                "model configuration names an undeclared credential pool"
            }
            Self::ConflictingPoolAdapters { .. } => {
                "model configuration gives one credential pool conflicting adapters"
            }
            Self::InvalidMemberPriority { .. } => {
                "model configuration contains an invalid credential pool priority"
            }
            Self::InvalidCredentialPoolPolicy => {
                "model configuration contains an invalid credential pool policy"
            }
            Self::UnknownCredentialPoolAction => {
                "model configuration contains an unknown credential pool action"
            }
            Self::InadmissibleCredentialPoolAction { .. } => {
                "model configuration gives a credential pool trigger an inadmissible action"
            }
            Self::InvalidHeadroomReserve => {
                "model configuration contains an invalid headroom reserve"
            }
            Self::UnprovableSubstitutionPolicy { .. } => {
                "model configuration gives a credential pool a substitution its adapter cannot prove"
            }
            Self::UnobservedCapacityPolicy { .. } => {
                "model configuration depends on provider capacity no adapter reports"
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
            Self::DuplicateModelFamily { .. } => {
                "model configuration repeats a model family mapping"
            }
            Self::UnmappedModelFamily { .. } => {
                "model configuration names an unmapped model family"
            }
            Self::ConflictingProviderModelRoute => {
                "model configuration routes one provider model to conflicting adapters"
            }
            Self::ConflictingAdapterCredentialProfiles { .. } => {
                "model configuration routes one adapter through conflicting credential profiles"
            }
            Self::MissingCodexCliConfiguration => {
                "model configuration maps Codex CLI without Codex CLI settings"
            }
            Self::InvalidCodexCliConfiguration => {
                "model configuration contains invalid Codex CLI settings"
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
            Self::InvalidBlobStorageConfiguration => {
                "model configuration contains invalid blob-storage settings"
            }
            Self::InvalidWebFetchPolicy => {
                "model configuration contains an invalid web_fetch egress policy"
            }
            Self::InvalidRepositoryWatchConfiguration => {
                "model configuration contains invalid repository-watch settings"
            }
            Self::InvalidWorkspaceInstructionConfiguration => {
                "model configuration contains invalid workspace-instruction settings"
            }
            Self::UnknownConvergenceSweepTemplate { .. } => {
                "model configuration names an unknown convergence template"
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
            Self::DuplicateRepositoryWatchWebhookHookId => {
                "model configuration repeats a repository-watch webhook hook ID"
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
    paths: Arc<HashMap<CredentialReference, PathBuf>>,
    maximum_bytes: Option<usize>,
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
            maximum_bytes: None,
        }
    }

    pub(crate) fn new_bounded(
        path: PathBuf,
        reference: CredentialReference,
        maximum_bytes: usize,
    ) -> Self {
        Self {
            paths: Arc::new(HashMap::from([(reference, path)])),
            maximum_bytes: Some(maximum_bytes),
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
        let file_bytes = match self.maximum_bytes {
            Some(maximum_bytes) => read_bounded_credential_file(path, maximum_bytes).await,
            None => tokio::fs::read(path).await,
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
pub(crate) mod tests {
    use std::{
        collections::HashSet,
        net::SocketAddr,
        num::NonZeroU64,
        path::{Path, PathBuf},
        sync::Arc,
        time::Duration,
    };

    use rust_decimal::Decimal;
    use signalbox_domain::{
        AnthropicServiceTier, DirectModelSelection, FastMode, FastModeOverlay, MergeableState,
        ModelAlias, ModelSelectionRequest, ModelSettingSource, ModelSettingsOverlay,
        PullRequestNumber, ReasoningLevel, RepoWatchDispatchContextShape, RepoWatchEventKindNameV1,
        RepoWatchRuleVersion, RepoWatchSingletonScope, RepoWatchTemplateContextDeclaration,
        ServiceTier, SessionTemplateName, SettingOverlay, ToolApprovalPosture,
    };
    use signalbox_model_runtime::{CredentialAccess, CredentialAccessFailure, CredentialReference};
    use signalbox_persistence::process_read::ProcessModelCallInputTokenSemantics;
    use signalbox_tools_basic::{CURRENT_TIME_NAME, ECHO_NAME};
    use signalbox_tools_web::{WEB_FETCH_NAME, WebFetchEgressPolicy};
    use uuid::Uuid;

    use crate::credential_pools::{
        CredentialDelivery, CredentialPoolAction, CredentialPoolExhaustion, CredentialPoolTieBreak,
        CredentialPoolTrigger, MAX_CREDENTIAL_CATALOG_NAME_UTF8_BYTES,
        MAX_CREDENTIAL_DELIVERY_PATH_UTF8_BYTES, MAX_CREDENTIAL_HOME_CONCURRENT_INVOCATIONS,
        MAX_CREDENTIAL_POOL_MEMBERS,
    };

    use super::{
        ANTHROPIC_CREDENTIAL_REFERENCE, BillingKind, DEFAULT_CONVERSATION_IMPORT_MAX_SOURCE_BYTES,
        DEFAULT_REPOSITORY_WATCH_WEBHOOK_BIND_ADDRESS, FileCredentialAccess, HubModelConfiguration,
        HubModelConfigurationError, MAX_COMPACTION_PROMPT_UTF8_BYTES,
        MIGRATED_ANTHROPIC_MODEL_FAMILY, ModelAdapter, ModelCallInputUsage,
        RepositoryWatchWebhookMode, UnknownSessionModel, absolute_search_entries, credential_bytes,
        resolved_mcp_bridge_reference, validate_alias_count, validate_model_count,
    };

    const CODEX_SUBSCRIPTION_PROFILE: &str = "codex-subscription-primary";
    const ANTHROPIC_OVERFLOW_PROFILE: &str = "anthropic-overflow";

    fn example_numeric_duration(field: &'static str) -> Duration {
        super::checked_in_example_configuration()
            .expect("checked-in example parses")
            .numeric_bounds()
            .duration(field)
            .flatten()
            .expect("example field is bounded")
    }

    /// The exact pool block [`CONFIGURATION`] declares, so a test that cares
    /// about pool shape states its own replacement in full.
    /// The exact pool name [`ANTHROPIC_POOL`] declares.
    ///
    /// Bound once so a rename of the fixture cannot leave an assertion
    /// comparing against a stale literal (testing-style rule 6); the helper
    /// below asserts the fixture still spells it.
    const ANTHROPIC_POOL_NAME: &str = "anthropic-main";

    const ANTHROPIC_POOL: &str = r#"[[credential_pools]]
name = "anthropic-main"
tie_break = "first_listed"
on_pool_exhausted = "park"
members = [{ profile = "anthropic-primary", priority = 1 }]"#;

    /// The exact Codex pool block [`CONFIGURATION`] declares.
    const CODEX_POOL: &str = r#"[[credential_pools]]
name = "codex-main"
tie_break = "first_listed"
on_pool_exhausted = "fail"
members = [{ profile = "codex-subscription-primary", priority = 1 }]"#;

    const WATCH_REPOSITORY: &str = "namespace/project";
    const SECOND_WATCH_REPOSITORY: &str = "namespace/second";
    const WATCH_CREDENTIAL_FILE: &str = "/run/credentials/repository-watch-token";
    const SECOND_WATCH_CREDENTIAL_FILE: &str = "/run/credentials/second-watch-token";
    const WATCH_WEBHOOK_SECRET_FILE: &str = "/run/credentials/repository-watch-webhook-secret";
    const SECOND_WATCH_WEBHOOK_SECRET_FILE: &str =
        "/run/credentials/second-repository-watch-webhook-secret";
    const RELATIVE_WATCH_WEBHOOK_SECRET_FILE: &str = "relative/webhook-secret";
    const PARENT_COMPONENT_WATCH_WEBHOOK_SECRET_FILE: &str =
        "/run/credentials/alias/../repository-watch-webhook-secret";
    const PARENT_COMPONENT_WATCH_CREDENTIAL_FILE: &str =
        "/run/credentials/alias/../repository-watch-token";
    const WATCH_CREDENTIAL_REFERENCE: &str = "repository-watch:namespace/project";
    const WATCH_WEBHOOK_SECRET_REFERENCE: &str = "repository-watch-webhook:namespace/project";
    const WATCH_WEBHOOK_HOOK_ID: NonZeroU64 =
        NonZeroU64::new(123_456_789).expect("fixture hook ID is positive");
    const SECOND_WATCH_WEBHOOK_HOOK_ID: NonZeroU64 =
        NonZeroU64::new(987_654_321).expect("fixture hook ID is positive");
    const WATCH_WEBHOOK_BIND_ADDRESS: &str = "127.0.0.1:3333";
    const IPV6_WATCH_WEBHOOK_BIND_ADDRESS: &str = "[::1]:4444";
    const INVALID_WATCH_WEBHOOK_BIND_ADDRESS: &str = "localhost:3333";
    const WATCH_WEBHOOK_PATH: &str = "/";
    const RELATIVE_WATCH_WEBHOOK_PATH: &str = "github/webhooks";
    const QUERY_WATCH_WEBHOOK_PATH: &str = "/github/webhooks?mode=shadow";
    const CAPTURE_WATCH_WEBHOOK_PATH: &str = "/github/{delivery}";
    const LEGACY_CAPTURE_WATCH_WEBHOOK_PATH: &str = "/github/:delivery";
    const WILDCARD_WATCH_WEBHOOK_PATH: &str = "/github/*rest";
    const WATCH_INTERVAL_SECONDS: u64 = 90;
    const SECOND_WATCH_INTERVAL_SECONDS: u64 = 120;
    const CONVERGENCE_PULL_REQUEST: u64 = 892;
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
    const WATCH_RULE_ID: &str = "watch-forward";
    const EAGER_WATCH_RULE_ID: &str = "merge-forward-on-base-advance";
    const EAGER_WATCH_HEAD_PATTERN: &str = "^agent/.+$";
    const WATCH_TEMPLATE: &str = "merge-forward";
    const REGISTERED_INSTRUCTION_ROOT: &str = "/srv/signalbox/instruction-library";
    pub(crate) const CONFIGURATION: &str = r#"
version = 1

[numeric_bounds]
repository_reconciliation_quantum = 16
webhook_drain_work_budget = "45s"
fenced_pool_min_connections = 48
fenced_pool_floor_reconciliation_interval = "5s"
fenced_pool_floor_reconciliation_attempt_bound = "30s"
max_concurrent_snapshot_readers = 8
max_blob_replica_count = 32
max_session_metadata_tags = 256
max_session_metadata_attributes = 256
max_session_metadata_required_tags = 256
max_system_prompt_utf8_bytes = 1048576
max_imported_text_preview_utf8_bytes = 256
max_review_orchestration_concerns = 32
max_imported_conversation_display_title_scalars = 256
graceful_shutdown_cleanup_window = "30s"
model_exchange_timeout = "600s"
codex_cli_version_probe_bound = "10s"
expired_pass_recovery_attempts = 4
expired_pass_recovery_attempt_bound = "3s"
expired_pass_recovery_lock_retry_delay = "6s"
expired_pass_recovery_conservative_retry_delay = "120s"
convergence_sweep_request_timeout = "30s"
max_convergence_sweep_connection_pages = 100
max_concurrent_convergence_sweep_targets = 8
max_convergence_sweep_request_attempts = 3
convergence_sweep_request_retry_delay = "250ms"
convergence_sweep_retry_backoff_base = "60s"
convergence_sweep_retry_backoff_cap = "900s"
terminalizations_per_liveness_scan = 64
turn_liveness_recovery_attempt_bound = "10s"
automatic_reconciliations_per_liveness_scan = 64
automatic_reconciliation_attempt_bound = "60s"
max_convergence_sweep_targets = 256
max_convergence_sweep_interval = "300s"
max_convergence_sweep_cool_off = "1800s"
automatic_resume_base_backoff = "120s"
automatic_resume_backoff_cap = "1800s"
automatic_resume_attempt_budget = 20
automatic_resume_attempt_ceiling = 100
automatic_resume_startup_retry_delay = "1s"
post_kill_reap_bound = "5s"
stale_active_turn_bound = "1800s"
turn_liveness_scan_interval = "60s"
automatic_reconciliation_base_backoff = "120s"
automatic_reconciliation_backoff_cap = "1800s"
automatic_reconciliation_attempt_budget = 5
terminal_input_channel_capacity = 1
max_message_utf8_bytes = 1048576
min_metadata_page_size = 1
max_metadata_page_size = 100
max_review_findings_per_run = 32
max_automatic_tool_rounds_per_turn = 32
max_same_credential_attempts_per_turn = 2
max_required_tags = 256
reconciliation_sweep_interval = "1s"
nudge_buffer_capacity = 1024
scheduler_pass_admission_cap = 16
scheduler_pass_occupancy_bound = "3600s"
max_native_message_bytes = 2048
terminalization_lock_wait = "250ms"
terminalization_acquire_wait = "250ms"
terminalization_write_lock_wait = "1s"
disposable_postgres_state_ceiling_bytes = 536870912
diagnostic_model_identity_limit = 128
code_host_request_timeout = "none"
max_job_log_bytes = "none"
max_stack_comparisons_in_flight = "none"
max_code_host_result_text_bytes = "none"
max_code_host_result_items = "none"
max_repository_file_content_bytes = "none"
session_admission_deadline = "none"
session_active_stall_deadline = "none"
session_waiting_deadline = "none"
session_lifecycle_metric_scan_interval = "none"

[[credential_profiles]]
name = "anthropic-primary"
adapter = "anthropic"
billing_kind = "api_metered"
delivery = "file"
file = "/run/secrets/anthropic-primary"

[[credential_profiles]]
name = "anthropic-overflow"
adapter = "anthropic"
billing_kind = "api_metered"
delivery = "file"
file = "/run/secrets/anthropic-overflow"

[[credential_profiles]]
name = "codex-subscription-primary"
adapter = "codex_cli"
billing_kind = "subscription"
delivery = "ambient"

[[credential_pools]]
name = "anthropic-main"
tie_break = "first_listed"
on_pool_exhausted = "park"
members = [{ profile = "anthropic-primary", priority = 1 }]

[[credential_pools]]
name = "codex-main"
tie_break = "first_listed"
on_pool_exhausted = "fail"
members = [{ profile = "codex-subscription-primary", priority = 1 }]

[[adapter_mappings]]
model_family = "anthropic"
adapter = "anthropic"
credential_pool = "anthropic-main"

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

    /// Replaces the whole Anthropic pool block, leaving every other table as
    /// [`CONFIGURATION`] declares it.
    fn configuration_with_anthropic_pool(pool: &str) -> String {
        assert!(
            CONFIGURATION.contains(ANTHROPIC_POOL),
            "fixture declares the pool block tests replace"
        );
        assert!(
            ANTHROPIC_POOL.contains(ANTHROPIC_POOL_NAME),
            "the bound pool name is the one the fixture block declares"
        );
        CONFIGURATION.replace(ANTHROPIC_POOL, pool)
    }

    #[test]
    fn configuration_lists_every_missing_required_numeric_bound() {
        const FIRST_FIELD: &str = "max_session_metadata_tags";
        const SECOND_FIELD: &str = "max_message_utf8_bytes";
        let missing = CONFIGURATION
            .replace("max_session_metadata_tags = 256\n", "")
            .replace("max_message_utf8_bytes = 1048576\n", "");

        let error = HubModelConfiguration::parse(&missing)
            .expect_err("a configuration missing required numeric bounds is refused");

        assert_eq!(
            error,
            HubModelConfigurationError::MissingNumericBounds {
                fields: vec![FIRST_FIELD, SECOND_FIELD],
            }
        );
        assert_eq!(
            error.to_string(),
            format!(
                "model configuration is missing required numeric bounds: {FIRST_FIELD}, {SECOND_FIELD}"
            )
        );
    }

    #[test]
    fn configuration_admits_none_for_each_numeric_bound_kind() {
        let unbounded = CONFIGURATION
            .replace(
                "max_message_utf8_bytes = 1048576",
                "max_message_utf8_bytes = \"none\"",
            )
            .replace(
                "turn_liveness_scan_interval = \"60s\"",
                "turn_liveness_scan_interval = \"none\"",
            );

        let configuration = HubModelConfiguration::parse(&unbounded)
            .expect("the exact none spelling is admitted for every bound kind");

        assert_eq!(
            configuration
                .numeric_bounds()
                .integer("max_message_utf8_bytes"),
            Some(None)
        );
        assert_eq!(
            configuration
                .numeric_bounds()
                .duration("turn_liveness_scan_interval"),
            Some(None)
        );
    }

    const OPENAI_PROFILE: &str = "openai-primary";
    const OPENAI_MAPPING_AND_MODEL: &str = r#"
[[credential_profiles]]
name = "openai-primary"
adapter = "openai"
billing_kind = "api_metered"
delivery = "file"
file = "/run/secrets/openai-primary"

[[credential_pools]]
name = "openai-main"
tie_break = "first_listed"
on_pool_exhausted = "fail"
members = [{ profile = "openai-primary", priority = 1 }]

[[adapter_mappings]]
model_family = "openai"
adapter = "openai"
credential_pool = "openai-main"

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

    /// One Claude process table whose executable and working directory both
    /// exist, so the bridge is the only value a test states.
    ///
    /// The returned directory is that working directory and is held by the
    /// caller: dropping it would delete the path the document names.
    fn configuration_varying_the_claude_bridge(
        mcp_bridge_executable: &Path,
    ) -> (String, tempfile::TempDir) {
        let workspace = tempfile::tempdir().expect("fixture directory is available");
        let executable = std::env::current_exe().expect("the test executable has a path");
        let configuration =
            configuration_with_claude_paths(&executable, mcp_bridge_executable, workspace.path());
        (configuration, workspace)
    }

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
adapter = "claude_cli"
billing_kind = "subscription"
delivery = "ambient"

[[credential_pools]]
name = "claude-code-main"
tie_break = "first_listed"
on_pool_exhausted = "fail"
members = [{{ profile = "{CLAUDE_SUBSCRIPTION_PROFILE}", priority = 1 }}]

[[adapter_mappings]]
model_family = "claude_code"
adapter = "claude_cli"
credential_pool = "claude-code-main"

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
credential_pool = "codex-main"

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
        let configuration = CONFIGURATION
            .replace("codex-subscription-primary", "codex-api-primary")
            .replace(
                "billing_kind = \"subscription\"",
                "billing_kind = \"api_metered\"",
            );
        format!(
            r#"{configuration}
[[adapter_mappings]]
model_family = "codex-api"
adapter = "codex_cli"
credential_pool = "codex-main"

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

    fn configuration_with_convergence_sweep() -> String {
        format!(
            r#"{}

[repository_watch.convergence_sweep]
template = "{WATCH_TEMPLATE}"
interval_seconds = {}
cool_off_seconds = {}
"#,
            configuration_with_repository_watch().replace(
                &format!("repository = \"{PROVIDER_WATCH_REPOSITORY}\""),
                &format!(
                    "repository = \"{PROVIDER_WATCH_REPOSITORY}\"\nconvergence_pull_requests = [{CONVERGENCE_PULL_REQUEST}]"
                ),
            ),
            example_numeric_duration("max_convergence_sweep_interval").as_secs(),
            example_numeric_duration("max_convergence_sweep_cool_off").as_secs(),
        )
    }

    fn configuration_with_repository_watch_webhook_entry() -> String {
        configuration_with_repository_watch().replace(
            &format!("credential_file = \"{WATCH_CREDENTIAL_FILE}\""),
            &format!(
                "credential_file = \"{WATCH_CREDENTIAL_FILE}\"\nwebhook_hook_id = {WATCH_WEBHOOK_HOOK_ID}\nwebhook_secret_file = \"{WATCH_WEBHOOK_SECRET_FILE}\""
            ),
        )
    }

    fn configuration_with_repository_watch_webhook() -> String {
        format!(
            r#"{}

[repository_watch.webhook]
bind_address = "{WATCH_WEBHOOK_BIND_ADDRESS}"
path = "{WATCH_WEBHOOK_PATH}"
"#,
            configuration_with_repository_watch_webhook_entry()
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

    fn configuration_with_eager_merge_forward_rule() -> String {
        format!(
            r#"{}

[[repository_watch.rules]]
id = "{EAGER_WATCH_RULE_ID}"
version = 1
singleton_per = "pull_request"
cooldown_seconds = 0

[repository_watch.rules.matcher]
event_kinds = ["base_advanced"]
repo = "{PROVIDER_WATCH_REPOSITORY}"
head_branch_regex = "{EAGER_WATCH_HEAD_PATTERN}"

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
    fn repository_watch_webhook_is_absent_by_default() {
        let configured = HubModelConfiguration::parse(&configuration_with_repository_watch())
            .expect("repository-watch fixture is valid");
        let repository_watch = configured
            .repository_watch()
            .expect("fixture configures repository watch");

        assert_eq!(repository_watch.webhook(), None);
        assert_eq!(repository_watch.repositories()[0].webhook(), None);
    }

    #[test]
    fn repository_watch_webhook_preserves_the_local_listener() {
        let configured =
            HubModelConfiguration::parse(&configuration_with_repository_watch_webhook())
                .expect("repository-watch webhook fixture is valid");
        let webhook = configured
            .repository_watch()
            .expect("fixture configures repository watch")
            .webhook()
            .expect("fixture configures the webhook listener");

        assert_eq!(
            webhook.bind_address(),
            DEFAULT_REPOSITORY_WATCH_WEBHOOK_BIND_ADDRESS
        );
        assert_eq!(webhook.path(), WATCH_WEBHOOK_PATH);
    }

    #[test]
    fn repository_watch_webhook_defaults_the_bind_address() {
        let configured = configuration_with_repository_watch_webhook().replace(
            &format!("bind_address = \"{WATCH_WEBHOOK_BIND_ADDRESS}\"\n"),
            "",
        );
        let parsed = HubModelConfiguration::parse(&configured)
            .expect("the omitted bind address selects the reference default");
        let webhook = parsed
            .repository_watch()
            .expect("fixture configures repository watch")
            .webhook()
            .expect("fixture configures the webhook listener");

        assert_eq!(
            webhook.bind_address(),
            DEFAULT_REPOSITORY_WATCH_WEBHOOK_BIND_ADDRESS
        );
    }

    #[test]
    fn repository_watch_webhook_accepts_a_configured_socket_address() {
        let configured = configuration_with_repository_watch_webhook().replace(
            &format!("bind_address = \"{WATCH_WEBHOOK_BIND_ADDRESS}\""),
            &format!("bind_address = \"{IPV6_WATCH_WEBHOOK_BIND_ADDRESS}\""),
        );
        let parsed = HubModelConfiguration::parse(&configured)
            .expect("the configured IPv6 loopback listener is valid");
        let webhook = parsed
            .repository_watch()
            .expect("fixture configures repository watch")
            .webhook()
            .expect("fixture configures the webhook listener");

        assert_eq!(
            webhook.bind_address(),
            IPV6_WATCH_WEBHOOK_BIND_ADDRESS
                .parse::<SocketAddr>()
                .expect("fixture address is valid")
        );
    }

    #[test]
    fn repository_watch_webhook_associates_hook_and_secret_with_repository() {
        let configured =
            HubModelConfiguration::parse(&configuration_with_repository_watch_webhook())
                .expect("repository-watch webhook fixture is valid");
        let watched = &configured
            .repository_watch()
            .expect("fixture configures repository watch")
            .repositories()[0];
        let webhook = watched
            .webhook()
            .expect("the first repository configures webhook intake");

        assert_eq!(webhook.hook_id(), WATCH_WEBHOOK_HOOK_ID);
        assert_eq!(webhook.secret_file(), Path::new(WATCH_WEBHOOK_SECRET_FILE));
        assert_eq!(
            watched
                .webhook_secret_reference()
                .expect("the webhook repository has a secret reference")
                .as_str(),
            WATCH_WEBHOOK_SECRET_REFERENCE
        );
    }

    #[test]
    fn repository_watch_webhook_defaults_to_shadow_mode() {
        let configured =
            HubModelConfiguration::parse(&configuration_with_repository_watch_webhook())
                .expect("repository-watch webhook fixture is valid");
        let webhook = configured
            .repository_watch()
            .expect("fixture configures repository watch")
            .repositories()[0]
            .webhook()
            .expect("the first repository configures webhook intake");

        assert_eq!(webhook.mode(), RepositoryWatchWebhookMode::Shadow);
    }

    #[test]
    fn repository_watch_webhook_selects_primary_mode() {
        let configured = configuration_with_repository_watch_webhook().replace(
            &format!("webhook_secret_file = \"{WATCH_WEBHOOK_SECRET_FILE}\""),
            &format!(
                "webhook_secret_file = \"{WATCH_WEBHOOK_SECRET_FILE}\"\nwebhook_mode = \"primary\""
            ),
        );
        let configured = HubModelConfiguration::parse(&configured)
            .expect("an explicit primary webhook mode is valid");
        let webhook = configured
            .repository_watch()
            .expect("fixture configures repository watch")
            .repositories()[0]
            .webhook()
            .expect("the first repository configures webhook intake");

        assert_eq!(webhook.mode(), RepositoryWatchWebhookMode::Primary);
    }

    /// Only an absent key defaults. A present non-string item is malformed
    /// configuration, not an omission, so it must not silently select shadow.
    #[test]
    fn repository_watch_webhook_rejects_a_non_string_mode() {
        let configured = configuration_with_repository_watch_webhook().replace(
            &format!("webhook_secret_file = \"{WATCH_WEBHOOK_SECRET_FILE}\""),
            &format!("webhook_secret_file = \"{WATCH_WEBHOOK_SECRET_FILE}\"\nwebhook_mode = true"),
        );

        assert_eq!(
            HubModelConfiguration::parse(&configured).err(),
            Some(HubModelConfigurationError::InvalidRepositoryWatchConfiguration)
        );
    }

    #[test]
    fn repository_watch_webhook_rejects_an_unknown_mode() {
        let configured = configuration_with_repository_watch_webhook().replace(
            &format!("webhook_secret_file = \"{WATCH_WEBHOOK_SECRET_FILE}\""),
            &format!(
                "webhook_secret_file = \"{WATCH_WEBHOOK_SECRET_FILE}\"\nwebhook_mode = \"authoritative\""
            ),
        );

        assert_eq!(
            HubModelConfiguration::parse(&configured).err(),
            Some(HubModelConfigurationError::InvalidRepositoryWatchConfiguration)
        );
    }

    #[test]
    fn repository_watch_webhook_rejects_a_mode_without_an_association() {
        let configured = configuration_with_repository_watch_webhook()
            .replace(
                &format!("webhook_hook_id = {WATCH_WEBHOOK_HOOK_ID}\n"),
                "webhook_mode = \"primary\"\n",
            )
            .replace(
                &format!("webhook_secret_file = \"{WATCH_WEBHOOK_SECRET_FILE}\"\n"),
                "",
            );

        assert_eq!(
            HubModelConfiguration::parse(&configured).err(),
            Some(HubModelConfigurationError::InvalidRepositoryWatchConfiguration)
        );
    }

    #[test]
    fn repository_watch_webhook_debug_redacts_the_secret_file_reference() {
        let configured =
            HubModelConfiguration::parse(&configuration_with_repository_watch_webhook())
                .expect("repository-watch webhook fixture is valid");
        let webhook = configured
            .repository_watch()
            .expect("fixture configures repository watch")
            .repositories()[0]
            .webhook()
            .expect("the first repository configures webhook intake");
        let debug = format!("{webhook:?}");

        assert!(!debug.contains(WATCH_WEBHOOK_SECRET_FILE));
        assert!(debug.contains("[REDACTED REFERENCE]"));
    }

    #[test]
    fn repository_watch_webhook_rejects_a_listener_without_enabled_repository() {
        let configured = format!(
            "{}\n[repository_watch.webhook]\npath = \"{WATCH_WEBHOOK_PATH}\"\n",
            configuration_with_repository_watch()
        );

        assert_eq!(
            HubModelConfiguration::parse(&configured).err(),
            Some(HubModelConfigurationError::InvalidRepositoryWatchConfiguration)
        );
    }

    #[test]
    fn repository_watch_webhook_rejects_an_entry_without_listener() {
        let configured = configuration_with_repository_watch_webhook_entry();

        assert_eq!(
            HubModelConfiguration::parse(&configured).err(),
            Some(HubModelConfigurationError::InvalidRepositoryWatchConfiguration)
        );
    }

    #[test]
    fn repository_watch_webhook_rejects_hook_id_without_secret_file() {
        let configured = configuration_with_repository_watch_webhook().replace(
            &format!("webhook_secret_file = \"{WATCH_WEBHOOK_SECRET_FILE}\"\n"),
            "",
        );

        assert_eq!(
            HubModelConfiguration::parse(&configured).err(),
            Some(HubModelConfigurationError::InvalidRepositoryWatchConfiguration)
        );
    }

    #[test]
    fn repository_watch_webhook_rejects_secret_file_without_hook_id() {
        let configured = configuration_with_repository_watch_webhook()
            .replace(&format!("webhook_hook_id = {WATCH_WEBHOOK_HOOK_ID}\n"), "");

        assert_eq!(
            HubModelConfiguration::parse(&configured).err(),
            Some(HubModelConfigurationError::InvalidRepositoryWatchConfiguration)
        );
    }

    #[test]
    fn repository_watch_webhook_rejects_zero_hook_id() {
        let configured = configuration_with_repository_watch_webhook().replace(
            &format!("webhook_hook_id = {WATCH_WEBHOOK_HOOK_ID}"),
            "webhook_hook_id = 0",
        );

        assert_eq!(
            HubModelConfiguration::parse(&configured).err(),
            Some(HubModelConfigurationError::InvalidRepositoryWatchConfiguration)
        );
    }

    #[test]
    fn repository_watch_webhook_rejects_duplicate_hook_ids() {
        let configured = configuration_with_repository_watch_webhook().replace(
            &format!("credential_file = \"{SECOND_WATCH_CREDENTIAL_FILE}\""),
            &format!(
                "credential_file = \"{SECOND_WATCH_CREDENTIAL_FILE}\"\nwebhook_hook_id = {WATCH_WEBHOOK_HOOK_ID}\nwebhook_secret_file = \"{SECOND_WATCH_WEBHOOK_SECRET_FILE}\""
            ),
        );

        assert_eq!(
            HubModelConfiguration::parse(&configured).err(),
            Some(HubModelConfigurationError::DuplicateRepositoryWatchWebhookHookId)
        );
    }

    #[test]
    fn repository_watch_webhook_rejects_a_relative_local_path() {
        let configured = configuration_with_repository_watch_webhook().replace(
            &format!("path = \"{WATCH_WEBHOOK_PATH}\""),
            &format!("path = \"{RELATIVE_WATCH_WEBHOOK_PATH}\""),
        );

        assert_eq!(
            HubModelConfiguration::parse(&configured).err(),
            Some(HubModelConfigurationError::InvalidRepositoryWatchConfiguration)
        );
    }

    #[test]
    fn repository_watch_webhook_rejects_query_in_local_path() {
        let configured = configuration_with_repository_watch_webhook().replace(
            &format!("path = \"{WATCH_WEBHOOK_PATH}\""),
            &format!("path = \"{QUERY_WATCH_WEBHOOK_PATH}\""),
        );

        assert_eq!(
            HubModelConfiguration::parse(&configured).err(),
            Some(HubModelConfigurationError::InvalidRepositoryWatchConfiguration)
        );
    }

    /// Parses the webhook fixture with `path` replaced, returning any failure.
    fn repository_watch_webhook_path_failure(path: &str) -> Option<HubModelConfigurationError> {
        let configured = configuration_with_repository_watch_webhook().replace(
            &format!("path = \"{WATCH_WEBHOOK_PATH}\""),
            &format!("path = \"{path}\""),
        );
        HubModelConfiguration::parse(&configured).err()
    }

    #[test]
    fn repository_watch_webhook_rejects_a_braced_capture_local_path() {
        assert_eq!(
            repository_watch_webhook_path_failure(CAPTURE_WATCH_WEBHOOK_PATH),
            Some(HubModelConfigurationError::InvalidRepositoryWatchConfiguration)
        );
    }

    #[test]
    fn repository_watch_webhook_rejects_a_legacy_capture_local_path() {
        assert_eq!(
            repository_watch_webhook_path_failure(LEGACY_CAPTURE_WATCH_WEBHOOK_PATH),
            Some(HubModelConfigurationError::InvalidRepositoryWatchConfiguration)
        );
    }

    #[test]
    fn repository_watch_webhook_rejects_a_wildcard_local_path() {
        assert_eq!(
            repository_watch_webhook_path_failure(WILDCARD_WATCH_WEBHOOK_PATH),
            Some(HubModelConfigurationError::InvalidRepositoryWatchConfiguration)
        );
    }

    #[test]
    fn repository_watch_webhook_rejects_an_invalid_bind_address() {
        let configured = configuration_with_repository_watch_webhook().replace(
            &format!("bind_address = \"{WATCH_WEBHOOK_BIND_ADDRESS}\""),
            &format!("bind_address = \"{INVALID_WATCH_WEBHOOK_BIND_ADDRESS}\""),
        );

        assert_eq!(
            HubModelConfiguration::parse(&configured).err(),
            Some(HubModelConfigurationError::InvalidRepositoryWatchConfiguration)
        );
    }

    #[test]
    fn repository_watch_webhook_rejects_poll_credential_as_secret() {
        let configured = configuration_with_repository_watch_webhook()
            .replace(WATCH_WEBHOOK_SECRET_FILE, WATCH_CREDENTIAL_FILE);

        assert_eq!(
            HubModelConfiguration::parse(&configured).err(),
            Some(HubModelConfigurationError::DuplicateRepositoryWatchCredentialFile)
        );
    }

    #[test]
    fn repository_watch_webhook_rejects_a_relative_secret_file() {
        let configured = configuration_with_repository_watch_webhook().replace(
            WATCH_WEBHOOK_SECRET_FILE,
            RELATIVE_WATCH_WEBHOOK_SECRET_FILE,
        );

        assert_eq!(
            HubModelConfiguration::parse(&configured).err(),
            Some(HubModelConfigurationError::InvalidRepositoryWatchConfiguration)
        );
    }

    #[test]
    fn repository_watch_webhook_rejects_parent_components_in_secret_file() {
        let configured = configuration_with_repository_watch_webhook().replace(
            WATCH_WEBHOOK_SECRET_FILE,
            PARENT_COMPONENT_WATCH_WEBHOOK_SECRET_FILE,
        );

        assert_eq!(
            HubModelConfiguration::parse(&configured).err(),
            Some(HubModelConfigurationError::InvalidRepositoryWatchConfiguration)
        );
    }

    #[test]
    fn repository_watch_webhook_rejects_a_symlink_to_poll_credential() {
        let directory = tempfile::tempdir().expect("the credential fixture directory exists");
        let credential = directory.path().join("watch-token");
        std::fs::write(&credential, []).expect("the polling credential fixture exists");
        let secret_alias = directory.path().join("webhook-secret-alias");
        std::os::unix::fs::symlink(&credential, &secret_alias)
            .expect("the webhook secret alias exists");
        let configured = configuration_with_repository_watch_webhook()
            .replace(WATCH_CREDENTIAL_FILE, &credential.display().to_string())
            .replace(
                WATCH_WEBHOOK_SECRET_FILE,
                &secret_alias.display().to_string(),
            );

        assert_eq!(
            HubModelConfiguration::parse(&configured).err(),
            Some(HubModelConfigurationError::DuplicateRepositoryWatchCredentialFile)
        );
    }

    #[cfg(unix)]
    #[test]
    fn repository_watch_webhook_rejects_a_hard_link_to_poll_credential() {
        let directory = tempfile::tempdir().expect("the credential fixture directory exists");
        let credential = directory.path().join("watch-token");
        std::fs::write(&credential, []).expect("the polling credential fixture exists");
        let secret_hard_link = directory.path().join("webhook-secret-hard-link");
        std::fs::hard_link(&credential, &secret_hard_link)
            .expect("the webhook secret hard link exists");
        let configured = configuration_with_repository_watch_webhook()
            .replace(WATCH_CREDENTIAL_FILE, &credential.display().to_string())
            .replace(
                WATCH_WEBHOOK_SECRET_FILE,
                &secret_hard_link.display().to_string(),
            );

        assert_eq!(
            HubModelConfiguration::parse(&configured).err(),
            Some(HubModelConfigurationError::DuplicateRepositoryWatchCredentialFile)
        );
    }

    #[test]
    fn repository_watch_webhook_rejects_a_dangling_alias_to_poll_credential() {
        let directory = tempfile::tempdir().expect("the credential fixture directory exists");
        let credential = directory.path().join("pending-watch-token");
        let secret_alias = directory.path().join("pending-webhook-secret-alias");
        std::os::unix::fs::symlink(&credential, &secret_alias)
            .expect("the dangling webhook secret alias exists");
        let configured = configuration_with_repository_watch_webhook()
            .replace(WATCH_CREDENTIAL_FILE, &credential.display().to_string())
            .replace(
                WATCH_WEBHOOK_SECRET_FILE,
                &secret_alias.display().to_string(),
            );

        assert_eq!(
            HubModelConfiguration::parse(&configured).err(),
            Some(HubModelConfigurationError::DuplicateRepositoryWatchCredentialFile)
        );
    }

    #[test]
    fn repository_watch_webhook_rejects_a_shared_secret_file() {
        let configured = configuration_with_repository_watch_webhook().replace(
            &format!("credential_file = \"{SECOND_WATCH_CREDENTIAL_FILE}\""),
            &format!(
                "credential_file = \"{SECOND_WATCH_CREDENTIAL_FILE}\"\nwebhook_hook_id = {SECOND_WATCH_WEBHOOK_HOOK_ID}\nwebhook_secret_file = \"{WATCH_WEBHOOK_SECRET_FILE}\""
            ),
        );

        assert_eq!(
            HubModelConfiguration::parse(&configured).err(),
            Some(HubModelConfigurationError::DuplicateRepositoryWatchCredentialFile)
        );
    }

    #[test]
    fn repository_watch_parses_the_structured_rule_fields() {
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
    fn repository_watch_parses_the_explicit_convergence_sweep() {
        let configured = HubModelConfiguration::parse(&configuration_with_convergence_sweep())
            .expect("convergence sweep fixture is valid");
        let watch = configured
            .repository_watch()
            .expect("fixture configures repository watch");
        let policy = watch
            .convergence_sweep()
            .expect("fixture enables convergence reconciliation");
        let repository = watch
            .repositories()
            .iter()
            .find(|entry| {
                entry.repository().as_str() == PROVIDER_WATCH_REPOSITORY.to_ascii_lowercase()
            })
            .expect("fixture repository is retained");
        let pull_request = PullRequestNumber::new(
            NonZeroU64::new(CONVERGENCE_PULL_REQUEST).expect("fixture number is positive"),
        );

        assert_eq!(policy.template().as_str(), WATCH_TEMPLATE);
        assert_eq!(
            policy.interval(),
            example_numeric_duration("max_convergence_sweep_interval")
        );
        assert_eq!(
            policy.cool_off(),
            example_numeric_duration("max_convergence_sweep_cool_off")
        );
        assert_eq!(repository.convergence_pull_requests(), [pull_request]);
    }

    #[test]
    fn repository_watch_rejects_an_unknown_convergence_template() {
        let configured = HubModelConfiguration::parse(&configuration_with_convergence_sweep())
            .expect("convergence sweep fixture is valid");
        let watch = configured
            .repository_watch()
            .expect("fixture configures repository watch");
        let available = SessionTemplateName::try_new(String::from("another-template"))
            .expect("available template fixture is valid");

        assert_eq!(
            watch.validate_convergence_template(std::iter::once(&available)),
            Err(
                HubModelConfigurationError::UnknownConvergenceSweepTemplate {
                    template: String::from(WATCH_TEMPLATE),
                }
            )
        );
    }

    #[test]
    fn repository_watch_rejects_convergence_policy_without_targets() {
        let configured = configuration_with_convergence_sweep().replace(
            &format!("convergence_pull_requests = [{CONVERGENCE_PULL_REQUEST}]\n"),
            "",
        );

        assert_eq!(
            HubModelConfiguration::parse(&configured).err(),
            Some(HubModelConfigurationError::InvalidRepositoryWatchConfiguration)
        );
    }

    #[test]
    fn repository_watch_rejects_convergence_targets_without_policy() {
        let policy = format!(
            r#"
[repository_watch.convergence_sweep]
template = "{WATCH_TEMPLATE}"
interval_seconds = {}
cool_off_seconds = {}
"#,
            example_numeric_duration("max_convergence_sweep_interval").as_secs(),
            example_numeric_duration("max_convergence_sweep_cool_off").as_secs(),
        );
        let configured = configuration_with_convergence_sweep().replace(&policy, "");

        assert_eq!(
            HubModelConfiguration::parse(&configured).err(),
            Some(HubModelConfigurationError::InvalidRepositoryWatchConfiguration)
        );
    }

    #[test]
    fn repository_watch_rejects_a_convergence_interval_above_its_ceiling() {
        let configured = configuration_with_convergence_sweep().replace(
            &format!(
                "interval_seconds = {}",
                example_numeric_duration("max_convergence_sweep_interval").as_secs()
            ),
            &format!(
                "interval_seconds = {}",
                example_numeric_duration("max_convergence_sweep_interval").as_secs() + 1
            ),
        );

        assert_eq!(
            HubModelConfiguration::parse(&configured).err(),
            Some(HubModelConfigurationError::InvalidRepositoryWatchConfiguration)
        );
    }

    #[test]
    fn repository_watch_rejects_a_convergence_cool_off_above_its_ceiling() {
        let configured = configuration_with_convergence_sweep().replace(
            &format!(
                "cool_off_seconds = {}",
                example_numeric_duration("max_convergence_sweep_cool_off").as_secs()
            ),
            &format!(
                "cool_off_seconds = {}",
                example_numeric_duration("max_convergence_sweep_cool_off").as_secs() + 1
            ),
        );

        assert_eq!(
            HubModelConfiguration::parse(&configured).err(),
            Some(HubModelConfigurationError::InvalidRepositoryWatchConfiguration)
        );
    }

    #[test]
    fn repository_watch_rejects_a_convergence_pull_request_above_graphql_int() {
        let configured = configuration_with_convergence_sweep().replace(
            &format!("convergence_pull_requests = [{CONVERGENCE_PULL_REQUEST}]"),
            &format!("convergence_pull_requests = [{}]", i64::from(i32::MAX) + 1),
        );

        assert_eq!(
            HubModelConfiguration::parse(&configured).err(),
            Some(HubModelConfigurationError::InvalidRepositoryWatchConfiguration)
        );
    }

    #[test]
    fn repository_watch_accepts_a_positive_rule_revision() {
        let revision =
            RepoWatchRuleVersion::new(NonZeroU64::new(2).expect("configured revision is positive"))
                .expect("configured revision is within the durable range");
        let configured = configuration_with_repository_watch_rule().replace(
            &format!(
                "id = \"{WATCH_RULE_ID}\"\nversion = {}",
                RepoWatchRuleVersion::V1.get()
            ),
            &format!("id = \"{WATCH_RULE_ID}\"\nversion = {}", revision.get()),
        );
        let configured = HubModelConfiguration::parse(&configured)
            .expect("repository-watch revision fixture is valid");
        let rule = &configured
            .repository_watch()
            .expect("fixture configures repository watch")
            .rules()[0];

        assert_eq!(rule.id().as_str(), WATCH_RULE_ID);
        assert_eq!(rule.version(), revision);
    }

    #[test]
    fn repository_watch_parses_the_eager_merge_forward_rule() {
        let configured =
            HubModelConfiguration::parse(&configuration_with_eager_merge_forward_rule())
                .expect("eager merge-forward rule fixture is valid");
        let rule = &configured
            .repository_watch()
            .expect("fixture configures repository watch")
            .rules()[0];

        assert_eq!(rule.id().as_str(), EAGER_WATCH_RULE_ID);
        assert_eq!(rule.version().get(), 1);
        assert_eq!(rule.singleton_per(), RepoWatchSingletonScope::PullRequest);
        assert_eq!(rule.cooldown(), Duration::ZERO);
        assert_eq!(
            rule.matcher().event_kinds(),
            [RepoWatchEventKindNameV1::BaseAdvanced]
        );
        assert_eq!(
            rule.matcher()
                .head_branch()
                .expect("live rule narrows dispatched pull requests")
                .as_str(),
            EAGER_WATCH_HEAD_PATTERN
        );
        assert_eq!(rule.matcher().base_branch(), None);
        assert!(rule.matcher().mergeable_state().is_empty());
        assert!(rule.matcher().conclusion().is_empty());
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
                "credential_file = \"{WATCH_CREDENTIAL_FILE}\"\nwebhook_secret_path = \"/not-v1\""
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
    fn retired_scheduler_table_is_an_unknown_top_level_field() {
        let configured = CONFIGURATION.replace(
            "[compaction]",
            "[scheduler]\nmax_in_flight_passes = 4\n\n[compaction]",
        );

        assert_eq!(
            HubModelConfiguration::parse(&configured).err(),
            Some(HubModelConfigurationError::UnknownField)
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
    fn widened_usage_aggregate_cost_prices_totals_above_u64() {
        let configuration =
            HubModelConfiguration::parse(CONFIGURATION).expect("fixture configuration is valid");
        let cost = configuration
            .derive_usage_aggregate_cost(
                configured_target(&configuration),
                "anthropic-primary",
                ProcessModelCallInputTokenSemantics::CacheExclusive,
                [None, Some(u128::from(u64::MAX) + 1), None, None],
            )
            .expect("the widened output total is exactly priceable");

        assert!(cost.amount_usd() > Decimal::ZERO);
    }

    #[test]
    fn widened_usage_aggregate_cost_fails_whole_when_one_reported_axis_is_unpriceable() {
        let configuration =
            HubModelConfiguration::parse(CONFIGURATION).expect("fixture configuration is valid");
        let cost = configuration.derive_usage_aggregate_cost(
            configured_target(&configuration),
            "anthropic-primary",
            ProcessModelCallInputTokenSemantics::CacheExclusive,
            [Some(u128::MAX), Some(1_000_000), None, None],
        );

        assert_eq!(cost, None, "an unpriceable input axis must not be dropped");
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
    fn tool_mapping_registry_rejects_noncanonical_workspace_root_spellings() {
        let trailing_separator = CONFIGURATION.replace(
            "workspace_root = \"/srv/signalbox/workspace\"",
            "workspace_root = \"/srv/signalbox/workspace/\"",
        );
        let dot_component = CONFIGURATION.replace(
            "workspace_root = \"/srv/signalbox/workspace\"",
            "workspace_root = \"/srv/signalbox/./workspace\"",
        );

        assert_eq!(
            HubModelConfiguration::parse(&trailing_separator).err(),
            Some(HubModelConfigurationError::InvalidToolMappings)
        );
        assert_eq!(
            HubModelConfiguration::parse(&dot_component).err(),
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
    fn daemon_tool_process_settings_admit_a_canonical_cargo_registry_cache() {
        let temporary = tempfile::tempdir().expect("fixture directory is available");
        let registry = temporary.path().join("registry");
        std::fs::create_dir(&registry).expect("fixture registry exists");
        let configured = CONFIGURATION.replace(
            &format!("exec_supervisor_executable = \"{EXEC_SUPERVISOR_EXECUTABLE}\""),
            &format!(
                "exec_supervisor_executable = \"{EXEC_SUPERVISOR_EXECUTABLE}\"\ncargo_registry_cache = \"{}\"",
                registry.display()
            ),
        );
        let expected =
            std::fs::canonicalize(&registry).expect("fixture registry has a canonical directory");

        let configuration = HubModelConfiguration::parse(&configured)
            .expect("an absolute Cargo registry directory is valid");

        assert_eq!(
            configuration
                .daemon_tools()
                .expect("mapped fixture has daemon tool settings")
                .cargo_registry_cache(),
            Some(expected.as_path())
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
    fn configuration_rejects_a_mapping_naming_an_undeclared_pool() {
        let undeclared_pool_name = "undeclared-pool";
        let undeclared_pool = CONFIGURATION.replace(
            "credential_pool = \"anthropic-main\"",
            &format!("credential_pool = \"{undeclared_pool_name}\""),
        );

        assert_eq!(
            HubModelConfiguration::parse(&undeclared_pool).err(),
            Some(HubModelConfigurationError::UnknownCredentialPool {
                model_family: Arc::from("anthropic"),
                credential_pool: Arc::from(undeclared_pool_name),
            })
        );
    }

    #[test]
    fn configuration_rejects_a_pool_member_naming_an_undeclared_profile() {
        let undeclared_profile_name = "undeclared-profile";
        let undeclared_profile = CONFIGURATION.replace(
            "members = [{ profile = \"anthropic-primary\", priority = 1 }]",
            &format!("members = [{{ profile = \"{undeclared_profile_name}\", priority = 1 }}]"),
        );

        assert_eq!(
            HubModelConfiguration::parse(&undeclared_profile).err(),
            Some(HubModelConfigurationError::UnknownPoolMemberProfile {
                credential_pool: Arc::from(ANTHROPIC_POOL_NAME),
                credential_profile: Arc::from(undeclared_profile_name),
            })
        );
    }

    #[test]
    fn configuration_admits_a_profile_name_no_build_constant_states() {
        let deployment_chosen_name = "acct-7f3";
        let renamed = CONFIGURATION.replace("anthropic-primary", deployment_chosen_name);
        let configured =
            HubModelConfiguration::parse(&renamed).expect("a deployment names its own accounts");

        let route = configured
            .resolve_direct_model(configured_judge_selection_fixture())
            .expect("the configured selection resolves");

        assert_eq!(route.credential_profile(), deployment_chosen_name);
    }

    #[test]
    fn route_pins_the_preferred_member_of_its_pool() {
        let pool_name = "anthropic-main";
        let preferred_profile = ANTHROPIC_CREDENTIAL_REFERENCE;
        let pool = format!(
            r#"[[credential_pools]]
name = "{pool_name}"
tie_break = "first_listed"
on_pool_exhausted = "park"
members = [
  {{ profile = "anthropic-overflow", priority = 2 }},
  {{ profile = "{preferred_profile}", priority = 1 }},
]"#
        );
        let configured = HubModelConfiguration::parse(&configuration_with_anthropic_pool(&pool))
            .expect("a two-member pool is valid");

        let route = configured
            .resolve_direct_model(configured_judge_selection_fixture())
            .expect("the configured selection resolves");

        assert_eq!(route.credential_pool(), pool_name);
        assert_eq!(route.credential_profile(), preferred_profile);
    }

    #[test]
    fn equal_priorities_resolve_to_the_first_listed_member() {
        let first_listed_profile = ANTHROPIC_OVERFLOW_PROFILE;
        let pool = format!(
            r#"[[credential_pools]]
name = "anthropic-main"
tie_break = "first_listed"
on_pool_exhausted = "park"
members = [
  {{ profile = "{first_listed_profile}", priority = 1 }},
  {{ profile = "anthropic-primary", priority = 1 }},
]"#,
        );
        let configured = HubModelConfiguration::parse(&configuration_with_anthropic_pool(&pool))
            .expect("equal priorities are valid");

        let route = configured
            .resolve_direct_model(configured_judge_selection_fixture())
            .expect("the configured selection resolves");

        assert_eq!(route.credential_profile(), first_listed_profile);
    }

    #[test]
    fn omitted_trigger_keys_select_the_staying_action() {
        let configured =
            HubModelConfiguration::parse(CONFIGURATION).expect("the fixture omits every trigger");

        let pool = configured
            .credential_pool("anthropic-main")
            .expect("the fixture declares the pool");

        assert_eq!(
            pool.action(CredentialPoolTrigger::QuotaExhausted),
            CredentialPoolAction::Stay
        );
        assert_eq!(
            pool.action(CredentialPoolTrigger::RateLimited),
            CredentialPoolAction::Stay
        );
        assert_eq!(
            pool.action(CredentialPoolTrigger::Overloaded),
            CredentialPoolAction::Stay
        );
        assert_eq!(
            pool.action(CredentialPoolTrigger::CredentialRejected),
            CredentialPoolAction::Stay
        );
        assert_eq!(
            pool.action(CredentialPoolTrigger::HeadroomLow),
            CredentialPoolAction::Stay
        );
    }

    #[test]
    fn configured_trigger_actions_are_typed() {
        let configured = HubModelConfiguration::parse(&configuration_with_anthropic_pool(
            r#"[[credential_pools]]
name = "anthropic-main"
tie_break = "first_listed"
on_pool_exhausted = "fail"
members = [{ profile = "anthropic-primary", priority = 1 }]
on_quota_exhausted = "switch_next_turn"
on_rate_limited = "switch_now"
on_overloaded = "avoid_new_sessions"
on_credential_rejected = "quarantine""#,
        ))
        .expect("every configured action is admitted for its trigger");

        let pool = configured
            .credential_pool("anthropic-main")
            .expect("the fixture declares the pool");

        assert_eq!(pool.tie_break(), CredentialPoolTieBreak::FirstListed);
        assert_eq!(pool.on_pool_exhausted(), CredentialPoolExhaustion::Fail);
        // Anthropic's mapping has no quota token, so only rate limiting can
        // carry the proof `switch_now` requires.
        assert_eq!(
            pool.action(CredentialPoolTrigger::QuotaExhausted),
            CredentialPoolAction::SwitchNextTurn
        );
        assert_eq!(
            pool.action(CredentialPoolTrigger::RateLimited),
            CredentialPoolAction::SwitchNow
        );
        assert_eq!(
            pool.action(CredentialPoolTrigger::Overloaded),
            CredentialPoolAction::AvoidNewSessions
        );
        assert_eq!(
            pool.action(CredentialPoolTrigger::CredentialRejected),
            CredentialPoolAction::Quarantine
        );
    }

    #[test]
    fn configuration_requires_at_least_one_credential_pool() {
        let without_pools = configuration_with_anthropic_pool("").replace(CODEX_POOL, "");

        assert_eq!(
            HubModelConfiguration::parse(&without_pools).err(),
            Some(HubModelConfigurationError::MissingCredentialPools)
        );
    }

    #[test]
    fn configuration_rejects_an_oversized_credential_profile_name() {
        let oversized_name = "p".repeat(MAX_CREDENTIAL_CATALOG_NAME_UTF8_BYTES + 1);
        let configuration = CONFIGURATION.replacen(
            "name = \"anthropic-primary\"",
            &format!("name = \"{oversized_name}\""),
            1,
        );

        assert_eq!(
            HubModelConfiguration::parse(&configuration).err(),
            Some(HubModelConfigurationError::InvalidField)
        );
    }

    #[test]
    fn configuration_rejects_an_oversized_credential_pool_name() {
        let oversized_name = "p".repeat(MAX_CREDENTIAL_CATALOG_NAME_UTF8_BYTES + 1);
        let configuration = CONFIGURATION.replacen(
            "name = \"anthropic-main\"",
            &format!("name = \"{oversized_name}\""),
            1,
        );

        assert_eq!(
            HubModelConfiguration::parse(&configuration).err(),
            Some(HubModelConfigurationError::InvalidField)
        );
    }

    #[test]
    fn configuration_rejects_too_many_credential_pool_members() {
        let repeated_member = "{ profile = \"anthropic-primary\", priority = 1 }";
        let members = vec![repeated_member; MAX_CREDENTIAL_POOL_MEMBERS + 1].join(",\n");
        let oversized_pool = configuration_with_anthropic_pool(&format!(
            r#"[[credential_pools]]
name = "anthropic-main"
tie_break = "first_listed"
on_pool_exhausted = "park"
members = [{members}]"#,
        ));

        assert_eq!(
            HubModelConfiguration::parse(&oversized_pool).err(),
            Some(HubModelConfigurationError::InvalidCredentialPoolPolicy)
        );
    }

    #[test]
    fn configuration_rejects_a_pool_with_no_members() {
        let empty_pool = configuration_with_anthropic_pool(
            r#"[[credential_pools]]
name = "anthropic-main"
tie_break = "first_listed"
on_pool_exhausted = "park"
members = []"#,
        );

        assert_eq!(
            HubModelConfiguration::parse(&empty_pool).err(),
            Some(HubModelConfigurationError::EmptyCredentialPool {
                credential_pool: Arc::from(ANTHROPIC_POOL_NAME),
            })
        );
    }

    #[test]
    fn configuration_rejects_a_repeated_pool_member() {
        let repeated_member = configuration_with_anthropic_pool(
            r#"[[credential_pools]]
name = "anthropic-main"
tie_break = "first_listed"
on_pool_exhausted = "park"
members = [
  { profile = "anthropic-primary", priority = 1 },
  { profile = "anthropic-primary", priority = 2 },
]"#,
        );

        assert_eq!(
            HubModelConfiguration::parse(&repeated_member).err(),
            Some(HubModelConfigurationError::DuplicatePoolMember {
                credential_pool: Arc::from(ANTHROPIC_POOL_NAME),
                credential_profile: Arc::from("anthropic-primary"),
            })
        );
    }

    #[test]
    fn configuration_rejects_a_repeated_pool_name() {
        let repeated_pool = configuration_with_anthropic_pool(
            r#"[[credential_pools]]
name = "anthropic-main"
tie_break = "first_listed"
on_pool_exhausted = "park"
members = [{ profile = "anthropic-primary", priority = 1 }]

[[credential_pools]]
name = "anthropic-main"
tie_break = "first_listed"
on_pool_exhausted = "park"
members = [{ profile = "anthropic-overflow", priority = 1 }]"#,
        );

        assert_eq!(
            HubModelConfiguration::parse(&repeated_pool).err(),
            Some(HubModelConfigurationError::DuplicateCredentialPool {
                credential_pool: Arc::from(ANTHROPIC_POOL_NAME),
            })
        );
    }

    #[test]
    fn configuration_rejects_a_member_priority_below_one() {
        let zero_priority = configuration_with_anthropic_pool(
            r#"[[credential_pools]]
name = "anthropic-main"
tie_break = "first_listed"
on_pool_exhausted = "park"
members = [{ profile = "anthropic-primary", priority = 0 }]"#,
        );

        assert_eq!(
            HubModelConfiguration::parse(&zero_priority).err(),
            Some(HubModelConfigurationError::InvalidMemberPriority {
                credential_pool: Arc::from(ANTHROPIC_POOL_NAME),
            })
        );
    }

    #[test]
    fn configuration_rejects_a_member_priority_above_u32() {
        let overflowing_priority = configuration_with_anthropic_pool(
            r#"[[credential_pools]]
name = "anthropic-main"
tie_break = "first_listed"
on_pool_exhausted = "park"
members = [{ profile = "anthropic-primary", priority = 4294967296 }]"#,
        );

        assert_eq!(
            HubModelConfiguration::parse(&overflowing_priority).err(),
            Some(HubModelConfigurationError::InvalidMemberPriority {
                credential_pool: Arc::from(ANTHROPIC_POOL_NAME),
            })
        );
    }

    #[test]
    fn configuration_rejects_pool_members_disagreeing_on_adapter() {
        let mixed_adapters = configuration_with_anthropic_pool(
            r#"[[credential_pools]]
name = "anthropic-main"
tie_break = "first_listed"
on_pool_exhausted = "park"
members = [
  { profile = "anthropic-primary", priority = 1 },
  { profile = "codex-subscription-primary", priority = 2 },
]"#,
        );

        assert_eq!(
            HubModelConfiguration::parse(&mixed_adapters).err(),
            Some(HubModelConfigurationError::ConflictingPoolAdapters {
                credential_pool: Arc::from(ANTHROPIC_POOL_NAME),
            })
        );
    }

    #[test]
    fn configuration_rejects_a_mapping_disagreeing_with_its_pool_adapter() {
        let disagreeing_mapping = CONFIGURATION.replace(
            "adapter = \"anthropic\"\ncredential_pool = \"anthropic-main\"",
            "adapter = \"codex_cli\"\ncredential_pool = \"anthropic-main\"",
        );

        assert_eq!(
            HubModelConfiguration::parse(&disagreeing_mapping).err(),
            Some(HubModelConfigurationError::ConflictingPoolAdapters {
                credential_pool: Arc::from(ANTHROPIC_POOL_NAME),
            })
        );
    }

    #[test]
    fn configuration_allows_a_direct_adapter_to_resolve_multiple_profiles() {
        let second_anthropic_pool = format!(
            r#"{CONFIGURATION}
[[credential_pools]]
name = "anthropic-batch"
tie_break = "first_listed"
on_pool_exhausted = "park"
members = [{{ profile = "anthropic-overflow", priority = 1 }}]

[[adapter_mappings]]
model_family = "anthropic-batch"
adapter = "anthropic"
credential_pool = "anthropic-batch"
"#
        );

        let configuration = HubModelConfiguration::parse(&second_anthropic_pool)
            .expect("direct HTTP profiles resolve per operation");

        assert_eq!(
            configuration
                .session_credential_pin()
                .credentials()
                .map(|credential| credential.credential_reference())
                .collect::<Vec<_>>(),
            vec!["anthropic-primary", "anthropic-overflow"]
        );
    }

    #[test]
    fn configuration_rejects_an_unknown_tie_break() {
        let unknown_tie_break = configuration_with_anthropic_pool(
            r#"[[credential_pools]]
name = "anthropic-main"
tie_break = "coin_flip"
on_pool_exhausted = "park"
members = [{ profile = "anthropic-primary", priority = 1 }]"#,
        );

        assert_eq!(
            HubModelConfiguration::parse(&unknown_tie_break).err(),
            Some(HubModelConfigurationError::InvalidCredentialPoolPolicy)
        );
    }

    #[test]
    fn configuration_rejects_round_robin_until_its_durable_cursor_exists() {
        let round_robin = configuration_with_anthropic_pool(
            r#"[[credential_pools]]
name = "anthropic-main"
tie_break = "round_robin"
on_pool_exhausted = "fail"
members = [{ profile = "anthropic-primary", priority = 1 }]"#,
        );

        assert_eq!(
            HubModelConfiguration::parse(&round_robin).err(),
            Some(HubModelConfigurationError::InvalidCredentialPoolPolicy)
        );
    }

    #[test]
    fn configuration_rejects_an_unknown_exhaustion_behavior() {
        let unknown_exhaustion = configuration_with_anthropic_pool(
            r#"[[credential_pools]]
name = "anthropic-main"
tie_break = "first_listed"
on_pool_exhausted = "retry_forever"
members = [{ profile = "anthropic-primary", priority = 1 }]"#,
        );

        assert_eq!(
            HubModelConfiguration::parse(&unknown_exhaustion).err(),
            Some(HubModelConfigurationError::InvalidCredentialPoolPolicy)
        );
    }

    #[test]
    fn configuration_rejects_an_unknown_trigger_action() {
        let unknown_action = configuration_with_anthropic_pool(
            r#"[[credential_pools]]
name = "anthropic-main"
tie_break = "first_listed"
on_pool_exhausted = "park"
members = [{ profile = "anthropic-primary", priority = 1 }]
on_rate_limited = "escalate""#,
        );

        assert_eq!(
            HubModelConfiguration::parse(&unknown_action).err(),
            Some(HubModelConfigurationError::UnknownCredentialPoolAction)
        );
    }

    #[test]
    fn configuration_rejects_switching_now_on_a_rejected_credential() {
        let switching_now = configuration_with_anthropic_pool(
            r#"[[credential_pools]]
name = "anthropic-main"
tie_break = "first_listed"
on_pool_exhausted = "park"
members = [{ profile = "anthropic-primary", priority = 1 }]
on_credential_rejected = "switch_now""#,
        );

        assert_eq!(
            HubModelConfiguration::parse(&switching_now).err(),
            Some(
                HubModelConfigurationError::InadmissibleCredentialPoolAction {
                    trigger: Arc::from("on_credential_rejected"),
                }
            )
        );
    }

    #[test]
    fn configuration_rejects_switching_now_on_low_headroom() {
        let switching_now = configuration_with_anthropic_pool(
            r#"[[credential_pools]]
name = "anthropic-main"
tie_break = "first_listed"
on_pool_exhausted = "park"
members = [{ profile = "anthropic-primary", priority = 1 }]
on_headroom_low = "switch_now""#,
        );

        assert_eq!(
            HubModelConfiguration::parse(&switching_now).err(),
            Some(
                HubModelConfigurationError::InadmissibleCredentialPoolAction {
                    trigger: Arc::from("on_headroom_low"),
                }
            )
        );
    }

    #[test]
    fn configuration_rejects_a_headroom_reserve_no_adapter_reports() {
        let reserved = configuration_with_anthropic_pool(
            r#"[[credential_pools]]
name = "anthropic-main"
tie_break = "first_listed"
on_pool_exhausted = "park"
headroom_reserve_percent = 10
members = [{ profile = "anthropic-primary", priority = 1 }]"#,
        );

        assert_eq!(
            HubModelConfiguration::parse(&reserved).err(),
            Some(HubModelConfigurationError::UnobservedCapacityPolicy {
                credential_pool: Arc::from(ANTHROPIC_POOL_NAME),
            })
        );
    }

    #[test]
    fn configuration_rejects_a_mistyped_headroom_reserve_table() {
        let reserved = configuration_with_anthropic_pool(
            r#"[[credential_pools]]
name = "anthropic-main"
tie_break = "first_listed"
on_pool_exhausted = "park"
members = [{ profile = "anthropic-primary", priority = 1 }]

[credential_pools.headroom_reserve_percent]
value = 10"#,
        );

        assert_eq!(
            HubModelConfiguration::parse(&reserved).err(),
            Some(HubModelConfigurationError::InvalidField)
        );
    }

    #[test]
    fn configuration_rejects_a_member_headroom_reserve_no_adapter_reports() {
        let reserved = configuration_with_anthropic_pool(
            r#"[[credential_pools]]
name = "anthropic-main"
tie_break = "first_listed"
on_pool_exhausted = "park"
members = [{ profile = "anthropic-primary", priority = 1, headroom_reserve_percent = 10 }]"#,
        );

        assert_eq!(
            HubModelConfiguration::parse(&reserved).err(),
            Some(HubModelConfigurationError::UnobservedCapacityPolicy {
                credential_pool: Arc::from(ANTHROPIC_POOL_NAME),
            })
        );
    }

    #[track_caller]
    fn assert_codex_action_rejected(trigger: &str, action: &str) {
        let configured =
            CONFIGURATION.replace(CODEX_POOL, &format!("{CODEX_POOL}\n{trigger} = {action:?}"));
        assert_eq!(
            HubModelConfiguration::parse(&configured).err(),
            Some(
                HubModelConfigurationError::InadmissibleCredentialPoolAction {
                    trigger: Arc::from(trigger),
                }
            ),
            "{trigger} = {action}",
        );
    }

    #[track_caller]
    fn assert_codex_trigger_rejects_actions(trigger: &str) {
        assert_codex_action_rejected(trigger, "switch_now");
        assert_codex_action_rejected(trigger, "switch_next_turn");
        assert_codex_action_rejected(trigger, "avoid_new_sessions");
        assert_codex_action_rejected(trigger, "quarantine");
        let configured =
            CONFIGURATION.replace(CODEX_POOL, &format!("{CODEX_POOL}\n{trigger} = \"stay\""));
        HubModelConfiguration::parse(&configured).expect("stay needs no classified failure");
    }

    #[test]
    fn configuration_rejects_actions_for_opaque_codex_failures() {
        assert_codex_trigger_rejects_actions("on_rate_limited");
        assert_codex_trigger_rejects_actions("on_quota_exhausted");
        assert_codex_trigger_rejects_actions("on_overloaded");
        assert_codex_trigger_rejects_actions("on_credential_rejected");
    }

    #[test]
    fn configuration_admits_switch_now_where_the_adapter_proves_non_acceptance() {
        let substituting = configuration_with_anthropic_pool(&format!(
            "{ANTHROPIC_POOL}\non_rate_limited = \"switch_now\""
        ));

        HubModelConfiguration::parse(&substituting)
            .expect("a decoded native envelope authorizes the successor for this adapter");
    }

    #[test]
    fn configuration_rejects_switch_now_for_a_cause_the_adapter_cannot_prove() {
        // Anthropic's mapping has no quota token, so this pair could reach
        // `switch_now` only through a status-derived fallback carrying no proof.
        let substituting = configuration_with_anthropic_pool(&format!(
            "{ANTHROPIC_POOL}\non_quota_exhausted = \"switch_now\""
        ));

        assert_eq!(
            HubModelConfiguration::parse(&substituting).err(),
            Some(HubModelConfigurationError::UnprovableSubstitutionPolicy {
                credential_pool: Arc::from(ANTHROPIC_POOL_NAME),
            })
        );
    }

    #[test]
    fn configuration_rejects_least_used_ties_no_adapter_can_resolve() {
        let least_used = configuration_with_anthropic_pool(
            r#"[[credential_pools]]
name = "anthropic-main"
tie_break = "least_used"
on_pool_exhausted = "park"
members = [{ profile = "anthropic-primary", priority = 1 }]"#,
        );

        assert_eq!(
            HubModelConfiguration::parse(&least_used).err(),
            Some(HubModelConfigurationError::UnobservedCapacityPolicy {
                credential_pool: Arc::from(ANTHROPIC_POOL_NAME),
            })
        );
    }

    #[test]
    fn configuration_rejects_a_headroom_reserve_leaving_nothing_usable() {
        let full_reserve = configuration_with_anthropic_pool(
            r#"[[credential_pools]]
name = "anthropic-main"
tie_break = "first_listed"
on_pool_exhausted = "park"
headroom_reserve_percent = 100
members = [{ profile = "anthropic-primary", priority = 1 }]"#,
        );

        assert_eq!(
            HubModelConfiguration::parse(&full_reserve).err(),
            Some(HubModelConfigurationError::InvalidHeadroomReserve)
        );
    }

    #[test]
    fn configuration_rejects_an_unknown_pool_field() {
        let unknown_field = configuration_with_anthropic_pool(
            r#"[[credential_pools]]
name = "anthropic-main"
tie_break = "first_listed"
on_pool_exhausted = "park"
on_provider_moody = "quarantine"
members = [{ profile = "anthropic-primary", priority = 1 }]"#,
        );

        assert_eq!(
            HubModelConfiguration::parse(&unknown_field).err(),
            Some(HubModelConfigurationError::UnknownField)
        );
    }

    #[test]
    fn configuration_rejects_an_unknown_pool_member_field() {
        let unknown_field = configuration_with_anthropic_pool(
            r#"[[credential_pools]]
name = "anthropic-main"
tie_break = "first_listed"
on_pool_exhausted = "park"
members = [{ profile = "anthropic-primary", priority = 1, weight = 3 }]"#,
        );

        assert_eq!(
            HubModelConfiguration::parse(&unknown_field).err(),
            Some(HubModelConfigurationError::UnknownField)
        );
    }

    #[test]
    fn configuration_rejects_a_delivery_its_adapter_does_not_admit() {
        let ambient_anthropic = CONFIGURATION.replace(
            "delivery = \"file\"\nfile = \"/run/secrets/anthropic-primary\"",
            "delivery = \"ambient\"",
        );

        assert_eq!(
            HubModelConfiguration::parse(&ambient_anthropic).err(),
            Some(HubModelConfigurationError::UnsupportedCredentialDelivery {
                adapter: ModelAdapter::Anthropic,
                delivery: Arc::from("ambient"),
            })
        );
    }

    #[test]
    fn configuration_admits_an_existing_nonempty_credential_home() {
        let temporary = tempfile::tempdir().expect("synthetic home root is created");
        let home = temporary.path().join("account-a");
        std::fs::create_dir(&home).expect("synthetic home is created");
        std::fs::write(home.join("fixture-marker"), "synthetic")
            .expect("synthetic home is nonempty");
        let credential_home = CONFIGURATION.replace(
            "delivery = \"ambient\"",
            &format!(
                "delivery = \"codex_home\"\ncodex_home = {:?}",
                home.to_string_lossy()
            ),
        );

        let parsed = HubModelConfiguration::parse(&credential_home)
            .expect("existing nonempty synthetic home is admitted");
        assert_eq!(
            parsed
                .credential_profile(CODEX_SUBSCRIPTION_PROFILE)
                .expect("Codex profile remains present")
                .delivery()
                .path(),
            Some(&home)
        );
    }

    #[test]
    fn configuration_rejects_a_relative_credential_home_with_a_typed_member_error() {
        let credential_home = CONFIGURATION.replace(
            "delivery = \"ambient\"",
            "delivery = \"codex_home\"\ncodex_home = \"relative/account-a\"",
        );

        assert_eq!(
            HubModelConfiguration::parse(&credential_home).err(),
            Some(HubModelConfigurationError::InvalidCredentialHome {
                credential_profile: Arc::from(CODEX_SUBSCRIPTION_PROFILE),
                failure: crate::CredentialHomeAdmissionFailure::InvalidPath,
            })
        );
    }

    #[test]
    fn configuration_rejects_a_missing_credential_home_with_a_typed_member_error() {
        let temporary = tempfile::tempdir().expect("synthetic home root is created");
        let missing = temporary.path().join("missing-account");
        let credential_home = CONFIGURATION.replace(
            "delivery = \"ambient\"",
            &format!(
                "delivery = \"codex_home\"\ncodex_home = {:?}",
                missing.to_string_lossy()
            ),
        );

        assert_eq!(
            HubModelConfiguration::parse(&credential_home).err(),
            Some(HubModelConfigurationError::InvalidCredentialHome {
                credential_profile: Arc::from(CODEX_SUBSCRIPTION_PROFILE),
                failure: crate::CredentialHomeAdmissionFailure::MissingOrNotDirectory,
            })
        );
    }

    #[test]
    fn configuration_rejects_an_empty_credential_home_with_a_typed_member_error() {
        let temporary = tempfile::tempdir().expect("synthetic home root is created");
        let empty = temporary.path().join("empty-account");
        std::fs::create_dir(&empty).expect("empty synthetic home is created");
        let credential_home = CONFIGURATION.replace(
            "delivery = \"ambient\"",
            &format!(
                "delivery = \"codex_home\"\ncodex_home = {:?}",
                empty.to_string_lossy()
            ),
        );

        assert_eq!(
            HubModelConfiguration::parse(&credential_home).err(),
            Some(HubModelConfigurationError::InvalidCredentialHome {
                credential_profile: Arc::from(CODEX_SUBSCRIPTION_PROFILE),
                failure: crate::CredentialHomeAdmissionFailure::EmptyDirectory,
            })
        );
    }

    #[test]
    fn configuration_rejects_a_credential_home_concurrency_bound_until_reservations_exist() {
        let temporary = tempfile::tempdir().expect("synthetic home root is created");
        let home = temporary.path().join("account-a");
        std::fs::create_dir(&home).expect("synthetic home is created");
        std::fs::write(home.join("fixture-marker"), "synthetic")
            .expect("synthetic home is nonempty");
        let credential_home = CONFIGURATION.replace(
            "delivery = \"ambient\"",
            &format!(
                "delivery = \"codex_home\"\ncodex_home = {:?}\nmax_concurrent_invocations = {MAX_CREDENTIAL_HOME_CONCURRENT_INVOCATIONS}",
                home.to_string_lossy()
            ),
        );

        assert_eq!(
            HubModelConfiguration::parse(&credential_home).err(),
            Some(HubModelConfigurationError::InvalidCredentialDelivery)
        );
    }

    #[test]
    fn configuration_rejects_a_credential_home_concurrency_bound_past_its_cap() {
        let temporary = tempfile::tempdir().expect("synthetic home root is created");
        let home = temporary.path().join("account-a");
        std::fs::create_dir(&home).expect("synthetic home is created");
        std::fs::write(home.join("fixture-marker"), "synthetic")
            .expect("synthetic home is nonempty");
        let credential_home = CONFIGURATION.replace(
            "delivery = \"ambient\"",
            &format!(
                "delivery = \"codex_home\"\ncodex_home = {:?}\nmax_concurrent_invocations = {}",
                home.to_string_lossy(),
                MAX_CREDENTIAL_HOME_CONCURRENT_INVOCATIONS + 1
            ),
        );

        assert_eq!(
            HubModelConfiguration::parse(&credential_home).err(),
            Some(HubModelConfigurationError::InvalidCredentialDelivery)
        );
    }

    #[test]
    fn configuration_rejects_an_oversized_credential_home_before_refusing_it() {
        let oversized_path = format!("/{}", "a".repeat(MAX_CREDENTIAL_DELIVERY_PATH_UTF8_BYTES));
        let credential_home = CONFIGURATION.replace(
            "delivery = \"ambient\"",
            &format!("delivery = \"codex_home\"\ncodex_home = \"{oversized_path}\""),
        );

        assert_eq!(
            HubModelConfiguration::parse(&credential_home).err(),
            Some(HubModelConfigurationError::InvalidCredentialHome {
                credential_profile: Arc::from(CODEX_SUBSCRIPTION_PROFILE),
                failure: crate::CredentialHomeAdmissionFailure::InvalidPath,
            })
        );
    }

    #[test]
    fn configuration_rejects_a_nul_containing_credential_file_path() {
        let credential_file = CONFIGURATION.replace(
            "file = \"/run/secrets/anthropic-primary\"",
            "file = \"/run/secrets/contains\\u0000nul\"",
        );

        assert_eq!(
            HubModelConfiguration::parse(&credential_file).err(),
            Some(HubModelConfigurationError::InvalidCredentialDelivery)
        );
    }

    #[test]
    fn configuration_validates_an_undelivered_codex_file_before_refusing_it() {
        // `file` admits only `api_metered`, so the profile takes that kind to
        // reach the env-key validation this test is about.
        let credential_file = CONFIGURATION
            .replace(
                "billing_kind = \"subscription\"",
                "billing_kind = \"api_metered\"",
            )
            .replace(
                "delivery = \"ambient\"",
                "delivery = \"file\"\nfile = \"/run/secrets/codex-primary\"\nenv_key = \"HOME\"",
            );

        assert_eq!(
            HubModelConfiguration::parse(&credential_file).err(),
            Some(HubModelConfigurationError::InvalidCredentialDelivery)
        );
    }

    #[test]
    fn configuration_parses_a_valid_codex_file_before_refusing_it() {
        // `file` admits only `api_metered`, so the profile takes that kind and
        // the refusal this test asserts is the undelivered one.
        let credential_file = CONFIGURATION
            .replace(
                "billing_kind = \"subscription\"",
                "billing_kind = \"api_metered\"",
            )
            .replace(
                "delivery = \"ambient\"",
                "delivery = \"file\"\nfile = \"/run/secrets/codex-primary\"\nenv_key = \"OPENAI_API_KEY\"",
            );

        assert_eq!(
            HubModelConfiguration::parse(&credential_file).err(),
            Some(HubModelConfigurationError::UndeliveredCredentialDelivery {
                delivery: Arc::from("file"),
            })
        );
    }

    #[test]
    fn configuration_validates_undelivered_oauth_before_refusing_it() {
        let oauth = CONFIGURATION.replace(
            "delivery = \"ambient\"",
            r#"delivery = "oauth"
client_id = "synthetic-client"
token_url = "http://example.test/token"
device_authorization_url = "https://example.test/device"
scopes = ["model:invoke"]"#,
        );

        assert_eq!(
            HubModelConfiguration::parse(&oauth).err(),
            Some(HubModelConfigurationError::InvalidCredentialDelivery)
        );
    }

    /// Asserts one OAuth scope element is refused by the scope-token byte set.
    ///
    /// Single-quoted TOML literals pass the byte through verbatim, which is
    /// what lets a space, quote, or backslash reach the check at all.
    #[track_caller]
    fn assert_oauth_scope_rejected(scope: &str) {
        let oauth = CONFIGURATION.replace(
            "delivery = \"ambient\"",
            &format!(
                "delivery = \"oauth\"\n\
                 client_id = \"synthetic-client\"\n\
                 token_url = \"https://example.test/token\"\n\
                 device_authorization_url = \"https://example.test/device\"\n\
                 scopes = ['{scope}']"
            ),
        );

        assert_eq!(
            HubModelConfiguration::parse(&oauth).err(),
            Some(HubModelConfigurationError::InvalidCredentialDelivery)
        );
    }

    #[test]
    fn configuration_rejects_an_oauth_scope_holding_a_space() {
        // A space would become two scopes on the wire.
        assert_oauth_scope_rejected("read write");
    }

    #[test]
    fn configuration_rejects_an_oauth_scope_holding_a_quote() {
        assert_oauth_scope_rejected("read\"quoted");
    }

    #[test]
    fn configuration_rejects_an_oauth_scope_holding_a_backslash() {
        assert_oauth_scope_rejected("read\\slash");
    }

    #[test]
    fn configuration_rejects_a_non_ascii_oauth_scope() {
        // Control bytes are outside the set too, but TOML rejects them first.
        assert_oauth_scope_rejected("r\u{e9}ad");
    }

    #[test]
    fn configuration_rejects_an_oauth_endpoint_holding_a_fragment() {
        let oauth = CONFIGURATION.replace(
            "delivery = \"ambient\"",
            r#"delivery = "oauth"
client_id = "synthetic-client"
token_url = "https://example.test/token#stale"
device_authorization_url = "https://example.test/device"
scopes = ["model:invoke"]"#,
        );

        assert_eq!(
            HubModelConfiguration::parse(&oauth).err(),
            Some(HubModelConfigurationError::InvalidCredentialDelivery)
        );
    }

    /// Asserts one OAuth `token_url` is refused by the endpoint grammar itself,
    /// before the delivery's undelivered result.
    ///
    /// User information never reaches the request target, so it cannot
    /// distinguish two provisioning tuples, and it would put a secret in the
    /// static catalog. The delivery is undelivered either way; these tests
    /// assert the grammar refuses the endpoint first, on its own terms.
    #[track_caller]
    fn assert_oauth_token_url_rejected(token_url: &str) {
        let oauth = CONFIGURATION.replace(
            "delivery = \"ambient\"",
            &format!(
                r#"delivery = "oauth"
client_id = "synthetic-client"
token_url = "{token_url}"
device_authorization_url = "https://example.test/device"
scopes = ["model:invoke"]"#
            ),
        );

        assert_eq!(
            HubModelConfiguration::parse(&oauth).err(),
            Some(HubModelConfigurationError::InvalidCredentialDelivery)
        );
    }

    #[test]
    fn configuration_rejects_a_subscription_billing_kind_on_a_file_delivery() {
        // `file` presents a provider API key, so its billing kind is fixed.
        // The refusal names the profile and both disagreeing spellings, because
        // naming only the profile leaves the operator to find which field to
        // edit.
        let disagreeing = CONFIGURATION.replace(
            r#"name = "anthropic-primary"
adapter = "anthropic"
billing_kind = "api_metered""#,
            r#"name = "anthropic-primary"
adapter = "anthropic"
billing_kind = "subscription""#,
        );

        assert_eq!(
            HubModelConfiguration::parse(&disagreeing).err(),
            Some(
                HubModelConfigurationError::DisagreeingCredentialBillingKind {
                    credential_profile: Arc::from("anthropic-primary"),
                    delivery: Arc::from("file"),
                    billing_kind: Arc::from("subscription"),
                }
            )
        );
    }

    #[test]
    fn configuration_rejects_an_api_metered_billing_kind_on_an_oauth_delivery() {
        // `oauth` constructs a subscription login. The disagreement is refused
        // on its own terms even though the delivery is undelivered, so the
        // contradiction is not masked by the refusal that would follow it.
        let disagreeing = CONFIGURATION
            .replace(
                r#"name = "codex-subscription-primary"
adapter = "codex_cli"
billing_kind = "subscription""#,
                r#"name = "codex-subscription-primary"
adapter = "codex_cli"
billing_kind = "api_metered""#,
            )
            .replace(
                "delivery = \"ambient\"",
                r#"delivery = "oauth"
client_id = "synthetic-client"
token_url = "https://example.test/token"
device_authorization_url = "https://example.test/device"
scopes = ["model:invoke"]"#,
            );

        assert_eq!(
            HubModelConfiguration::parse(&disagreeing).err(),
            Some(
                HubModelConfigurationError::DisagreeingCredentialBillingKind {
                    credential_profile: Arc::from("codex-subscription-primary"),
                    delivery: Arc::from("oauth"),
                    billing_kind: Arc::from("api_metered"),
                }
            )
        );
    }

    #[test]
    fn configuration_admits_a_subscription_oauth_profile_before_refusing_it() {
        // The legal `oauth` pairing passes the agreement rule and reaches the
        // undelivered refusal, which is what proves the rule admitted it.
        let oauth = CONFIGURATION.replace(
            "delivery = \"ambient\"",
            r#"delivery = "oauth"
client_id = "synthetic-client"
token_url = "https://example.test/token"
device_authorization_url = "https://example.test/device"
scopes = ["model:invoke"]"#,
        );

        assert_eq!(
            HubModelConfiguration::parse(&oauth).err(),
            Some(HubModelConfigurationError::UndeliveredCredentialDelivery {
                delivery: Arc::from("oauth"),
            })
        );
    }

    #[test]
    fn configuration_admits_an_api_metered_ambient_profile() {
        // `ambient` names a login the operator established outside the daemon,
        // which may be billed either way, so both kinds are admitted.
        let ambient = CONFIGURATION.replace(
            r#"name = "codex-subscription-primary"
adapter = "codex_cli"
billing_kind = "subscription""#,
            r#"name = "codex-subscription-primary"
adapter = "codex_cli"
billing_kind = "api_metered""#,
        );

        assert!(HubModelConfiguration::parse(&ambient).is_ok());
    }

    #[test]
    fn configuration_admits_a_subscription_ambient_profile() {
        // The checked-in fixture already pairs `ambient` with `subscription`;
        // asserting it here states the other half of that delivery's rule.
        assert!(HubModelConfiguration::parse(CONFIGURATION).is_ok());
    }

    #[test]
    fn configuration_rejects_an_oauth_endpoint_holding_a_username() {
        assert_oauth_token_url_rejected("https://alice@example.test/token");
    }

    #[test]
    fn configuration_rejects_an_oauth_endpoint_holding_a_username_and_password() {
        assert_oauth_token_url_rejected("https://alice:secret@example.test/token");
    }

    #[test]
    fn configuration_rejects_an_oauth_endpoint_holding_a_password_alone() {
        assert_oauth_token_url_rejected("https://:secret@example.test/token");
    }

    #[test]
    fn configuration_rejects_a_device_endpoint_holding_user_information() {
        let oauth = CONFIGURATION.replace(
            "delivery = \"ambient\"",
            r#"delivery = "oauth"
client_id = "synthetic-client"
token_url = "https://example.test/token"
device_authorization_url = "https://alice:secret@example.test/device"
scopes = ["model:invoke"]"#,
        );

        assert_eq!(
            HubModelConfiguration::parse(&oauth).err(),
            Some(HubModelConfigurationError::InvalidCredentialDelivery)
        );
    }

    #[test]
    fn configuration_parses_valid_oauth_before_refusing_it() {
        let oauth = CONFIGURATION.replace(
            "delivery = \"ambient\"",
            r#"delivery = "oauth"
client_id = "synthetic-client"
token_url = "https://example.test/token"
device_authorization_url = "https://example.test/device"
scopes = ["model:invoke"]"#,
        );

        assert_eq!(
            HubModelConfiguration::parse(&oauth).err(),
            Some(HubModelConfigurationError::UndeliveredCredentialDelivery {
                delivery: Arc::from("oauth"),
            })
        );
    }

    #[test]
    fn configuration_rejects_an_unknown_delivery() {
        let unknown_delivery =
            CONFIGURATION.replace("delivery = \"ambient\"", "delivery = \"telepathy\"");

        assert_eq!(
            HubModelConfiguration::parse(&unknown_delivery).err(),
            Some(HubModelConfigurationError::InvalidCredentialDelivery)
        );
    }

    #[test]
    fn configuration_rejects_a_relative_credential_file() {
        let relative_file = CONFIGURATION.replace(
            "file = \"/run/secrets/anthropic-primary\"",
            "file = \"secrets/anthropic-primary\"",
        );

        assert_eq!(
            HubModelConfiguration::parse(&relative_file).err(),
            Some(HubModelConfigurationError::InvalidCredentialDelivery)
        );
    }

    #[test]
    fn configuration_rejects_a_direct_adapter_file_environment_key() {
        let environment_key = CONFIGURATION.replace(
            "file = \"/run/secrets/anthropic-primary\"",
            "file = \"/run/secrets/anthropic-primary\"\nenv_key = \"ANTHROPIC_API_KEY\"",
        );

        assert_eq!(
            HubModelConfiguration::parse(&environment_key).err(),
            Some(HubModelConfigurationError::InvalidCredentialDelivery)
        );
    }

    #[test]
    fn configuration_admits_claude_file_delivery_with_its_fixed_environment_key() {
        const CLAUDE_FILE: &str = "/run/secrets/claude-api-primary";
        const CLAUDE_ENV_KEY: &str = "ANTHROPIC_API_KEY";
        let claude_file = CONFIGURATION.replace(
            r#"adapter = "codex_cli"
billing_kind = "subscription"
delivery = "ambient""#,
            &format!(
                r#"adapter = "claude_cli"
billing_kind = "api_metered"
delivery = "file"
file = "{CLAUDE_FILE}"
env_key = "{CLAUDE_ENV_KEY}""#,
            ),
        );

        let configured = HubModelConfiguration::parse(&claude_file)
            .expect("Claude file delivery is part of the supplied grammar");
        let profile = configured
            .credential_profile(CODEX_SUBSCRIPTION_PROFILE)
            .expect("the replaced fixture profile remains declared");
        assert_eq!(
            profile.delivery(),
            &CredentialDelivery::File {
                path: PathBuf::from(CLAUDE_FILE),
                env_key: Some(Arc::from(CLAUDE_ENV_KEY)),
            }
        );
    }

    #[test]
    fn configuration_rejects_another_claude_file_environment_key() {
        let claude_file = CONFIGURATION.replace(
            r#"adapter = "codex_cli"
billing_kind = "subscription"
delivery = "ambient""#,
            r#"adapter = "claude_cli"
billing_kind = "api_metered"
delivery = "file"
file = "/run/secrets/claude-api-primary"
env_key = "HOME""#,
        );

        assert_eq!(
            HubModelConfiguration::parse(&claude_file).err(),
            Some(HubModelConfigurationError::InvalidCredentialDelivery)
        );
    }

    #[test]
    fn configuration_rejects_duplicate_normalized_file_paths_for_one_adapter() {
        let duplicate_path = CONFIGURATION.replace(
            "file = \"/run/secrets/anthropic-overflow\"",
            "file = \"/run/secrets/./nested/../anthropic-primary\"",
        );

        assert_eq!(
            HubModelConfiguration::parse(&duplicate_path).err(),
            Some(HubModelConfigurationError::InvalidCredentialDelivery)
        );
    }

    #[test]
    fn configuration_rejects_duplicate_ambient_profiles_for_one_cli_adapter() {
        let duplicate_ambient = CONFIGURATION.replace(
            r#"[[credential_profiles]]
name = "codex-subscription-primary"
adapter = "codex_cli"
billing_kind = "subscription"
delivery = "ambient""#,
            r#"[[credential_profiles]]
name = "codex-subscription-primary"
adapter = "codex_cli"
billing_kind = "subscription"
delivery = "ambient"

[[credential_profiles]]
name = "codex-subscription-overflow"
adapter = "codex_cli"
billing_kind = "subscription"
delivery = "ambient""#,
        );

        assert_eq!(
            HubModelConfiguration::parse(&duplicate_ambient).err(),
            Some(HubModelConfigurationError::InvalidCredentialDelivery)
        );
    }

    #[test]
    fn configuration_rejects_mixed_ambient_and_home_delivery_for_codex() {
        let temporary = tempfile::tempdir().expect("synthetic home root is created");
        let home = temporary.path().join("account-b");
        std::fs::create_dir(&home).expect("synthetic home is created");
        std::fs::write(home.join("fixture-marker"), "synthetic")
            .expect("synthetic home is nonempty");
        let mixed_delivery = CONFIGURATION.replace(
            r#"[[credential_profiles]]
name = "codex-subscription-primary"
adapter = "codex_cli"
billing_kind = "subscription"
delivery = "ambient""#,
            &format!(
                r#"[[credential_profiles]]
name = "codex-subscription-primary"
adapter = "codex_cli"
billing_kind = "subscription"
delivery = "ambient"

[[credential_profiles]]
name = "codex-subscription-overflow"
adapter = "codex_cli"
billing_kind = "subscription"
delivery = "codex_home"
codex_home = {:?}"#,
                home.to_string_lossy()
            ),
        );

        assert_eq!(
            HubModelConfiguration::parse(&mixed_delivery).err(),
            Some(HubModelConfigurationError::InvalidCredentialDelivery)
        );
    }

    #[test]
    fn configuration_rejects_mixed_home_and_ambient_delivery_for_codex_in_reverse_order() {
        let temporary = tempfile::tempdir().expect("synthetic home root is created");
        let home = temporary.path().join("account-b");
        std::fs::create_dir(&home).expect("synthetic home is created");
        std::fs::write(home.join("fixture-marker"), "synthetic")
            .expect("synthetic home is nonempty");
        let mixed_delivery = CONFIGURATION.replace(
            r#"[[credential_profiles]]
name = "codex-subscription-primary"
adapter = "codex_cli"
billing_kind = "subscription"
delivery = "ambient""#,
            &format!(
                r#"[[credential_profiles]]
name = "codex-subscription-overflow"
adapter = "codex_cli"
billing_kind = "subscription"
delivery = "codex_home"
codex_home = {:?}

[[credential_profiles]]
name = "codex-subscription-primary"
adapter = "codex_cli"
billing_kind = "subscription"
delivery = "ambient""#,
                home.to_string_lossy()
            ),
        );

        assert_eq!(
            HubModelConfiguration::parse(&mixed_delivery).err(),
            Some(HubModelConfigurationError::InvalidCredentialDelivery)
        );
    }

    #[test]
    fn configuration_admits_a_claude_ambient_profile_declared_before_a_codex_home() {
        let temporary = tempfile::tempdir().expect("synthetic home root is created");
        let home = temporary.path().join("account-b");
        std::fs::create_dir(&home).expect("synthetic home is created");
        std::fs::write(home.join("fixture-marker"), "synthetic")
            .expect("synthetic home is nonempty");
        // The Claude `ambient` profile precedes the Codex home in table order,
        // which is the arrangement an adapter-blind conflict scan rejects.
        let cross_adapter = CONFIGURATION.replace(
            r#"[[credential_profiles]]
name = "codex-subscription-primary"
adapter = "codex_cli"
billing_kind = "subscription"
delivery = "ambient""#,
            &format!(
                r#"[[credential_profiles]]
name = "claude-subscription-primary"
adapter = "claude_cli"
billing_kind = "subscription"
delivery = "ambient"

[[credential_profiles]]
name = "codex-subscription-primary"
adapter = "codex_cli"
billing_kind = "subscription"
delivery = "codex_home"
codex_home = {:?}"#,
                home.to_string_lossy()
            ),
        );

        let parsed = HubModelConfiguration::parse(&cross_adapter)
            .expect("a Claude ambient profile does not contest a Codex credential home");
        assert_eq!(
            parsed
                .credential_profile(CODEX_SUBSCRIPTION_PROFILE)
                .expect("Codex profile remains present")
                .delivery()
                .path(),
            Some(&home)
        );
    }

    #[test]
    fn file_delivery_records_the_absolute_path_it_reads() {
        let configured = HubModelConfiguration::parse(CONFIGURATION).expect("the fixture is valid");

        let profile = configured
            .credential_profile("anthropic-primary")
            .expect("the fixture declares the profile");

        assert_eq!(
            profile.delivery(),
            &CredentialDelivery::File {
                path: PathBuf::from("/run/secrets/anthropic-primary"),
                env_key: None,
            }
        );
    }

    #[test]
    fn configuration_debug_redacts_credential_file_paths() {
        let configured = HubModelConfiguration::parse(CONFIGURATION).expect("the fixture is valid");
        let credential_path = configured
            .credential_profile("anthropic-primary")
            .expect("the fixture declares the profile")
            .delivery()
            .path()
            .expect("the fixture profile uses file delivery")
            .to_string_lossy();

        assert!(!format!("{configured:?}").contains(credential_path.as_ref()));
    }

    #[test]
    fn file_profile_catalog_is_complete_within_and_closed_across_adapters() {
        let configured =
            HubModelConfiguration::parse(&format!("{CONFIGURATION}\n{OPENAI_MAPPING_AND_MODEL}"))
                .expect("the combined fixture is valid");
        let anthropic_profiles = configured
            .file_credential_profiles(ModelAdapter::Anthropic)
            .map(|(reference, _)| reference)
            .collect::<HashSet<_>>();
        let openai_profiles = configured
            .file_credential_profiles(ModelAdapter::OpenAi)
            .map(|(reference, _)| reference)
            .collect::<HashSet<_>>();

        assert_eq!(
            anthropic_profiles,
            HashSet::from([ANTHROPIC_CREDENTIAL_REFERENCE, ANTHROPIC_OVERFLOW_PROFILE,])
        );
        assert_eq!(openai_profiles, HashSet::from([OPENAI_PROFILE]));
    }

    #[test]
    fn codex_only_configuration_delivers_no_anthropic_file() {
        let executable = tempfile::NamedTempFile::new().expect("a temporary executable is created");
        let working_directory = tempfile::tempdir().expect("a temporary directory is created");
        let configured = HubModelConfiguration::parse_test_fixture(&format!(
            r#"
version = 1

[[credential_profiles]]
name = "codex-subscription-primary"
adapter = "codex_cli"
billing_kind = "subscription"
delivery = "ambient"

[[credential_pools]]
name = "codex-main"
tie_break = "first_listed"
on_pool_exhausted = "fail"
members = [{{ profile = "codex-subscription-primary", priority = 1 }}]

[[adapter_mappings]]
model_family = "codex"
adapter = "codex_cli"
credential_pool = "codex-main"

[codex_cli]
executable = "{}"
working_directory = "{}"

[compaction]
prompt = "Summarize."

[[models]]
selection_id = "10000000-0000-4000-8000-000000000009"
target_id = "20000000-0000-4000-8000-000000000009"
model_family = "codex"
provider_model = "gpt-example"
max_output_tokens = 256
context_window_tokens = 200000
"#,
            executable.path().display(),
            working_directory.path().display(),
        ))
        .expect("a Codex-only configuration is valid");

        assert_eq!(
            configured
                .file_credential_profiles(ModelAdapter::Anthropic)
                .next(),
            None
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
    fn codex_model_context_window_overrides_are_positive_exact_target_values() {
        let temporary = tempfile::tempdir().expect("fixture directory is available");
        let executable = std::env::current_exe().expect("the test executable has a path");
        let configuration = format!(
            "{}model_context_window_overrides = {{ \"gpt-5.6-sol\" = 1000000 }}\n\n\
             [[models]]\n\
             selection_id = \"10000000-0000-4000-8000-00000000000f\"\n\
             target_id = \"20000000-0000-4000-8000-00000000000f\"\n\
             model_family = \"codex\"\n\
             provider_model = \"gpt-5.6-sol\"\n\
             max_output_tokens = 8192\n\
             context_window_tokens = 828400\n",
            configuration_with_codex_paths(&executable, temporary.path())
        );

        let parsed = HubModelConfiguration::parse(&configuration)
            .expect("a positive exact-target override is valid");

        assert_eq!(
            parsed
                .codex_cli()
                .and_then(|codex| codex.model_context_window_overrides.get("gpt-5.6-sol")),
            Some(&1_000_000)
        );
    }

    #[test]
    fn configuration_rejects_a_zero_codex_model_context_window_override() {
        let temporary = tempfile::tempdir().expect("fixture directory is available");
        let executable = std::env::current_exe().expect("the test executable has a path");
        let configuration = format!(
            "{}model_context_window_overrides = {{ \"gpt-5.6-sol\" = 0 }}\n",
            configuration_with_codex_paths(&executable, temporary.path())
        );

        assert_eq!(
            HubModelConfiguration::parse(&configuration).err(),
            Some(HubModelConfigurationError::InvalidCodexCliConfiguration)
        );
    }

    #[test]
    fn configuration_rejects_an_unknown_codex_model_context_window_override() {
        let temporary = tempfile::tempdir().expect("fixture directory is available");
        let executable = std::env::current_exe().expect("the test executable has a path");
        let configuration = format!(
            "{}model_context_window_overrides = {{ \"gpt-5.6-sol\" = 1000000 }}\n",
            configuration_with_codex_paths(&executable, temporary.path())
        );

        assert_eq!(
            HubModelConfiguration::parse(&configuration).err(),
            Some(HubModelConfigurationError::InvalidCodexCliConfiguration)
        );
    }

    #[test]
    fn configuration_rejects_a_codex_override_for_another_adapter() {
        let temporary = tempfile::tempdir().expect("fixture directory is available");
        let executable = std::env::current_exe().expect("the test executable has a path");
        let configuration = format!(
            "{}model_context_window_overrides = {{ \"claude-example\" = 1000000 }}\n",
            configuration_with_codex_paths(&executable, temporary.path())
        );

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
                .codex_cli_runtime(None, None)
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
        let (configuration, _workspace) =
            configuration_varying_the_claude_bridge(Path::new(ABSENT_MCP_BRIDGE_NAME));

        assert_eq!(
            HubModelConfiguration::parse(&configuration).err(),
            Some(HubModelConfigurationError::UnresolvedClaudeMcpBridgeExecutable)
        );
    }

    /// A relative value carries a path separator, so it is a path and keeps
    /// the absolute-path rule instead of being looked up as a program name.
    #[test]
    fn configuration_rejects_a_relative_claude_mcp_bridge_path() {
        let (configuration, _workspace) =
            configuration_varying_the_claude_bridge(Path::new("bin/signalbox-claude-mcp-bridge"));

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
    /// this process cannot execute shadows nothing.
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
        let empty_entry = Path::new("");
        let relative_entry = Path::new("synthetic/relative/bin");
        let absolute_entry = Path::new("/synthetic/absolute/bin");
        let search_path = synthetic_search_path(&[empty_entry, relative_entry, absolute_entry]);

        assert_eq!(
            absolute_search_entries(Some(&search_path)),
            vec![absolute_entry.to_path_buf()]
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
                .claude_cli_runtime(None, None, None)
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
    fn configuration_accepts_an_opaque_openai_profile_name() {
        let other_profile = "openai-secondary";
        let configuration = format!("{CONFIGURATION}{OPENAI_MAPPING_AND_MODEL}")
            .replace(
                "name = \"openai-primary\"",
                &format!("name = \"{other_profile}\""),
            )
            .replace(
                "profile = \"openai-primary\"",
                &format!("profile = \"{other_profile}\""),
            );
        let selection = DirectModelSelection::from_uuid(
            Uuid::parse_str("10000000-0000-4000-8000-00000000000e").expect("fixture UUID is valid"),
        );

        assert_eq!(
            HubModelConfiguration::parse(&configuration)
                .expect("opaque profile name is valid")
                .resolve_direct_model(selection)
                .expect("the OpenAI route exists")
                .credential_profile(),
            other_profile
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
    fn configuration_admits_explicit_workspace_instruction_roots() {
        let configured = format!(
            "{CONFIGURATION}\n[workspace_instructions]\nversion = 1\nregistered_roots = [\"{REGISTERED_INSTRUCTION_ROOT}\"]\n"
        );
        let configuration = HubModelConfiguration::parse(&configured)
            .expect("one canonical explicit instruction root is admitted");
        assert_eq!(configuration.workspace_instructions().roots().len(), 1);
        assert_eq!(
            configuration.workspace_instructions().roots()[0].as_str(),
            REGISTERED_INSTRUCTION_ROOT
        );
    }

    #[test]
    fn configuration_defaults_instruction_roots_to_empty() {
        let configuration = HubModelConfiguration::parse(CONFIGURATION)
            .expect("the base fixture omits explicit instruction roots");
        assert!(configuration.workspace_instructions().roots().is_empty());
    }

    #[test]
    fn configuration_rejects_relative_instruction_roots() {
        let relative = format!(
            "{CONFIGURATION}\n[workspace_instructions]\nversion = 1\nregistered_roots = [\"relative/root\"]\n"
        );
        assert_eq!(
            HubModelConfiguration::parse(&relative).err(),
            Some(HubModelConfigurationError::InvalidWorkspaceInstructionConfiguration)
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

    #[test]
    fn alternate_target_selects_its_serving_family_credential_profile() {
        let fast_family = "anthropic-fast";
        let fast_profile = ANTHROPIC_OVERFLOW_PROFILE;
        let configuration = format!(
            r#"{}

[[credential_pools]]
name = "{fast_family}"
tie_break = "first_listed"
on_pool_exhausted = "fail"
members = [{{ profile = "{fast_profile}", priority = 1 }}]

[[adapter_mappings]]
model_family = "{fast_family}"
adapter = "anthropic"
credential_pool = "{fast_family}"

[[serving_targets]]
target_id = "20000000-0000-4000-8000-000000000002"
model_family = "{fast_family}"
provider_model = "synthetic-fast-target"
max_output_tokens = 256
context_window_tokens = 200000
"#,
            CONFIGURATION.replace(
                "context_window_tokens = 200000",
                "context_window_tokens = 200000\nfast_mode = \"alternate_target\"\nfast_target_id = \"20000000-0000-4000-8000-000000000002\"",
            )
        );
        let configuration =
            HubModelConfiguration::parse(&configuration).expect("serving family is valid");
        let selected_target = configured_target(&configuration);
        let credential_pin = configuration.session_credential_pin();
        let serving_credential = credential_pin
            .credentials()
            .find(|credential| credential.model_family() == fast_family)
            .expect("serving family has a pinned credential");

        assert_eq!(
            configuration
                .credential_family_catalog()
                .family_for_call(selected_target, FastMode::Enabled),
            Some(fast_family)
        );
        assert_eq!(serving_credential.credential_reference(), fast_profile);
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
                .resolve(
                    &source
                        .credential_reference()
                        .expect("the fixture source has one reference"),
                )
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
        let reference = source
            .credential_reference()
            .expect("the fixture source has one reference");
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

    /// INV-035: a historical session pin can resolve any declared file
    /// profile, not only the member currently preferred by a pool.
    #[tokio::test]
    async fn inv035_file_credential_catalog_resolves_each_declared_profile() {
        let directory = tempfile::tempdir().expect("fixture directory is available");
        let primary_path = directory.path().join("primary");
        let historical_path = directory.path().join("historical");
        let primary_value = b"primary-test-value";
        let historical_value = b"historical-test-value";
        std::fs::write(&primary_path, primary_value).expect("primary fixture is writable");
        std::fs::write(&historical_path, historical_value).expect("historical fixture is writable");
        let primary = CredentialReference::new("primary-profile");
        let historical = CredentialReference::new("historical-profile");
        let source = FileCredentialAccess::from_files([
            (primary.clone(), primary_path),
            (historical.clone(), historical_path),
        ]);

        assert_eq!(
            source
                .resolve(&primary)
                .await
                .expect("primary profile resolves")
                .expose_bytes(),
            primary_value
        );
        assert_eq!(
            source
                .resolve(&historical)
                .await
                .expect("historical profile resolves")
                .expose_bytes(),
            historical_value
        );
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
                .resolve(
                    &source
                        .credential_reference()
                        .expect("the fixture source has one reference"),
                )
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
            .resolve(
                &source
                    .credential_reference()
                    .expect("the fixture source has one reference"),
            )
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
