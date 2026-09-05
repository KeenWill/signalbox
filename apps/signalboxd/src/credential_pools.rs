//! Deployment-owned credential profiles, deliveries, and pools.
//!
//! `docs/spec/configuration-and-credentials.md` owns this grammar. This module
//! parses it and admits only what the composed build can deliver: a profile
//! naming a delivery no adapter surface provides, and a pool whose selection
//! depends on capacity no adapter observes, are refused at startup rather than
//! accepted inert.

use std::{
    collections::{HashMap, HashSet},
    fmt,
    num::NonZeroU32,
    path::{Component, Path, PathBuf},
    sync::Arc,
};

use signalbox_model_runtime_claude_cli::CLAUDE_CLI_FILE_CREDENTIAL_ENV_KEY;
use toml_edit::{InlineTable, Item, Table};
use url::Url;

use crate::configuration::{
    AvailabilityCause, BillingKind, HubModelConfigurationError, ModelAdapter,
    reject_unknown_fields, required_string, validated_name,
};

/// Maximum admitted headroom reserve, leaving at least one percent usable.
const MAX_HEADROOM_RESERVE_PERCENT: i64 = 99;

/// Maximum UTF-8 byte length admitted for a credential-delivery path.
pub(crate) const MAX_CREDENTIAL_DELIVERY_PATH_UTF8_BYTES: usize = 4_096;

/// Maximum UTF-8 byte length admitted for a credential profile or pool name.
pub(crate) const MAX_CREDENTIAL_CATALOG_NAME_UTF8_BYTES: usize = 256;

/// Maximum number of members admitted in one credential pool.
pub(crate) const MAX_CREDENTIAL_POOL_MEMBERS: usize = 1_024;

/// Maximum concurrent invocations admitted for one bounded credential home.
///
/// Capped rather than left at the integer domain because a contended wait
/// durably names the identity of every live reservation holding the bound and
/// rewrites that snapshot on release, so this bound is the multiplier on every
/// wake's rewrite. It bounds one member's contribution and not the whole wait,
/// whose evidence is the sum across every otherwise-admissible bounded member;
/// what this cap buys is that no single misconfigured member supplies an
/// unbounded share of that sum.
pub(crate) const MAX_CREDENTIAL_HOME_CONCURRENT_INVOCATIONS: u32 = 1_024;

/// Every delivery spelling the grammar recognizes, whether or not this build
/// supplies a surface for it.
const DELIVERY_KEYS: [&str; 4] = ["ambient", "file", "codex_home", "oauth"];

/// Fields common to every credential profile, before its delivery adds its own.
const PROFILE_COMMON_FIELDS: [&str; 4] = ["name", "adapter", "billing_kind", "delivery"];

/// How one credential profile's secret reaches its provider.
///
/// The variants below are the deliveries this build supplies. The grammar also
/// recognizes `oauth`; parsing rejects it as undelivered so a deployment learns
/// at startup that no surface honors it, rather than from
/// a call that silently authenticated as some other account.
#[derive(Clone, Eq, PartialEq)]
pub enum CredentialDelivery {
    /// The adapter's own client resolves its login and the daemon supplies no
    /// credential material. The Codex CLI owns its external login this way.
    Ambient,
    /// An absolute deployment-owned file read per preparation and never cached.
    File {
        /// Absolute path the deployment writes the credential value to.
        path: PathBuf,
        /// Process environment key a spawned adapter supplies the value under.
        env_key: Option<Arc<str>>,
    },
    /// An operator-provisioned Codex login directory selected by reference.
    /// The daemon validates directory shape but never reads its auth material,
    /// per `docs/spec/configuration-and-credentials.md`.
    CodexHome {
        /// Absolute existing nonempty directory passed only as `CODEX_HOME`.
        path: PathBuf,
        /// Optional per-home process concurrency declaration.
        max_concurrent_invocations: Option<NonZeroU32>,
    },
}

/// Typed startup failure for one configured credential home.
///
/// `docs/spec/configuration-and-credentials.md` owns
/// these fail-closed admission conditions.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CredentialHomeAdmissionFailure {
    /// The configured path was not an absolute normalized path.
    InvalidPath,
    /// The path does not name an existing directory.
    MissingOrNotDirectory,
    /// Directory enumeration failed closed.
    UnreadableDirectory,
    /// The directory contains no provisioned entries.
    EmptyDirectory,
}

impl CredentialHomeAdmissionFailure {
    /// Operator-facing spelling of this closed cause.
    ///
    /// The returned text names the admission condition only. It never carries
    /// the configured path or any authentication material, so error display
    /// may quote it beside the profile reference.
    pub const fn cause(self) -> &'static str {
        match self {
            Self::InvalidPath => "path is not absolute and normalized",
            Self::MissingOrNotDirectory => "path is not an existing directory",
            Self::UnreadableDirectory => "directory could not be enumerated",
            Self::EmptyDirectory => "directory contains no provisioned entries",
        }
    }
}

