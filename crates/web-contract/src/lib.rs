//! Rust-authoritative browser HTTP data-transfer contract.
//!
//! The generated TypeScript declarations and runtime decoders are derived from
//! these serde and JSON Schema definitions. Browser DTOs deliberately remain
//! distinct from domain, persistence, and local process-protocol values.

use std::{collections::BTreeMap, error::Error, fmt};

use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize, de};
use serde_json::{Value, json};
use signalbox_application::{
    max_search_page_items, max_search_query_bytes, max_search_snippet_bytes,
    max_timeline_window_bytes, max_timeline_window_items, max_usage_aggregate_calls,
    max_usage_aggregate_groups, max_usage_call_page_items,
};

/// Exact browser HTTP contract version served by this daemon build.
pub const WEB_CONTRACT_VERSION: &str = "1";
/// Stable name of the browser HTTP contract family.
pub const WEB_CONTRACT_NAME: &str = "signalbox.web-http";

/// Hard safety ceiling protecting daemon memory while buffering one JSON body.
pub const MAX_JSON_BODY_BYTES: usize = 64 * 1024;
/// Hard safety ceiling protecting client and daemon memory per NDJSON item.
pub const MAX_NDJSON_ITEM_BYTES: usize = 64 * 1024;

/// Identity of the one exact browser contract this daemon serves.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WebContractIdentity {
    /// Stable contract family.
    pub name: String,
    /// Exact version; clients do not negotiate ranges.
    pub version: String,
}

/// Transport capabilities present in the exact contract version.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WebContractCapabilities {
    /// Ordinary bounded JSON responses are available under `/api/`.
    pub bounded_json: bool,
    /// JSON mutations validate a supplied browser origin against authority.
    pub same_origin_json_mutations: bool,
    /// Incremental response items use newline-delimited JSON.
    pub ndjson_streaming: bool,
    /// Stable bounded session descriptors and historical windows are available.
    pub bounded_session_timeline: bool,
    /// Bounded lexical search with stable history reveal addresses is available.
    pub bounded_lexical_search: bool,
    /// Dedicated bounded aggregate and per-call usage/cost reads are available.
    pub bounded_usage_cost: bool,
}

/// Effective hard limits clients must honor for this contract version.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WebContractLimits {
    /// Maximum bytes accepted for one JSON request body.
    pub max_json_body_bytes: u32,
    /// Maximum encoded bytes for one NDJSON item, excluding its newline.
    pub max_ndjson_item_bytes: u32,
    /// Maximum durable event headers returned in one timeline window.
    pub max_timeline_window_items: u32,
    /// Maximum projected structured item bytes in one timeline window.
    pub max_timeline_window_bytes: u32,
    /// Maximum UTF-8 bytes in one product search expression.
    pub max_search_query_bytes: u32,
    /// Maximum results in one search page.
    pub max_search_page_items: u32,
    /// Maximum UTF-8 bytes in one search result snippet.
    pub max_search_snippet_bytes: u32,
    /// Maximum compatibility-preserving groups in one usage summary.
    pub max_usage_aggregate_groups: u32,
    /// Maximum individual calls in one usage detail page.
    pub max_usage_call_page_items: u32,
}

/// Response from the contract bootstrap endpoint.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WebContractBootstrap {
    /// Exact contract identity.
    pub contract: WebContractIdentity,
    /// Supported transport capabilities.
    pub capabilities: WebContractCapabilities,
    /// Bounds shared by server and generated client contract.
    pub limits: WebContractLimits,
}

impl WebContractBootstrap {
    /// Describes this daemon build's one exact browser contract.
    #[must_use]
    pub fn current() -> Self {
        Self {
            contract: WebContractIdentity {
                name: WEB_CONTRACT_NAME.to_owned(),
                version: WEB_CONTRACT_VERSION.to_owned(),
            },
            capabilities: WebContractCapabilities {
                bounded_json: true,
                same_origin_json_mutations: true,
                ndjson_streaming: true,
                bounded_session_timeline: true,
                bounded_lexical_search: true,
                bounded_usage_cost: true,
            },
            limits: WebContractLimits {
                max_json_body_bytes: MAX_JSON_BODY_BYTES as u32,
                max_ndjson_item_bytes: MAX_NDJSON_ITEM_BYTES as u32,
                max_timeline_window_items: u32::from(max_timeline_window_items()),
                max_timeline_window_bytes: max_timeline_window_bytes(),
                max_search_query_bytes: max_search_query_bytes() as u32,
                max_search_page_items: u32::from(max_search_page_items()),
                max_search_snippet_bytes: max_search_snippet_bytes() as u32,
                max_usage_aggregate_groups: u32::from(max_usage_aggregate_groups()),
                max_usage_call_page_items: u32::from(max_usage_call_page_items()),
            },
        }
    }
}

/// Small generated-contract fixture proving Rust/TypeScript round trips.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WebContractExample {
    /// Opaque request correlation chosen by the caller.
    pub request_id: String,
    /// Bounded example payload.
    pub message: String,
}

/// Checked positive durable-event sequence encoded losslessly for JavaScript.
#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(transparent)]
pub struct WebTimelineEventSequence(#[schemars(regex(pattern = r"^[1-9][0-9]*$"))] String);

fn canonical_u64(value: &str) -> Option<u64> {
    let canonical = !value.is_empty()
        && value.bytes().all(|byte| byte.is_ascii_digit())
        && (value == "0" || !value.starts_with('0'));
    canonical.then(|| value.parse::<u64>().ok()).flatten()
}

fn canonical_session_id(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(index, byte)| match index {
            8 | 13 | 18 | 23 => byte == b'-',
            _ => byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte),
        })
}

/// Checked canonical UUID used for browser-visible session identities.
#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(transparent)]
pub struct WebSessionId(
    #[schemars(regex(
        pattern = r"^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$"
    ))]
    String,
);

impl WebSessionId {
    /// Encodes an already-validated UUID in canonical lowercase form.
    #[must_use]
    pub fn from_validated_uuid(value: String) -> Self {
        debug_assert!(canonical_session_id(&value));
        Self(value)
    }

    /// Constructs a canonical lowercase UUID from its 16 wire-order bytes.
    #[must_use]
    pub fn from_uuid_bytes(bytes: [u8; 16]) -> Self {
        Self(format!(
            "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
            bytes[0],
            bytes[1],
            bytes[2],
            bytes[3],
            bytes[4],
            bytes[5],
            bytes[6],
            bytes[7],
            bytes[8],
            bytes[9],
            bytes[10],
            bytes[11],
            bytes[12],
            bytes[13],
            bytes[14],
            bytes[15],
        ))
    }

    /// Constructs a session identity from its canonical lowercase UUID spelling.
    #[must_use]
    pub fn from_canonical(value: String) -> Option<Self> {
        canonical_session_id(&value).then_some(Self(value))
    }

    /// Returns the canonical lowercase UUID spelling.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for WebSessionId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::from_canonical(value)
            .ok_or_else(|| de::Error::custom("session ID must be a canonical lowercase UUID"))
    }
}

/// Checked canonical UUID used for browser-visible non-session identities.
#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(transparent)]
pub struct WebUuid(
    #[schemars(regex(
        pattern = r"^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$"
    ))]
    String,
);

