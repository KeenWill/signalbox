//! Rust-authoritative browser HTTP data-transfer contract.
//!
//! The generated TypeScript declarations and runtime decoders are derived from
//! these serde and JSON Schema definitions. Browser DTOs deliberately remain
//! distinct from domain, persistence, and local process-protocol values.

use std::{collections::BTreeMap, error::Error, fmt};

use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize, de};
use serde_json::{Value, json};
use signalbox_application::{max_timeline_window_bytes, max_timeline_window_items};

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
            },
            limits: WebContractLimits {
                max_json_body_bytes: MAX_JSON_BODY_BYTES as u32,
                max_ndjson_item_bytes: MAX_NDJSON_ITEM_BYTES as u32,
                max_timeline_window_items: u32::from(max_timeline_window_items()),
                max_timeline_window_bytes: max_timeline_window_bytes(),
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

/// Checked canonical UUID used for browser-visible turn identities.
#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(transparent)]
pub struct WebTurnId(
    #[schemars(regex(
        pattern = r"^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$"
    ))]
    String,
);

impl WebTurnId {
    /// Constructs a canonical lowercase UUID from its 16 wire-order bytes.
    #[must_use]
    pub fn from_uuid_bytes(bytes: [u8; 16]) -> Self {
        Self(WebSessionId::from_uuid_bytes(bytes).0)
    }
}

impl<'de> Deserialize<'de> for WebTurnId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        canonical_session_id(&value)
            .then_some(Self(value))
            .ok_or_else(|| de::Error::custom("turn ID must be a canonical lowercase UUID"))
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