impl fmt::Debug for CredentialDelivery {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ambient => formatter.write_str("Ambient"),
            Self::File { env_key, .. } => formatter
                .debug_struct("File")
                .field("path", &"[credential file path]")
                .field("env_key", env_key)
                .finish(),
            Self::CodexHome {
                max_concurrent_invocations,
                ..
            } => formatter
                .debug_struct("CodexHome")
                .field("path", &"[credential home path]")
                .field("max_concurrent_invocations", max_concurrent_invocations)
                .finish(),
        }
    }
}

impl CredentialDelivery {
    /// Exact configuration spelling of this delivery.
    pub const fn key(&self) -> &'static str {
        match self {
            Self::Ambient => "ambient",
            Self::File { .. } => "file",
            Self::CodexHome { .. } => "codex_home",
        }
    }

    /// Absolute deployment path this delivery references, where it has one.
    pub fn path(&self) -> Option<&PathBuf> {
        match self {
            Self::Ambient => None,
            Self::File { path, .. } => Some(path),
            Self::CodexHome { path, .. } => Some(path),
        }
    }

    /// Process environment key a spawned adapter supplies the value under.
    pub fn env_key(&self) -> Option<&str> {
        match self {
            Self::Ambient => None,
            Self::File { env_key, .. } => env_key.as_deref(),
            Self::CodexHome { .. } => None,
        }
    }

    fn parse(
        profile: &Table,
        adapter: ModelAdapter,
        name: &Arc<str>,
        billing_kind: BillingKind,
    ) -> Result<Self, HubModelConfigurationError> {
        let key = required_string(profile, "delivery")?;
        if !DELIVERY_KEYS.contains(&key) {
            return Err(HubModelConfigurationError::InvalidCredentialDelivery);
        }
        if !adapter.admits_delivery(key) {
            return Err(HubModelConfigurationError::UnsupportedCredentialDelivery {
                adapter,
                delivery: Arc::from(key),
            });
        }
        reject_disagreeing_billing_kind(key, name, billing_kind)?;
        match key {
            "ambient" => {
                reject_unknown_fields(profile, &PROFILE_COMMON_FIELDS)?;
                reject_undelivered(adapter, key)?;
                Ok(Self::Ambient)
            }
            "file" => {
                let mut allowed = PROFILE_COMMON_FIELDS.to_vec();
                allowed.extend_from_slice(&["file", "env_key"]);
                reject_unknown_fields(profile, &allowed)?;
                let path = normalize_absolute_path(required_string(profile, "file")?)?;
                let env_key = parse_file_env_key(profile, adapter)?;
                reject_undelivered(adapter, key)?;
                Ok(Self::File { path, env_key })
            }
            "codex_home" => {
                let mut allowed = PROFILE_COMMON_FIELDS.to_vec();
                allowed.extend_from_slice(&["codex_home", "max_concurrent_invocations"]);
                reject_unknown_fields(profile, &allowed)?;
                let path = normalize_absolute_path(required_string(profile, "codex_home")?)
                    .map_err(|_| HubModelConfigurationError::InvalidCredentialHome {
                        credential_profile: Arc::clone(name),
                        failure: CredentialHomeAdmissionFailure::InvalidPath,
                    })?;
                admit_credential_home(name, &path)?;
                let max_concurrent_invocations = parse_max_concurrent_invocations(profile)?;
                if max_concurrent_invocations.is_some() {
                    return Err(HubModelConfigurationError::InvalidCredentialDelivery);
                }
                Ok(Self::CodexHome {
                    path,
                    max_concurrent_invocations,
                })
            }
            "oauth" => {
                let mut allowed = PROFILE_COMMON_FIELDS.to_vec();
                allowed.extend_from_slice(&[
                    "client_id",
                    "token_url",
                    "device_authorization_url",
                    "scopes",
                ]);
                reject_unknown_fields(profile, &allowed)?;
                parse_oauth_delivery(profile)?;
                Err(undelivered(key))
            }
            _ => Err(HubModelConfigurationError::InvalidCredentialDelivery),
        }
    }
}

fn admit_credential_home(
    profile: &Arc<str>,
    path: &Path,
) -> Result<(), HubModelConfigurationError> {
    if !path.is_dir() {
        return Err(HubModelConfigurationError::InvalidCredentialHome {
            credential_profile: Arc::clone(profile),
            failure: CredentialHomeAdmissionFailure::MissingOrNotDirectory,
        });
    }
    let mut entries =
        std::fs::read_dir(path).map_err(|_| HubModelConfigurationError::InvalidCredentialHome {
            credential_profile: Arc::clone(profile),
            failure: CredentialHomeAdmissionFailure::UnreadableDirectory,
        })?;
    match entries.next() {
        Some(Ok(_)) => Ok(()),
        Some(Err(_)) => Err(HubModelConfigurationError::InvalidCredentialHome {
            credential_profile: Arc::clone(profile),
            failure: CredentialHomeAdmissionFailure::UnreadableDirectory,
        }),
        None => Err(HubModelConfigurationError::InvalidCredentialHome {
            credential_profile: Arc::clone(profile),
            failure: CredentialHomeAdmissionFailure::EmptyDirectory,
        }),
    }
}