impl WebUuid {
    /// Encodes an already-validated UUID in canonical lowercase form.
    #[must_use]
    pub fn from_validated_uuid(value: String) -> Self {
        debug_assert!(canonical_session_id(&value));
        Self(value)
    }

    /// Constructs an identity from its canonical lowercase UUID spelling.
    #[must_use]
    pub fn from_canonical(value: String) -> Option<Self> {
        canonical_session_id(&value).then_some(Self(value))
    }
}

impl<'de> Deserialize<'de> for WebUuid {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::from_canonical(value)
            .ok_or_else(|| de::Error::custom("identity must be a canonical lowercase UUID"))
    }
}

impl WebTimelineEventSequence {
    /// Encodes one already-validated positive durable-event sequence.
    #[must_use]
    pub fn from_nonzero(sequence: std::num::NonZeroU64) -> Self {
        Self(sequence.get().to_string())
    }

    /// Returns the canonical positive decimal wire spelling.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for WebTimelineEventSequence {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        let positive = canonical_u64(&value).and_then(std::num::NonZeroU64::new);
        if positive.is_none() {
            return Err(de::Error::custom(
                "timeline event sequence must be a canonical positive u64",
            ));
        }
        Ok(Self(value))
    }
}

/// Checked unsigned 64-bit value encoded losslessly for JavaScript.
#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(transparent)]
pub struct WebU64(#[schemars(regex(pattern = r"^(0|[1-9][0-9]*)$"))] String);

impl WebU64 {
    /// Encodes one unsigned 64-bit value in canonical decimal form.
    #[must_use]
    pub fn from_u64(value: u64) -> Self {
        Self(value.to_string())
    }

    /// Returns the canonical unsigned decimal wire spelling.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for WebU64 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        if canonical_u64(&value).is_none() {
            return Err(de::Error::custom(
                "wire value must be a canonical unsigned 64-bit integer",
            ));
        }
        Ok(Self(value))
    }
}

/// Checked unsigned 128-bit value encoded losslessly for JavaScript.
#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(transparent)]
pub struct WebU128(#[schemars(regex(pattern = r"^(0|[1-9][0-9]{0,38})$"))] String);

impl WebU128 {
    /// Encodes one unsigned 128-bit value in canonical decimal form.
    #[must_use]
    pub fn from_u128(value: u128) -> Self {
        Self(value.to_string())
    }
}

impl<'de> Deserialize<'de> for WebU128 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        if value
            .parse::<u128>()
            .ok()
            .is_none_or(|parsed| parsed.to_string() != value)
        {
            return Err(de::Error::custom(
                "wire value must be a canonical unsigned 128-bit integer",
            ));
        }
        Ok(Self(value))
    }
}

/// Stable browser-visible location of one durable session event.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WebTimelineAddress {
    /// Positive global durable event sequence encoded losslessly for JavaScript.
    pub event_sequence: WebTimelineEventSequence,
}

/// Explicit lifetime size facts used only for browser loading policy.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WebSessionTimelineSizeFacts {
    pub item_count: WebU64,
    pub projected_text_bytes: WebU64,
    pub projected_structured_bytes: WebU64,
    pub referenced_blob_count: WebU64,
    pub referenced_blob_bytes: WebU64,
}

/// Current work facts carried by the lightweight session descriptor.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WebSessionWorkFacts {
    pub active_turn_count: WebU64,
    pub queued_turn_count: WebU64,
}

/// Browser descriptor for one authoritative bounded session projection.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WebSessionTimelineDescriptor {
    pub session_id: WebSessionId,
    pub sizes: WebSessionTimelineSizeFacts,
    pub first_address: WebTimelineAddress,
    pub latest_address: WebTimelineAddress,
    pub work: WebSessionWorkFacts,
    pub observed_through: WebU64,
}

/// Closed durable event categories in the browser timeline foundation.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WebSessionTimelineEventKind {
    SessionCreated,
    SessionModelSettingsChanged,
    TurnModelSettingsResolved,
    InputAccepted,
    GoalTurnRetired,
    TurnActivated,
    TurnFailed,
    ModelCallTransition,
    ToolBatchTransition,
    ToolApprovalDecided,
    ContextCompacted,
    TurnCompleted,
    TurnRefused,
    TurnCancelled,
    TurnReconciliationRequired,
    RunnerStateTransition,
    DelegationUpdate,
    DelegationWake,
}

/// One typed, header-only event in a bounded browser window.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WebSessionTimelineItem {
    pub address: WebTimelineAddress,
    pub kind: WebSessionTimelineEventKind,
    pub projected_structured_bytes: u32,
}

/// One bounded, logically ordered browser timeline window.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WebSessionTimelineWindow {
    pub session_id: WebSessionId,
    pub items: Vec<WebSessionTimelineItem>,
    pub projected_structured_bytes: u32,
    pub continuation_before: Option<WebTimelineAddress>,
    pub continuation_after: Option<WebTimelineAddress>,
}

/// Closed browser-visible class of matched indexed content.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WebSearchContentClass {
    UserTranscript,
    AssistantTranscript,
    ToolArguments,
    ToolResult,
    SessionMetadata,
    AttachmentFilename,
    AttachmentMediaMetadata,
    DerivedTextArtifact,
}

/// Typed durable source of one browser search result.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum WebSearchResultSource {
    Session {
        session_id: WebSessionId,
    },
    AcceptedInput {
        accepted_input_id: WebUuid,
        turn_id: WebUuid,
    },
    SteeringInput {
        accepted_input_id: WebUuid,
        source_turn_id: WebUuid,
    },
    TurnTranscriptEntry {
        semantic_entry_id: WebUuid,
        turn_id: WebUuid,
    },
    SessionTranscriptEntry {
        semantic_entry_id: WebUuid,
    },
    ToolRequest {
        tool_request_id: WebUuid,
        turn_id: WebUuid,
    },
    ToolAttempt {
        tool_attempt_id: WebUuid,
        turn_id: WebUuid,
    },
    Attachment {
        attachment_id: WebUuid,
    },
    DerivedArtifact {
        artifact_id: WebUuid,
    },
}

/// One half-open UTF-8 byte range within a bounded snippet.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WebSearchHighlight {
    pub start_byte: u32,
    pub end_byte: u32,
}

/// Checked positive PostgreSQL projection identity encoded losslessly for JavaScript.
#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(transparent)]
pub struct WebSearchProjectionId(#[schemars(regex(pattern = r"^[1-9][0-9]{0,18}$"))] String);

impl WebSearchProjectionId {
    /// Encodes one already-validated positive projection identity.
    #[must_use]
    pub fn from_nonzero(value: std::num::NonZeroU64) -> Self {
        debug_assert!(i64::try_from(value.get()).is_ok());
        Self(value.get().to_string())
    }
}

impl<'de> Deserialize<'de> for WebSearchProjectionId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        let positive = canonical_u64(&value)
            .and_then(std::num::NonZeroU64::new)
            .filter(|value| i64::try_from(value.get()).is_ok());
        if positive.is_none() {
            return Err(de::Error::custom(
                "search projection identity must be a canonical positive i64",
            ));
        }
        Ok(Self(value))
    }
}

/// Stable opaque descending search keyset boundary.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WebSearchCursor {
    pub address: WebTimelineAddress,
    pub projection_id: WebSearchProjectionId,
}