/// Checked positive unsigned 64-bit value encoded losslessly for JavaScript.
#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(transparent)]
pub struct WebPositiveU64(#[schemars(regex(pattern = r"^[1-9][0-9]*$"))] String);

impl WebPositiveU64 {
    /// Encodes one already-validated positive value in canonical decimal form.
    #[must_use]
    pub fn from_nonzero(value: std::num::NonZeroU64) -> Self {
        Self(value.get().to_string())
    }

    /// Returns the canonical positive decimal wire spelling.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for WebPositiveU64 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        let positive = canonical_u64(&value).and_then(std::num::NonZeroU64::new);
        if positive.is_none() {
            return Err(de::Error::custom(
                "wire value must be a canonical positive u64",
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
    #[serde(deserialize_with = "deserialize_present_option")]
    #[schemars(required)]
    pub continuation_before: Option<WebTimelineAddress>,
    #[serde(deserialize_with = "deserialize_present_option")]
    #[schemars(required)]
    pub continuation_after: Option<WebTimelineAddress>,
}

fn deserialize_present_option<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

/// Title summaries carry at most this many Unicode scalar values; production
/// truncates longer stored titles to exactly this bound and marks
/// `title_truncated`.
const MAX_ATTENTION_TITLE_SCALARS: u32 = 128;

/// Present-nullable bounded title text. `#[schemars(required)]` alone renders
/// an `Option` field as its inner type, dropping the `null` arm, so this keeps
/// `null` a legal value while absence stays rejected.
fn nullable_title_summary_schema(_generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({
        "type": ["string", "null"],
        "maxLength": MAX_ATTENTION_TITLE_SCALARS,
    })
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

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WebAttentionState {
    Active,
    Queued,
    Blocked,
    AwaitingApproval,
    Ambiguous,
    AwaitingReconciliation,
    RunnerLost,
    Idle,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WebAttentionAction {
    ProvideGoalNeed,
    DecideApproval,
    ReconcileTurn,
    RestoreRunner,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WebAttentionBlockedReason {
    UserInputRequired,
    ExternalChangeRequired,
    AuthorizationRequired,
    ExecutionFailure,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WebAttentionActivityKind {
    Session,
    Turn,
    Goal,
    ApprovalJudge,
    Runner,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WebAttentionGoalBlock {
    /// Goal generations are strictly positive in the domain and its storage
    /// constraint, so zero is not a valid wire spelling.
    pub generation: WebPositiveU64,
    pub reason: WebAttentionBlockedReason,
    /// At least 1 and at most 128 Unicode scalar values; exact text is in
    /// session detail. The stored goal need is never empty, so an empty
    /// summary is contract-invalid.
    #[schemars(length(min = 1, max = 128))]
    pub need_summary: String,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WebAttentionJudgeFacts {
    pub actionable: WebU64,
    pub completed: WebU64,
    pub escalated: WebU64,
    pub failed: WebU64,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WebAttentionActivity {
    pub unix_microseconds: WebU64,
    pub kind: WebAttentionActivityKind,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WebAttentionSummary {
    pub session_id: WebSessionId,
    #[serde(deserialize_with = "deserialize_present_option")]
    #[schemars(required, schema_with = "nullable_title_summary_schema")]
    pub title_summary: Option<String>,
    pub title_truncated: bool,
    pub archived: bool,
    #[serde(deserialize_with = "deserialize_present_option")]
    #[schemars(required)]
    pub current_turn_id: Option<WebTurnId>,
    pub active_turn_count: WebU64,
    pub queued_turn_count: WebU64,
    pub state: WebAttentionState,
    pub action: Option<WebAttentionAction>,
    pub goal_block: Option<WebAttentionGoalBlock>,
    pub judge: WebAttentionJudgeFacts,
    pub last_activity: WebAttentionActivity,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WebAttentionSort {
    LastActivityDescending,
    SessionIdentityAscending,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum WebAttentionContinuation {
    LastActivity {
        unix_microseconds: WebU64,
        session_id: WebSessionId,
    },
    SessionIdentity {
        session_id: WebSessionId,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WebAttentionSnapshot {
    pub cursor: WebU64,
    pub total: WebU64,
    pub sort: WebAttentionSort,
    #[schemars(length(max = 16))]
    pub summaries: Vec<WebAttentionSummary>,
    #[serde(deserialize_with = "deserialize_present_option")]
    #[schemars(required)]
    pub continuation: Option<WebAttentionContinuation>,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum WebAttentionStreamEvent {
    Snapshot {
        snapshot: WebAttentionSnapshot,
    },
    Update {
        cursor: WebU64,
        #[schemars(length(min = 1, max = 16))]
        summaries: Vec<WebAttentionSummary>,
    },
    ResyncRequired {
        cursor: WebU64,
    },
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

/// Produces all checked-in browser contract artifacts.
///
/// # Errors
///
/// Returns a closed build-time error when serde cannot encode a generated value
/// or a DTO schema grows beyond the generator's focused supported shapes.
pub fn generated_artifacts() -> Result<Vec<GeneratedArtifact>, GenerateWebContractError> {
    let bootstrap_schema = canonical_schema(schemars::schema_for!(WebContractBootstrap).to_value());
    let example_schema = canonical_schema(schemars::schema_for!(WebContractExample).to_value());
    let error_schema = canonical_schema(schemars::schema_for!(WebApiErrorResponse).to_value());
    let descriptor_schema =
        canonical_schema(schemars::schema_for!(WebSessionTimelineDescriptor).to_value());
    let mut window_schema =
        canonical_schema(schemars::schema_for!(WebSessionTimelineWindow).to_value());
    make_property_nullable(&mut window_schema, "continuation_before")?;
    make_property_nullable(&mut window_schema, "continuation_after")?;
    let mut attention_snapshot_schema =
        canonical_schema(schemars::schema_for!(WebAttentionSnapshot).to_value());
    make_property_nullable(&mut attention_snapshot_schema, "continuation")?;
    make_pointer_nullable(
        &mut attention_snapshot_schema,
        "/$defs/WebAttentionSummary/properties/current_turn_id",
    )?;
    let mut attention_event_schema =
        canonical_schema(schemars::schema_for!(WebAttentionStreamEvent).to_value());
    make_pointer_nullable(
        &mut attention_event_schema,
        "/$defs/WebAttentionSnapshot/properties/continuation",
    )?;
    make_pointer_nullable(
        &mut attention_event_schema,
        "/$defs/WebAttentionSummary/properties/current_turn_id",
    )?;
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
            contents: runtime_module(
                &bootstrap_schema,
                &example_schema,
                &error_schema,
                &descriptor_schema,
                &window_schema,
                &attention_snapshot_schema,
                &attention_event_schema,
            )?,
        },
        GeneratedArtifact {
            path: "clients/web/src/generated/web-contract.d.mts",
            contents: declaration_module(
                &bootstrap_schema,
                &example_schema,
                &error_schema,
                &descriptor_schema,
                &window_schema,
                &attention_snapshot_schema,
                &attention_event_schema,
            )?,
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

fn make_property_nullable(
    schema: &mut Value,
    property_name: &str,
) -> Result<(), GenerateWebContractError> {
    make_pointer_nullable(schema, &format!("/properties/{property_name}"))
}

fn make_pointer_nullable(
    schema: &mut Value,
    pointer: &str,
) -> Result<(), GenerateWebContractError> {
    let property = schema
        .pointer_mut(pointer)
        .ok_or(GenerateWebContractError::UnsupportedSchema)?;
    let concrete = property.take();
    *property = json!({ "anyOf": [concrete, { "type": "null" }] });
    Ok(())
}

fn runtime_module(
    bootstrap_schema: &Value,
    example_schema: &Value,
    error_schema: &Value,
    descriptor_schema: &Value,
    window_schema: &Value,
    attention_snapshot_schema: &Value,
    attention_event_schema: &Value,
) -> Result<String, GenerateWebContractError> {
    let mut schemas = json!({
        "WebContractBootstrap": bootstrap_schema,
        "WebContractExample": example_schema,
        "WebApiErrorResponse": error_schema,
        "WebSessionTimelineDescriptor": descriptor_schema,
        "WebSessionTimelineWindow": window_schema,
        "WebAttentionSnapshot": attention_snapshot_schema,
        "WebAttentionStreamEvent": attention_event_schema,
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

function isWellFormedUnicode(value) {{
  for (let index = 0; index < value.length; index += 1) {{
    const codeUnit = value.charCodeAt(index);
    if (codeUnit >= 0xd800 && codeUnit <= 0xdbff) {{
      const next = value.charCodeAt(index + 1);
      if (!(next >= 0xdc00 && next <= 0xdfff)) {{
        return false;
      }}
      index += 1;
    }} else if (codeUnit >= 0xdc00 && codeUnit <= 0xdfff) {{
      return false;
    }}
  }}
  return true;
}}

function exceedsScalarLength(value, maxLength) {{
  let count = 0;
  const scalars = value[Symbol.iterator]();
  while (!scalars.next().done) {{
    count += 1;
    if (count > maxLength) {{
      return true;
    }}
  }}
  return false;
}}

function scalarLengthAtLeast(value, minLength) {{
  let count = 0;
  const scalars = value[Symbol.iterator]();
  while (!scalars.next().done) {{
    count += 1;
    if (count >= minLength) {{
      return true;
    }}
  }}
  return count >= minLength;
}}

function scalarLength(value) {{
  let count = 0;
  const scalars = value[Symbol.iterator]();
  while (!scalars.next().done) {{
    count += 1;
  }}
  return count;
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
    const actual = Array.isArray(value) ? "array" : typeof value;
    if (!concrete.includes(actual)) {{
      fail(path, concrete.join(" or "));
    }}
    assertSchema(root, {{ ...schema, type: actual }}, value, path);
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
    if (schema.minItems !== undefined && value.length < schema.minItems) {{
      fail(path, `at least ${{schema.minItems}} items`);
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
  if (schema.type === "string" && !isWellFormedUnicode(value)) {{
    fail(path, "well-formed Unicode text");
  }}
  if (
    schema.type === "string" &&
    schema.maxLength !== undefined &&
    exceedsScalarLength(value, schema.maxLength)
  ) {{
    fail(path, `at most ${{schema.maxLength}} Unicode scalar values`);
  }}
  if (
    schema.type === "string" &&
    schema.minLength !== undefined &&
    !scalarLengthAtLeast(value, schema.minLength)
  ) {{
    fail(path, `at least ${{schema.minLength}} Unicode scalar values`);
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
    (schema.pattern === "^[1-9][0-9]*$" || schema.pattern === "^(0|[1-9][0-9]*)$") &&
    BigInt(value) > 18446744073709551615n
  ) {{
    fail(path, "an unsigned 64-bit integer");
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

export function decodeWebAttentionSnapshot(value) {{
  assertSchema(schemas.WebAttentionSnapshot, schemas.WebAttentionSnapshot, value, "attention_snapshot");
  assertAttentionSnapshot(value, "attention_snapshot");
  return value;
}}

export function decodeWebAttentionStreamEvent(value) {{
  assertSchema(schemas.WebAttentionStreamEvent, schemas.WebAttentionStreamEvent, value, "attention_event");
  if (value.kind === "snapshot") {{
    assertAttentionSnapshot(value.snapshot, "attention_event.snapshot");
    if (value.snapshot.sort !== "last_activity_descending") {{
      fail("attention_event.snapshot.sort", "the fixed hot-page activity sort");
    }}
    assertUnarchivedSummaries(value.snapshot.summaries, "attention_event.snapshot.summaries");
  }} else {{
    value.summaries?.forEach((summary, index) =>
      assertAttentionSummary(summary, `attention_event.summaries[${{index}}]`),
    );
    assertUnarchivedSummaries(value.summaries ?? [], "attention_event.summaries");
    const identities = new Set();
    for (const summary of value.summaries ?? []) {{
      if (identities.has(summary.session_id)) {{
        fail("attention_event.summaries", "at most one replacement per session");
      }}
      identities.add(summary.session_id);
    }}
  }}
  return value;
}}

function assertUnarchivedSummaries(summaries, path) {{
  summaries.forEach((summary, index) => {{
    if (summary.archived) {{
      fail(`${{path}}[${{index}}].archived`, "false on the hot attention stream");
    }}
  }});
}}

function assertAttentionSnapshot(snapshot, path) {{
  snapshot.summaries.forEach((summary, index) =>
    assertAttentionSummary(summary, `${{path}}.summaries[${{index}}]`),
  );
  for (let index = 1; index < snapshot.summaries.length; index += 1) {{
    const previous = snapshot.summaries[index - 1];
    const current = snapshot.summaries[index];
    let ordered;
    if (snapshot.sort === "session_identity_ascending") {{
      ordered = previous.session_id < current.session_id;
    }} else {{
      const previousActivity = BigInt(previous.last_activity.unix_microseconds);
      const currentActivity = BigInt(current.last_activity.unix_microseconds);
      ordered =
        previousActivity > currentActivity ||
        (previousActivity === currentActivity && previous.session_id < current.session_id);
    }}
    if (!ordered) {{
      fail(`${{path}}.summaries[${{index}}]`, `strictly ordered by sort ${{snapshot.sort}}`);
    }}
  }}
  if (BigInt(snapshot.total) < BigInt(snapshot.summaries.length)) {{
    fail(`${{path}}.total`, "at least the number of returned summaries");
  }}
  const continuationKind = snapshot.continuation?.kind ?? null;
  const expectedContinuationKind = {{
    last_activity_descending: "last_activity",
    session_identity_ascending: "session_identity",
  }}[snapshot.sort];
  if (continuationKind !== null && continuationKind !== expectedContinuationKind) {{
    fail(`${{path}}.continuation`, `the continuation required by sort ${{snapshot.sort}}`);
  }}
  if (snapshot.continuation !== null) {{
    const boundary = snapshot.summaries[snapshot.summaries.length - 1];
    if (boundary === undefined) {{
      fail(`${{path}}.continuation`, "absent when no summaries are returned");
    }}
    if (snapshot.continuation.session_id !== boundary.session_id) {{
      fail(`${{path}}.continuation.session_id`, "the session of the last returned summary");
    }}
    if (
      snapshot.continuation.kind === "last_activity" &&
      snapshot.continuation.unix_microseconds !== boundary.last_activity.unix_microseconds
    ) {{
      fail(
        `${{path}}.continuation.unix_microseconds`,
        "the activity timestamp of the last returned summary",
      );
    }}
  }}
}}

function assertAttentionSummary(summary, path) {{
  const expectedAction = {{
    active: null,
    queued: null,
    blocked: "provide_goal_need",
    awaiting_approval: "decide_approval",
    ambiguous: "reconcile_turn",
    awaiting_reconciliation: "reconcile_turn",
    runner_lost: "restore_runner",
    idle: null,
  }}[summary.state];
  if (summary.action !== expectedAction) {{
    fail(`${{path}}.action`, `the action required by state ${{summary.state}}`);
  }}
  const turnBacked = [
    "active",
    "queued",
    "awaiting_approval",
    "ambiguous",
    "awaiting_reconciliation",
  ].includes(summary.state);
  if (turnBacked && summary.current_turn_id === null) {{
    fail(`${{path}}.current_turn_id`, `a turn identity for state ${{summary.state}}`);
  }}
  const activeBacked = ["active", "awaiting_approval", "ambiguous"].includes(summary.state);
  if (activeBacked && BigInt(summary.active_turn_count) === 0n) {{
    fail(`${{path}}.active_turn_count`, `at least one active turn for state ${{summary.state}}`);
  }}
  if (summary.state === "queued" && BigInt(summary.queued_turn_count) === 0n) {{
    fail(`${{path}}.queued_turn_count`, "at least one queued turn for queued state");
  }}
  const hasGoalBlock = Object.hasOwn(summary, "goal_block") && summary.goal_block !== null;
  if ((summary.state === "blocked") !== hasGoalBlock) {{
    fail(`${{path}}.goal_block`, "present exactly for blocked state");
  }}
  if (summary.title_summary === null && summary.title_truncated) {{
    fail(`${{path}}.title_truncated`, "false when title_summary is null");
  }}
  if (
    summary.title_truncated &&
    summary.title_summary !== null &&
    scalarLength(summary.title_summary) !== {max_title_scalars}
  ) {{
    fail(
      `${{path}}.title_summary`,
      "exactly {max_title_scalars} Unicode scalar values when title_truncated is true",
    );
  }}
}}
"##,
        contract_name = WEB_CONTRACT_NAME,
        max_title_scalars = MAX_ATTENTION_TITLE_SCALARS,
        contract_version = WEB_CONTRACT_VERSION,
    ))
}

fn declaration_module(
    bootstrap_schema: &Value,
    example_schema: &Value,
    error_schema: &Value,
    descriptor_schema: &Value,
    window_schema: &Value,
    attention_snapshot_schema: &Value,
    attention_event_schema: &Value,
) -> Result<String, GenerateWebContractError> {
    let mut definitions = BTreeMap::new();
    let bootstrap = typescript_type(bootstrap_schema, bootstrap_schema, &mut definitions)?;
    let example = typescript_type(example_schema, example_schema, &mut definitions)?;
    let error = typescript_type(error_schema, error_schema, &mut definitions)?;
    let descriptor = typescript_type(descriptor_schema, descriptor_schema, &mut definitions)?;
    let window = typescript_type(window_schema, window_schema, &mut definitions)?;
    let attention_snapshot = typescript_type(
        attention_snapshot_schema,
        attention_snapshot_schema,
        &mut definitions,
    )?;
    let attention_event = typescript_type(
        attention_event_schema,
        attention_event_schema,
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
    output.push_str(&format!(
        "export type WebAttentionSnapshot = {attention_snapshot};\n\n"
    ));
    output.push_str(&format!(
        "export type WebAttentionStreamEvent = {attention_event};\n\n"
    ));
    output.push_str(
        "export function decodeWebContractBootstrap(value: unknown): WebContractBootstrap;\nexport function decodeWebContractExample(value: unknown): WebContractExample;\nexport function decodeWebApiErrorResponse(value: unknown): WebApiErrorResponse;\nexport function decodeWebSessionTimelineDescriptor(value: unknown): WebSessionTimelineDescriptor;\nexport function decodeWebSessionTimelineWindow(value: unknown): WebSessionTimelineWindow;\nexport function decodeWebAttentionSnapshot(value: unknown): WebAttentionSnapshot;\nexport function decodeWebAttentionStreamEvent(value: unknown): WebAttentionStreamEvent;\n",
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
    let required = schema
        .get("required")
        .and_then(Value::as_array)
        .ok_or(GenerateWebContractError::UnsupportedSchema)?;
    let mut output = String::from("{\n");
    for (name, property) in properties {
        let optional = if required.iter().any(|required| required == name) {
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
        WebContractBootstrap, WebContractExample, WebSessionId, WebTimelineEventSequence, WebU64,
        generated_artifacts,
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
    fn session_id_rejects_noncanonical_uuid_spellings() {
        assert!(serde_json::from_str::<WebSessionId>(r#""not-a-uuid""#).is_err());
        assert!(
            serde_json::from_str::<WebSessionId>(r#""00000000-0000-0000-0000-000000000991""#)
                .is_ok()
        );
    }
}
