use std::{
    collections::{BTreeSet, HashMap},
    error::Error,
    fmt,
};

use rust_decimal::Decimal;
use serde::Deserialize;
use url::Url;

use crate::GENERATED_PROJECTION_BANNER;

const EXPECTED_SCHEMA_VERSION: u32 = 2;

/// First-party provider represented by this initial reference-data slice.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[serde(rename_all = "snake_case")]
pub enum Provider {
    /// OpenAI.
    Openai,
    /// Anthropic.
    Anthropic,
}

impl Provider {
    fn label(self) -> &'static str {
        match self {
            Self::Openai => "OpenAI",
            Self::Anthropic => "Anthropic",
        }
    }
}

/// Commercial surface on which an identity was observed or a rate applies.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[serde(rename_all = "snake_case")]
pub enum CommercialChannel {
    /// Direct, synchronous first-party API use.
    Api,
    /// First-party asynchronous batch API use.
    BatchApi,
    /// ChatGPT subscription or consumer-product use.
    ChatgptSubscription,
    /// Codex app or cloud subscription use.
    CodexSubscription,
    /// Codex CLI authenticated through a subscription.
    CodexCliSubscription,
    /// Claude consumer-product subscription use.
    ClaudeSubscription,
    /// Claude Code authenticated through a subscription.
    ClaudeCodeSubscription,
}

/// How usage on a commercial channel was actually billed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ActualBillingKind {
    /// Provider API usage metered directly by the published commercial API schedule.
    ApiMetered,
    /// Consumer-product usage covered by a subscription or its included credits.
    Subscription,
}

impl ActualBillingKind {
    fn label(self) -> &'static str {
        match self {
            Self::ApiMetered => "api_metered",
            Self::Subscription => "subscription",
        }
    }
}

impl CommercialChannel {
    /// Classifies actual billing independently from any retrospective equivalent API cost.
    pub fn actual_billing_kind(self) -> ActualBillingKind {
        if self.is_api_rate_channel() {
            ActualBillingKind::ApiMetered
        } else {
            ActualBillingKind::Subscription
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Api => "api",
            Self::BatchApi => "batch_api",
            Self::ChatgptSubscription => "chatgpt_subscription",
            Self::CodexSubscription => "codex_subscription",
            Self::CodexCliSubscription => "codex_cli_subscription",
            Self::ClaudeSubscription => "claude_subscription",
            Self::ClaudeCodeSubscription => "claude_code_subscription",
        }
    }

    fn is_api_rate_channel(self) -> bool {
        matches!(self, Self::Api | Self::BatchApi)
    }

    fn consumer_provider(self) -> Option<Provider> {
        match self {
            Self::Api | Self::BatchApi => None,
            Self::ChatgptSubscription | Self::CodexSubscription | Self::CodexCliSubscription => {
                Some(Provider::Openai)
            }
            Self::ClaudeSubscription | Self::ClaudeCodeSubscription => Some(Provider::Anthropic),
        }
    }
}

/// What stability semantics a reference model identifier has.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[serde(rename_all = "snake_case")]
pub enum ModelIdentityKind {
    /// A family concept that is not itself an invocable identifier.
    Family,
    /// A provider alias whose target may move.
    RollingAlias,
    /// A dated, pinned provider snapshot.
    DatedSnapshot,
    /// A provider's pinned release identifier without an embedded date.
    PinnedRelease,
}

impl ModelIdentityKind {
    fn label(self) -> &'static str {
        match self {
            Self::Family => "family",
            Self::RollingAlias => "rolling_alias",
            Self::DatedSnapshot => "dated_snapshot",
            Self::PinnedRelease => "pinned_release",
        }
    }
}

/// Strength of a consumer/subscription identity mapping.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[serde(rename_all = "snake_case")]
pub enum MappingQuality {
    /// The observed identity is the provider's exact model-identifier spelling.
    Exact,
    /// First-party evidence directly relates the consumer identity to this API model.
    Strong,
    /// Only the model family can be defended.
    FamilyOnly,
    /// A useful but non-identical API analogue.
    Approximate,
    /// No defensible normalized identity is known.
    Unknown,
}

impl MappingQuality {
    fn label(self) -> &'static str {
        match self {
            Self::Exact => "exact",
            Self::Strong => "strong",
            Self::FamilyOnly => "family_only",
            Self::Approximate => "approximate",
            Self::Unknown => "unknown",
        }
    }
}

/// Evidence confidence, kept separate from mapping quality.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    /// Direct, precise first-party evidence.
    High,
    /// First-party evidence with a material qualification or inference.
    Medium,
    /// Weak or incomplete corroboration retained with an explicit limitation.
    Low,
}

impl Confidence {
    fn label(self) -> &'static str {
        match self {
            Self::High => "high",
            Self::Medium => "medium",
            Self::Low => "low",
        }
    }
}

/// A separately billable rate dimension.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[serde(rename_all = "snake_case")]
pub enum RateDimension {
    /// Ordinary input tokens.
    Input,
    /// Generated output tokens, including provider-billed reasoning tokens.
    Output,
    /// Reused or cache-hit input tokens.
    CachedInput,
    /// Input tokens written into a provider cache.
    CacheWrite,
    /// A historical undifferentiated input-plus-output token rate.
    CombinedTokens,
    /// A non-token request or operation fee.
    Operation,
}