/// One bounded lexical match with enough identity to reveal unloaded history.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WebSearchResult {
    pub session_id: WebSessionId,
    pub address: WebTimelineAddress,
    pub projection_id: WebSearchProjectionId,
    pub source: WebSearchResultSource,
    pub content_class: WebSearchContentClass,
    #[schemars(length(max = 512))]
    pub snippet: String,
    #[schemars(length(max = 512))]
    pub highlights: Vec<WebSearchHighlight>,
}

/// One bounded, stable page of lexical matches.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WebSearchPage {
    pub results: Vec<WebSearchResult>,
    pub continuation: Option<WebSearchCursor>,
}

/// Closed physical class of one terminal usage record.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WebUsageCallKind {
    ModelCall,
    ApprovalJudge,
    ContextCompaction,
}

/// Closed provenance of one token-evidence projection.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WebUsageProvenance {
    Reported,
    Estimated,
}

/// Meaning of one provider target's input-token axis.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WebUsageInputSemantics {
    Unknown,
    CacheExclusive,
    CacheInclusive,
}

/// Independently nullable token axes; null is never interpreted as zero.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(untagged)]
pub enum WebNullableU64 {
    Value(WebU64),
    Null,
}

impl WebNullableU64 {
    /// Preserves a missing axis as an explicit JSON null.
    #[must_use]
    pub fn from_option(value: Option<u64>) -> Self {
        match value {
            Some(value) => Self::Value(WebU64::from_u64(value)),
            None => Self::Null,
        }
    }
}

/// Independently nullable aggregate token axis widened beyond one call.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(untagged)]
pub enum WebNullableU128 {
    Value(WebU128),
    Null,
}

impl WebNullableU128 {
    /// Preserves a missing aggregate axis as an explicit JSON null.
    #[must_use]
    pub fn from_option(value: Option<u128>) -> Self {
        match value {
            Some(value) => Self::Value(WebU128::from_u128(value)),
            None => Self::Null,
        }
    }
}

/// Independently nullable token axes; null is never interpreted as zero.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WebUsageTokenAxes {
    pub input: WebNullableU64,
    pub output: WebNullableU64,
    pub cache_creation_input: WebNullableU64,
    pub cache_read_input: WebNullableU64,
}

/// Aggregate token axes widened beyond one physical call.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WebUsageAggregateTokenAxes {
    pub input: WebNullableU128,
    pub output: WebNullableU128,
    pub cache_creation_input: WebNullableU128,
    pub cache_read_input: WebNullableU128,
}

/// Explicit presence shape retained by compatibility-preserving aggregates.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WebUsageTokenCoverage {
    pub input: bool,
    pub output: bool,
    pub cache_creation_input: bool,
    pub cache_read_input: bool,
}

/// Browser-visible billing label derived from the serving credential profile.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WebUsageCostLabel {
    Real,
    MeteredEquivalent,
}

/// Why no configured dollar derivation is available for exact token evidence.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WebUsageCostUnavailableReason {
    NoTokenEvidence,
    UnknownInputSemantics,
    IncompleteCacheAxes,
    InvalidCacheBreakdown,
    ConfigurationUnavailable,
}

/// Canonical nonnegative fixed-point USD amount derived by the daemon.
#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(transparent)]
pub struct WebDollarAmount(
    #[schemars(regex(pattern = r"^(0|[1-9][0-9]*)(\.[0-9]{0,27}[1-9])?$"))] String,
);

impl WebDollarAmount {
    /// Wraps configuration arithmetic already represented by `rust_decimal`.
    #[must_use]
    pub fn from_derived(value: String) -> Self {
        Self(value)
    }
}

impl<'de> Deserialize<'de> for WebDollarAmount {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        let (whole, fractional) = value
            .split_once('.')
            .map_or((value.as_str(), None), |(whole, fractional)| {
                (whole, Some(fractional))
            });
        let whole_is_canonical = !whole.is_empty()
            && whole.bytes().all(|byte| byte.is_ascii_digit())
            && (whole == "0" || !whole.starts_with('0'));
        let fractional_is_canonical = fractional.is_none_or(|fractional| {
            !fractional.is_empty()
                && fractional.len() <= 28
                && fractional.bytes().all(|byte| byte.is_ascii_digit())
                && !fractional.ends_with('0')
        });
        let coefficient = format!("{whole}{}", fractional.unwrap_or_default());
        let significant_coefficient = coefficient.trim_start_matches('0');
        let coefficient_fits = significant_coefficient.len() < 29
            || (significant_coefficient.len() == 29
                && significant_coefficient <= "79228162514264337593543950335");
        if !whole_is_canonical || !fractional_is_canonical || !coefficient_fits {
            return Err(de::Error::custom(
                "dollar amount must be a canonical nonnegative decimal",
            ));
        }
        Ok(Self(value))
    }
}

/// Checked nonempty configured rate version exposed to browser clients.
#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(transparent)]
pub struct WebUsageRateVersion(#[schemars(length(min = 1, max = 128))] String);

impl WebUsageRateVersion {
    /// Wraps a rate version already admitted by daemon configuration.
    #[must_use]
    pub fn from_configured(value: String) -> Self {
        debug_assert!(!value.is_empty() && value.len() <= 128);
        Self(value)
    }
}

impl<'de> Deserialize<'de> for WebUsageRateVersion {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        if value.is_empty() || value.len() > 128 {
            return Err(de::Error::custom(
                "usage rate version must contain 1 through 128 UTF-8 bytes",
            ));
        }
        Ok(Self(value))
    }
}

/// Labeled configured cost, or an explicit reason it cannot be derived.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum WebUsageCost {
    Derived {
        amount_usd: WebDollarAmount,
        rate_version: WebUsageRateVersion,
        label: WebUsageCostLabel,
    },
    Unavailable {
        reason: WebUsageCostUnavailableReason,
    },
}

/// Non-secret bounded profile identity retained by usage summaries.
#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(transparent)]
pub struct WebUsageProfileId(#[schemars(length(min = 1, max = 256))] String);

impl WebUsageProfileId {
    /// Wraps a profile identity already validated by the persistence boundary.
    #[must_use]
    pub fn from_bounded(value: String) -> Self {
        debug_assert!(!value.is_empty() && value.len() <= 256);
        Self(value)
    }
}

impl<'de> Deserialize<'de> for WebUsageProfileId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        if value.is_empty() || value.len() > 256 {
            return Err(de::Error::custom(
                "usage profile identity must contain 1 through 256 UTF-8 bytes",
            ));
        }
        Ok(Self(value))
    }
}

/// Checked positive summary call count encoded losslessly for JavaScript.
#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(transparent)]
pub struct WebUsageCallCount(#[schemars(regex(pattern = r"^([1-9][0-9]{0,3}|10000)$"))] String);

impl WebUsageCallCount {
    /// Encodes a positive aggregate count produced by persistence.
    #[must_use]
    pub fn from_positive(value: u64) -> Self {
        debug_assert!(value > 0 && value <= u64::from(max_usage_aggregate_calls()));
        Self(value.to_string())
    }
}

impl<'de> Deserialize<'de> for WebUsageCallCount {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        if canonical_u64(&value)
            .is_none_or(|parsed| parsed == 0 || parsed > u64::from(max_usage_aggregate_calls()))
        {
            return Err(de::Error::custom(
                "usage summary call count must be canonical and within the aggregation ceiling",
            ));
        }
        Ok(Self(value))
    }
}