/// Rejects a `billing_kind` the profile's delivery cannot authenticate.
///
/// Where a delivery fixes the authentication kind, the two fields cannot
/// disagree: `file` presents a provider API key, so it is `api_metered`, and
/// `oauth` constructs a subscription login, so it is `subscription`. `ambient`
/// and `codex_home` name a login the operator established outside the daemon,
/// which may be either, and the daemon cannot tell which — so they admit both.
///
/// Why reject rather than infer: `billing_kind` is what terminal cost
/// derivation trusts to choose between a real charge and a metered equivalent,
/// so an accepted contradiction silently misreports spend, and inferring the
/// value would overwrite an operator's statement about the two deliveries where
/// the answer genuinely varies.
///
/// This runs before the undelivered decision, so a reserved delivery's
/// contradiction is refused on its own terms rather than being masked by the
/// refusal that follows it.
fn reject_disagreeing_billing_kind(
    delivery: &str,
    name: &Arc<str>,
    billing_kind: BillingKind,
) -> Result<(), HubModelConfigurationError> {
    let admitted = match delivery {
        "file" => billing_kind == BillingKind::ApiMetered,
        "oauth" => billing_kind == BillingKind::Subscription,
        // `ambient` and `codex_home` admit either, so nothing is checked.
        _ => true,
    };
    if admitted {
        return Ok(());
    }
    Err(
        HubModelConfigurationError::DisagreeingCredentialBillingKind {
            credential_profile: Arc::clone(name),
            delivery: Arc::from(delivery),
            billing_kind: Arc::from(match billing_kind {
                BillingKind::ApiMetered => "api_metered",
                BillingKind::Subscription => "subscription",
            }),
        },
    )
}

fn undelivered(delivery: &str) -> HubModelConfigurationError {
    HubModelConfigurationError::UndeliveredCredentialDelivery {
        delivery: Arc::from(delivery),
    }
}

fn reject_undelivered(
    adapter: ModelAdapter,
    delivery: &str,
) -> Result<(), HubModelConfigurationError> {
    if adapter.delivers(delivery) {
        Ok(())
    } else {
        Err(undelivered(delivery))
    }
}

fn parse_file_env_key(
    profile: &Table,
    adapter: ModelAdapter,
) -> Result<Option<Arc<str>>, HubModelConfigurationError> {
    match adapter {
        ModelAdapter::Anthropic | ModelAdapter::OpenAi => {
            if profile.contains_key("env_key") {
                return Err(HubModelConfigurationError::InvalidCredentialDelivery);
            }
            Ok(None)
        }
        ModelAdapter::ClaudeCli => {
            let env_key = validated_name(required_string(profile, "env_key")?)?;
            if env_key.as_ref() != CLAUDE_CLI_FILE_CREDENTIAL_ENV_KEY {
                return Err(HubModelConfigurationError::InvalidCredentialDelivery);
            }
            Ok(Some(env_key))
        }
        ModelAdapter::CodexCli => {
            let env_key = validated_name(required_string(profile, "env_key")?)?;
            if env_key.as_ref() != "OPENAI_API_KEY" {
                return Err(HubModelConfigurationError::InvalidCredentialDelivery);
            }
            Ok(Some(env_key))
        }
    }
}

fn normalize_absolute_path(value: &str) -> Result<PathBuf, HubModelConfigurationError> {
    if value.is_empty()
        || value.len() > MAX_CREDENTIAL_DELIVERY_PATH_UTF8_BYTES
        || value.contains('\0')
    {
        return Err(HubModelConfigurationError::InvalidCredentialDelivery);
    }
    let path = Path::new(value);
    if !path.is_absolute() {
        return Err(HubModelConfigurationError::InvalidCredentialDelivery);
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
                    return Err(HubModelConfigurationError::InvalidCredentialDelivery);
                }
            }
        }
    }
    Ok(normalized)
}

fn parse_max_concurrent_invocations(
    profile: &Table,
) -> Result<Option<NonZeroU32>, HubModelConfigurationError> {
    profile
        .get("max_concurrent_invocations")
        .map(|value| {
            value
                .as_integer()
                .and_then(|value| u32::try_from(value).ok())
                .filter(|value| *value <= MAX_CREDENTIAL_HOME_CONCURRENT_INVOCATIONS)
                .and_then(NonZeroU32::new)
                .ok_or(HubModelConfigurationError::InvalidCredentialDelivery)
        })
        .transpose()
}

