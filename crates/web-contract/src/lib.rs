//! Rust-authoritative browser HTTP data-transfer contract.
//!
//! The generated TypeScript declarations and runtime decoders are derived from
//! these serde and JSON Schema definitions. Browser DTOs deliberately remain
//! distinct from domain, persistence, and local process-protocol values.

use std::{collections::BTreeMap, error::Error, fmt};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

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
}

/// Effective hard limits clients must honor for this contract version.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WebContractLimits {
    /// Maximum bytes accepted for one JSON request body.
    pub max_json_body_bytes: u32,
    /// Maximum encoded bytes for one NDJSON item, excluding its newline.
    pub max_ndjson_item_bytes: u32,
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
            },
            limits: WebContractLimits {
                max_json_body_bytes: MAX_JSON_BODY_BYTES as u32,
                max_ndjson_item_bytes: MAX_NDJSON_ITEM_BYTES as u32,
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
    pub generation: String,
    pub reason: WebAttentionBlockedReason,
    /// At most 128 Unicode scalar values; exact text is in session detail.
    pub need_summary: String,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WebAttentionJudgeFacts {
    pub actionable: String,
    pub completed: String,
    pub escalated: String,
    pub failed: String,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WebAttentionActivity {
    pub unix_milliseconds: String,
    pub kind: WebAttentionActivityKind,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WebAttentionSummary {
    pub session_id: String,
    pub current_turn_id: Option<String>,
    pub state: WebAttentionState,
    pub action: Option<WebAttentionAction>,
    pub goal_block: Option<WebAttentionGoalBlock>,
    pub judge: WebAttentionJudgeFacts,
    pub last_activity: WebAttentionActivity,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WebAttentionSnapshot {
    pub cursor: String,
    #[schemars(length(max = 64))]
    pub summaries: Vec<WebAttentionSummary>,
    pub continuation_after_session_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WebAttentionStreamEvent {
    Snapshot {
        snapshot: WebAttentionSnapshot,
    },
    Update {
        cursor: String,
        #[schemars(length(max = 64))]
        summaries: Vec<WebAttentionSummary>,
    },
    ResyncRequired {
        cursor: String,
    },
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WebRepoWatchEventKind {
    PullRequestOpened,
    PullRequestClosed,
    PullRequestMerged,
    HeadChanged,
    MergeableStateChanged,
    ChecksCompleted,
    CheckRunCompleted,
    BranchWorkflowRunCompleted,
    ReviewSubmitted,
    ThreadOpened,
    ThreadResolved,
    Labeled,
    Unlabeled,
    BaseAdvanced,
    ReactionChanged,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WebRepoWatchEvent {
    pub id: String,
    pub cursor_generation: String,
    pub event_ordinal: u32,
    pub kind: WebRepoWatchEventKind,
    pub pull_request: Option<String>,
    pub observed_at_unix_milliseconds: String,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WebRepoWatchDispatch {
    pub id: String,
    pub event_id: String,
    pub rule: String,
    pub attempted_at_unix_milliseconds: String,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WebRepoWatchSettlement {
    pub dispatch_id: String,
    pub event_id: String,
    pub settled_at_unix_milliseconds: String,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WebRepoWatchLatestWebhook {
    pub receipt_sequence: String,
    pub event_name: String,
    pub action_name: Option<String>,
    pub received_at_unix_milliseconds: String,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WebRepoWatchWebhookWindow {
    pub seconds: u32,
    pub received: String,
    pub projected: String,
    pub terminal: String,
    pub quarantined: String,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WebRepoWatchEventKindCount {
    pub kind: WebRepoWatchEventKind,
    pub count: String,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WebRepoWatchRepositoryStatus {
    pub repository: String,
    pub cursor_generation: String,
    pub observed_at_unix_milliseconds: String,
    pub latest_webhook: Option<WebRepoWatchLatestWebhook>,
    pub previous_five_minutes: WebRepoWatchWebhookWindow,
    pub previous_hour: WebRepoWatchWebhookWindow,
    pub latest_projection_latency_milliseconds: Option<String>,
    pub maximum_projection_latency_milliseconds_previous_hour: Option<String>,
    pub event_kind_counts_previous_hour: Vec<WebRepoWatchEventKindCount>,
    pub last_observed_event: Option<WebRepoWatchEvent>,
    pub last_actionable_event: Option<WebRepoWatchEvent>,
    pub last_dispatch_attempt: Option<WebRepoWatchDispatch>,
    pub last_automation_settlement: Option<WebRepoWatchSettlement>,
    pub held_slot_count: String,
    pub queued_obligation_count: String,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WebRepoWatchRepositoryStatusPage {
    #[schemars(length(max = 64))]
    pub repositories: Vec<WebRepoWatchRepositoryStatus>,
    pub continuation_after_repository: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WebRepoWatchLifecycle {
    Open,
    Closed,
    Merged,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WebRepoWatchMergeable {
    Mergeable,
    Conflicting,
    Unknown,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WebRepoWatchDraftStatus {
    Draft,
    ReadyForReview,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WebRepoWatchChecksStatus {
    NoCompletedSuites,
    Passing,
    Failing,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WebRepoWatchReviewDecision {
    None,
    Commented,
    Approved,
    ChangesRequested,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WebRepoWatchAutomationStatus {
    Unattempted,
    Held {
        dispatch_id: String,
    },
    Queued {
        latest_event_id: String,
    },
    NonConverged {
        dispatch_id: String,
    },
    StaleSeal {
        dispatch_id: String,
        sealed_event_id: String,
    },
    CurrentHeadSealed {
        dispatch_id: String,
        sealed_event_id: String,
        settled_at_unix_milliseconds: String,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WebRepoWatchPullRequest {
    pub number: String,
    pub title: String,
    pub head: String,
    pub head_repository: String,
    pub head_branch: String,
    pub base_branch: String,
    pub lifecycle: WebRepoWatchLifecycle,
    pub mergeable: WebRepoWatchMergeable,
    pub draft: WebRepoWatchDraftStatus,
    pub checks: WebRepoWatchChecksStatus,
    pub review_decision: WebRepoWatchReviewDecision,
    pub stale_review_count: String,
    pub unresolved_thread_count: String,
    pub open_parent: Option<String>,
    pub open_child_count: String,
    pub automation: WebRepoWatchAutomationStatus,
    pub last_observed_event: Option<WebRepoWatchEvent>,
    pub last_actionable_event: Option<WebRepoWatchEvent>,
    pub last_dispatch_attempt: Option<WebRepoWatchDispatch>,
    pub last_automation_settlement: Option<WebRepoWatchSettlement>,
    pub held_slot_count: String,
    pub queued_obligation_count: String,
    pub commissioned_session_count: String,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WebRepoWatchPullRequestPage {
    pub repository: String,
    #[schemars(length(max = 64))]
    pub pull_requests: Vec<WebRepoWatchPullRequest>,
    pub continuation_after_pull_request: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WebRepoWatchHeldSlotBlocker {
    UndeliveredAction,
    DeliveryTurnRuntimeRelevant,
    LiveRuntimeTurn,
    PursuingGoal,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WebRepoWatchHeldSlot {
    pub dispatch_id: String,
    pub pull_request: Option<String>,
    pub rule: String,
    pub held_since_unix_milliseconds: String,
    pub session_ids: Vec<String>,
    pub blockers: Vec<WebRepoWatchHeldSlotBlocker>,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WebRepoWatchObligationReadiness {
    Ready,
    Occupied {
        dispatch_id: String,
        session_ids: Vec<String>,
    },
    Cooldown {
        eligible_at_unix_milliseconds: Option<String>,
    },
    Parked {
        parked_at_unix_milliseconds: String,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WebRepoWatchQueuedObligation {
    pub id: String,
    pub pull_request: Option<String>,
    pub rule: String,
    pub first_event_id: String,
    pub latest_event_id: String,
    pub matched_event_count: String,
    pub owed_since_unix_milliseconds: String,
    pub latest_match_at_unix_milliseconds: String,
    pub failed_attempts: String,
    pub readiness: WebRepoWatchObligationReadiness,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WebRepoWatchHeldCursor {
    pub held_since_unix_milliseconds: String,
    pub dispatch_id: String,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WebRepoWatchObligationCursor {
    pub owed_since_unix_milliseconds: String,
    pub obligation_id: String,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WebRepoWatchWorkPage {
    #[schemars(length(max = 64))]
    pub held_slots: Vec<WebRepoWatchHeldSlot>,
    pub held_continuation_after: Option<WebRepoWatchHeldCursor>,
    #[schemars(length(max = 64))]
    pub queued_obligations: Vec<WebRepoWatchQueuedObligation>,
    pub obligation_continuation_after: Option<WebRepoWatchObligationCursor>,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WebRepoWatchSessionPurpose {
    RuleDispatch {
        dispatch_id: String,
        event_id: String,
        rule: String,
        template: String,
    },
    OperatorCommission {
        dispatch_id: String,
        template: String,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WebRepoWatchPullRequestSession {
    pub commissioned_at_unix_milliseconds: String,
    pub purpose: WebRepoWatchSessionPurpose,
    pub attention: WebAttentionSummary,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WebRepoWatchSessionCursor {
    pub commissioned_at_unix_milliseconds: String,
    pub session_id: String,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WebRepoWatchPullRequestSessionPage {
    #[schemars(length(max = 64))]
    pub sessions: Vec<WebRepoWatchPullRequestSession>,
    pub continuation_before: Option<WebRepoWatchSessionCursor>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WebRepoWatchWebhookDisposition {
    Projected,
    DuplicateState,
    Superseded,
    Ignored,
    Quarantined,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WebRepoWatchWebhookActivity {
    pub receipt_sequence: String,
    pub event_name: String,
    pub action_name: Option<String>,
    pub received_at_unix_milliseconds: String,
    pub projection_count: String,
    pub latest_projected_at_unix_milliseconds: Option<String>,
    pub disposition: Option<WebRepoWatchWebhookDisposition>,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WebRepoWatchEventCursor {
    pub cursor_generation: String,
    pub event_ordinal: u32,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WebRepoWatchActivityPage {
    #[schemars(length(max = 100))]
    pub events: Vec<WebRepoWatchEvent>,
    pub event_continuation_before: Option<WebRepoWatchEventCursor>,
    #[schemars(length(max = 100))]
    pub webhooks: Vec<WebRepoWatchWebhookActivity>,
    pub webhook_continuation_before_receipt_sequence: Option<String>,
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
    let schema_set = GeneratedSchemaSet {
        bootstrap: canonical_schema(schemars::schema_for!(WebContractBootstrap).to_value()),
        example: canonical_schema(schemars::schema_for!(WebContractExample).to_value()),
        error: canonical_schema(schemars::schema_for!(WebApiErrorResponse).to_value()),
        attention_snapshot: canonical_schema(
            schemars::schema_for!(WebAttentionSnapshot).to_value(),
        ),
        attention_event: canonical_schema(
            schemars::schema_for!(WebAttentionStreamEvent).to_value(),
        ),
        repository_status_page: canonical_schema(
            schemars::schema_for!(WebRepoWatchRepositoryStatusPage).to_value(),
        ),
        pull_request_page: canonical_schema(
            schemars::schema_for!(WebRepoWatchPullRequestPage).to_value(),
        ),
        work_page: canonical_schema(schemars::schema_for!(WebRepoWatchWorkPage).to_value()),
        pull_request_session_page: canonical_schema(
            schemars::schema_for!(WebRepoWatchPullRequestSessionPage).to_value(),
        ),
        activity_page: canonical_schema(schemars::schema_for!(WebRepoWatchActivityPage).to_value()),
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
            contents: runtime_module(&schema_set)?,
        },
        GeneratedArtifact {
            path: "clients/web/src/generated/web-contract.d.mts",
            contents: declaration_module(&schema_set)?,
        },
        GeneratedArtifact {
            path: "crates/web-contract/tests/fixtures/example.json",
            contents: example_json,
        },
    ])
}

struct GeneratedSchemaSet {
    bootstrap: Value,
    example: Value,
    error: Value,
    attention_snapshot: Value,
    attention_event: Value,
    repository_status_page: Value,
    pull_request_page: Value,
    work_page: Value,
    pull_request_session_page: Value,
    activity_page: Value,
}

fn canonical_schema(mut schema: Value) -> Value {
    // Schemars' default feature set preserves declaration order, while its
    // focused no-default build uses sorted maps. Workspace feature unification
    // must not change checked-in artifacts.
    schema.sort_all_objects();
    schema
}

fn runtime_module(schema_set: &GeneratedSchemaSet) -> Result<String, GenerateWebContractError> {
    let mut schemas = json!({
        "WebContractBootstrap": schema_set.bootstrap,
        "WebContractExample": schema_set.example,
        "WebApiErrorResponse": schema_set.error,
        "WebAttentionSnapshot": schema_set.attention_snapshot,
        "WebAttentionStreamEvent": schema_set.attention_event,
        "WebRepoWatchRepositoryStatusPage": schema_set.repository_status_page,
        "WebRepoWatchPullRequestPage": schema_set.pull_request_page,
        "WebRepoWatchWorkPage": schema_set.work_page,
        "WebRepoWatchPullRequestSessionPage": schema_set.pull_request_session_page,
        "WebRepoWatchActivityPage": schema_set.activity_page,
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

export function decodeWebAttentionSnapshot(value) {{
  assertSchema(schemas.WebAttentionSnapshot, schemas.WebAttentionSnapshot, value, "attention_snapshot");
  return value;
}}

export function decodeWebAttentionStreamEvent(value) {{
  assertSchema(schemas.WebAttentionStreamEvent, schemas.WebAttentionStreamEvent, value, "attention_event");
  return value;
}}

export function decodeWebRepoWatchRepositoryStatusPage(value) {{
  assertSchema(schemas.WebRepoWatchRepositoryStatusPage, schemas.WebRepoWatchRepositoryStatusPage, value, "repository_status_page");
  return value;
}}

export function decodeWebRepoWatchPullRequestPage(value) {{
  assertSchema(schemas.WebRepoWatchPullRequestPage, schemas.WebRepoWatchPullRequestPage, value, "pull_request_page");
  return value;
}}

export function decodeWebRepoWatchWorkPage(value) {{
  assertSchema(schemas.WebRepoWatchWorkPage, schemas.WebRepoWatchWorkPage, value, "work_page");
  return value;
}}

export function decodeWebRepoWatchPullRequestSessionPage(value) {{
  assertSchema(schemas.WebRepoWatchPullRequestSessionPage, schemas.WebRepoWatchPullRequestSessionPage, value, "pull_request_session_page");
  return value;
}}

export function decodeWebRepoWatchActivityPage(value) {{
  assertSchema(schemas.WebRepoWatchActivityPage, schemas.WebRepoWatchActivityPage, value, "activity_page");
  return value;
}}
"##,
        contract_name = WEB_CONTRACT_NAME,
        contract_version = WEB_CONTRACT_VERSION,
    ))
}

fn declaration_module(schema_set: &GeneratedSchemaSet) -> Result<String, GenerateWebContractError> {
    let mut definitions = BTreeMap::new();
    let bootstrap = typescript_type(
        &schema_set.bootstrap,
        &schema_set.bootstrap,
        &mut definitions,
    )?;
    let example = typescript_type(&schema_set.example, &schema_set.example, &mut definitions)?;
    let error = typescript_type(&schema_set.error, &schema_set.error, &mut definitions)?;
    let attention_snapshot = typescript_type(
        &schema_set.attention_snapshot,
        &schema_set.attention_snapshot,
        &mut definitions,
    )?;
    let attention_event = typescript_type(
        &schema_set.attention_event,
        &schema_set.attention_event,
        &mut definitions,
    )?;
    let repository_status_page = typescript_type(
        &schema_set.repository_status_page,
        &schema_set.repository_status_page,
        &mut definitions,
    )?;
    let pull_request_page = typescript_type(
        &schema_set.pull_request_page,
        &schema_set.pull_request_page,
        &mut definitions,
    )?;
    let work_page = typescript_type(
        &schema_set.work_page,
        &schema_set.work_page,
        &mut definitions,
    )?;
    let pull_request_session_page = typescript_type(
        &schema_set.pull_request_session_page,
        &schema_set.pull_request_session_page,
        &mut definitions,
    )?;
    let activity_page = typescript_type(
        &schema_set.activity_page,
        &schema_set.activity_page,
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
        "export type WebAttentionSnapshot = {attention_snapshot};\n\n"
    ));
    output.push_str(&format!(
        "export type WebAttentionStreamEvent = {attention_event};\n\n"
    ));
    output.push_str(&format!(
        "export type WebRepoWatchRepositoryStatusPage = {repository_status_page};\n\n"
    ));
    output.push_str(&format!(
        "export type WebRepoWatchPullRequestPage = {pull_request_page};\n\n"
    ));
    output.push_str(&format!(
        "export type WebRepoWatchWorkPage = {work_page};\n\n"
    ));
    output.push_str(&format!(
        "export type WebRepoWatchPullRequestSessionPage = {pull_request_session_page};\n\n"
    ));
    output.push_str(&format!(
        "export type WebRepoWatchActivityPage = {activity_page};\n\n"
    ));
    output.push_str(
        "export function decodeWebContractBootstrap(value: unknown): WebContractBootstrap;\nexport function decodeWebContractExample(value: unknown): WebContractExample;\nexport function decodeWebApiErrorResponse(value: unknown): WebApiErrorResponse;\nexport function decodeWebAttentionSnapshot(value: unknown): WebAttentionSnapshot;\nexport function decodeWebAttentionStreamEvent(value: unknown): WebAttentionStreamEvent;\nexport function decodeWebRepoWatchRepositoryStatusPage(value: unknown): WebRepoWatchRepositoryStatusPage;\nexport function decodeWebRepoWatchPullRequestPage(value: unknown): WebRepoWatchPullRequestPage;\nexport function decodeWebRepoWatchWorkPage(value: unknown): WebRepoWatchWorkPage;\nexport function decodeWebRepoWatchPullRequestSessionPage(value: unknown): WebRepoWatchPullRequestSessionPage;\nexport function decodeWebRepoWatchActivityPage(value: unknown): WebRepoWatchActivityPage;\n",
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

    use super::{WebContractBootstrap, WebContractExample, generated_artifacts};

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
}