/// Checked application-range usage timestamp encoded losslessly for JavaScript.
#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(transparent)]
pub struct WebUsageTimestampMicros(#[schemars(regex(pattern = r"^(0|[1-9][0-9]{0,17})$"))] String);

impl WebUsageTimestampMicros {
    /// Encodes one timestamp already admitted by the application boundary.
    #[must_use]
    pub fn from_application(value: u64) -> Self {
        debug_assert!(value <= 253_402_300_799_999_999);
        Self(value.to_string())
    }
}

impl<'de> Deserialize<'de> for WebUsageTimestampMicros {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        if canonical_u64(&value).is_none_or(|parsed| parsed > 253_402_300_799_999_999) {
            return Err(de::Error::custom(
                "usage timestamp must be canonical and within the application range",
            ));
        }
        Ok(Self(value))
    }
}

/// One compatibility-preserving usage and configured-cost summary row.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WebUsageAggregateGroup {
    pub call_kind: WebUsageCallKind,
    pub model_id: WebUuid,
    pub profile_id: WebUsageProfileId,
    pub provenance: WebUsageProvenance,
    pub input_semantics: WebUsageInputSemantics,
    pub coverage: WebUsageTokenCoverage,
    pub call_count: WebUsageCallCount,
    pub tokens: WebUsageAggregateTokenAxes,
    pub cost: WebUsageCost,
}

/// Bounded aggregate response; truncation is never implicit.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WebUsageSummary {
    #[schemars(length(max = 256))]
    pub groups: Vec<WebUsageAggregateGroup>,
    pub truncated: bool,
}

/// One terminal call with exact token, provenance, rate, and billing evidence.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WebUsageCall {
    pub call_kind: WebUsageCallKind,
    pub call_id: WebUuid,
    pub session_id: WebSessionId,
    #[schemars(required)]
    pub turn_id: Option<WebUuid>,
    pub model_id: WebUuid,
    pub provenance: WebUsageProvenance,
    pub input_semantics: WebUsageInputSemantics,
    pub tokens: WebUsageTokenAxes,
    pub recorded_at_micros: WebUsageTimestampMicros,
    pub cost: WebUsageCost,
}

/// Stable terminal-time/UUID keyset boundary for usage detail traversal.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WebUsageCallCursor {
    pub recorded_at_micros: WebUsageTimestampMicros,
    pub call_id: WebUuid,
}

/// One bounded page of exact call evidence.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WebUsageCallPage {
    #[schemars(length(max = 100))]
    pub calls: Vec<WebUsageCall>,
    pub continuation: Option<WebUsageCallCursor>,
}

/// Layer that owns one browser API failure.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WebApiErrorKind {
    /// HTTP framing, decoding, compatibility, or trust-boundary rejection.
    Transport,
    /// A valid request reached an application operation that rejected it.
    Application,
}

/// Stable error detail carried independently from HTTP status.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WebApiError {
    /// Boundary that owns the failure.
    pub kind: WebApiErrorKind,
    /// Stable machine-readable code within that boundary.
    pub code: String,
    /// Small human-readable explanation with no sensitive detail.
    pub message: String,
}

impl fmt::Display for WebApiError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for WebApiError {}

/// Error response envelope shared by ordinary JSON endpoints.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WebApiErrorResponse {
    /// Typed failure detail.
    pub error: WebApiError,
}

/// One generated file and its repository-relative destination.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GeneratedArtifact {
    /// Repository-relative output path.
    pub path: &'static str,
    /// Canonical generated contents.
    pub contents: String,
}

/// Build-time failure while deriving browser artifacts from Rust schemas.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GenerateWebContractError {
    /// A serde-owned value could not be serialized.
    Serialization,
    /// A Rust schema used a shape the focused TypeScript generator does not support.
    UnsupportedSchema,
}

impl fmt::Display for GenerateWebContractError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Serialization => formatter.write_str("web contract could not be serialized"),
            Self::UnsupportedSchema => {
                formatter.write_str("web contract contains an unsupported schema shape")
            }
        }
    }
}

impl Error for GenerateWebContractError {}

struct GeneratedSchemas {
    bootstrap: Value,
    example: Value,
    error: Value,
    descriptor: Value,
    window: Value,
    search_page: Value,
    usage_summary: Value,
    usage_call_page: Value,
}

/// Produces all checked-in browser contract artifacts.
///
/// # Errors
///
/// Returns a closed build-time error when serde cannot encode a generated value
/// or a DTO schema grows beyond the generator's focused supported shapes.
pub fn generated_artifacts() -> Result<Vec<GeneratedArtifact>, GenerateWebContractError> {
    let mut search_page = schemars::schema_for!(WebSearchPage).to_value();
    search_page["properties"]["results"]["maxItems"] = json!(max_search_page_items());
    let schemas = GeneratedSchemas {
        bootstrap: canonical_schema(schemars::schema_for!(WebContractBootstrap).to_value()),
        example: canonical_schema(schemars::schema_for!(WebContractExample).to_value()),
        error: canonical_schema(schemars::schema_for!(WebApiErrorResponse).to_value()),
        descriptor: canonical_schema(
            schemars::schema_for!(WebSessionTimelineDescriptor).to_value(),
        ),
        window: canonical_schema(schemars::schema_for!(WebSessionTimelineWindow).to_value()),
        search_page: canonical_schema(search_page),
        usage_summary: canonical_schema(schemars::schema_for!(WebUsageSummary).to_value()),
        usage_call_page: canonical_schema(schemars::schema_for!(WebUsageCallPage).to_value()),
    };
    let example = WebContractExample {
        request_id: "contract-round-trip".to_owned(),
        message: "browser contract fixture".to_owned(),
    };
    let example_json = serde_json::to_string_pretty(&example)
        .map_err(|_| GenerateWebContractError::Serialization)?
        + "\n";

    Ok(vec![
        GeneratedArtifact {
            path: "clients/web/src/generated/web-contract.mjs",
            contents: runtime_module(&schemas)?,
        },
        GeneratedArtifact {
            path: "clients/web/src/generated/web-contract.d.mts",
            contents: declaration_module(&schemas)?,
        },
        GeneratedArtifact {
            path: "crates/web-contract/tests/fixtures/example.json",
            contents: example_json,
        },
    ])
}

fn canonical_schema(mut schema: Value) -> Value {
    // Schemars' default feature set preserves declaration order, while its
    // focused no-default build uses sorted maps. Workspace feature unification
    // must not change checked-in artifacts.
    schema.sort_all_objects();
    schema
}