fn parse_oauth_delivery(profile: &Table) -> Result<(), HubModelConfigurationError> {
    let client_id = required_string(profile, "client_id")?;
    if client_id.is_empty() || client_id.len() > 1_024 || client_id.contains('\0') {
        return Err(HubModelConfigurationError::InvalidCredentialDelivery);
    }
    validate_https_url(required_string(profile, "token_url")?)?;
    validate_https_url(required_string(profile, "device_authorization_url")?)?;
    let scopes = profile
        .get("scopes")
        .and_then(Item::as_array)
        .ok_or(HubModelConfigurationError::InvalidCredentialDelivery)?;
    if scopes.is_empty() || scopes.len() > 64 {
        return Err(HubModelConfigurationError::InvalidCredentialDelivery);
    }
    let mut declared = HashSet::with_capacity(scopes.len());
    for scope in scopes {
        let scope = scope
            .as_str()
            .ok_or(HubModelConfigurationError::InvalidCredentialDelivery)?;
        if scope.is_empty()
            || scope.len() > 256
            || !scope.bytes().all(is_oauth_scope_token_byte)
            || !declared.insert(scope)
        {
            return Err(HubModelConfigurationError::InvalidCredentialDelivery);
        }
    }
    Ok(())
}

/// Reports whether one byte is an RFC 6749 `scope-token` character.
///
/// The grammar is `%x21 / %x23-5B / %x5D-7E`, which admits ordinary ASCII
/// graphics while excluding the space, the double quote, the backslash, every
/// control byte including NUL, and every non-ASCII byte. OAuth transmits
/// `scope` as a space-delimited sequence, so an element containing a space
/// would become two scopes on the wire while the exact-duplicate rule and the
/// persisted provisioning tuple treat it as one value.
const fn is_oauth_scope_token_byte(byte: u8) -> bool {
    matches!(byte, 0x21 | 0x23..=0x5B | 0x5D..=0x7E)
}

fn validate_https_url(value: &str) -> Result<(), HubModelConfigurationError> {
    let parsed =
        Url::parse(value).map_err(|_| HubModelConfigurationError::InvalidCredentialDelivery)?;
    // Neither a fragment nor user information reaches the request target, so
    // admitting either would give a single wire endpoint several stored
    // provisioning-tuple identities. User information additionally carries a
    // secret the reference-only at-rest boundary refuses to hold.
    if parsed.scheme() != "https"
        || parsed.host_str().is_none()
        || parsed.fragment().is_some()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
    {
        return Err(HubModelConfigurationError::InvalidCredentialDelivery);
    }
    Ok(())
}

/// One provider account and the delivery that authenticates calls through it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CredentialProfile {
    name: Arc<str>,
    adapter: ModelAdapter,
    billing_kind: BillingKind,
    delivery: CredentialDelivery,
}

impl CredentialProfile {
    /// Exact deployment-owned profile name, opaque to this build.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Build-provided adapter this profile authenticates.
    pub const fn adapter(&self) -> ModelAdapter {
        self.adapter
    }

    /// How calls authenticated by this profile are billed.
    pub const fn billing_kind(&self) -> BillingKind {
        self.billing_kind
    }

    /// How this profile's secret reaches its provider.
    pub const fn delivery(&self) -> &CredentialDelivery {
        &self.delivery
    }
}

/// Rule resolving equal priorities within one pool.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CredentialPoolTieBreak {
    /// The earliest tied member in declaration order.
    FirstListed,
    /// Reserved spelling for a future durable priority cursor.
    RoundRobin,
    /// The tied member with the most remaining reported capacity.
    LeastUsed,
}

impl CredentialPoolTieBreak {
    fn parse(value: &str) -> Result<Self, HubModelConfigurationError> {
        let tie_break = match value {
            "first_listed" => Self::FirstListed,
            "round_robin" => Self::RoundRobin,
            "least_used" => Self::LeastUsed,
            _ => return Err(HubModelConfigurationError::InvalidCredentialPoolPolicy),
        };
        if tie_break == Self::RoundRobin {
            return Err(HubModelConfigurationError::InvalidCredentialPoolPolicy);
        }
        Ok(tie_break)
    }

    /// Reports whether resolving a tie this way needs observed capacity.
    const fn needs_observed_capacity(self) -> bool {
        matches!(self, Self::LeastUsed)
    }
}

/// What a pool does for a turn when it admits no member.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CredentialPoolExhaustion {
    /// Park the turn in the durable wait carrying the earliest reported reset.
    Park,
    /// Fail the turn as a known failure.
    Fail,
}