impl RateDimension {
    fn label(self) -> &'static str {
        match self {
            Self::Input => "input",
            Self::Output => "output",
            Self::CachedInput => "cached_input",
            Self::CacheWrite => "cache_write",
            Self::CombinedTokens => "combined_tokens",
            Self::Operation => "operation",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[serde(rename_all = "snake_case")]
pub enum DatePrecision {
    /// Provider evidence establishes the effective calendar day.
    ExactDay,
    /// Evidence establishes observations bracketing an otherwise unknown effective day.
    ObservationWindow,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
struct DateWindow {
    precision: DatePrecision,
    effective_from: Option<String>,
    effective_until: Option<String>,
    first_observed_new_rate: String,
    last_observed_old_rate: Option<String>,
}

impl DateWindow {
    fn contains(&self, date: &str) -> bool {
        match self.precision {
            DatePrecision::ExactDay => {
                self.effective_from
                    .as_deref()
                    .is_none_or(|from| date >= from)
                    && self
                        .effective_until
                        .as_deref()
                        .is_none_or(|until| date < until)
            }
            DatePrecision::ObservationWindow => {
                date >= self.first_observed_new_rate.as_str()
                    && self
                        .effective_until
                        .as_deref()
                        .is_none_or(|until| date < until)
            }
        }
    }

    fn label(&self) -> String {
        let start = self
            .effective_from
            .as_deref()
            .unwrap_or(self.first_observed_new_rate.as_str());
        let end = self.effective_until.as_deref().unwrap_or("open");
        match self.precision {
            DatePrecision::ExactDay => format!("{start}..{end}"),
            DatePrecision::ObservationWindow => format!("observed {start}..{end}"),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
struct Source {
    id: String,
    provider: Provider,
    title: String,
    url: String,
    published: Option<String>,
    retrieved: String,
    evidence: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
struct CapabilityEvidence {
    capability: String,
    support: CapabilitySupport,
    source_ids: Vec<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
enum CapabilitySupport {
    Supported,
    Unsupported,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
struct ReferenceModel {
    id: String,
    provider: Provider,
    provider_model_id: Option<String>,
    display_name: String,
    identity_kind: ModelIdentityKind,
    family: Option<String>,
    priced_as: Option<String>,
    available_from: Option<String>,
    available_until: Option<String>,
    capabilities: Vec<CapabilityEvidence>,
    source_ids: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[serde(deny_unknown_fields)]
struct RateQualifier {
    service_tier: Option<String>,
    context_band: Option<String>,
    cache_ttl: Option<String>,
    region: Option<String>,
}

impl RateQualifier {
    fn label(&self) -> String {
        let mut labels = Vec::new();
        if let Some(value) = &self.service_tier {
            labels.push(format!("tier={value}"));
        }
        if let Some(value) = &self.context_band {
            labels.push(format!("context={value}"));
        }
        if let Some(value) = &self.cache_ttl {
            labels.push(format!("ttl={value}"));
        }
        if let Some(value) = &self.region {
            labels.push(format!("region={value}"));
        }
        if labels.is_empty() {
            String::from("-")
        } else {
            labels.join(", ")
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
struct OriginalAmount {
    amount: String,
    currency: String,
    unit: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
struct Rate {
    dimension: RateDimension,
    qualifier: RateQualifier,
    usd_per_million_tokens: Option<String>,
    original: OriginalAmount,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
struct RateSet {
    id: String,
    provider: Provider,
    commercial_channel: CommercialChannel,
    model_id: String,
    window: DateWindow,
    rates: Vec<Rate>,
    confidence: Confidence,
    source_ids: Vec<String>,
    limitations: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
struct ConsumerMapping {
    id: String,
    provider: Provider,
    commercial_channel: CommercialChannel,
    observed_identity: String,
    normalized_model: Option<String>,
    window: DateWindow,
    quality: MappingQuality,
    confidence: Confidence,
    source_ids: Vec<String>,
    limitations: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
struct ResearchGap {
    id: String,
    provider: Provider,
    question: String,
    consequence: String,
    status: String,
    source_ids: Vec<String>,
}

/// The date through which each provider's evidence was audited.
///
/// The horizon is per provider because an audit is per provider: recovering a
/// new Anthropic launch source says nothing about whether a mutable OpenAI
/// price page still reads the way it did at its own retrieval. One shared date
/// would let evidence for one provider silently extend every other provider's
/// answers past what was actually checked.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EvidenceHorizons {
    openai: String,
    anthropic: String,
}

impl EvidenceHorizons {
    /// Borrows the date through which this provider's evidence was audited.
    fn for_provider(&self, provider: Provider) -> &str {
        match provider {
            Provider::Openai => &self.openai,
            Provider::Anthropic => &self.anthropic,
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCatalog {
    schema_version: u32,
    verified_through: EvidenceHorizons,
    sources: Vec<Source>,
    models: Vec<ReferenceModel>,
    rate_sets: Vec<RateSet>,
    consumer_mappings: Vec<ConsumerMapping>,
    research_gaps: Vec<ResearchGap>,
}

/// A validated reference catalog with no runtime routing authority.
#[derive(Clone, Debug)]
pub struct Catalog {
    raw: RawCatalog,
    model_index: HashMap<String, usize>,
}

/// Why reference data could not be parsed or admitted.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CatalogError {
    detail: String,
}

impl CatalogError {
    fn new(detail: impl Into<String>) -> Self {
        Self {
            detail: detail.into(),
        }
    }
}

impl fmt::Display for CatalogError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "invalid reference catalog: {}", self.detail)
    }
}

impl Error for CatalogError {}

/// One comparable rate dimension used by a dated lookup.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedRate {
    /// Provider billing dimension.
    pub dimension: RateDimension,
    /// Material provider qualifier, such as cache TTL or service tier.
    pub qualifier: String,
    /// Exact decimal USD per million tokens, absent only for non-token fees.
    pub usd_per_million_tokens: Option<Decimal>,
    /// Amount and unit as the provider published them.
    pub original_amount: String,
    /// Currency as the provider published it.
    pub original_currency: String,
    /// Unit as the provider published it.
    pub original_unit: String,
}

/// Structured effective or observation boundary for a resolved rate set.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedDateWindow {
    /// Whether the start is a known effective day or only a first observation.
    pub precision: DatePrecision,
    /// Exact effective start when known.
    pub effective_from: Option<String>,
    /// Exclusive effective end when known.
    pub effective_until: Option<String>,
    /// First date on which the new rate is supported by admitted evidence.
    pub first_observed_new_rate: String,
    /// Last date on which the preceding rate is supported, when relevant.
    pub last_observed_old_rate: Option<String>,
}

/// One exact rate-set match used by a dated lookup.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedRateSet {
    /// Stable rate-set identifier.
    pub id: String,
    /// Rate dimensions and exact decimal USD-per-million values when comparable.
    pub rates: Vec<ResolvedRate>,
    /// Effective or observation interval label.
    pub interval: String,
    /// Structured effective or observation boundaries.
    pub window: ResolvedDateWindow,
    /// Confidence of the price evidence.
    pub confidence: Confidence,
    /// Source identifiers supporting the record.
    pub source_ids: Vec<String>,
    /// Preserved assumptions and limitations.
    pub limitations: Vec<String>,
}

/// Result of looking up published first-party API rates for a resolved model.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PriceResolution {
    /// One or more compatible rate sets apply on the requested date.
    Resolved(Vec<ResolvedRateSet>),
    /// The date falls between the last old-rate observation and first new-rate observation.
    TransitionAmbiguous {
        /// Last old-rate observation before the unresolved interval.
        last_observed_old_rate: String,
        /// First new-rate observation after the unresolved interval.
        first_observed_new_rate: String,
        /// Candidate rate-set identifiers bracketing the transition.
        candidate_rate_set_ids: Vec<String>,
    },
    /// No comparable published price was recorded; this is never zero.
    Unknown,
}

impl PriceResolution {
    /// Returns exact matching rate sets, or `None` for ambiguity/unknown.
    pub fn resolved_rate_sets(&self) -> Option<&[ResolvedRateSet]> {
        match self {
            Self::Resolved(rate_sets) => Some(rate_sets),
            Self::TransitionAmbiguous { .. } | Self::Unknown => None,
        }
    }
}

/// Deterministic model-and-price resolution outcome.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReferenceResolution {
    /// A defensible model resolution, with price resolution kept separate.
    Resolved {
        /// Canonical reference-model identifier.
        model_id: String,
        /// Identity-mapping quality.
        mapping_quality: MappingQuality,
        /// Confidence in the identity mapping.
        mapping_confidence: Confidence,
        /// First-party API pricing at the requested date.
        price: PriceResolution,
        /// Source identifiers supporting the identity mapping.
        mapping_source_ids: Vec<String>,
        /// Preserved assumptions and limitations.
        limitations: Vec<String>,
    },
    /// Only a family-level identity is defensible; no exact snapshot is guessed.
    FamilyOnly {
        /// Canonical family reference.
        family_id: String,
        /// Confidence in the family association.
        mapping_confidence: Confidence,
        /// Source identifiers supporting the family association.
        mapping_source_ids: Vec<String>,
        /// Preserved assumptions and limitations.
        limitations: Vec<String>,
    },
    /// More than one model remains defensible.
    Ambiguous {
        /// Canonical candidate references in stable order.
        candidate_model_ids: Vec<String>,
    },
    /// No model mapping is defensible.
    Unknown,
}

impl ReferenceResolution {
    /// Returns the exact normalized model, or `None` for family/ambiguous/unknown results.
    pub fn resolved_model_id(&self) -> Option<&str> {
        match self {
            Self::Resolved { model_id, .. } => Some(model_id),
            Self::FamilyOnly { .. } | Self::Ambiguous { .. } | Self::Unknown => None,
        }
    }

    /// Returns resolved pricing, or `None` when no exact model was selected.
    pub fn price(&self) -> Option<&PriceResolution> {
        match self {
            Self::Resolved { price, .. } => Some(price),
            Self::FamilyOnly { .. } | Self::Ambiguous { .. } | Self::Unknown => None,
        }
    }

    /// Returns the mapping quality for an exactly normalized model.
    pub fn resolved_mapping_quality(&self) -> Option<MappingQuality> {
        match self {
            Self::Resolved {
                mapping_quality, ..
            } => Some(*mapping_quality),
            Self::FamilyOnly { .. } | Self::Ambiguous { .. } | Self::Unknown => None,
        }
    }
}

impl ResolvedRateSet {
    /// Finds one rate by dimension and rendered qualifier.
    pub fn rate(&self, dimension: RateDimension, qualifier: &str) -> Option<&ResolvedRate> {
        self.rates
            .iter()
            .find(|rate| rate.dimension == dimension && rate.qualifier == qualifier)
    }
}

impl Catalog {
    /// Parses JSON and applies cross-record validation before returning data.
    pub fn from_json(json: &str) -> Result<Self, CatalogError> {
        let raw: RawCatalog = serde_json::from_str(json)
            .map_err(|error| CatalogError::new(format!("JSON does not match schema: {error}")))?;
        let model_index = validate(&raw)?;
        Ok(Self { raw, model_index })
    }

    /// Date through which the bundled evidence audit was performed for one
    /// provider. Each provider carries its own horizon.
    pub fn verified_through(&self, provider: Provider) -> &str {
        self.raw.verified_through.for_provider(provider)
    }

    /// Number of reference identities. This count has no runtime meaning.
    pub fn model_count(&self) -> usize {
        self.raw.models.len()
    }

    /// Resolves one observed identity without consulting any runtime adapter or catalog.
    pub fn resolve(
        &self,
        provider: Provider,
        model_hint: &str,
        date: &str,
        commercial_channel: CommercialChannel,
    ) -> Result<ReferenceResolution, CatalogError> {
        validate_date(date, "query date")?;
        if date > self.raw.verified_through.for_provider(provider) {
            return Ok(ReferenceResolution::Unknown);
        }
        if commercial_channel.is_api_rate_channel() {
            let candidates = self
                .raw
                .models
                .iter()
                .filter(|model| model.provider == provider)
                .filter(|model| model.provider_model_id.as_deref() == Some(model_hint))
                .filter(|model| model_available(model, date))
                .collect::<Vec<_>>();
            return self.resolve_direct_candidates(candidates, date, commercial_channel);
        }

        let mappings = self
            .raw
            .consumer_mappings
            .iter()
            .filter(|mapping| mapping.provider == provider)
            .filter(|mapping| mapping.commercial_channel == commercial_channel)
            .filter(|mapping| mapping.observed_identity == model_hint)
            .filter(|mapping| mapping.window.contains(date))
            .collect::<Vec<_>>();
        if mappings.is_empty() {
            return Ok(ReferenceResolution::Unknown);
        }
        if mappings
            .iter()
            .any(|mapping| mapping.normalized_model.is_none())
        {
            return Ok(ReferenceResolution::Unknown);
        }
        let normalized = mappings
            .iter()
            .filter_map(|mapping| mapping.normalized_model.as_deref())
            .collect::<BTreeSet<_>>();
        if normalized.len() > 1 {
            return Ok(ReferenceResolution::Ambiguous {
                candidate_model_ids: normalized.into_iter().map(String::from).collect(),
            });
        }
        let Some(model_id) = normalized.first().copied() else {
            return Ok(ReferenceResolution::Unknown);
        };
        let mapping = mappings
            .iter()
            .find(|mapping| mapping.normalized_model.as_deref() == Some(model_id))
            .ok_or_else(|| CatalogError::new("mapping candidate disappeared"))?;
        if mapping.quality == MappingQuality::FamilyOnly {
            return Ok(ReferenceResolution::FamilyOnly {
                family_id: String::from(model_id),
                mapping_confidence: mapping.confidence,
                mapping_source_ids: mapping.source_ids.clone(),
                limitations: mapping.limitations.clone(),
            });
        }
        let price = self.resolve_prices(model_id, date, CommercialChannel::Api)?;
        Ok(ReferenceResolution::Resolved {
            model_id: String::from(model_id),
            mapping_quality: mapping.quality,
            mapping_confidence: mapping.confidence,
            price,
            mapping_source_ids: mapping.source_ids.clone(),
            limitations: mapping.limitations.clone(),
        })
    }

    fn resolve_direct_candidates(
        &self,
        candidates: Vec<&ReferenceModel>,
        date: &str,
        channel: CommercialChannel,
    ) -> Result<ReferenceResolution, CatalogError> {
        match candidates.as_slice() {
            [] => Ok(ReferenceResolution::Unknown),
            [model] => {
                let price = self.resolve_prices(&model.id, date, channel)?;
                Ok(ReferenceResolution::Resolved {
                    model_id: model.id.clone(),
                    mapping_quality: MappingQuality::Exact,
                    mapping_confidence: Confidence::High,
                    price,
                    mapping_source_ids: model.source_ids.clone(),
                    limitations: Vec::new(),
                })
            }
            _ => Ok(ReferenceResolution::Ambiguous {
                candidate_model_ids: candidates.iter().map(|model| model.id.clone()).collect(),
            }),
        }
    }

    fn resolve_prices(
        &self,
        model_id: &str,
        date: &str,
        channel: CommercialChannel,
    ) -> Result<PriceResolution, CatalogError> {
        let model = self.model(model_id)?;
        let pricing_model = model.priced_as.as_deref().unwrap_or(model_id);
        let applicable = self
            .raw
            .rate_sets
            .iter()
            .filter(|set| set.model_id == pricing_model)
            .filter(|set| set.commercial_channel == channel)
            .filter(|set| set.window.contains(date))
            .collect::<Vec<_>>();
        if !applicable.is_empty() {
            let mut resolved = applicable
                .into_iter()
                .map(resolve_rate_set)
                .collect::<Result<Vec<_>, _>>()?;
            resolved.sort_by(|left, right| left.id.cmp(&right.id));
            return Ok(PriceResolution::Resolved(resolved));
        }

        let transition = self
            .raw
            .rate_sets
            .iter()
            .filter(|new| new.model_id == pricing_model)
            .filter(|new| new.commercial_channel == channel)
            .filter(|new| new.window.first_observed_new_rate.as_str() > date)
            .filter(|new| {
                new.window
                    .last_observed_old_rate
                    .as_deref()
                    .is_some_and(|observed| observed < date)
            })
            .flat_map(|new| {
                self.raw
                    .rate_sets
                    .iter()
                    .filter(|old| old.model_id == pricing_model)
                    .filter(|old| old.commercial_channel == channel)
                    .filter(|old| {
                        old.window
                            .effective_until
                            .as_deref()
                            .is_some_and(|until| until <= date)
                    })
                    .filter(|old| {
                        new.window
                            .last_observed_old_rate
                            .as_deref()
                            .is_some_and(|observed| old.window.contains(observed))
                    })
                    .filter(|old| rate_sets_share_dimension_and_qualifier(old, new))
                    .map(move |old| (new, old))
            })
            .min_by(|(left_new, left_old), (right_new, right_old)| {
                left_new
                    .window
                    .first_observed_new_rate
                    .cmp(&right_new.window.first_observed_new_rate)
                    .then_with(|| {
                        right_old
                            .window
                            .effective_until
                            .cmp(&left_old.window.effective_until)
                    })
                    .then_with(|| left_new.id.cmp(&right_new.id))
                    .then_with(|| left_old.id.cmp(&right_old.id))
            });
        if let Some((new, old)) = transition {
            return Ok(PriceResolution::TransitionAmbiguous {
                last_observed_old_rate: new
                    .window
                    .last_observed_old_rate
                    .clone()
                    .ok_or_else(|| CatalogError::new("transition boundary disappeared"))?,
                first_observed_new_rate: new.window.first_observed_new_rate.clone(),
                candidate_rate_set_ids: vec![old.id.clone(), new.id.clone()],
            });
        }
        Ok(PriceResolution::Unknown)
    }

    fn model(&self, model_id: &str) -> Result<&ReferenceModel, CatalogError> {
        self.model_index
            .get(model_id)
            .map(|index| &self.raw.models[*index])
            .ok_or_else(|| CatalogError::new(format!("unknown model {model_id}")))
    }

    pub(crate) fn render_models(&self) -> String {
        let mut models = self.raw.models.iter().collect::<Vec<_>>();
        models.sort_by(|left, right| left.id.cmp(&right.id));
        let mut output = String::from(GENERATED_PROJECTION_BANNER);
        output.push_str("# Reference models and recorded capabilities\n\n");
        output.push_str(
            "These identities are non-routable reference data. An absent capability is\nunknown, not unsupported.\n\n```text\n",
        );
        output.push_str(
            "| Provider | Reference ID | Provider model ID | Kind | Family | Capabilities |\n",
        );
        output.push_str("| --- | --- | --- | --- | --- | --- |\n");
        for model in models {
            let capabilities = model
                .capabilities
                .iter()
                .map(|capability| {
                    let marker = match capability.support {
                        CapabilitySupport::Supported => "+",
                        CapabilitySupport::Unsupported => "-",
                    };
                    format!("{marker}{}", capability.capability)
                })
                .collect::<Vec<_>>()
                .join(", ");
            output.push_str(&format!(
                "| {} | `{}` | {} | {} | {} | {} |\n",
                model.provider.label(),
                model.id,
                code_or_dash(model.provider_model_id.as_deref()),
                model.identity_kind.label(),
                code_or_dash(model.family.as_deref()),
                if capabilities.is_empty() {
                    "unknown"
                } else {
                    &capabilities
                },
            ));
        }
        output.push_str("```\n");
        output
    }

    pub(crate) fn render_historical_pricing(&self) -> String {
        let mut sets = self.raw.rate_sets.iter().collect::<Vec<_>>();
        sets.sort_by(|left, right| left.id.cmp(&right.id));
        let mut output = String::from(GENERATED_PROJECTION_BANNER);
        output.push_str("# Historical first-party API pricing\n\n");
        output.push_str(
            "Amounts are exact decimal USD per one million tokens unless the original unit\nsays otherwise.\n\n```text\n",
        );
        output.push_str("| Provider | Rate set | Channel | Model | Interval | Dimension | Qualifier | USD / MTok | Original | Confidence | Sources | Limitations |\n");
        output.push_str(
            "| --- | --- | --- | --- | --- | --- | --- | ---: | --- | --- | --- | --- |\n",
        );
        for set in sets {
            for rate in &set.rates {
                output.push_str(&format!(
                    "| {} | `{}` | {} | `{}` | {} | {} | {} | {} | {} {}/{} | {} | {} | {} |\n",
                    set.provider.label(),
                    set.id,
                    set.commercial_channel.label(),
                    set.model_id,
                    set.window.label(),
                    rate.dimension.label(),
                    rate.qualifier.label(),
                    rate.usd_per_million_tokens.as_deref().unwrap_or("n/a"),
                    rate.original.amount,
                    rate.original.currency,
                    rate.original.unit,
                    set.confidence.label(),
                    set.source_ids
                        .iter()
                        .map(|id| format!("`{id}`"))
                        .collect::<Vec<_>>()
                        .join(", "),
                    set.limitations.join(" "),
                ));
            }
        }
        output.push_str("```\n");
        output
    }

    pub(crate) fn render_consumer_equivalence(&self) -> String {
        let mut mappings = self.raw.consumer_mappings.iter().collect::<Vec<_>>();
        mappings.sort_by(|left, right| left.id.cmp(&right.id));
        let mut output = String::from(GENERATED_PROJECTION_BANNER);
        output.push_str("# Consumer/subscription-to-API equivalence\n\n");
        output.push_str(
            "Equivalent API cost is the estimated first-party API cost of the observed usage\nat the contemporaneous applicable published API rate. It is not the user's\nactual subscription charge.\n\n```text\n",
        );
        output.push_str("| Provider | Channel | Actual billing | Observed identity | Normalized reference | Interval | Quality | Confidence | Sources | Limitations |\n");
        output.push_str("| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |\n");
        for mapping in mappings {
            output.push_str(&format!(
                "| {} | {} | {} | `{}` | {} | {} | {} | {} | {} | {} |\n",
                mapping.provider.label(),
                mapping.commercial_channel.label(),
                mapping.commercial_channel.actual_billing_kind().label(),
                mapping.observed_identity,
                code_or_dash(mapping.normalized_model.as_deref()),
                mapping.window.label(),
                mapping.quality.label(),
                mapping.confidence.label(),
                mapping
                    .source_ids
                    .iter()
                    .map(|id| format!("`{id}`"))
                    .collect::<Vec<_>>()
                    .join(", "),
                mapping.limitations.join(" "),
            ));
        }
        output.push_str("```\n");
        output
    }

    pub(crate) fn render_sources(&self) -> String {
        let mut sources = self.raw.sources.iter().collect::<Vec<_>>();
        sources.sort_by(|left, right| left.id.cmp(&right.id));
        let mut output = String::from(GENERATED_PROJECTION_BANNER);
        output.push_str("# Source and provenance ledger\n\n");
        output.push_str("```text\n");
        output.push_str(
            "| ID | Provider | Published | Retrieved | First-party source | Evidence used |\n",
        );
        output.push_str("| --- | --- | --- | --- | --- | --- |\n");
        for source in sources {
            output.push_str(&format!(
                "| `{}` | {} | {} | {} | {} ({}) | {} |\n",
                source.id,
                source.provider.label(),
                source.published.as_deref().unwrap_or("not stated"),
                source.retrieved,
                source.title,
                source.url,
                source.evidence,
            ));
        }
        output.push_str("```\n");
        output
    }

    pub(crate) fn render_research_gaps(&self) -> String {
        let mut gaps = self.raw.research_gaps.iter().collect::<Vec<_>>();
        gaps.sort_by(|left, right| left.id.cmp(&right.id));
        let mut output = String::from(GENERATED_PROJECTION_BANNER);
        output.push_str("# Explicit research gaps\n\n");
        output.push_str(
            "Unknowns remain unknown; these records are not permission to infer missing rates\nor identities.\n\n```text\n",
        );
        output.push_str(
            "| ID | Provider | Status | Question | Accounting consequence | Sources checked |\n",
        );
        output.push_str("| --- | --- | --- | --- | --- | --- |\n");
        for gap in gaps {
            output.push_str(&format!(
                "| `{}` | {} | {} | {} | {} | {} |\n",
                gap.id,
                gap.provider.label(),
                gap.status,
                gap.question,
                gap.consequence,
                gap.source_ids
                    .iter()
                    .map(|id| format!("`{id}`"))
                    .collect::<Vec<_>>()
                    .join(", "),
            ));
        }
        output.push_str("```\n");
        output
    }
}

fn rate_sets_share_dimension_and_qualifier(left: &RateSet, right: &RateSet) -> bool {
    left.rates.iter().any(|left_rate| {
        right.rates.iter().any(|right_rate| {
            left_rate.dimension == right_rate.dimension
                && left_rate.qualifier == right_rate.qualifier
        })
    })
}

fn validate(raw: &RawCatalog) -> Result<HashMap<String, usize>, CatalogError> {
    if raw.schema_version != EXPECTED_SCHEMA_VERSION {
        return Err(CatalogError::new(format!(
            "schema_version must be {EXPECTED_SCHEMA_VERSION}"
        )));
    }
    validate_date(&raw.verified_through.openai, "verified_through openai")?;
    validate_date(
        &raw.verified_through.anthropic,
        "verified_through anthropic",
    )?;
    let source_ids = unique_ids(
        raw.sources.iter().map(|source| source.id.as_str()),
        "source",
    )?;
    for source in &raw.sources {
        validate_source(source)?;
        if source.retrieved.as_str() > raw.verified_through.for_provider(source.provider) {
            return Err(CatalogError::new(format!(
                "source {} was retrieved after its provider's verified_through",
                source.id
            )));
        }
    }
    let _model_ids = unique_ids(raw.models.iter().map(|model| model.id.as_str()), "model")?;
    let mut model_index = HashMap::new();
    let mut provider_spellings = BTreeSet::new();
    for (index, model) in raw.models.iter().enumerate() {
        validate_nonempty(&model.id, "model id")?;
        validate_nonempty(&model.display_name, "model display_name")?;
        if let Some(spelling) = &model.provider_model_id {
            validate_nonempty(spelling, "provider_model_id")?;
            if !provider_spellings.insert((model.provider, spelling.as_str())) {
                return Err(CatalogError::new(format!(
                    "duplicate provider model spelling {}",
                    spelling
                )));
            }
        }
        match (model.identity_kind, model.provider_model_id.is_some()) {
            (ModelIdentityKind::Family, true) => {
                return Err(CatalogError::new(format!(
                    "family model {} cannot have a provider spelling",
                    model.id
                )));
            }
            (ModelIdentityKind::Family, false) | (_, true) => {}
            (_, false) => {
                return Err(CatalogError::new(format!(
                    "non-family model {} requires a provider spelling",
                    model.id
                )));
            }
        }
        if let Some(date) = &model.available_from {
            validate_date(date, "model available_from")?;
        }
        if let Some(date) = &model.available_until {
            validate_date(date, "model available_until")?;
        }
        if let (Some(from), Some(until)) = (&model.available_from, &model.available_until)
            && from >= until
        {
            return Err(CatalogError::new(format!(
                "model {} has an empty or reversed availability window",
                model.id
            )));
        }
        validate_source_refs(&model.source_ids, &source_ids, &model.id)?;
        validate_source_providers(raw, model.provider, &model.source_ids, &model.id)?;
        let expected_prefix = match model.provider {
            Provider::Openai => "openai:",
            Provider::Anthropic => "anthropic:",
        };
        if !model.id.starts_with(expected_prefix) {
            return Err(CatalogError::new(format!(
                "model {} does not use its provider namespace",
                model.id
            )));
        }
        let mut capabilities = BTreeSet::new();
        for capability in &model.capabilities {
            validate_nonempty(&capability.capability, "capability")?;
            if !capabilities.insert(capability.capability.as_str()) {
                return Err(CatalogError::new(format!(
                    "model {} repeats capability {}",
                    model.id, capability.capability
                )));
            }
            validate_source_refs(&capability.source_ids, &source_ids, &model.id)?;
            validate_source_providers(raw, model.provider, &capability.source_ids, &model.id)?;
        }
        model_index.insert(model.id.clone(), index);
    }
    for model in &raw.models {
        validate_model_ref(raw, model.provider, model.family.as_deref(), &model.id)?;
        if let Some(family) = &model.family {
            let family = raw
                .models
                .iter()
                .find(|candidate| candidate.id == *family)
                .ok_or_else(|| CatalogError::new(format!("unknown family {family}")))?;
            if family.identity_kind != ModelIdentityKind::Family {
                return Err(CatalogError::new(format!(
                    "model {} family reference does not target a family",
                    model.id
                )));
            }
        }
        validate_model_ref(raw, model.provider, model.priced_as.as_deref(), &model.id)?;
        if let Some(priced_as) = &model.priced_as {
            let target = raw
                .models
                .iter()
                .find(|candidate| candidate.id == *priced_as)
                .ok_or_else(|| CatalogError::new(format!("unknown priced_as {priced_as}")))?;
            if target.priced_as.is_some() {
                return Err(CatalogError::new(format!(
                    "model {} has a transitive priced_as chain",
                    model.id
                )));
            }
            if target.identity_kind == ModelIdentityKind::Family {
                return Err(CatalogError::new(format!(
                    "model {} prices through a family rather than a model",
                    model.id
                )));
            }
        }
    }

    unique_ids(raw.rate_sets.iter().map(|set| set.id.as_str()), "rate set")?;
    for set in &raw.rate_sets {
        if !set.commercial_channel.is_api_rate_channel() {
            return Err(CatalogError::new(format!(
                "rate set {} uses a non-API commercial channel",
                set.id
            )));
        }
        let model = raw
            .models
            .iter()
            .find(|model| model.id == set.model_id)
            .ok_or_else(|| CatalogError::new(format!("rate set {} names unknown model", set.id)))?;
        if model.provider != set.provider {
            return Err(CatalogError::new(format!(
                "rate set {} crosses provider boundary",
                set.id
            )));
        }
        if model.identity_kind == ModelIdentityKind::Family {
            return Err(CatalogError::new(format!(
                "rate set {} prices a model family",
                set.id
            )));
        }
        validate_window(&set.window, &set.id)?;
        validate_source_refs(&set.source_ids, &source_ids, &set.id)?;
        validate_source_providers(raw, set.provider, &set.source_ids, &set.id)?;
        validate_limitations(set.confidence, &set.limitations, &set.id)?;
        let rate_start = set
            .window
            .effective_from
            .as_deref()
            .unwrap_or(set.window.first_observed_new_rate.as_str());
        if model
            .available_from
            .as_deref()
            .is_some_and(|available| rate_start < available)
        {
            return Err(CatalogError::new(format!(
                "rate set {} predates model availability",
                set.id
            )));
        }
        if let Some(model_until) = model.available_until.as_deref()
            && set
                .window
                .effective_until
                .as_deref()
                .is_none_or(|rate_until| rate_until > model_until)
        {
            return Err(CatalogError::new(format!(
                "rate set {} extends beyond model availability",
                set.id
            )));
        }
        if set.rates.is_empty() {
            return Err(CatalogError::new(format!(
                "rate set {} has no rates",
                set.id
            )));
        }
        let mut dimensions = BTreeSet::new();
        for rate in &set.rates {
            if !dimensions.insert((rate.dimension, rate.qualifier.clone())) {
                return Err(CatalogError::new(format!(
                    "rate set {} repeats a dimension and qualifier",
                    set.id
                )));
            }
            validate_rate(rate, &set.id)?;
        }
    }
    validate_rate_overlaps(&raw.rate_sets)?;

    unique_ids(
        raw.consumer_mappings
            .iter()
            .map(|mapping| mapping.id.as_str()),
        "consumer mapping",
    )?;
    for mapping in &raw.consumer_mappings {
        if mapping.commercial_channel.is_api_rate_channel() {
            return Err(CatalogError::new(format!(
                "consumer mapping {} uses an API commercial channel",
                mapping.id
            )));
        }
        if mapping.commercial_channel.consumer_provider() != Some(mapping.provider) {
            return Err(CatalogError::new(format!(
                "consumer mapping {} uses another provider's commercial channel",
                mapping.id
            )));
        }
        validate_nonempty(&mapping.observed_identity, "observed identity")?;
        validate_window(&mapping.window, &mapping.id)?;
        validate_source_refs(&mapping.source_ids, &source_ids, &mapping.id)?;
        validate_source_providers(raw, mapping.provider, &mapping.source_ids, &mapping.id)?;
        validate_limitations(mapping.confidence, &mapping.limitations, &mapping.id)?;
        validate_model_ref(
            raw,
            mapping.provider,
            mapping.normalized_model.as_deref(),
            &mapping.id,
        )?;
        if let Some(model_id) = &mapping.normalized_model {
            let model = raw
                .models
                .iter()
                .find(|model| model.id == *model_id)
                .ok_or_else(|| CatalogError::new(format!("unknown model {model_id}")))?;
            let mapping_start = mapping
                .window
                .effective_from
                .as_deref()
                .unwrap_or(mapping.window.first_observed_new_rate.as_str());
            if model
                .available_from
                .as_deref()
                .is_some_and(|available| mapping_start < available)
            {
                return Err(CatalogError::new(format!(
                    "mapping {} predates model availability",
                    mapping.id
                )));
            }
            if let Some(model_until) = model.available_until.as_deref()
                && (mapping_start >= model_until
                    || mapping
                        .window
                        .effective_until
                        .as_deref()
                        .is_none_or(|mapping_until| mapping_until > model_until))
            {
                return Err(CatalogError::new(format!(
                    "mapping {} extends beyond model availability",
                    mapping.id
                )));
            }
            if mapping.quality == MappingQuality::FamilyOnly
                && model.identity_kind != ModelIdentityKind::Family
            {
                return Err(CatalogError::new(format!(
                    "family-only mapping {} targets a non-family model",
                    mapping.id
                )));
            }
            if mapping.quality != MappingQuality::FamilyOnly
                && mapping.quality != MappingQuality::Unknown
                && model.identity_kind == ModelIdentityKind::Family
            {
                return Err(CatalogError::new(format!(
                    "non-family mapping {} stops at a model family",
                    mapping.id
                )));
            }
            if mapping.quality == MappingQuality::Exact
                && model.provider_model_id.as_deref() != Some(mapping.observed_identity.as_str())
            {
                return Err(CatalogError::new(format!(
                    "exact mapping {} does not use the target model's provider spelling",
                    mapping.id
                )));
            }
        }
        if mapping.quality == MappingQuality::Unknown && mapping.normalized_model.is_some() {
            return Err(CatalogError::new(format!(
                "unknown mapping {} cannot claim a normalized model",
                mapping.id
            )));
        }
        if mapping.quality != MappingQuality::Unknown && mapping.normalized_model.is_none() {
            return Err(CatalogError::new(format!(
                "mapping {} has quality but no normalized model",
                mapping.id
            )));
        }
    }
    validate_mapping_overlaps(&raw.consumer_mappings)?;
    unique_ids(
        raw.research_gaps.iter().map(|gap| gap.id.as_str()),
        "research gap",
    )?;
    for gap in &raw.research_gaps {
        validate_nonempty(&gap.question, "research gap question")?;
        validate_nonempty(&gap.consequence, "research gap consequence")?;
        validate_nonempty(&gap.status, "research gap status")?;
        validate_source_refs(&gap.source_ids, &source_ids, &gap.id)?;
        validate_source_providers(raw, gap.provider, &gap.source_ids, &gap.id)?;
    }
    Ok(model_index)
}

fn validate_source(source: &Source) -> Result<(), CatalogError> {
    validate_nonempty(&source.id, "source id")?;
    validate_nonempty(&source.title, "source title")?;
    validate_nonempty(&source.url, "source URL")?;
    validate_nonempty(&source.evidence, "source evidence")?;
    validate_date(&source.retrieved, "source retrieved")?;
    if let Some(published) = &source.published {
        validate_date(published, "source published")?;
        if published > &source.retrieved {
            return Err(CatalogError::new(format!(
                "source {} was retrieved before publication",
                source.id
            )));
        }
    }
    let url = Url::parse(&source.url)
        .map_err(|_| CatalogError::new(format!("source {} has an invalid URL", source.id)))?;
    let allowed = url.scheme() == "https"
        && url.username().is_empty()
        && url.password().is_none()
        && url.port().is_none()
        && match (source.provider, url.host_str()) {
            (
                Provider::Openai,
                Some(
                    "openai.com"
                    | "developers.openai.com"
                    | "platform.openai.com"
                    | "help.openai.com"
                    | "community.openai.com"
                    | "cdn.openai.com",
                ),
            ) => true,
            (Provider::Openai, Some("github.com")) => github_path_is_owned_by(&url, "openai"),
            (
                Provider::Anthropic,
                Some(
                    "www.anthropic.com"
                    | "anthropic.com"
                    | "docs.anthropic.com"
                    | "platform.claude.com"
                    | "support.anthropic.com"
                    | "assets.anthropic.com"
                    | "www-cdn.anthropic.com",
                ),
            ) => true,
            (Provider::Anthropic, Some("github.com")) => {
                github_path_is_owned_by(&url, "anthropics")
            }
            _ => false,
        };
    if !allowed {
        return Err(CatalogError::new(format!(
            "source {} is not a recognized first-party URL",
            source.id
        )));
    }
    Ok(())
}

fn github_path_is_owned_by(url: &Url, organization: &str) -> bool {
    let Some(mut segments) = url.path_segments() else {
        return false;
    };
    segments.next() == Some(organization)
        && segments
            .next()
            .is_some_and(|repository| !repository.is_empty())
}

fn validate_window(window: &DateWindow, subject: &str) -> Result<(), CatalogError> {
    validate_date(&window.first_observed_new_rate, "first_observed_new_rate")?;
    if let Some(date) = &window.effective_from {
        validate_date(date, "effective_from")?;
    }
    if let Some(date) = &window.effective_until {
        validate_date(date, "effective_until")?;
    }
    if let Some(date) = &window.last_observed_old_rate {
        validate_date(date, "last_observed_old_rate")?;
    }
    if window.precision == DatePrecision::ExactDay && window.effective_from.is_none() {
        return Err(CatalogError::new(format!(
            "exact-day window {subject} has no effective_from"
        )));
    }
    if window.precision == DatePrecision::ObservationWindow && window.effective_from.is_some() {
        return Err(CatalogError::new(format!(
            "observation window {subject} claims an exact effective_from"
        )));
    }
    if window.precision == DatePrecision::ExactDay
        && window.effective_from.as_deref() != Some(window.first_observed_new_rate.as_str())
    {
        return Err(CatalogError::new(format!(
            "exact-day window {subject} has inconsistent effective and observation dates"
        )));
    }
    if let (Some(from), Some(until)) = (&window.effective_from, &window.effective_until)
        && from >= until
    {
        return Err(CatalogError::new(format!(
            "window {subject} is empty or reversed"
        )));
    }
    if window
        .effective_until
        .as_deref()
        .is_some_and(|until| window.first_observed_new_rate.as_str() >= until)
    {
        return Err(CatalogError::new(format!(
            "window {subject} observes its rate after the window ends"
        )));
    }
    if let Some(last_old) = &window.last_observed_old_rate
        && last_old >= &window.first_observed_new_rate
    {
        return Err(CatalogError::new(format!(
            "window {subject} does not leave an ordered observation boundary"
        )));
    }
    Ok(())
}

fn validate_rate(rate: &Rate, subject: &str) -> Result<(), CatalogError> {
    for (field, value) in [
        ("service_tier", rate.qualifier.service_tier.as_deref()),
        ("context_band", rate.qualifier.context_band.as_deref()),
        ("cache_ttl", rate.qualifier.cache_ttl.as_deref()),
        ("region", rate.qualifier.region.as_deref()),
    ] {
        if let Some(value) = value {
            validate_nonempty(value, field)?;
            if value.contains(',') || value.contains('=') {
                return Err(CatalogError::new(format!(
                    "rate {subject} qualifier {field} contains a reserved delimiter"
                )));
            }
        }
    }
    validate_nonempty(&rate.original.amount, "original amount")?;
    validate_nonempty(&rate.original.currency, "original currency")?;
    validate_nonempty(&rate.original.unit, "original unit")?;
    let original = decimal(&rate.original.amount, subject)?;
    if original.is_sign_negative() {
        return Err(CatalogError::new(format!("rate {subject} is negative")));
    }
    match (&rate.usd_per_million_tokens, rate.dimension) {
        (Some(_amount), RateDimension::Operation) => {
            return Err(CatalogError::new(format!(
                "operation rate {subject} is forced into a token-rate field"
            )));
        }
        (None, RateDimension::Operation) => {}
        (Some(amount), _) => {
            let normalized = decimal(amount, subject)?;
            if normalized.is_sign_negative() {
                return Err(CatalogError::new(format!("rate {subject} is negative")));
            }
            let expected = match rate.original.unit.as_str() {
                "usd_per_million_tokens" => original,
                "usd_per_thousand_tokens" => original
                    .checked_mul(Decimal::from(1_000_u32))
                    .ok_or_else(|| CatalogError::new(format!("rate {subject} overflows")))?,
                other => {
                    return Err(CatalogError::new(format!(
                        "token rate {subject} has incomparable original unit {other}"
                    )));
                }
            };
            if rate.original.currency != "USD" || normalized != expected {
                return Err(CatalogError::new(format!(
                    "rate {subject} normalized amount disagrees with original"
                )));
            }
        }
        (None, _) => {
            return Err(CatalogError::new(format!(
                "token rate {subject} lacks a comparable normalized amount"
            )));
        }
    }
    Ok(())
}

fn validate_rate_overlaps(rate_sets: &[RateSet]) -> Result<(), CatalogError> {
    for (left_index, left) in rate_sets.iter().enumerate() {
        for right in rate_sets.iter().skip(left_index + 1) {
            if left.provider != right.provider
                || left.commercial_channel != right.commercial_channel
                || left.model_id != right.model_id
                || !windows_overlap(&left.window, &right.window)
            {
                continue;
            }
            for left_rate in &left.rates {
                if let Some(right_rate) = right.rates.iter().find(|right_rate| {
                    right_rate.dimension == left_rate.dimension
                        && right_rate.qualifier == left_rate.qualifier
                }) && (left_rate.usd_per_million_tokens != right_rate.usd_per_million_tokens
                    || left_rate.original != right_rate.original)
                {
                    return Err(CatalogError::new(format!(
                        "incompatible overlapping rate sets {} and {}",
                        left.id, right.id
                    )));
                }
            }
        }
    }
    Ok(())
}

fn validate_mapping_overlaps(mappings: &[ConsumerMapping]) -> Result<(), CatalogError> {
    for (left_index, left) in mappings.iter().enumerate() {
        for right in mappings.iter().skip(left_index + 1) {
            if left.provider != right.provider
                || left.commercial_channel != right.commercial_channel
                || left.observed_identity != right.observed_identity
                || left.normalized_model != right.normalized_model
                || !windows_overlap(&left.window, &right.window)
            {
                continue;
            }
            return Err(CatalogError::new(format!(
                "redundant overlapping consumer mappings {} and {}",
                left.id, right.id
            )));
        }
    }
    Ok(())
}

fn windows_overlap(left: &DateWindow, right: &DateWindow) -> bool {
    let left_start = left
        .effective_from
        .as_deref()
        .unwrap_or(left.first_observed_new_rate.as_str());
    let right_start = right
        .effective_from
        .as_deref()
        .unwrap_or(right.first_observed_new_rate.as_str());
    left.effective_until
        .as_deref()
        .is_none_or(|until| right_start < until)
        && right
            .effective_until
            .as_deref()
            .is_none_or(|until| left_start < until)
}

fn resolve_rate_set(set: &RateSet) -> Result<ResolvedRateSet, CatalogError> {
    let mut rates = set
        .rates
        .iter()
        .map(|rate| {
            let amount = rate
                .usd_per_million_tokens
                .as_deref()
                .map(|value| decimal(value, &set.id))
                .transpose()?;
            Ok(ResolvedRate {
                dimension: rate.dimension,
                qualifier: rate.qualifier.label(),
                usd_per_million_tokens: amount,
                original_amount: rate.original.amount.clone(),
                original_currency: rate.original.currency.clone(),
                original_unit: rate.original.unit.clone(),
            })
        })
        .collect::<Result<Vec<_>, CatalogError>>()?;
    rates.sort_by(|left, right| {
        (left.dimension, left.qualifier.as_str()).cmp(&(right.dimension, right.qualifier.as_str()))
    });
    Ok(ResolvedRateSet {
        id: set.id.clone(),
        rates,
        interval: set.window.label(),
        window: ResolvedDateWindow {
            precision: set.window.precision,
            effective_from: set.window.effective_from.clone(),
            effective_until: set.window.effective_until.clone(),
            first_observed_new_rate: set.window.first_observed_new_rate.clone(),
            last_observed_old_rate: set.window.last_observed_old_rate.clone(),
        },
        confidence: set.confidence,
        source_ids: set.source_ids.clone(),
        limitations: set.limitations.clone(),
    })
}

fn decimal(value: &str, subject: &str) -> Result<Decimal, CatalogError> {
    Decimal::from_str_exact(value)
        .map_err(|_| CatalogError::new(format!("{subject} contains an invalid exact decimal")))
}

fn unique_ids<'a>(
    ids: impl IntoIterator<Item = &'a str>,
    kind: &str,
) -> Result<BTreeSet<&'a str>, CatalogError> {
    let mut unique = BTreeSet::new();
    for id in ids {
        validate_nonempty(id, kind)?;
        if !unique.insert(id) {
            return Err(CatalogError::new(format!("duplicate {kind} id {id}")));
        }
    }
    Ok(unique)
}

fn validate_source_refs(
    references: &[String],
    sources: &BTreeSet<&str>,
    subject: &str,
) -> Result<(), CatalogError> {
    if references.is_empty() {
        return Err(CatalogError::new(format!(
            "{subject} has no provenance source"
        )));
    }
    for source in references {
        if !sources.contains(source.as_str()) {
            return Err(CatalogError::new(format!(
                "{subject} names unknown source {source}"
            )));
        }
    }
    Ok(())
}

fn validate_source_providers(
    raw: &RawCatalog,
    provider: Provider,
    references: &[String],
    subject: &str,
) -> Result<(), CatalogError> {
    for reference in references {
        let source = raw
            .sources
            .iter()
            .find(|source| source.id == *reference)
            .ok_or_else(|| {
                CatalogError::new(format!("{subject} names unknown source {reference}"))
            })?;
        if source.provider != provider {
            return Err(CatalogError::new(format!(
                "{subject} cites a source from another provider"
            )));
        }
    }
    Ok(())
}

fn validate_model_ref(
    raw: &RawCatalog,
    provider: Provider,
    reference: Option<&str>,
    subject: &str,
) -> Result<(), CatalogError> {
    let Some(reference) = reference else {
        return Ok(());
    };
    let model = raw
        .models
        .iter()
        .find(|model| model.id == reference)
        .ok_or_else(|| CatalogError::new(format!("{subject} names unknown model {reference}")))?;
    if model.provider != provider {
        return Err(CatalogError::new(format!(
            "{subject} crosses provider boundary through {reference}"
        )));
    }
    Ok(())
}

fn validate_nonempty(value: &str, field: &str) -> Result<(), CatalogError> {
    if value.is_empty() || value.trim() != value || value.contains('\0') {
        Err(CatalogError::new(format!(
            "{field} is empty, padded, or NUL-bearing"
        )))
    } else if value.chars().any(char::is_control) || value.contains(['|', '`']) {
        Err(CatalogError::new(format!(
            "{field} contains a projection-breaking character"
        )))
    } else {
        Ok(())
    }
}

fn validate_limitations(
    confidence: Confidence,
    limitations: &[String],
    subject: &str,
) -> Result<(), CatalogError> {
    if confidence == Confidence::Low && limitations.is_empty() {
        return Err(CatalogError::new(format!(
            "low-confidence record {subject} has no explicit limitation"
        )));
    }
    for limitation in limitations {
        validate_nonempty(limitation, "limitation")?;
    }
    Ok(())
}

fn validate_date(value: &str, field: &str) -> Result<(), CatalogError> {
    let date = jiff::civil::Date::strptime("%Y-%m-%d", value)
        .map_err(|_| CatalogError::new(format!("{field} is not a YYYY-MM-DD calendar date")))?;
    if date.strftime("%Y-%m-%d").to_string() != value {
        return Err(CatalogError::new(format!(
            "{field} is not a YYYY-MM-DD calendar date"
        )));
    }
    Ok(())
}

fn model_available(model: &ReferenceModel, date: &str) -> bool {
    model
        .available_from
        .as_deref()
        .is_none_or(|from| date >= from)
        && model
            .available_until
            .as_deref()
            .is_none_or(|until| date < until)
}

fn code_or_dash(value: Option<&str>) -> String {
    value.map_or_else(|| String::from("-"), |value| format!("`{value}`"))
}