fn runtime_module(schemas: &GeneratedSchemas) -> Result<String, GenerateWebContractError> {
    let mut schemas = json!({
        "WebContractBootstrap": schemas.bootstrap,
        "WebContractExample": schemas.example,
        "WebApiErrorResponse": schemas.error,
        "WebSessionTimelineDescriptor": schemas.descriptor,
        "WebSessionTimelineWindow": schemas.window,
        "WebSearchPage": schemas.search_page,
        "WebUsageSummary": schemas.usage_summary,
        "WebUsageCallPage": schemas.usage_call_page,
    });
    schemas.sort_all_objects();
    let schemas = serde_json::to_string_pretty(&schemas)
        .map_err(|_| GenerateWebContractError::Serialization)?;
    Ok(format!(
        r##"// @generated by `cargo run -p signalbox-web-contract --bin generate-web-contract`.
// Do not edit by hand.

const schemas = {schemas};

function fail(path, expected) {{
  throw new TypeError(`${{path}} must be ${{expected}}`);
}}

function resolveReference(root, reference) {{
  const prefix = "#/$defs/";
  if (!reference.startsWith(prefix)) {{
    throw new TypeError(`unsupported schema reference ${{reference}}`);
  }}
  const resolved = root.$defs?.[reference.slice(prefix.length)];
  if (resolved === undefined) {{
    throw new TypeError(`unknown schema reference ${{reference}}`);
  }}
  return resolved;
}}

function assertSchema(root, schema, value, path) {{
  if (schema.$ref !== undefined) {{
    assertSchema(root, resolveReference(root, schema.$ref), value, path);
    return;
  }}
  if (schema.enum !== undefined) {{
    if (!schema.enum.some((candidate) => Object.is(candidate, value))) {{
      fail(path, `one of ${{JSON.stringify(schema.enum)}}`);
    }}
    return;
  }}
  if (schema.const !== undefined) {{
    if (!Object.is(schema.const, value)) {{
      fail(path, JSON.stringify(schema.const));
    }}
    return;
  }}
  if (schema.oneOf !== undefined) {{
    const accepted = schema.oneOf.some((candidate) => {{
      try {{
        assertSchema(root, candidate, value, path);
        return true;
      }} catch {{
        return false;
      }}
    }});
    if (!accepted) {{
      fail(path, "one recognized variant");
    }}
    return;
  }}
  if (schema.anyOf !== undefined) {{
    const accepted = schema.anyOf.some((candidate) => {{
      try {{
        assertSchema(root, candidate, value, path);
        return true;
      }} catch {{
        return false;
      }}
    }});
    if (!accepted) {{
      fail(path, "one recognized variant");
    }}
    return;
  }}
  if (Array.isArray(schema.type)) {{
    if (value === null && schema.type.includes("null")) {{
      return;
    }}
    const concrete = schema.type.filter((candidate) => candidate !== "null");
    const accepted = concrete.some((candidate) => {{
      try {{
        assertSchema(root, {{ ...schema, type: candidate }}, value, path);
        return true;
      }} catch {{
        return false;
      }}
    }});
    if (!accepted) {{
      fail(path, concrete.join(" or "));
    }}
    return;
  }}
  if (schema.type === "object") {{
    if (value === null || typeof value !== "object" || Array.isArray(value)) {{
      fail(path, "an object");
    }}
    const properties = schema.properties ?? {{}};
    for (const required of schema.required ?? []) {{
      if (!Object.hasOwn(value, required)) {{
        fail(`${{path}}.${{required}}`, "present");
      }}
    }}
    if (schema.additionalProperties === false) {{
      for (const key of Object.keys(value)) {{
        if (!Object.hasOwn(properties, key)) {{
          fail(`${{path}}.${{key}}`, "absent");
        }}
      }}
    }}
    for (const [key, property] of Object.entries(properties)) {{
      if (Object.hasOwn(value, key)) {{
        assertSchema(root, property, value[key], `${{path}}.${{key}}`);
      }}
    }}
    return;
  }}
  if (schema.type === "array") {{
    if (!Array.isArray(value)) {{
      fail(path, "an array");
    }}
    if (schema.maxItems !== undefined && value.length > schema.maxItems) {{
      fail(path, `at most ${{schema.maxItems}} items`);
    }}
    value.forEach((item, index) => assertSchema(root, schema.items, item, `${{path}}[${{index}}]`));
    return;
  }}
  if (schema.type === "integer") {{
    if (!Number.isSafeInteger(value)) {{
      fail(path, "a safe integer");
    }}
    if (schema.format === "uint32" && (value < 0 || value > 4294967295)) {{
      fail(path, "an unsigned 32-bit integer");
    }}
    if (schema.minimum !== undefined && value < schema.minimum) {{
      fail(path, `at least ${{schema.minimum}}`);
    }}
    if (schema.maximum !== undefined && value > schema.maximum) {{
      fail(path, `at most ${{schema.maximum}}`);
    }}
    return;
  }}
  if (schema.type === "null") {{
    if (value !== null) {{
      fail(path, "null");
    }}
    return;
  }}
  if (typeof value !== schema.type) {{
    fail(path, schema.type);
  }}
  if (
    schema.type === "string" &&
    (schema.pattern === "^[1-9][0-9]*$" || schema.pattern === "^(0|[1-9][0-9]*)$") &&
    value.length > 20
  ) {{
    fail(path, "an unsigned 64-bit integer");
  }}
  if (schema.type === "string" && schema.pattern !== undefined && !(new RegExp(schema.pattern)).test(value)) {{
    fail(path, `a string matching ${{schema.pattern}}`);
  }}
  if (
    schema.type === "string" &&
    schema.pattern === "^(0|[1-9][0-9]*)(\\.[0-9]{{0,27}}[1-9])?$" &&
    BigInt(value.replace(".", "")) > 79228162514264337593543950335n
  ) {{
    fail(path, "a rust_decimal coefficient");
  }}
  if (
    schema.type === "string" &&
    schema.pattern === "^[1-9][0-9]{{0,18}}$" &&
    BigInt(value) > 9223372036854775807n
  ) {{
    fail(path, "a positive signed 64-bit integer");
  }}
  if (
    schema.type === "string" &&
    schema.pattern === "^(0|[1-9][0-9]{{0,17}})$" &&
    BigInt(value) > 253402300799999999n
  ) {{
    fail(path, "an application-range usage timestamp");
  }}
  if (
    schema.type === "string" &&
    (schema.pattern === "^[1-9][0-9]*$" || schema.pattern === "^(0|[1-9][0-9]*)$") &&
    BigInt(value) > 18446744073709551615n
  ) {{
    fail(path, "an unsigned 64-bit integer");
  }}
  if (
    schema.type === "string" &&
    schema.pattern === "^(0|[1-9][0-9]{{0,38}})$" &&
    BigInt(value) > 340282366920938463463374607431768211455n
  ) {{
    fail(path, "an unsigned 128-bit integer");
  }}
}}

export function decodeWebContractBootstrap(value) {{
  assertSchema(schemas.WebContractBootstrap, schemas.WebContractBootstrap, value, "bootstrap");
  if (value.contract.name !== {contract_name:?} || value.contract.version !== {contract_version:?}) {{
    throw new TypeError("bootstrap carries an incompatible web contract");
  }}
  return value;
}}

export function decodeWebContractExample(value) {{
  assertSchema(schemas.WebContractExample, schemas.WebContractExample, value, "example");
  return value;
}}

export function decodeWebApiErrorResponse(value) {{
  assertSchema(schemas.WebApiErrorResponse, schemas.WebApiErrorResponse, value, "error_response");
  return value;
}}

export function decodeWebSessionTimelineDescriptor(value) {{
  assertSchema(schemas.WebSessionTimelineDescriptor, schemas.WebSessionTimelineDescriptor, value, "session_descriptor");
  return value;
}}

export function decodeWebSessionTimelineWindow(value) {{
  assertSchema(schemas.WebSessionTimelineWindow, schemas.WebSessionTimelineWindow, value, "timeline_window");
  return value;
}}

function validSearchSourceCorrelation(result) {{
  switch (result.source.kind) {{
    case "session":
      return result.source.session_id === result.session_id && result.content_class === "session_metadata";
    case "accepted_input":
    case "steering_input":
      return result.content_class === "user_transcript";
    case "turn_transcript_entry":
      return result.content_class === "assistant_transcript";
    case "session_transcript_entry":
      return result.content_class === "derived_text_artifact";
    case "tool_request":
      return result.content_class === "tool_arguments";
    case "tool_attempt":
      return result.content_class === "tool_result";
    case "attachment":
      return result.content_class === "attachment_filename" ||
        result.content_class === "attachment_media_metadata";
    case "derived_artifact":
      return result.content_class === "derived_text_artifact";
    default:
      return false;
  }}
}}

export function decodeWebSearchPage(value) {{
  assertSchema(schemas.WebSearchPage, schemas.WebSearchPage, value, "search_page");
  if (value.continuation !== null) {{
    const lastResult = value.results.at(-1);
    if (
      lastResult === undefined ||
      value.continuation.address.event_sequence !== lastResult.address.event_sequence ||
      value.continuation.projection_id !== lastResult.projection_id
    ) {{
      fail(
        "search_page.continuation",
        "a cursor anchored to the final search result",
      );
    }}
  }}
  const encoder = new TextEncoder();
  let previousKey = null;
  value.results.forEach((result, resultIndex) => {{
    const address = BigInt(result.address.event_sequence);
    const projection = BigInt(result.projection_id);
    if (
      previousKey !== null &&
      (address > previousKey.address ||
        (address === previousKey.address && projection >= previousKey.projection))
    ) {{
      fail(
        `search_page.results[${{resultIndex}}]`,
        "a strictly descending search result key",
      );
    }}
    previousKey = {{ address, projection }};
    if (!validSearchSourceCorrelation(result)) {{
      fail(
        `search_page.results[${{resultIndex}}].source`,
        "a source consistent with the result session and content class",
      );
    }}
    const bytes = encoder.encode(result.snippet);
    if (bytes.length > {max_search_snippet_bytes}) {{
      fail(
        `search_page.results[${{resultIndex}}].snippet`,
        `at most {max_search_snippet_bytes} UTF-8 bytes`,
      );
    }}
    let previousEnd = 0;
    result.highlights.forEach((highlight, highlightIndex) => {{
      const rangePath = `search_page.results[${{resultIndex}}].highlights[${{highlightIndex}}]`;
      if (
        highlight.start_byte < previousEnd ||
        highlight.start_byte >= highlight.end_byte ||
        highlight.end_byte > bytes.length
      ) {{
        fail(rangePath, "an ordered non-overlapping in-bounds UTF-8 byte range");
      }}
      if (
        (highlight.start_byte > 0 && (bytes[highlight.start_byte] & 0xc0) === 0x80) ||
        (highlight.end_byte < bytes.length && (bytes[highlight.end_byte] & 0xc0) === 0x80)
      ) {{
        fail(rangePath, "a range on UTF-8 boundaries");
      }}
      previousEnd = highlight.end_byte;
    }});
  }});
  return value;
}}

export function decodeWebUsageSummary(value) {{
  assertSchema(schemas.WebUsageSummary, schemas.WebUsageSummary, value, "usage_summary");
  const encoder = new TextEncoder();
  const compatibilityKeys = new Set();
  value.groups.forEach((group, index) => {{
    const callCount = BigInt(group.call_count);
    assertUsageEvidence(
      group.input_semantics,
      group.tokens,
      group.cost,
      `usage_summary.groups[${{index}}]`,
      group.input_semantics === "cache_inclusive" && callCount > 1n,
    );
    const compatibilityKey = JSON.stringify([
      group.call_kind,
      group.model_id,
      group.profile_id,
      group.provenance,
      group.input_semantics,
      group.coverage.input,
      group.coverage.output,
      group.coverage.cache_creation_input,
      group.coverage.cache_read_input,
    ]);
    if (compatibilityKeys.has(compatibilityKey)) {{
      fail(`usage_summary.groups[${{index}}]`, "a unique compatibility key");
    }}
    compatibilityKeys.add(compatibilityKey);
    const profileBytes = encoder.encode(group.profile_id).length;
    if (profileBytes === 0 || profileBytes > 256) {{
      fail(`usage_summary.groups[${{index}}].profile_id`, "1 through 256 UTF-8 bytes");
    }}
    for (const axis of ["input", "output", "cache_creation_input", "cache_read_input"]) {{
      if (group.coverage[axis] !== (group.tokens[axis] !== null)) {{
        fail(`usage_summary.groups[${{index}}].coverage.${{axis}}`, "consistent with token evidence");
      }}
      if (
        group.tokens[axis] !== null &&
        BigInt(group.tokens[axis]) > callCount * 18446744073709551615n
      ) {{
        fail(`usage_summary.groups[${{index}}].tokens.${{axis}}`, "bounded by call_count times u64::MAX");
      }}
    }}
  }});
  return value;
}}

export function decodeWebUsageCallPage(value, order) {{
  assertSchema(schemas.WebUsageCallPage, schemas.WebUsageCallPage, value, "usage_call_page");
  if (order !== "newest") {{
    fail("usage_call_page.order", "newest");
  }}
  let previousKey = null;
  value.calls.forEach((call, index) => {{
    assertUsageEvidence(
      call.input_semantics,
      call.tokens,
      call.cost,
      `usage_call_page.calls[${{index}}]`,
      false,
    );
    const isCompaction = call.call_kind === "context_compaction";
    if (!Object.hasOwn(call, "turn_id") || isCompaction !== (call.turn_id === null)) {{
      fail(
        `usage_call_page.calls[${{index}}].turn_id`,
        "null exactly for context compaction calls",
      );
    }}
    const key = {{ recordedAt: BigInt(call.recorded_at_micros), callId: call.call_id }};
    if (previousKey !== null) {{
      const comparison = key.recordedAt === previousKey.recordedAt
        ? key.callId < previousKey.callId ? -1 : key.callId > previousKey.callId ? 1 : 0
        : key.recordedAt < previousKey.recordedAt ? -1 : 1;
      if (comparison >= 0) {{
        fail(
          `usage_call_page.calls[${{index}}]`,
          "strictly descending by call key",
        );
      }}
    }}
    previousKey = key;
  }});
  if (value.continuation != null) {{
    const lastCall = value.calls.at(-1);
    if (
      lastCall === undefined ||
      value.continuation.recorded_at_micros !== lastCall.recorded_at_micros ||
      value.continuation.call_id !== lastCall.call_id
    ) {{
      fail("usage_call_page.continuation", "a cursor anchored to the final usage call");
    }}
  }}
  return value;
}}

function assertUsageEvidence(inputSemantics, tokens, cost, path, allowHiddenInvalidBreakdown) {{
  if (cost.status === "derived") {{
    const rateVersionBytes = new TextEncoder().encode(cost.rate_version).length;
    if (rateVersionBytes === 0 || rateVersionBytes > 128) {{
      fail(`${{path}}.cost.rate_version`, "1 through 128 UTF-8 bytes");
    }}
  }}
  const hasTokenEvidence = Object.values(tokens).some((value) => value !== null);
  const incompleteCacheAxes =
    inputSemantics === "cache_inclusive" &&
    tokens.input !== null &&
    tokens.output === null &&
    tokens.cache_creation_input === null &&
    tokens.cache_read_input === null;
  const invalidCacheBreakdown =
    inputSemantics === "cache_inclusive" &&
    tokens.input !== null &&
    tokens.cache_creation_input !== null &&
    tokens.cache_read_input !== null &&
    BigInt(tokens.input) <
      BigInt(tokens.cache_creation_input) + BigInt(tokens.cache_read_input);
  const requiredReason = !hasTokenEvidence
    ? "no_token_evidence"
    : inputSemantics === "unknown"
      ? "unknown_input_semantics"
      : incompleteCacheAxes
        ? "incomplete_cache_axes"
        : invalidCacheBreakdown
          ? "invalid_cache_breakdown"
          : null;
  if (requiredReason !== null) {{
    if (cost.status !== "unavailable" || cost.reason !== requiredReason) {{
      fail(`${{path}}.cost`, `unavailable with reason ${{requiredReason}}`);
    }}
    return;
  }}
  if (
    cost.status === "unavailable" &&
    (cost.reason === "no_token_evidence" ||
      cost.reason === "unknown_input_semantics" ||
      cost.reason === "incomplete_cache_axes" ||
      (cost.reason === "invalid_cache_breakdown" && !allowHiddenInvalidBreakdown))
  ) {{
    fail(`${{path}}.cost.reason`, "consistent with token evidence and input semantics");
  }}
}}
"##,
        contract_name = WEB_CONTRACT_NAME,
        contract_version = WEB_CONTRACT_VERSION,
        max_search_snippet_bytes = max_search_snippet_bytes(),
    ))
}