impl CredentialPoolExhaustion {
    fn parse(value: &str) -> Result<Self, HubModelConfigurationError> {
        match value {
            "park" => Ok(Self::Park),
            "fail" => Ok(Self::Fail),
            _ => Err(HubModelConfigurationError::InvalidCredentialPoolPolicy),
        }
    }
}

/// Cause a pool may react to.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum CredentialPoolTrigger {
    /// The provider reported the account's quota spent.
    QuotaExhausted,
    /// The provider rate-limited the request.
    RateLimited,
    /// The provider reported itself overloaded.
    Overloaded,
    /// The provider rejected the credential itself.
    CredentialRejected,
    /// Reported remaining capacity fell below the configured reserve.
    HeadroomLow,
}

impl CredentialPoolTrigger {
    /// Every admitted trigger, in the order the grammar lists them.
    pub const ALL: [Self; 5] = [
        Self::QuotaExhausted,
        Self::RateLimited,
        Self::Overloaded,
        Self::CredentialRejected,
        Self::HeadroomLow,
    ];

    /// Exact configuration key selecting this trigger's action.
    pub const fn key(self) -> &'static str {
        match self {
            Self::QuotaExhausted => "on_quota_exhausted",
            Self::RateLimited => "on_rate_limited",
            Self::Overloaded => "on_overloaded",
            Self::CredentialRejected => "on_credential_rejected",
            Self::HeadroomLow => "on_headroom_low",
        }
    }

    /// Reports whether this cause carries proof the request was not accepted,
    /// which is what admits a successor call on the same turn.
    const fn admits_switch_now(self) -> bool {
        matches!(
            self,
            Self::QuotaExhausted | Self::RateLimited | Self::Overloaded
        )
    }

    /// Reports whether reacting to this cause needs observed capacity.
    const fn needs_observed_capacity(self) -> bool {
        matches!(self, Self::HeadroomLow)
    }
}

/// What a pool does with a member whose trigger fired.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CredentialPoolAction {
    /// Keep the member; the failure terminalizes as it would with no pool.
    Stay,
    /// Fail this turn and exclude the member from the next turn's preparation.
    SwitchNextTurn,
    /// Create a successor attempt against the next admitted member.
    SwitchNow,
    /// Keep pinned sessions; exclude the member for sessions with no prior
    /// completed call on this pool.
    AvoidNewSessions,
    /// Exclude the member from every pool and across restarts until cleared.
    Quarantine,
}

impl CredentialPoolAction {
    fn parse(value: &str) -> Result<Self, HubModelConfigurationError> {
        match value {
            "stay" => Ok(Self::Stay),
            "switch_next_turn" => Ok(Self::SwitchNextTurn),
            "switch_now" => Ok(Self::SwitchNow),
            "avoid_new_sessions" => Ok(Self::AvoidNewSessions),
            "quarantine" => Ok(Self::Quarantine),
            _ => Err(HubModelConfigurationError::UnknownCredentialPoolAction),
        }
    }
}

/// One profile's membership in one pool.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CredentialPoolMember {
    profile: Arc<str>,
    priority: NonZeroU32,
    headroom_reserve_percent: Option<u8>,
}

impl CredentialPoolMember {
    /// Exact profile name this membership admits.
    pub fn profile(&self) -> &str {
        &self.profile
    }

    /// Rank within this pool, where a lower value is preferred.
    pub const fn priority(&self) -> NonZeroU32 {
        self.priority
    }

    /// Reserve overriding the pool value for this member alone.
    pub const fn headroom_reserve_percent(&self) -> Option<u8> {
        self.headroom_reserve_percent
    }
}

/// The set of profiles that may substitute for one another for one family.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CredentialPool {
    name: Arc<str>,
    adapter: ModelAdapter,
    members: Vec<CredentialPoolMember>,
    tie_break: CredentialPoolTieBreak,
    on_pool_exhausted: CredentialPoolExhaustion,
    headroom_reserve_percent: Option<u8>,
    quota_exhausted: CredentialPoolAction,
    rate_limited: CredentialPoolAction,
    overloaded: CredentialPoolAction,
    credential_rejected: CredentialPoolAction,
    headroom_low: CredentialPoolAction,
}

impl CredentialPool {
    /// Exact deployment-owned pool name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Build-provided adapter every member of this pool authenticates.
    pub const fn adapter(&self) -> ModelAdapter {
        self.adapter
    }

    /// Members in declaration order.
    pub fn members(&self) -> &[CredentialPoolMember] {
        &self.members
    }

    /// Rule resolving equal priorities.
    pub const fn tie_break(&self) -> CredentialPoolTieBreak {
        self.tie_break
    }

    /// What a turn does when the pool admits no member.
    pub const fn on_pool_exhausted(&self) -> CredentialPoolExhaustion {
        self.on_pool_exhausted
    }

    /// Pool-wide reserve, overridable per member.
    pub const fn headroom_reserve_percent(&self) -> Option<u8> {
        self.headroom_reserve_percent
    }

