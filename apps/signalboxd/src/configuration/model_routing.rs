use super::error::HubModelConfigurationError;
use crate::credential_pools::{CredentialPoolAction, CredentialPoolExhaustion};
use rust_decimal::Decimal;
use signalbox_domain::ResolvedProviderTarget;
use signalbox_persistence::{
    model_execution::{CredentialPoolRuntimeAction, CredentialPoolRuntimeExhaustion},
    process_read::ProcessModelCallInputTokenSemantics,
};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
};

pub(super) const fn runtime_pool_action(
    action: CredentialPoolAction,
) -> CredentialPoolRuntimeAction {
    match action {
        CredentialPoolAction::Stay => CredentialPoolRuntimeAction::Stay,
        CredentialPoolAction::SwitchNextTurn => CredentialPoolRuntimeAction::SwitchNextTurn,
        CredentialPoolAction::SwitchNow => CredentialPoolRuntimeAction::SwitchNow,
        CredentialPoolAction::AvoidNewSessions => CredentialPoolRuntimeAction::AvoidNewSessions,
        CredentialPoolAction::Quarantine => CredentialPoolRuntimeAction::Quarantine,
    }
}

pub(super) const fn runtime_pool_exhaustion(
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

pub(super) const MIGRATED_ANTHROPIC_MODEL_FAMILY: &str = "anthropic";

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
    pub(crate) const fn reports_remaining_capacity(self) -> bool {
        match self {
            Self::CodexCli => true,
            Self::Anthropic | Self::ClaudeCli | Self::OpenAi => false,
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
    pub(super) version: Arc<str>,
    pub(super) input: Decimal,
    pub(super) output: Decimal,
    pub(super) cache_creation_input: Decimal,
    pub(super) cache_read_input: Decimal,
}

impl ModelBillingRates {
    /// Exact deployment-owned rate version.
    pub fn version(&self) -> &str {
        &self.version
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ModelCallInputUsage {
    pub(super) tokens: Option<u64>,
    pub(super) semantics: Option<ProcessModelCallInputTokenSemantics>,
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
    pub(super) amount_usd: Decimal,
    pub(super) rate_version: Arc<str>,
    pub(super) billing_kind: BillingKind,
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
    pub(super) executable: PathBuf,
    pub(super) working_directory: PathBuf,
    pub(super) model_context_window_overrides: HashMap<String, u32>,
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
    pub(super) executable: PathBuf,
    pub(super) mcp_bridge_executable: PathBuf,
    pub(super) working_directory: PathBuf,
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
    pub(super) model_family: Arc<str>,
    pub(super) adapter: ModelAdapter,
    pub(super) credential_pool: Arc<str>,
    pub(super) credential_profile: Arc<str>,
    pub(super) target: ResolvedProviderTarget,
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
pub(super) struct AdapterMapping {
    pub(super) adapter: ModelAdapter,
    pub(super) credential_pool: Arc<str>,
    pub(super) credential_profile: Arc<str>,
}