fn declaration_module(schemas: &GeneratedSchemas) -> Result<String, GenerateWebContractError> {
    let mut definitions = BTreeMap::new();
    let bootstrap = typescript_type(&schemas.bootstrap, &schemas.bootstrap, &mut definitions)?;
    let example = typescript_type(&schemas.example, &schemas.example, &mut definitions)?;
    let error = typescript_type(&schemas.error, &schemas.error, &mut definitions)?;
    let descriptor = typescript_type(&schemas.descriptor, &schemas.descriptor, &mut definitions)?;
    let window = typescript_type(&schemas.window, &schemas.window, &mut definitions)?;
    let search_page =
        typescript_type(&schemas.search_page, &schemas.search_page, &mut definitions)?;
    let usage_summary = typescript_type(
        &schemas.usage_summary,
        &schemas.usage_summary,
        &mut definitions,
    )?;
    let usage_call_page = typescript_type(
        &schemas.usage_call_page,
        &schemas.usage_call_page,
        &mut definitions,
    )?;
    let mut output = String::from(
        "// @generated by `cargo run -p signalbox-web-contract --bin generate-web-contract`.\n// Do not edit by hand.\n\n",
    );
    for (name, definition) in definitions {
        output.push_str(&format!("type {name} = {definition};\n\n"));
    }
    output.push_str(&format!(
        "export type WebContractBootstrap = {bootstrap};\n"
    ));
    output.push_str(&format!("export type WebContractExample = {example};\n\n"));
    output.push_str(&format!("export type WebApiErrorResponse = {error};\n\n"));
    output.push_str(&format!(
        "export type WebSessionTimelineDescriptor = {descriptor};\n\n"
    ));
    output.push_str(&format!(
        "export type WebSessionTimelineWindow = {window};\n\n"
    ));
    output.push_str(&format!("export type WebSearchPage = {search_page};\n\n"));
    output.push_str(&format!(
        "export type WebUsageSummary = {usage_summary};\n\n"
    ));
    output.push_str(&format!(
        "export type WebUsageCallPage = {usage_call_page};\n\n"
    ));
    output.push_str(
        "export function decodeWebContractBootstrap(value: unknown): WebContractBootstrap;\nexport function decodeWebContractExample(value: unknown): WebContractExample;\nexport function decodeWebApiErrorResponse(value: unknown): WebApiErrorResponse;\nexport function decodeWebSessionTimelineDescriptor(value: unknown): WebSessionTimelineDescriptor;\nexport function decodeWebSessionTimelineWindow(value: unknown): WebSessionTimelineWindow;\nexport function decodeWebSearchPage(value: unknown): WebSearchPage;\nexport function decodeWebUsageSummary(value: unknown): WebUsageSummary;\nexport function decodeWebUsageCallPage(value: unknown, order: \"newest\"): WebUsageCallPage;\n",
    );
    Ok(output)
}