    /// Action configured for one trigger, defaulting to
    /// [`CredentialPoolAction::Stay`] where the document omitted its key.
    pub const fn action(&self, trigger: CredentialPoolTrigger) -> CredentialPoolAction {
        match trigger {
            CredentialPoolTrigger::QuotaExhausted => self.quota_exhausted,
            CredentialPoolTrigger::RateLimited => self.rate_limited,
            CredentialPoolTrigger::Overloaded => self.overloaded,
            CredentialPoolTrigger::CredentialRejected => self.credential_rejected,
            CredentialPoolTrigger::HeadroomLow => self.headroom_low,
        }
    }

    /// Returns the member this pool prefers while no member is excluded: the
    /// lowest priority value, earliest in declaration order among equals.
    ///
    /// Configuration parsing calls this once per mapping to derive the family's
    /// initial pin. Persistence then consults the complete pool policy for each
    /// fresh availability chain and records exclusions and successor state as
    /// specified by `docs/spec/credential-availability.md`.
    pub fn preferred_member(&self) -> Option<&CredentialPoolMember> {
        self.members.iter().min_by_key(|member| member.priority)
    }
}

/// Parses the complete `[[credential_profiles]]` array.
pub(crate) fn parse_credential_profiles(
    item: Option<&Item>,
) -> Result<HashMap<Arc<str>, CredentialProfile>, HubModelConfigurationError> {
    let tables = item
        .and_then(Item::as_array_of_tables)
        .ok_or(HubModelConfigurationError::MissingCredentialProfiles)?;
    if tables.is_empty() {
        return Err(HubModelConfigurationError::MissingCredentialProfiles);
    }
    let mut profiles: HashMap<Arc<str>, CredentialProfile> = HashMap::with_capacity(tables.len());
    let mut ambient_adapters = HashSet::new();
    let mut file_paths = HashSet::new();
    for profile in tables {
        let name = validated_credential_catalog_name(required_string(profile, "name")?)?;
        let adapter = ModelAdapter::parse(required_string(profile, "adapter")?)?;
        let billing_kind = BillingKind::parse(required_string(profile, "billing_kind")?)?;
        let delivery = CredentialDelivery::parse(profile, adapter, &name, billing_kind)?;
        // The mixed-delivery rule is a property of one adapter's login store,
        // so both sides of the pair must be Codex profiles. Another adapter's
        // `ambient` profile shares no store with a Codex home and must not be
        // matched here, or admission would turn on profile-table order.
        let conflicts_with_codex_ambient = adapter == ModelAdapter::CodexCli
            && profiles.values().any(|existing| {
                existing.adapter() == ModelAdapter::CodexCli
                    && matches!(
                        (existing.delivery(), &delivery),
                        (
                            CredentialDelivery::Ambient,
                            CredentialDelivery::CodexHome { .. }
                        ) | (
                            CredentialDelivery::CodexHome { .. },
                            CredentialDelivery::Ambient
                        )
                    )
            });
        if conflicts_with_codex_ambient {
            return Err(HubModelConfigurationError::InvalidCredentialDelivery);
        }
        if delivery == CredentialDelivery::Ambient && !ambient_adapters.insert(adapter) {
            return Err(HubModelConfigurationError::InvalidCredentialDelivery);
        }
        if let CredentialDelivery::File { path, .. } | CredentialDelivery::CodexHome { path, .. } =
            &delivery
            && !file_paths.insert((adapter, path.clone()))
        {
            return Err(HubModelConfigurationError::InvalidCredentialDelivery);
        }
        let parsed = CredentialProfile {
            name: Arc::clone(&name),
            adapter,
            billing_kind,
            delivery,
        };
        if profiles.insert(Arc::clone(&name), parsed).is_some() {
            return Err(HubModelConfigurationError::DuplicateCredentialProfile {
                credential_profile: name,
            });
        }
    }
    Ok(profiles)
}