fn typescript_type(
    root: &Value,
    schema: &Value,
    definitions: &mut BTreeMap<String, String>,
) -> Result<String, GenerateWebContractError> {
    if let Some(reference) = schema.get("$ref").and_then(Value::as_str) {
        let name = reference
            .strip_prefix("#/$defs/")
            .ok_or(GenerateWebContractError::UnsupportedSchema)?;
        if !definitions.contains_key(name) {
            definitions.insert(name.to_owned(), String::new());
            let definition = root
                .pointer(&format!("/$defs/{name}"))
                .ok_or(GenerateWebContractError::UnsupportedSchema)?;
            let rendered = typescript_type(root, definition, definitions)?;
            definitions.insert(name.to_owned(), rendered);
        }
        return Ok(name.to_owned());
    }
    if let Some(values) = schema.get("enum").and_then(Value::as_array) {
        return Ok(values
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join(" | "));
    }
    if let Some(value) = schema.get("const") {
        return Ok(value.to_string());
    }
    if let Some(variants) = schema.get("oneOf").and_then(Value::as_array) {
        return Ok(variants
            .iter()
            .map(|variant| typescript_type(root, variant, definitions))
            .collect::<Result<Vec<_>, _>>()?
            .join(" | "));
    }
    if let Some(variants) = schema.get("anyOf").and_then(Value::as_array) {
        return Ok(variants
            .iter()
            .map(|variant| typescript_type(root, variant, definitions))
            .collect::<Result<Vec<_>, _>>()?
            .join(" | "));
    }
    if let Some(types) = schema.get("type").and_then(Value::as_array) {
        return Ok(types
            .iter()
            .map(|kind| match kind.as_str() {
                Some("null") => Ok("null".to_owned()),
                Some(kind) => {
                    let mut concrete = schema.clone();
                    concrete["type"] = Value::String(kind.to_owned());
                    typescript_type(root, &concrete, definitions)
                }
                None => Err(GenerateWebContractError::UnsupportedSchema),
            })
            .collect::<Result<Vec<_>, _>>()?
            .join(" | "));
    }
    match schema.get("type").and_then(Value::as_str) {
        Some("object") => typescript_object(root, schema, definitions),
        Some("array") => {
            let item = schema
                .get("items")
                .ok_or(GenerateWebContractError::UnsupportedSchema)?;
            Ok(format!(
                "ReadonlyArray<{}>",
                typescript_type(root, item, definitions)?
            ))
        }
        Some("integer" | "number") => Ok("number".to_owned()),
        Some("boolean") => Ok("boolean".to_owned()),
        Some("null") => Ok("null".to_owned()),
        Some("string") => Ok("string".to_owned()),
        _ => Err(GenerateWebContractError::UnsupportedSchema),
    }
}

fn typescript_object(
    root: &Value,
    schema: &Value,
    definitions: &mut BTreeMap<String, String>,
) -> Result<String, GenerateWebContractError> {
    let properties = schema
        .get("properties")
        .and_then(Value::as_object)
        .ok_or(GenerateWebContractError::UnsupportedSchema)?;
    let required = schema.get("required").and_then(Value::as_array);
    let mut output = String::from("{\n");
    for (name, property) in properties {
        let optional =
            if required.is_some_and(|required| required.iter().any(|required| required == name)) {
                ""
            } else {
                "?"
            };
        output.push_str(&format!(
            "  readonly {name}{optional}: {};\n",
            typescript_type(root, property, definitions)?
        ));
    }
    output.push('}');
    Ok(output)
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path};

    use super::{
        WebContractBootstrap, WebContractExample, WebDollarAmount, WebSessionId,
        WebTimelineEventSequence, WebU64, WebUsageCallCount, WebUsageRateVersion,
        WebUsageTimestampMicros, generated_artifacts,
    };

    #[track_caller]
    fn assert_generated_artifact_current(path: &str) {
        let repository_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let artifact = generated_artifacts()
            .expect("the Rust schemas can generate browser artifacts")
            .into_iter()
            .find(|artifact| artifact.path == path)
            .expect("the requested generated artifact exists");
        let checked_in = fs::read_to_string(repository_root.join(artifact.path))
            .expect("generated web-contract artifact is checked in");

        assert_eq!(checked_in, artifact.contents);
    }

    #[test]
    fn checked_in_runtime_decoder_matches_rust_authority() {
        assert_generated_artifact_current("clients/web/src/generated/web-contract.mjs");
    }

    #[test]
    fn checked_in_typescript_declarations_match_rust_authority() {
        assert_generated_artifact_current("clients/web/src/generated/web-contract.d.mts");
    }

    #[test]
    fn checked_in_round_trip_fixture_matches_rust_authority() {
        assert_generated_artifact_current("crates/web-contract/tests/fixtures/example.json");
    }

    #[test]
    fn bootstrap_round_trips_through_its_rust_dto() {
        let bootstrap = WebContractBootstrap::current();
        let encoded = serde_json::to_vec(&bootstrap).expect("bootstrap serializes");
        let decoded: WebContractBootstrap =
            serde_json::from_slice(&encoded).expect("bootstrap decodes");

        assert_eq!(decoded, bootstrap);
    }

    #[test]
    fn generated_example_fixture_round_trips_through_its_rust_dto() {
        let fixture = include_str!("../tests/fixtures/example.json");
        let decoded: WebContractExample =
            serde_json::from_str(fixture).expect("generated example decodes");
        let encoded = serde_json::to_value(&decoded).expect("generated example re-encodes");
        let fixture_value: serde_json::Value =
            serde_json::from_str(fixture).expect("generated fixture is JSON");

        assert_eq!(encoded, fixture_value);
    }

    #[test]
    fn timeline_event_sequence_rejects_invalid_wire_spellings() {
        assert!(serde_json::from_str::<WebTimelineEventSequence>(r#""0""#).is_err());
        assert!(serde_json::from_str::<WebTimelineEventSequence>(r#""+1""#).is_err());
        assert!(serde_json::from_str::<WebTimelineEventSequence>(r#""01""#).is_err());
        assert!(
            serde_json::from_str::<WebTimelineEventSequence>(r#""18446744073709551616""#).is_err()
        );
        assert!(serde_json::from_str::<WebTimelineEventSequence>(r#""1""#).is_ok());
    }

    #[test]
    fn unsigned_wire_value_rejects_invalid_spellings() {
        assert!(serde_json::from_str::<WebU64>(r#""+1""#).is_err());
        assert!(serde_json::from_str::<WebU64>(r#""01""#).is_err());
        assert!(serde_json::from_str::<WebU64>(r#""18446744073709551616""#).is_err());
        assert!(serde_json::from_str::<WebU64>(r#""0""#).is_ok());
        assert!(serde_json::from_str::<WebU64>(r#""18446744073709551615""#).is_ok());
    }

    #[test]
    fn usage_call_count_requires_canonical_positive_u64() {
        assert!(serde_json::from_str::<WebUsageCallCount>(r#""0""#).is_err());
        assert!(serde_json::from_str::<WebUsageCallCount>(r#""01""#).is_err());
        assert!(serde_json::from_str::<WebUsageCallCount>(r#""1""#).is_ok());
        assert!(serde_json::from_str::<WebUsageCallCount>(r#""10000""#).is_ok());
        assert!(serde_json::from_str::<WebUsageCallCount>(r#""10001""#).is_err());
    }

    #[test]
    fn usage_timestamp_requires_the_application_range() {
        assert!(serde_json::from_str::<WebUsageTimestampMicros>(r#""0""#).is_ok());
        assert!(serde_json::from_str::<WebUsageTimestampMicros>(r#""253402300799999999""#).is_ok());
        assert!(
            serde_json::from_str::<WebUsageTimestampMicros>(r#""253402300800000000""#).is_err()
        );
    }

    #[test]
    fn usage_rate_version_requires_one_through_128_utf8_bytes() {
        assert!(serde_json::from_str::<WebUsageRateVersion>(r#""""#).is_err());
        let oversized = serde_json::to_string(&"é".repeat(65)).expect("fixture serializes");
        assert!(serde_json::from_str::<WebUsageRateVersion>(&oversized).is_err());
        assert!(serde_json::from_str::<WebUsageRateVersion>(r#""fixture-v2""#).is_ok());
    }

    #[test]
    fn dollar_amount_rejects_noncanonical_wire_spellings() {
        assert!(serde_json::from_str::<WebDollarAmount>(r#""-1""#).is_err());
        assert!(serde_json::from_str::<WebDollarAmount>(r#""01""#).is_err());
        assert!(serde_json::from_str::<WebDollarAmount>(r#""1.""#).is_err());
        assert!(
            serde_json::from_str::<WebDollarAmount>(r#""0.12345678901234567890123456789""#)
                .is_err()
        );
        assert!(serde_json::from_str::<WebDollarAmount>(r#""0""#).is_ok());
        assert!(serde_json::from_str::<WebDollarAmount>(r#""0.17""#).is_ok());
        assert!(serde_json::from_str::<WebDollarAmount>(r#""0.170""#).is_err());
        assert!(
            serde_json::from_str::<WebDollarAmount>(r#""79228162514264337593543950336""#).is_err()
        );
    }

    #[test]
    fn session_id_rejects_noncanonical_uuid_spellings() {
        assert!(serde_json::from_str::<WebSessionId>(r#""not-a-uuid""#).is_err());
        assert!(
            serde_json::from_str::<WebSessionId>(r#""00000000-0000-0000-0000-000000000991""#)
                .is_ok()
        );
    }
}