/// Parses the complete `[[credential_pools]]` array against declared profiles.
pub(crate) fn parse_credential_pools(
    item: Option<&Item>,
    profiles: &HashMap<Arc<str>, CredentialProfile>,
) -> Result<HashMap<Arc<str>, CredentialPool>, HubModelConfigurationError> {
    let tables = item
        .and_then(Item::as_array_of_tables)
        .ok_or(HubModelConfigurationError::MissingCredentialPools)?;
    if tables.is_empty() {
        return Err(HubModelConfigurationError::MissingCredentialPools);
    }
    let mut pools = HashMap::with_capacity(tables.len());
    for pool in tables {
        let mut allowed = vec![
            "name",
            "members",
            "tie_break",
            "on_pool_exhausted",
            "headroom_reserve_percent",
        ];
        allowed.extend(CredentialPoolTrigger::ALL.map(CredentialPoolTrigger::key));
        reject_unknown_fields(pool, &allowed)?;
        let name = validated_credential_catalog_name(required_string(pool, "name")?)?;
        let members = parse_pool_members(&name, pool.get("members"), profiles)?;
        let adapter = pool_adapter(&name, &members, profiles)?;
        let parsed = CredentialPool {
            name: Arc::clone(&name),
            adapter,
            members,
            tie_break: CredentialPoolTieBreak::parse(required_string(pool, "tie_break")?)?,
            on_pool_exhausted: CredentialPoolExhaustion::parse(required_string(
                pool,
                "on_pool_exhausted",
            )?)?,
            headroom_reserve_percent: parse_pool_headroom_reserve_percent(
                pool.get("headroom_reserve_percent"),
            )?,
            quota_exhausted: parse_trigger_action(pool, CredentialPoolTrigger::QuotaExhausted)?,
            rate_limited: parse_trigger_action(pool, CredentialPoolTrigger::RateLimited)?,
            overloaded: parse_trigger_action(pool, CredentialPoolTrigger::Overloaded)?,
            credential_rejected: parse_trigger_action(
                pool,
                CredentialPoolTrigger::CredentialRejected,
            )?,
            headroom_low: parse_trigger_action(pool, CredentialPoolTrigger::HeadroomLow)?,
        };
        reject_unobserved_capacity_policy(&parsed)?;
        reject_unprovable_substitution(&parsed)?;
        if pools.insert(Arc::clone(&name), parsed).is_some() {
            return Err(HubModelConfigurationError::DuplicateCredentialPool {
                credential_pool: name,
            });
        }
    }
    Ok(pools)
}

fn parse_pool_members(
    pool: &Arc<str>,
    item: Option<&Item>,
    profiles: &HashMap<Arc<str>, CredentialProfile>,
) -> Result<Vec<CredentialPoolMember>, HubModelConfigurationError> {
    let values = item
        .and_then(Item::as_array)
        .ok_or(HubModelConfigurationError::InvalidField)?;
    if values.is_empty() {
        return Err(HubModelConfigurationError::EmptyCredentialPool {
            credential_pool: Arc::clone(pool),
        });
    }
    if values.len() > MAX_CREDENTIAL_POOL_MEMBERS {
        return Err(HubModelConfigurationError::InvalidCredentialPoolPolicy);
    }
    let mut declared = HashSet::with_capacity(values.len());
    let mut members = Vec::with_capacity(values.len());
    for value in values {
        let member = value
            .as_inline_table()
            .ok_or(HubModelConfigurationError::InvalidField)?;
        reject_unknown_member_fields(member)?;
        let profile = validated_credential_catalog_name(
            member
                .get("profile")
                .and_then(|value| value.as_str())
                .ok_or(HubModelConfigurationError::InvalidField)?,
        )?;
        if !profiles.contains_key(&profile) {
            return Err(HubModelConfigurationError::UnknownPoolMemberProfile {
                credential_pool: Arc::clone(pool),
                credential_profile: profile,
            });
        }
        if !declared.insert(Arc::clone(&profile)) {
            return Err(HubModelConfigurationError::DuplicatePoolMember {
                credential_pool: Arc::clone(pool),
                credential_profile: profile,
            });
        }
        let priority = member
            .get("priority")
            .and_then(|value| value.as_integer())
            .and_then(|value| u32::try_from(value).ok())
            .and_then(NonZeroU32::new)
            .ok_or_else(|| HubModelConfigurationError::InvalidMemberPriority {
                credential_pool: Arc::clone(pool),
            })?;
        members.push(CredentialPoolMember {
            profile,
            priority,
            headroom_reserve_percent: parse_headroom_reserve_percent(
                member.get("headroom_reserve_percent"),
            )?,
        });
    }
    Ok(members)
}

fn validated_credential_catalog_name(value: &str) -> Result<Arc<str>, HubModelConfigurationError> {
    let name = validated_name(value)?;
    if name.len() > MAX_CREDENTIAL_CATALOG_NAME_UTF8_BYTES {
        Err(HubModelConfigurationError::InvalidField)
    } else {
        Ok(name)
    }
}

fn reject_unknown_member_fields(member: &InlineTable) -> Result<(), HubModelConfigurationError> {
    let allowed = ["profile", "priority", "headroom_reserve_percent"];
    if member.iter().any(|(key, _)| !allowed.contains(&key)) {
        Err(HubModelConfigurationError::UnknownField)
    } else {
        Ok(())
    }
}

fn pool_adapter(
    pool: &Arc<str>,
    members: &[CredentialPoolMember],
    profiles: &HashMap<Arc<str>, CredentialProfile>,
) -> Result<ModelAdapter, HubModelConfigurationError> {
    let adapters = members
        .iter()
        .filter_map(|member| profiles.get(&member.profile))
        .map(CredentialProfile::adapter)
        .collect::<HashSet<_>>();
    let mut adapters = adapters.into_iter();
    match (adapters.next(), adapters.next()) {
        (Some(adapter), None) => Ok(adapter),
        _ => Err(HubModelConfigurationError::ConflictingPoolAdapters {
            credential_pool: Arc::clone(pool),
        }),
    }
}

fn parse_trigger_action(
    pool: &Table,
    trigger: CredentialPoolTrigger,
) -> Result<CredentialPoolAction, HubModelConfigurationError> {
    let Some(configured) = pool.get(trigger.key()) else {
        return Ok(CredentialPoolAction::Stay);
    };
    let action = CredentialPoolAction::parse(
        configured
            .as_str()
            .ok_or(HubModelConfigurationError::InvalidField)?,
    )?;
    if action == CredentialPoolAction::SwitchNow && !trigger.admits_switch_now() {
        return Err(
            HubModelConfigurationError::InadmissibleCredentialPoolAction {
                trigger: Arc::from(trigger.key()),
            },
        );
    }
    Ok(action)
}

fn parse_headroom_reserve_percent(
    value: Option<&toml_edit::Value>,
) -> Result<Option<u8>, HubModelConfigurationError> {
    value
        .map(|value| {
            let percent = value
                .as_integer()
                .ok_or(HubModelConfigurationError::InvalidField)?;
            u8::try_from(percent)
                .ok()
                .filter(|_| percent <= MAX_HEADROOM_RESERVE_PERCENT)
                .ok_or(HubModelConfigurationError::InvalidHeadroomReserve)
        })
        .transpose()
}

fn parse_pool_headroom_reserve_percent(
    item: Option<&Item>,
) -> Result<Option<u8>, HubModelConfigurationError> {
    match item {
        None => Ok(None),
        Some(item) => parse_headroom_reserve_percent(Some(
            item.as_value()
                .ok_or(HubModelConfigurationError::InvalidField)?,
        )),
    }
}

/// Rejects `switch_now` for any trigger whose cause this pool's adapter cannot
/// prove the provider refused before accepting, and actions on opaque Codex failures.
///
/// Only a decoded native error envelope supplies that proof, and each adapter
/// names native tokens for only some causes, so the check is per adapter *and*
/// per trigger. A pool's members already agree on one adapter, so it is exact
/// per pool. Left admitted, the action would read as failover the deployment
/// does not have while every matching response terminalized exactly as `stay`.
fn reject_unprovable_substitution(pool: &CredentialPool) -> Result<(), HubModelConfigurationError> {
    if pool.adapter == ModelAdapter::CodexCli {
        for (trigger, action) in [
            ("on_quota_exhausted", pool.quota_exhausted),
            ("on_rate_limited", pool.rate_limited),
            ("on_overloaded", pool.overloaded),
            ("on_credential_rejected", pool.credential_rejected),
        ] {
            if action != CredentialPoolAction::Stay {
                return Err(
                    HubModelConfigurationError::InadmissibleCredentialPoolAction {
                        trigger: Arc::from(trigger),
                    },
                );
            }
        }
    }
    let unprovable = [
        (pool.quota_exhausted, AvailabilityCause::QuotaExhausted),
        (pool.rate_limited, AvailabilityCause::RateLimited),
        (pool.overloaded, AvailabilityCause::Overloaded),
    ]
    .into_iter()
    .any(|(action, cause)| {
        action == CredentialPoolAction::SwitchNow && !pool.adapter.proves_non_acceptance(cause)
    });
    if unprovable {
        return Err(HubModelConfigurationError::UnprovableSubstitutionPolicy {
            credential_pool: Arc::clone(&pool.name),
        });
    }
    Ok(())
}

/// Refuses configuration whose effect depends on remaining provider capacity
/// this build's adapters do not report.
///
/// A reserve that silently never fires would read as protection the deployment
/// does not have, so it is a startup failure instead. The grammar still admits
/// the keys, so an adapter that later supplies the observation needs no
/// configuration contract change.
fn reject_unobserved_capacity_policy(
    pool: &CredentialPool,
) -> Result<(), HubModelConfigurationError> {
    if pool.adapter.reports_remaining_capacity() {
        return Ok(());
    }
    let reserves_headroom = pool.headroom_reserve_percent.is_some()
        || pool
            .members
            .iter()
            .any(|member| member.headroom_reserve_percent.is_some());
    let reacts_to_headroom = CredentialPoolTrigger::HeadroomLow.needs_observed_capacity()
        && pool.headroom_low != CredentialPoolAction::Stay;
    if reserves_headroom || pool.tie_break.needs_observed_capacity() || reacts_to_headroom {
        return Err(HubModelConfigurationError::UnobservedCapacityPolicy {
            credential_pool: Arc::clone(&pool.name),
        });
    }
    Ok(())
}
