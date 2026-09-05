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
    MAX_SEARCH_HIGHLIGHTS_PER_RESULT, max_search_page_items, max_search_query_bytes,
    max_search_snippet_bytes, max_session_live_queued_turns, max_timeline_detail_bytes,
    max_timeline_detail_items, max_timeline_window_bytes, max_timeline_window_items,
    max_usage_aggregate_calls, max_usage_aggregate_groups, max_usage_call_page_items,
    timeline_detail_envelope_bytes,
};

/// Exact browser HTTP contract version served by this daemon build.
pub const WEB_CONTRACT_VERSION: &str = "2";
/// Stable name of the browser HTTP contract family.
pub const WEB_CONTRACT_NAME: &str = "signalbox.web-http";

/// Hard safety ceiling protecting daemon memory while buffering one JSON body.
pub const MAX_JSON_BODY_BYTES: usize = 64 * 1024;
/// Hard safety ceiling protecting client and daemon memory per NDJSON item.
pub const MAX_NDJSON_ITEM_BYTES: usize = 64 * 1024;
/// Hard safety ceiling on one ephemeral provider text fragment. Production
/// splits deltas at this bound, so the generated decoder rejects anything
/// larger as a value the server cannot emit.
// numeric-bound: hard safety - leaves room for worst-case JSON escaping and the event envelope
pub const MAX_WEB_PROVIDER_TEXT_FRAGMENT_BYTES: usize = 8_192;

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
    /// Immutable same-origin blob descriptors and byte delivery are available.
    pub immutable_blob_content: bool,
    /// Blob-to-blob provenance reads are present on derivative views.
    pub blob_derivations: bool,
    /// The daemon can lazily produce isolated deterministic image derivatives.
    pub image_derivatives: bool,
    /// Bounded imported-conversation discovery and entry windows are available.
    pub import_discovery: bool,
    /// Imported frontiers can seed a native session through an idempotent command.
    pub imported_continuations: bool,
    /// Stable bounded session descriptors and historical windows are available.
    pub bounded_session_timeline: bool,
    /// Typed item, turn, and contiguous-region detail reads are available.
    pub bounded_session_timeline_detail: bool,
    /// Bounded current snapshots and snapshot-first live follow are available.
    pub bounded_session_live: bool,
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
    /// Maximum detailed timeline records in one response.
    pub max_timeline_detail_items: u32,
    /// Maximum projected typed-body bytes in one detail response.
    pub max_timeline_detail_bytes: u32,
    /// Maximum queued turn identities retained in one live snapshot.
    pub max_session_live_queued_turns: u32,
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
        Self::for_runtime(false, false)
    }

    /// Describes this contract with deployment-bound blob capabilities.
    #[must_use]
    pub fn for_runtime(immutable_blob_content: bool, image_derivatives: bool) -> Self {
        Self {
            contract: WebContractIdentity {
                name: WEB_CONTRACT_NAME.to_owned(),
                version: WEB_CONTRACT_VERSION.to_owned(),
            },
            capabilities: WebContractCapabilities {
                bounded_json: true,
                same_origin_json_mutations: true,
                ndjson_streaming: true,
                immutable_blob_content,
                blob_derivations: image_derivatives,
                image_derivatives,
                import_discovery: true,
                imported_continuations: true,
                bounded_session_timeline: true,
                bounded_session_timeline_detail: true,
                bounded_session_live: true,
                bounded_lexical_search: true,
                bounded_usage_cost: true,
            },
            limits: WebContractLimits {
                max_json_body_bytes: MAX_JSON_BODY_BYTES as u32,
                max_ndjson_item_bytes: MAX_NDJSON_ITEM_BYTES as u32,
                max_timeline_window_items: u32::from(max_timeline_window_items()),
                max_timeline_window_bytes: max_timeline_window_bytes(),
                max_timeline_detail_items: u32::from(max_timeline_detail_items()),
                max_timeline_detail_bytes: max_timeline_detail_bytes(),
                max_session_live_queued_turns: u32::from(max_session_live_queued_turns()),
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

/// Closed browser renderer capability advertised by the daemon.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WebBlobViewKind {
    Download,
    BrowserNative,
    Thumbnail,
    Preview,
}

/// Exact producer provenance projected without persistence representation.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "class", rename_all = "snake_case", deny_unknown_fields)]
pub enum WebBlobDerivationProducer {
    Deterministic {
        implementation_digest: String,
        cache_key: String,
    },
    Executed {
        execution_id: String,
        implementation_digest: String,
    },
    ModelDerived {
        model_call_id: String,
    },
}

/// Immutable blob-to-blob relation attached to an available derivative view.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WebBlobDerivation {
    pub derivation_id: String,
    #[schemars(length(min = 1, max = 16))]
    pub input_digests: Vec<String>,
    #[schemars(length(min = 1, max = 64), regex(pattern = "^[a-z][a-z0-9_.-]{0,63}$"))]
    pub transformation_name: String,
    #[schemars(range(min = 1))]
    pub transformation_version: u32,
    pub parameters_json: String,
    pub producer: WebBlobDerivationProducer,
    #[schemars(length(min = 1, max = 16))]
    pub output_digests: Vec<String>,
}

/// One server-admitted representation; clients select by `kind`, never MIME inference.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WebBlobAvailableView {
    pub kind: WebBlobViewKind,
    #[schemars(length(max = 255))]
    pub media_type: String,
    pub byte_length: String,
    pub content_url: String,
    #[schemars(length(max = 1))]
    pub derivations: Vec<WebBlobDerivation>,
}

/// Browser read projection for one semantic use of immutable bytes.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WebBlobDescriptor {
    pub digest: String,
    pub byte_length: String,
    #[schemars(length(max = 255))]
    pub declared_media_type: String,
    #[schemars(length(max = 1))]
    pub display_filename: Vec<String>,
    #[schemars(length(max = 4))]
    pub available_views: Vec<WebBlobAvailableView>,
}

/// Hard safety ceiling protecting one imports catalog response.
pub const MAX_IMPORT_LIST_ITEMS: u32 = 100;
/// Hard safety ceiling protecting one imported-entry window response.
pub const MAX_IMPORT_ENTRY_WINDOW_ITEMS: u32 = 101;
/// Hard safety ceiling protecting one imported text preview in UTF-8 bytes.
pub const MAX_IMPORT_TEXT_PREVIEW_BYTES: usize = 512;
/// Hard safety ceiling protecting source-session evidence in catalog responses.
pub const MAX_IMPORT_SOURCE_SESSION_BYTES: usize = 512;

/// Exact source format and converter interpretation for one import.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WebImportFormat {
    /// Claude Code JSONL interpreted by Signalbox converter version 1.
    ClaudeCodeSessionJsonlV1,
    /// Claude Code JSONL interpreted by Signalbox converter version 2.
    ClaudeCodeSessionJsonlV2,
    /// Codex rollout JSONL interpreted by Signalbox converter version 1.
    CodexRolloutJsonlV1,
}

/// Bounded imports catalog request carried as query parameters. An exact
/// source-session filter is carried separately as the bounded raw UTF-8 body of
/// `POST /api/imports/searches`; empty text and edge whitespace are preserved.
#[derive(Clone, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WebImportListRequest {
    /// Exclusive imported-conversation UUID cursor.
    pub after: Option<String>,
    /// Requested page size; the server rejects values above its hard ceiling.
    pub limit: Option<u32>,
    /// Optional exact source/converter filter.
    pub format: Option<WebImportFormat>,
    /// Optional exact converter-attested source-session identifier.
    pub source_session_id: Option<String>,
    /// Client-selected UUID echoed by an exact-search response.
    pub search_correlation: Option<String>,
}

/// One bounded imports catalog row.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WebImportSummary {
    /// Immutable imported-conversation UUID.
    pub imported_conversation_id: String,
    /// Evidence-derived display title, when the source supplied one.
    pub display_title: Option<String>,
    /// Exact source format and converter interpretation.
    pub format: WebImportFormat,
    /// Bounded converter-attested source-session evidence, when consistent.
    pub source_session_id: Option<WebImportSourceSessionEvidence>,
    /// SHA-256 of the complete source-session identifier, when present.
    pub source_session_id_sha256: Option<String>,
    /// Number of normalized imported entries.
    pub entry_count: u64,
}

/// Bounded projection of exact converter-attested source-session evidence.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WebImportSourceSessionEvidence {
    /// Exact leading UTF-8 text within the response ceiling.
    pub leading_text: String,
    /// Whether the projection contains the complete identifier.
    pub completeness: WebImportTextCompleteness,
}

/// One keyset page of imports.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WebImportListPage {
    /// Rows in stable UUID order.
    pub items: Vec<WebImportSummary>,
    /// Exclusive cursor for the next page, absent at the end.
    pub next_cursor: Option<String>,
    /// Client-selected exact-search correlation UUID, absent for ordinary catalog reads.
    pub search_correlation: Option<String>,
    /// SHA-256 of the complete exact-search value, absent for ordinary catalog reads.
    pub exact_source_session_id_sha256: Option<String>,
}

/// Source and converter evidence retained by one immutable import.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WebImportSourceEvidence {
    /// Exact source format and converter interpretation.
    pub format: WebImportFormat,
    /// SHA-256 digest of the exact ordered source records.
    pub source_digest_sha256: String,
    /// Bounded converter-attested source-session evidence, when consistent.
    pub source_session_id: Option<WebImportSourceSessionEvidence>,
}

/// Byte facts projected from immutable stored import members.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WebImportSizeFacts {
    /// Sum of exact raw source-record occurrence bytes.
    pub raw_source_bytes: u64,
    /// Sum of normalized source-record encoding bytes.
    pub normalized_source_record_bytes: u64,
    /// Sum of normalized entry and source-metadata encoding bytes.
    pub normalized_entry_bytes: u64,
}

/// One immutable imported frontier suitable for precise continuation.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WebImportContinuationReference {
    /// Owning imported-conversation UUID.
    pub imported_conversation_id: String,
    /// Exact imported-entry UUID at the inclusive frontier.
    pub imported_entry_id: String,
    /// One-based immutable imported position.
    pub position: u64,
}

/// First and latest immutable positions in an imported timeline.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WebImportTimelineBounds {
    /// First selectable frontier.
    pub first: WebImportContinuationReference,
    /// Latest selectable frontier.
    pub latest: WebImportContinuationReference,
}

/// Complete bounded descriptor for one immutable import.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WebImportDescriptor {
    /// Immutable imported-conversation UUID.
    pub imported_conversation_id: String,
    /// Evidence-derived display title, when available.
    pub display_title: Option<String>,
    /// Number of exact raw source records.
    pub raw_record_count: u64,
    /// Number of normalized imported entries.
    pub entry_count: u64,
    /// Source and converter evidence, distinct from native execution evidence.
    pub source: WebImportSourceEvidence,
    /// Projected byte facts; no raw blob bytes are included.
    pub sizes: WebImportSizeFacts,
    /// Addressable first and latest imported frontiers.
    pub timeline: WebImportTimelineBounds,
}

/// Logical anchor for an imported-entry window.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WebImportWindowAnchor {
    /// Anchor at imported position one.
    First,
    /// Anchor at the immutable latest position.
    Latest,
    /// Anchor at the supplied exact position.
    Position,
}

/// Bounded imported-entry window request carried as query parameters.
#[derive(Clone, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WebImportEntryWindowRequest {
    /// Logical anchor; defaults to `first` when omitted.
    pub anchor: Option<WebImportWindowAnchor>,
    /// Required only for the `position` anchor.
    pub position: Option<u64>,
    /// Number of entries requested before the anchor.
    pub before: Option<u32>,
    /// Number of entries requested after the anchor.
    pub after: Option<u32>,
}

/// Source-attested speaker evidence for one imported entry.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WebImportedSpeakerEvidence {
    /// The source omitted speaker evidence.
    NotAttested,
    /// The source explicitly attested no speaker.
    AttestedAbsent,
    /// The source attested a user-role speaker.
    User,
    /// The source attested an assistant-role speaker.
    Assistant,
}

/// Closed normalized imported content kind.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WebImportedContentKind {
    /// Non-message source record.
    SourceEvent,
    /// Source-defined message block.
    SourceMessageBlock,
    /// Source text or explicit text absence.
    Text,
    /// Source tool call.
    ToolCall,
    /// Source tool result.
    ToolResult,
    /// Source-visible thinking.
    Thinking,
    /// Source redacted-thinking data.
    RedactedThinking,
    /// Source document descriptor.
    Document,
    /// Precisely classified absent message content.
    MessageContentAbsent,
}

/// Completeness of a bounded attested-text preview.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WebImportTextCompleteness {
    /// The exact attested text fits the preview bound.
    Complete,
    /// Only the leading UTF-8 prefix fits the preview bound.
    Truncated,
}

/// Bounded text evidence for an imported entry.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum WebImportTextEvidence {
    /// The text member was omitted by the source.
    NotAttested,
    /// The source explicitly supplied no text.
    AttestedAbsent,
    /// The source supplied exact text, possibly represented by a bounded prefix.
    Attested {
        /// Exact leading text within the byte ceiling.
        leading_text: String,
        /// Whether the prefix is the complete text.
        completeness: WebImportTextCompleteness,
    },
}

/// One normalized imported entry in a bounded window.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WebImportedEntry {
    /// Exact immutable continuation frontier.
    pub frontier: WebImportContinuationReference,
    /// One-based physical source-record occurrence.
    pub raw_record_position: u64,
    /// One-based normalized entry position within that source record.
    pub record_entry_position: u64,
    /// Source speaker attestation, never native author evidence.
    pub source_speaker: WebImportedSpeakerEvidence,
    /// Source-neutral normalized content kind.
    pub content_kind: WebImportedContentKind,
    /// Bounded text evidence only for normalized text content.
    pub text: Option<WebImportTextEvidence>,
}

/// One bounded imported-entry window.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WebImportEntryWindow {
    /// Resolved immutable anchor position.
    pub anchor_position: u64,
    /// First position returned.
    pub first_position: u64,
    /// Last position returned.
    pub last_position: u64,
    /// Whether earlier entries exist.
    pub has_before: bool,
    /// Whether later entries exist.
    pub has_after: bool,
    /// Entries in ascending immutable position order.
    pub items: Vec<WebImportedEntry>,
}

/// Resume or fork relationship chosen for a new native session.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WebImportedSessionRelationship {
    /// Resume the selected imported history.
    Resume,
    /// Fork from the selected imported history.
    Fork,
}

/// Initial model-selection request for a continued native session.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum WebModelSelection {
    /// Exact direct model-selection UUID.
    Direct {
        /// Direct model-selection UUID.
        selection_id: String,
    },
    /// Alias UUID resolved by the daemon at command admission.
    Alias {
        /// Model alias UUID.
        alias_id: String,
    },
}

/// Idempotent continuation command for one selected immutable frontier.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WebImportContinuationRequest {
    /// Durable command UUID minted before network I/O and retained for retry.
    pub command_id: String,
    /// Exact selected immutable imported frontier.
    pub frontier: WebImportContinuationReference,
    /// Resume or fork relationship.
    pub relationship: WebImportedSessionRelationship,
    /// Initial model selection; other settings use provider defaults.
    pub initial_model_selection: WebModelSelection,
}

/// Durable applied result of an imported continuation command.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WebImportContinuationResponse {
    /// Replayed durable command UUID.
    pub command_id: String,
    /// Newly created or replayed native session UUID.
    pub session_id: String,
    /// Exact selected immutable imported frontier.
    pub frontier: WebImportContinuationReference,
    /// Recorded resume or fork relationship.
    pub relationship: WebImportedSessionRelationship,
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

/// Checked canonical UUID used for browser-visible live resource identities.
#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(transparent)]
pub struct WebLiveResourceId(
    #[schemars(regex(
        pattern = r"^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$"
    ))]
    String,
);

impl WebLiveResourceId {
    /// Constructs a canonical lowercase UUID from its 16 wire-order bytes.
    #[must_use]
    pub fn from_uuid_bytes(bytes: [u8; 16]) -> Self {
        Self(WebSessionId::from_uuid_bytes(bytes).0)
    }
}

impl<'de> Deserialize<'de> for WebLiveResourceId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        canonical_session_id(&value)
            .then_some(Self(value))
            .ok_or_else(|| de::Error::custom("live resource ID must be a canonical lowercase UUID"))
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
    SessionStateChanged,
    SessionTerminal,
    GoalChanged,
    CommandSettled,
    InjectionSettled,
    SessionOwnershipChanged,
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

/// Text-bearing field within one typed timeline body.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WebTimelineBodyField {
    InputText,
    ModelResponse,
}

/// Exact continuation within an oversized typed body.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WebTimelineBodyContinuation {
    pub address: WebTimelineAddress,
    pub field: WebTimelineBodyField,
    pub member_index: u32,
    pub offset_bytes: WebU64,
}

/// Bounded UTF-8 excerpt with explicit completeness evidence.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WebTimelineTextExcerpt {
    /// The generator stamps `max_timeline_detail_bytes()` onto this field as
    /// `maxLength`: UTF-16 length never exceeds UTF-8 length, so every valid
    /// excerpt within the detail byte budget passes that pre-encoding bound.
    pub text: String,
    pub offset_bytes: WebU64,
    pub total_bytes: WebU64,
    pub continuation: Option<WebTimelineBodyContinuation>,
}

/// Checked canonical SHA-256 identity used for browser-visible blob references.
#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(transparent)]
pub struct WebBlobId(#[schemars(regex(pattern = r"^sha256:[0-9a-f]{64}$"))] String);

impl WebBlobId {
    /// Constructs a blob identity from its canonical external spelling.
    #[must_use]
    pub fn from_canonical(value: String) -> Option<Self> {
        let digest = value.strip_prefix("sha256:")?;
        (digest.len() == 64
            && digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)))
        .then_some(Self(value))
    }
}

impl<'de> Deserialize<'de> for WebBlobId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::from_canonical(value)
            .ok_or_else(|| de::Error::custom("blob ID must be a canonical SHA-256 identity"))
    }
}

/// Reference-only blob fact carried without blob bytes.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WebTimelineBlobReference {
    pub blob_id: WebBlobId,
    pub length_bytes: WebU64,
    /// Visible-ASCII pattern plus the 255 bound express the multipart
    /// contract's "at most 255 visible ASCII bytes"; for visible ASCII,
    /// UTF-16 length equals byte length, so maxLength is a byte bound.
    #[schemars(length(max = 255), regex(pattern = r"^[!-~]+$"))]
    pub media_type: Option<String>,
}

/// Closed model-call lifecycle checkpoint with terminal disposition in-band.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case", tag = "type")]
pub enum WebTimelineModelCallState {
    Prepared {},
    InFlight {},
    CancellationRequested {},
    Terminal {
        disposition: WebTimelineModelCallDisposition,
    },
}

/// Closed terminal model-call disposition.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WebTimelineModelCallDisposition {
    Completed,
    KnownFailed,
    Refused,
    Cancelled,
    Ambiguous,
}

/// Closed provider-neutral failure cause exposed at the browser boundary.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WebProviderModelCallFailureCause {
    CredentialRejected,
    PermissionDenied,
    InvalidRequest,
    TargetNotFound,
    RequestTooLarge,
    RateLimited,
    QuotaExhausted,
    Overloaded,
    ProviderInternal,
    Unrecognized,
}

/// Independently optional provider-reported usage counts.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WebTimelineModelUsage {
    pub input_tokens: Option<WebU64>,
    pub output_tokens: Option<WebU64>,
    pub cache_creation_input_tokens: Option<WebU64>,
    pub cache_read_input_tokens: Option<WebU64>,
}

/// Closed turn lifecycle boundary.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WebTimelineTurnLifecycleKind {
    Activated,
    Terminalized,
}

/// Typed browser body, distinct from application and persistence projections.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case", tag = "type")]
pub enum WebSessionTimelineDetailBody {
    UserInput {
        turn_id: WebSessionId,
        text: WebTimelineTextExcerpt,
        #[schemars(length(max = 256))]
        attachments: Vec<WebTimelineBlobReference>,
    },
    ModelCall {
        turn_id: WebSessionId,
        model_call_id: WebSessionId,
        state: WebTimelineModelCallState,
        model_identity_id: WebSessionId,
        request_context_items: WebU64,
        response: Option<WebTimelineTextExcerpt>,
        usage: WebTimelineModelUsage,
        provider_failure_cause: Option<WebProviderModelCallFailureCause>,
    },
    TurnLifecycle {
        turn_id: WebSessionId,
        lifecycle: WebTimelineTurnLifecycleKind,
        cause_code: String,
    },
    EventFact {
        kind: WebSessionTimelineEventKind,
    },
}

/// One typed body at a stable timeline address.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WebSessionTimelineDetail {
    pub address: WebTimelineAddress,
    pub kind: WebSessionTimelineEventKind,
    pub body: WebSessionTimelineDetailBody,
    pub projected_body_bytes: u32,
}

/// Explicit next position after a bounded detail response.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case", tag = "type")]
pub enum WebTimelineDetailContinuation {
    MoreAt { address: WebTimelineAddress },
    MoreBody { body: WebTimelineBodyContinuation },
}

/// One bounded item, turn, or contiguous-region detail response.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WebSessionTimelineDetailPage {
    pub session_id: WebSessionId,
    // The generator stamps `max_timeline_detail_items()` onto this field as
    // `maxItems`; the bound lives in the application crate, not restated here.
    pub items: Vec<WebSessionTimelineDetail>,
    pub projected_body_bytes: u32,
    pub continuation: Option<WebTimelineDetailContinuation>,
}

fn deserialize_present_option<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

/// Current durable state of one active turn.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum WebSessionLiveActiveState {
    Running {
        #[serde(deserialize_with = "deserialize_present_option")]
        #[schemars(required)]
        model_call_id: Option<WebLiveResourceId>,
    },
    AwaitingModelCallRecovery {
        model_call_id: WebLiveResourceId,
    },
    AwaitingToolApproval {
        tool_request_id: WebLiveResourceId,
    },
    AwaitingChild {
        tool_request_id: WebLiveResourceId,
        child_session_id: WebSessionId,
    },
    AwaitingToolRecovery {
        tool_attempt_id: WebLiveResourceId,
    },
    AwaitingRunnerRecovery {
        runner_id: WebLiveResourceId,
        placement_revision: WebPositiveU64,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WebSessionLiveActiveTurn {
    pub turn_id: WebTurnId,
    pub state: WebSessionLiveActiveState,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum WebSessionLiveReconciliation {
    ModelCall {
        turn_id: WebTurnId,
        model_call_id: WebLiveResourceId,
    },
    ToolAttempt {
        turn_id: WebTurnId,
        tool_attempt_id: WebLiveResourceId,
    },
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WebSessionLiveRunnerConnectionHealth {
    Connected,
    Suspect,
    Shutdown,
    Lost,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum WebSessionLiveRunner {
    Unpinned {
        placement_revision: WebPositiveU64,
    },
    Pinned {
        runner_id: WebLiveResourceId,
        placement_revision: WebPositiveU64,
        connection_health: WebSessionLiveRunnerConnectionHealth,
    },
    RunnerLostBeforePin {
        runner_id: WebLiveResourceId,
        placement_revision: WebPositiveU64,
    },
    RunnerLost {
        runner_id: WebLiveResourceId,
        placement_revision: WebPositiveU64,
    },
    RunnerAbandoned {
        runner_id: WebLiveResourceId,
        placement_revision: WebPositiveU64,
    },
}

/// Bounded repeatable-read current projection for one open workspace.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WebSessionLiveSnapshot {
    pub session_id: WebSessionId,
    pub observed_through: WebPositiveU64,
    #[serde(deserialize_with = "deserialize_present_option")]
    #[schemars(required)]
    pub active: Option<WebSessionLiveActiveTurn>,
    pub queued_turn_count: WebU64,
    pub queued_turn_ids: Vec<WebTurnId>,
    #[serde(deserialize_with = "deserialize_present_option")]
    #[schemars(required)]
    pub reconciliation: Option<WebSessionLiveReconciliation>,
    #[serde(deserialize_with = "deserialize_present_option")]
    #[schemars(required)]
    pub runner: Option<WebSessionLiveRunner>,
}

/// Snapshot-first event stream for one open workspace.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum WebSessionLiveStreamEvent {
    Snapshot {
        snapshot: Box<WebSessionLiveSnapshot>,
    },
    Durable {
        cursor: WebU64,
        address: WebTimelineAddress,
        event_kind: WebSessionTimelineEventKind,
    },
    ProviderTextDelta {
        turn_id: WebTurnId,
        model_call_id: WebLiveResourceId,
        part_index: u32,
        content: String,
    },
    ResyncRequired {
        /// Positive because production starts from a positive snapshot cursor.
        cursor: WebPositiveU64,
    },
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
    #[schemars(length(max = MAX_SEARCH_HIGHLIGHTS_PER_RESULT))]
    pub highlights: Vec<WebSearchHighlight>,
}

/// One bounded, stable page of lexical matches.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WebSearchPage {
    pub results: Vec<WebSearchResult>,
    #[serde(deserialize_with = "deserialize_present_option")]
    #[schemars(required)]
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
    /// Owning turn, present-but-null exactly for context compaction.
    #[serde(deserialize_with = "deserialize_present_option")]
    #[schemars(required)]
    pub turn_id: Option<WebUuid>,
    pub model_id: WebUuid,
    pub profile_id: WebUsageProfileId,
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
    /// Present-but-null when the page exhausts the matching evidence, so an
    /// omitted member is an incompatibility rather than a silent exhaustion.
    #[serde(deserialize_with = "deserialize_present_option")]
    #[schemars(required)]
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

/// Title summaries carry at most this many Unicode scalar values; production
/// truncates longer stored titles to exactly this bound and marks
/// `title_truncated`.
const MAX_ATTENTION_TITLE_SCALARS: u32 = 128;

fn nullable_title_summary_schema(_generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({
        "type": ["string", "null"],
        "maxLength": MAX_ATTENTION_TITLE_SCALARS,
    })
}

fn nullable_attention_action_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
    let action = generator.subschema_for::<WebAttentionAction>();
    schemars::json_schema!({
        "anyOf": [action, {"type": "null"}],
    })
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WebAttentionState {
    Active,
    Queued,
    Blocked,
    AwaitingApproval,
    Ambiguous,
    AwaitingToolRecovery,
    AwaitingReconciliation,
    RunnerLost,
    Parked,
    Idle,
}

/// The durable session state one attention summary projects.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WebAttentionLifecycleState {
    Created,
    Dispatched,
    Active,
    Waiting,
    Recovering,
    Blocked,
    Parked,
    Terminal,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WebAttentionAction {
    ProvideGoalNeed,
    DecideApproval,
    ReconcileTurn,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WebAttentionBlockedReason {
    UserInputRequired,
    ExternalChangeRequired,
    AuthorizationRequired,
    ExecutionFailure,
    FinishCheckFailed,
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
    #[schemars(regex(pattern = r"^(0|[1-9][0-9]*)$"))]
    pub generation: String,
    pub reason: WebAttentionBlockedReason,
    /// At most 128 Unicode scalar values; exact text is in session detail.
    #[schemars(length(max = 128))]
    pub need_summary: String,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WebAttentionJudgeFacts {
    #[schemars(regex(pattern = r"^(0|[1-9][0-9]*)$"))]
    pub actionable: String,
    #[schemars(regex(pattern = r"^(0|[1-9][0-9]*)$"))]
    pub completed: String,
    #[schemars(regex(pattern = r"^(0|[1-9][0-9]*)$"))]
    pub escalated: String,
    #[schemars(regex(pattern = r"^(0|[1-9][0-9]*)$"))]
    pub failed: String,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WebAttentionActivity {
    #[schemars(regex(pattern = r"^(0|[1-9][0-9]*)$"))]
    pub unix_milliseconds: String,
    pub kind: WebAttentionActivityKind,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WebAttentionSummary {
    #[schemars(regex(
        pattern = r"^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$"
    ))]
    pub session_id: String,
    #[schemars(regex(
        pattern = r"^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$"
    ))]
    pub current_turn_id: Option<String>,
    pub state: WebAttentionState,
    pub lifecycle_state: WebAttentionLifecycleState,
    pub action: Option<WebAttentionAction>,
    pub goal_block: Option<WebAttentionGoalBlock>,
    pub judge: WebAttentionJudgeFacts,
    pub last_activity: WebAttentionActivity,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WebAttentionSnapshot {
    #[schemars(regex(pattern = r"^(0|[1-9][0-9]*)$"))]
    pub cursor: String,
    #[schemars(length(max = 32))]
    pub summaries: Vec<WebAttentionSummary>,
    #[schemars(regex(
        pattern = r"^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$"
    ))]
    pub continuation_after_session_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum WebAttentionStreamEvent {
    Snapshot {
        snapshot: WebAttentionSnapshot,
    },
    Update {
        #[schemars(regex(pattern = r"^(0|[1-9][0-9]*)$"))]
        cursor: String,
        #[schemars(length(max = 32))]
        summaries: Vec<WebAttentionSummary>,
    },
    ResyncRequired {
        #[schemars(regex(pattern = r"^(0|[1-9][0-9]*)$"))]
        cursor: String,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WebSessionCatalogActivity {
    pub unix_microseconds: WebU64,
    pub kind: WebAttentionActivityKind,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WebSessionCatalogSummary {
    pub session_id: WebSessionId,
    #[serde(deserialize_with = "deserialize_present_option")]
    #[schemars(required, schema_with = "nullable_title_summary_schema")]
    pub title_summary: Option<String>,
    pub title_truncated: bool,
    pub archived: bool,
    #[serde(deserialize_with = "deserialize_present_option")]
    #[schemars(required)]
    pub current_turn_id: Option<WebUuid>,
    pub active_turn_count: WebU64,
    pub queued_turn_count: WebU64,
    pub state: WebAttentionState,
    #[serde(deserialize_with = "deserialize_present_option")]
    #[schemars(required, schema_with = "nullable_attention_action_schema")]
    pub action: Option<WebAttentionAction>,
    pub goal_block: Option<WebAttentionGoalBlock>,
    pub judge: WebAttentionJudgeFacts,
    pub last_activity: WebSessionCatalogActivity,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WebSessionCatalogSort {
    LastActivityDescending,
    SessionIdentityAscending,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum WebSessionCatalogContinuation {
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
pub struct WebSessionCatalogSnapshot {
    pub cursor: WebU64,
    pub total: WebU64,
    pub sort: WebSessionCatalogSort,
    #[schemars(length(max = 32))]
    pub summaries: Vec<WebSessionCatalogSummary>,
    #[serde(deserialize_with = "deserialize_present_option")]
    #[schemars(required)]
    pub continuation: Option<WebSessionCatalogContinuation>,
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
    let schemas = contract_schemas()?;
    let example = WebContractExample {
        request_id: "contract-round-trip".to_owned(),
        message: "browser contract fixture".to_owned(),
    };
    let example_json = serde_json::to_string_pretty(&example)
        .map_err(|_| GenerateWebContractError::Serialization)?
        + "\n";
    let bootstrap_json = serde_json::to_string_pretty(&WebContractBootstrap::current())
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
        GeneratedArtifact {
            path: "clients/web/src/generated/web-contract-bootstrap.json",
            contents: bootstrap_json,
        },
    ])
}

struct ContractSchema {
    name: &'static str,
    decoder: &'static str,
    schema: Value,
}

fn contract_schemas() -> Result<Vec<ContractSchema>, GenerateWebContractError> {
    let mut timeline_window_schema =
        canonical_schema(schemars::schema_for!(WebSessionTimelineWindow).to_value());
    make_property_nullable(&mut timeline_window_schema, "continuation_before")?;
    make_property_nullable(&mut timeline_window_schema, "continuation_after")?;

    let mut timeline_detail_schema =
        canonical_schema(schemars::schema_for!(WebSessionTimelineDetailPage).to_value());
    set_string_max_length(
        &mut timeline_detail_schema,
        "/$defs/WebTimelineTextExcerpt/properties/text",
        max_timeline_detail_bytes(),
    )?;
    set_array_max_items(
        &mut timeline_detail_schema,
        "/properties/items",
        u32::from(max_timeline_detail_items()),
    )?;

    let attention_snapshot_schema =
        canonical_schema(schemars::schema_for!(WebAttentionSnapshot).to_value());
    let attention_event_schema =
        canonical_schema(schemars::schema_for!(WebAttentionStreamEvent).to_value());

    let mut session_catalog_schema =
        canonical_schema(schemars::schema_for!(WebSessionCatalogSnapshot).to_value());
    make_property_nullable(&mut session_catalog_schema, "continuation")?;
    make_pointer_nullable(
        &mut session_catalog_schema,
        "/$defs/WebSessionCatalogSummary/properties/current_turn_id",
    )?;

    let mut live_snapshot_schema =
        canonical_schema(schemars::schema_for!(WebSessionLiveSnapshot).to_value());
    make_property_nullable(&mut live_snapshot_schema, "active")?;
    make_property_nullable(&mut live_snapshot_schema, "reconciliation")?;
    make_property_nullable(&mut live_snapshot_schema, "runner")?;
    make_pointer_nullable(
        &mut live_snapshot_schema,
        "/$defs/WebSessionLiveActiveState/oneOf/0/properties/model_call_id",
    )?;
    set_array_max_items(
        &mut live_snapshot_schema,
        "/properties/queued_turn_ids",
        u32::from(max_session_live_queued_turns()),
    )?;

    let mut live_event_schema =
        canonical_schema(schemars::schema_for!(WebSessionLiveStreamEvent).to_value());
    make_pointer_nullable(
        &mut live_event_schema,
        "/$defs/WebSessionLiveSnapshot/properties/active",
    )?;
    make_pointer_nullable(
        &mut live_event_schema,
        "/$defs/WebSessionLiveSnapshot/properties/reconciliation",
    )?;
    make_pointer_nullable(
        &mut live_event_schema,
        "/$defs/WebSessionLiveSnapshot/properties/runner",
    )?;
    make_pointer_nullable(
        &mut live_event_schema,
        "/$defs/WebSessionLiveActiveState/oneOf/0/properties/model_call_id",
    )?;
    set_array_max_items(
        &mut live_event_schema,
        "/$defs/WebSessionLiveSnapshot/properties/queued_turn_ids",
        u32::from(max_session_live_queued_turns()),
    )?;

    let mut search_page_schema = schemars::schema_for!(WebSearchPage).to_value();
    search_page_schema["properties"]["results"]["maxItems"] = json!(max_search_page_items());
    make_property_nullable(&mut search_page_schema, "continuation")?;
    let search_page_schema = canonical_schema(search_page_schema);

    let mut usage_call_page_schema = schemars::schema_for!(WebUsageCallPage).to_value();
    make_property_nullable(&mut usage_call_page_schema, "continuation")?;
    make_definition_property_nullable(&mut usage_call_page_schema, "WebUsageCall", "turn_id")?;
    let usage_call_page_schema = canonical_schema(usage_call_page_schema);

    Ok(vec![
        ContractSchema {
            name: "WebContractBootstrap",
            decoder: "decodeWebContractBootstrap",
            schema: canonical_schema(schemars::schema_for!(WebContractBootstrap).to_value()),
        },
        ContractSchema {
            name: "WebContractExample",
            decoder: "decodeWebContractExample",
            schema: canonical_schema(schemars::schema_for!(WebContractExample).to_value()),
        },
        ContractSchema {
            name: "WebApiErrorResponse",
            decoder: "decodeWebApiErrorResponse",
            schema: canonical_schema(schemars::schema_for!(WebApiErrorResponse).to_value()),
        },
        ContractSchema {
            name: "WebBlobDescriptor",
            decoder: "decodeWebBlobDescriptor",
            schema: canonical_schema(schemars::schema_for!(WebBlobDescriptor).to_value()),
        },
        ContractSchema {
            name: "WebSessionTimelineDescriptor",
            decoder: "decodeWebSessionTimelineDescriptor",
            schema: canonical_schema(
                schemars::schema_for!(WebSessionTimelineDescriptor).to_value(),
            ),
        },
        ContractSchema {
            name: "WebSessionTimelineWindow",
            decoder: "decodeWebSessionTimelineWindow",
            schema: timeline_window_schema,
        },
        ContractSchema {
            name: "WebSessionTimelineDetailPage",
            decoder: "decodeWebSessionTimelineDetailPage",
            schema: timeline_detail_schema,
        },
        ContractSchema {
            name: "WebAttentionSnapshot",
            decoder: "decodeWebAttentionSnapshot",
            schema: attention_snapshot_schema,
        },
        ContractSchema {
            name: "WebAttentionStreamEvent",
            decoder: "decodeWebAttentionStreamEvent",
            schema: attention_event_schema,
        },
        ContractSchema {
            name: "WebSessionCatalogSnapshot",
            decoder: "decodeWebSessionCatalogSnapshot",
            schema: session_catalog_schema,
        },
        ContractSchema {
            name: "WebSessionLiveSnapshot",
            decoder: "decodeWebSessionLiveSnapshot",
            schema: live_snapshot_schema,
        },
        ContractSchema {
            name: "WebSessionLiveStreamEvent",
            decoder: "decodeWebSessionLiveStreamEvent",
            schema: live_event_schema,
        },
        ContractSchema {
            name: "WebImportListRequest",
            decoder: "decodeWebImportListRequest",
            schema: canonical_schema(schemars::schema_for!(WebImportListRequest).to_value()),
        },
        ContractSchema {
            name: "WebImportListPage",
            decoder: "decodeWebImportListPage",
            schema: canonical_schema(schemars::schema_for!(WebImportListPage).to_value()),
        },
        ContractSchema {
            name: "WebImportDescriptor",
            decoder: "decodeWebImportDescriptor",
            schema: canonical_schema(schemars::schema_for!(WebImportDescriptor).to_value()),
        },
        ContractSchema {
            name: "WebImportEntryWindowRequest",
            decoder: "decodeWebImportEntryWindowRequest",
            schema: canonical_schema(schemars::schema_for!(WebImportEntryWindowRequest).to_value()),
        },
        ContractSchema {
            name: "WebImportEntryWindow",
            decoder: "decodeWebImportEntryWindow",
            schema: canonical_schema(schemars::schema_for!(WebImportEntryWindow).to_value()),
        },
        ContractSchema {
            name: "WebImportContinuationRequest",
            decoder: "decodeWebImportContinuationRequest",
            schema: canonical_schema(
                schemars::schema_for!(WebImportContinuationRequest).to_value(),
            ),
        },
        ContractSchema {
            name: "WebImportContinuationResponse",
            decoder: "decodeWebImportContinuationResponse",
            schema: canonical_schema(
                schemars::schema_for!(WebImportContinuationResponse).to_value(),
            ),
        },
        ContractSchema {
            name: "WebSearchPage",
            decoder: "decodeWebSearchPage",
            schema: search_page_schema,
        },
        ContractSchema {
            name: "WebUsageSummary",
            decoder: "decodeWebUsageSummary",
            schema: canonical_schema(schemars::schema_for!(WebUsageSummary).to_value()),
        },
        ContractSchema {
            name: "WebUsageCallPage",
            decoder: "decodeWebUsageCallPage",
            schema: usage_call_page_schema,
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

fn set_string_max_length(
    schema: &mut Value,
    property_pointer: &str,
    max_length: u32,
) -> Result<(), GenerateWebContractError> {
    let property = schema
        .pointer_mut(property_pointer)
        .and_then(Value::as_object_mut)
        .filter(|property| property.get("type").and_then(Value::as_str) == Some("string"))
        .ok_or(GenerateWebContractError::UnsupportedSchema)?;
    property.insert("maxLength".to_owned(), json!(max_length));
    Ok(())
}

/// Stamps an array bound onto a generated schema from the value's owning crate.
///
/// Restating the ceiling as a `schemars` literal lets the generated client and
/// the advertised bootstrap limit drift apart when the owning crate changes, so
/// the bound is written here from the same function bootstrap reports.
fn set_array_max_items(
    schema: &mut Value,
    property_pointer: &str,
    max_items: u32,
) -> Result<(), GenerateWebContractError> {
    let property = schema
        .pointer_mut(property_pointer)
        .and_then(Value::as_object_mut)
        .filter(|property| property.get("type").and_then(Value::as_str) == Some("array"))
        .ok_or(GenerateWebContractError::UnsupportedSchema)?;
    property.insert("maxItems".to_owned(), json!(max_items));
    Ok(())
}

// `#[schemars(required)]` marks an `Option` member required by emitting the
// inner type alone, so a required-present nullable member restores its null
// branch here. Definitions carry the members of referenced types.
fn make_definition_property_nullable(
    schema: &mut Value,
    definition_name: &str,
    property_name: &str,
) -> Result<(), GenerateWebContractError> {
    let property = schema
        .pointer_mut(&format!(
            "/$defs/{definition_name}/properties/{property_name}"
        ))
        .ok_or(GenerateWebContractError::UnsupportedSchema)?;
    let concrete = property.take();
    *property = json!({ "anyOf": [concrete, { "type": "null" }] });
    Ok(())
}

fn runtime_module(schemas: &[ContractSchema]) -> Result<String, GenerateWebContractError> {
    let current_bootstrap = WebContractBootstrap::current();
    let mut schema_values = serde_json::Map::new();
    for schema in schemas {
        schema_values.insert(schema.name.to_owned(), schema.schema.clone());
    }
    let mut schema_values = Value::Object(schema_values);
    schema_values.sort_all_objects();
    let schema_values = serde_json::to_string_pretty(&schema_values)
        .map_err(|_| GenerateWebContractError::Serialization)?;
    let max_detail_bytes = max_timeline_detail_bytes();
    let detail_envelope_bytes = timeline_detail_envelope_bytes();
    let mut output = format!(
        r##"// @generated by `cargo run -p signalbox-web-contract --bin generate-web-contract`.
// Do not edit by hand.

const schemas = {schema_values};

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
  const alternatives = schema.oneOf ?? schema.anyOf;
  if (alternatives !== undefined) {{
    const accepted = alternatives.some((candidate) => {{
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
  if (schema.type === "string") {{
    if (typeof value !== "string") {{
      fail(path, "string");
    }}
    if (!isWellFormedUnicode(value)) {{
      fail(path, "well-formed Unicode scalar values");
    }}
    if (
      (schema.pattern === "^[1-9][0-9]*$" || schema.pattern === "^(0|[1-9][0-9]*)$") &&
      value.length > 20
    ) {{
      fail(path, "an unsigned 64-bit integer");
    }}
    if (schema.minLength !== undefined && Array.from(value).length < schema.minLength) {{
      fail(path, `at least ${{schema.minLength}} Unicode scalar values`);
    }}
    if (schema.pattern !== undefined && !new RegExp(schema.pattern, "u").test(value)) {{
      fail(path, `matching ${{schema.pattern}}`);
    }}
    if (
      (schema.pattern === "^[1-9][0-9]*$" || schema.pattern === "^(0|[1-9][0-9]*)$") &&
      BigInt(value) > 18446744073709551615n
    ) {{
      fail(path, "an unsigned 64-bit integer");
    }}
    if (
      schema.pattern === "^[1-9][0-9]{{0,18}}$" &&
      BigInt(value) > 9223372036854775807n
    ) {{
      fail(path, "a positive signed 64-bit integer");
    }}
    if (
      schema.pattern === "^(0|[1-9][0-9]*)(\\.[0-9]{{0,27}}[1-9])?$" &&
      BigInt(value.replace(".", "")) > 79228162514264337593543950335n
    ) {{
      fail(path, "a rust_decimal coefficient");
    }}
    if (
      schema.pattern === "^(0|[1-9][0-9]{{0,17}})$" &&
      BigInt(value) > 253402300799999999n
    ) {{
      fail(path, "an application-range usage timestamp");
    }}
    if (
      schema.pattern === "^(0|[1-9][0-9]{{0,38}})$" &&
      BigInt(value) > 340282366920938463463374607431768211455n
    ) {{
      fail(path, "an unsigned 128-bit integer");
    }}
    if (schema.maxLength !== undefined && Array.from(value).length > schema.maxLength) {{
      fail(path, `at most ${{schema.maxLength}} Unicode scalar values`);
    }}
    return;
  }}
  if (typeof value !== schema.type) {{
    fail(path, schema.type);
  }}
}}

function sameTimelineAddress(left, right) {{
  return left.event_sequence === right.event_sequence;
}}

function sameBodyContinuation(left, right) {{
  return (
    sameTimelineAddress(left.address, right.address) &&
    left.field === right.field &&
    left.member_index === right.member_index &&
    left.offset_bytes === right.offset_bytes
  );
}}

function assertTimelineExcerpt(excerpt, address, field, path) {{
  const offset = BigInt(excerpt.offset_bytes);
  const total = BigInt(excerpt.total_bytes);
  const end = offset + BigInt(new TextEncoder().encode(excerpt.text).byteLength);
  if (offset > total || end > total) {{
    fail(path, "an excerpt within its declared byte range");
  }}
  if (excerpt.continuation === undefined || excerpt.continuation === null) {{
    if (end !== total) {{
      fail(path, "complete when no continuation is present");
    }}
    return null;
  }}
  const continuation = excerpt.continuation;
  if (continuation.member_index !== 0) {{
    fail(`${{path}}.continuation.member_index`, "zero for a singular body field");
  }}
  if (end >= total) {{
    fail(`${{path}}.continuation`, "present only before the declared body end");
  }}
  if (!sameTimelineAddress(continuation.address, address) || continuation.field !== field) {{
    fail(`${{path}}.continuation`, "the same body field at the same address");
  }}
  if (BigInt(continuation.offset_bytes) !== end) {{
    fail(`${{path}}.continuation.offset_bytes`, "the byte immediately after the excerpt");
  }}
  return continuation;
}}

function assertTimelineDetailPage(value) {{
  const maxProjectedBodyBytes = {max_detail_bytes};
  const detailEnvelopeBytes = {detail_envelope_bytes};
  const terminalKinds = new Set([
    "turn_failed",
    "turn_completed",
    "turn_refused",
    "turn_cancelled",
    "turn_reconciliation_required",
  ]);
  const bodyOwnedKinds = new Set([
    "input_accepted",
    "model_call_transition",
    "turn_activated",
    ...terminalKinds,
  ]);
  let expectedBodyContinuation = null;
  let computedProjectedBodyBytes = 0;
  let previousAddress = null;
  value.items.forEach((item, index) => {{
    const path = `timeline_detail_page.items[${{index}}]`;
    if (expectedBodyContinuation !== null) {{
      fail(path, "absent after a continued body");
    }}
    const address = BigInt(item.address.event_sequence);
    if (previousAddress !== null && address <= previousAddress) {{
      fail(`${{path}}.address`, "strictly increasing after the previous item");
    }}
    previousAddress = address;
    let continuation = null;
    let textBytes = 0;
    switch (item.body.type) {{
      case "user_input":
        if (item.kind !== "input_accepted") {{
          fail(`${{path}}.kind`, "input_accepted for a user_input body");
        }}
        continuation = assertTimelineExcerpt(
          item.body.text,
          item.address,
          "input_text",
          `${{path}}.body.text`,
        );
        textBytes = new TextEncoder().encode(item.body.text.text).byteLength;
        break;
      case "model_call":
        if (item.kind !== "model_call_transition") {{
          fail(`${{path}}.kind`, "model_call_transition for a model_call body");
        }}
        if (item.body.response !== undefined && item.body.response !== null) {{
          continuation = assertTimelineExcerpt(
            item.body.response,
            item.address,
            "model_response",
            `${{path}}.body.response`,
          );
          textBytes = new TextEncoder().encode(item.body.response.text).byteLength;
        }}
        if (item.body.state.type !== "terminal") {{
          const hasUsage = Object.values(item.body.usage).some(
            (count) => count !== undefined && count !== null,
          );
          if (
            (item.body.response !== undefined && item.body.response !== null) ||
            hasUsage ||
            (item.body.provider_failure_cause !== undefined &&
              item.body.provider_failure_cause !== null)
          ) {{
            fail(
              `${{path}}.body`,
              "terminal evidence only at a terminal model-call state",
            );
          }}
        }} else {{
          const hasFailureCause =
            item.body.provider_failure_cause !== undefined &&
            item.body.provider_failure_cause !== null;
          if (hasFailureCause && item.body.state.disposition !== "known_failed") {{
            fail(
              `${{path}}.body.provider_failure_cause`,
              "present only for a known_failed terminal model call",
            );
          }}
          if (
            item.body.response !== undefined &&
            item.body.response !== null &&
            item.body.state.disposition !== "completed"
          ) {{
            fail(
              `${{path}}.body.response`,
              "present only for a completed terminal model call",
            );
          }}
          const hasUsage = Object.values(item.body.usage).some(
            (count) => count !== undefined && count !== null,
          );
          if (hasUsage && item.body.state.disposition === "cancelled") {{
            fail(
              `${{path}}.body.usage`,
              "unreported for a cancelled terminal model call",
            );
          }}
        }}
        break;
      case "turn_lifecycle":
        if (item.body.lifecycle === "activated" && item.kind !== "turn_activated") {{
          fail(`${{path}}.kind`, "turn_activated for an activated lifecycle");
        }}
        if (item.body.lifecycle === "terminalized" && !terminalKinds.has(item.kind)) {{
          fail(`${{path}}.kind`, "a terminal turn event for a terminalized lifecycle");
        }}
        const lifecycleCauseByKind = {{
          turn_activated: "activated",
          turn_failed: "failed",
          turn_completed: "completed",
          turn_refused: "refused",
          turn_cancelled: "cancelled",
          turn_reconciliation_required: "reconciliation_required",
        }};
        if (item.body.cause_code !== lifecycleCauseByKind[item.kind]) {{
          fail(`${{path}}.body.cause_code`, `the cause for ${{item.kind}}`);
        }}
        break;
      case "event_fact":
        if (item.body.kind !== item.kind || bodyOwnedKinds.has(item.kind)) {{
          fail(`${{path}}.body.kind`, "the matching header-only event kind");
        }}
        break;
      default:
        fail(`${{path}}.body.type`, "a detail body variant this decoder classifies");
    }}
    const computedItemBytes = detailEnvelopeBytes + textBytes;
    if (item.projected_body_bytes !== computedItemBytes) {{
      fail(`${{path}}.projected_body_bytes`, `the computed ${{computedItemBytes}} bytes`);
    }}
    computedProjectedBodyBytes += computedItemBytes;
    if (computedProjectedBodyBytes > maxProjectedBodyBytes) {{
      fail("timeline_detail_page.projected_body_bytes", `at most ${{maxProjectedBodyBytes}} bytes`);
    }}
    if (continuation !== null) {{
      expectedBodyContinuation = continuation;
    }}
  }});
  if (value.projected_body_bytes !== computedProjectedBodyBytes) {{
    fail(
      "timeline_detail_page.projected_body_bytes",
      `the computed ${{computedProjectedBodyBytes}} bytes`,
    );
  }}

  if (value.continuation === undefined || value.continuation === null) {{
    if (expectedBodyContinuation !== null) {{
      fail("timeline_detail_page.continuation", "the excerpt body continuation");
    }}
    return;
  }}
  if (value.continuation.type === "more_body") {{
    if (
      expectedBodyContinuation === null ||
      !sameBodyContinuation(value.continuation.body, expectedBodyContinuation)
    ) {{
      fail("timeline_detail_page.continuation.body", "the excerpt body continuation");
    }}
  }} else {{
    if (expectedBodyContinuation !== null) {{
      fail("timeline_detail_page.continuation", "more_body for a continued excerpt");
    }}
    if (previousAddress === null) {{
      fail("timeline_detail_page.continuation", "absent on an empty page");
    }}
    if (BigInt(value.continuation.address.event_sequence) <= previousAddress) {{
      fail("timeline_detail_page.continuation.address", "after the final returned item");
    }}
  }}
}}

export function decodeWebSessionTimelineDetailPage(value) {{
  assertSchema(schemas.WebSessionTimelineDetailPage, schemas.WebSessionTimelineDetailPage, value, "timeline_detail_page");
  assertTimelineDetailPage(value);
  return value;
}}

function assertLiveSnapshot(snapshot, path) {{
  const queuedTurnCount = BigInt(snapshot.queued_turn_count);
  const previewLimit = BigInt({live_preview_limit});
  const expectedPreviewLength = queuedTurnCount > previewLimit ? previewLimit : queuedTurnCount;
  if (BigInt(snapshot.queued_turn_ids.length) !== expectedPreviewLength) {{
    fail(`${{path}}.queued_turn_ids`, `exactly ${{expectedPreviewLength}} IDs for queued_turn_count`);
  }}
  if (new Set(snapshot.queued_turn_ids).size !== snapshot.queued_turn_ids.length) {{
    fail(`${{path}}.queued_turn_ids`, "unique turn IDs");
  }}
  const occupiedTurnId = snapshot.active?.turn_id ?? snapshot.reconciliation?.turn_id;
  if (occupiedTurnId !== undefined && snapshot.queued_turn_ids.includes(occupiedTurnId)) {{
    fail(`${{path}}.queued_turn_ids`, "disjoint from active and reconciliation turn IDs");
  }}
  if (snapshot.active != null && snapshot.reconciliation != null) {{
    fail(`${{path}}.reconciliation`, "absent while an active turn is present");
  }}
  if (
    snapshot.active?.state.kind === "awaiting_child" &&
    snapshot.active.state.child_session_id === snapshot.session_id
  ) {{
    fail(`${{path}}.active.state.child_session_id`, "different from the parent session ID");
  }}
  if (snapshot.active?.state.kind === "awaiting_runner_recovery") {{
    const recovery = snapshot.active.state;
    const runner = snapshot.runner;
    const compatibleRunner =
      runner != null &&
      (runner.state === "runner_lost" || runner.state === "runner_lost_before_pin") &&
      runner.runner_id === recovery.runner_id &&
      runner.placement_revision === recovery.placement_revision;
    if (!compatibleRunner) {{
      fail(`${{path}}.runner`, "the runner placement required by awaiting_runner_recovery");
    }}
  }}
}}

function assertAttentionSummary(summary, path) {{
  const action = summary.action ?? null;
  const goalBlock = summary.goal_block ?? null;
  const valid =
    (summary.state === "blocked" &&
      (action === "provide_goal_need" ||
        (action === null && goalBlock?.reason === "execution_failure"))) ||
    (summary.state === "awaiting_approval" &&
      (action === null || action === "decide_approval")) ||
    (summary.state === "ambiguous" && action === "reconcile_turn") ||
    ([
      "active",
      "queued",
      "awaiting_tool_recovery",
      "awaiting_reconciliation",
      "runner_lost",
      "parked",
      "idle",
    ].includes(summary.state) && action === null);
  if (!valid) {{
    fail(`${{path}}.action`, `consistent with attention state ${{JSON.stringify(summary.state)}}`);
  }}
  const validGoalBlock =
    (summary.state === "blocked" && goalBlock !== null) ||
    summary.state === "runner_lost" ||
    (summary.state !== "blocked" && goalBlock === null);
  if (!validGoalBlock) {{
    fail(
      `${{path}}.goal_block`,
      `consistent with attention state ${{JSON.stringify(summary.state)}}`,
    );
  }}
}}

function assertSessionCatalogSummary(summary, path) {{
  assertAttentionSummary(summary, path);
  const turnBacked = [
    "active",
    "queued",
    "awaiting_approval",
    "ambiguous",
    "awaiting_tool_recovery",
    "awaiting_reconciliation",
  ].includes(summary.state);
  if (turnBacked && summary.current_turn_id === null) {{
    fail(`${{path}}.current_turn_id`, `a turn identity for state ${{summary.state}}`);
  }}
  const activeBacked = [
    "active",
    "awaiting_approval",
    "ambiguous",
    "awaiting_tool_recovery",
  ].includes(summary.state);
  if (activeBacked && BigInt(summary.active_turn_count) === 0n) {{
    fail(`${{path}}.active_turn_count`, `at least one active turn for state ${{summary.state}}`);
  }}
  if (summary.state === "queued" && BigInt(summary.queued_turn_count) === 0n) {{
    fail(`${{path}}.queued_turn_count`, "at least one queued turn for queued state");
  }}
  if (summary.title_summary === null && summary.title_truncated) {{
    fail(`${{path}}.title_truncated`, "false when title_summary is null");
  }}
  if (
    summary.title_truncated &&
    summary.title_summary !== null &&
    Array.from(summary.title_summary).length !== {max_attention_title_scalars}
  ) {{
    fail(
      `${{path}}.title_summary`,
      "exactly {max_attention_title_scalars} Unicode scalar values when title_truncated is true",
    );
  }}
}}

function assertAttentionSummaries(summaries, path) {{
  summaries.forEach((summary, index) =>
    assertAttentionSummary(summary, `${{path}}[${{index}}]`),
  );
}}

function assertAttentionSnapshot(snapshot, path) {{
  assertAttentionSummaries(snapshot.summaries, `${{path}}.summaries`);
  const continuation = snapshot.continuation_after_session_id ?? null;
  if (continuation !== null) {{
    const last = snapshot.summaries.at(-1);
    if (last === undefined || continuation !== last.session_id) {{
      fail(
        `${{path}}.continuation_after_session_id`,
        "the last returned session identity",
      );
    }}
  }}
}}

function assertSessionCatalogSnapshot(snapshot, path) {{
  snapshot.summaries.forEach((summary, index) =>
    assertSessionCatalogSummary(summary, `${{path}}.summaries[${{index}}]`),
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
    const boundary = snapshot.summaries.at(-1);
    if (boundary === undefined) {{
      fail(`${{path}}.continuation`, "absent when no summaries are returned");
    }}
    if (snapshot.continuation.session_id !== boundary.session_id) {{
      fail(
        `${{path}}.continuation.session_id`,
        "the session of the last returned summary",
      );
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

function assertCanonicalU64(value, path) {{
  if (!/^[1-9][0-9]{{0,19}}$/.test(value) || BigInt(value) > 18446744073709551615n) {{
    fail(path, "a positive canonical decimal u64 string");
  }}
}}

const utf8 = new TextEncoder();

function compareUtf8(left, right) {{
  const leftBytes = utf8.encode(left);
  const rightBytes = utf8.encode(right);
  const shared = Math.min(leftBytes.length, rightBytes.length);
  for (let index = 0; index < shared; index += 1) {{
    if (leftBytes[index] !== rightBytes[index]) return leftBytes[index] - rightBytes[index];
  }}
  return leftBytes.length - rightBytes.length;
}}

function parseCanonicalJson(source) {{
  let cursor = 0;
  const parseString = () => {{
    const start = cursor;
    cursor += 1;
    while (cursor < source.length) {{
      if (source[cursor] === "\\") cursor += source[cursor + 1] === "u" ? 6 : 2;
      else if (source[cursor] === '"') {{
        cursor += 1;
        const spelling = source.slice(start, cursor);
        const decoded = JSON.parse(spelling);
        if (JSON.stringify(decoded) !== spelling) throw new TypeError();
        for (let index = 0; index < decoded.length; index += 1) {{
          const unit = decoded.charCodeAt(index);
          if (unit >= 0xd800 && unit <= 0xdbff) {{
            const next = decoded.charCodeAt(index + 1);
            if (!(next >= 0xdc00 && next <= 0xdfff)) throw new TypeError();
            index += 1;
          }} else if (unit >= 0xdc00 && unit <= 0xdfff) throw new TypeError();
        }}
        return decoded;
      }} else cursor += 1;
    }}
    throw new TypeError();
  }};
  const parseValue = () => {{
    const byte = source[cursor];
    if (byte === '"') {{ parseString(); return; }}
    if (byte === "[") {{
      cursor += 1;
      if (source[cursor] === "]") {{ cursor += 1; return; }}
      while (true) {{
        parseValue();
        if (source[cursor] === "]") {{ cursor += 1; return; }}
        if (source[cursor] !== ",") throw new TypeError();
        cursor += 1;
      }}
    }}
    if (byte === "{{") {{
      cursor += 1;
      if (source[cursor] === "}}") {{ cursor += 1; return; }}
      let previous;
      while (true) {{
        if (source[cursor] !== '"') throw new TypeError();
        const key = parseString();
        if (previous !== undefined && compareUtf8(previous, key) >= 0) throw new TypeError();
        previous = key;
        if (source[cursor] !== ":") throw new TypeError();
        cursor += 1;
        parseValue();
        if (source[cursor] === "}}") {{ cursor += 1; return; }}
        if (source[cursor] !== ",") throw new TypeError();
        cursor += 1;
      }}
    }}
    for (const literal of ["true", "false", "null"]) {{
      if (source.startsWith(literal, cursor)) {{ cursor += literal.length; return; }}
    }}
    const number = /^-?(?:0|[1-9][0-9]*)(?:\.[0-9]+)?(?:[eE][+-]?[0-9]+)?/u.exec(source.slice(cursor))?.[0];
    if (number === undefined) throw new TypeError();
    cursor += number.length;
  }};
  parseValue();
  if (cursor !== source.length) throw new TypeError();
}}

function assertCanonicalParametersJson(value, path) {{
  if (utf8.encode(value).length > 4096) {{
    fail(path, "canonical JSON of at most 4096 UTF-8 bytes");
  }}
  try {{
    parseCanonicalJson(value);
  }} catch {{
    fail(path, "canonical JSON");
  }}
}}

function assertDisplayFilename(value, path) {{
  if (
    typeof value !== "string" ||
    value.length === 0 ||
    new TextEncoder().encode(value).length > 1024 ||
    /\p{{Cc}}/u.test(value)
  ) {{
    fail(path, "a nonempty, control-free filename of at most 1024 UTF-8 bytes");
  }}
}}

function isMimeTokenCharacter(value) {{
  return /^[!#$%&'*+.^_`|~0-9A-Za-z-]$/u.test(value);
}}

function consumeMimeToken(value, cursor) {{
  const start = cursor;
  while (cursor < value.length && isMimeTokenCharacter(value[cursor])) cursor += 1;
  return cursor === start ? -1 : cursor;
}}

function isMimeValue(value) {{
  let cursor = 0;
  cursor = consumeMimeToken(value, cursor);
  if (cursor < 0 || value[cursor] !== "/") return false;
  cursor += 1;
  cursor = consumeMimeToken(value, cursor);
  if (cursor < 0) return false;
  if (cursor === value.length) return true;
  if (value[cursor] !== ";") return false;
  cursor += 1;
  while (cursor < value.length) {{
    while (value[cursor] === " ") cursor += 1;
    if (cursor === value.length) return true;
    cursor = consumeMimeToken(value, cursor);
    if (cursor < 0 || value[cursor] !== "=") return false;
    cursor += 1;
    if (value[cursor] === '"') {{
      cursor += 1;
      const start = cursor;
      while (cursor < value.length && value[cursor] !== '"') {{
        const unit = value.charCodeAt(cursor);
        if (unit <= 31 || unit === 127) return false;
        cursor += 1;
      }}
      if (cursor === start || value[cursor] !== '"') return false;
      cursor += 1;
      while (value[cursor] === " ") cursor += 1;
    }} else {{
      cursor = consumeMimeToken(value, cursor);
      if (cursor < 0) return false;
    }}
    if (cursor === value.length) return true;
    if (value[cursor] !== ";") return false;
    cursor += 1;
  }}
  return true;
}}

function assertMediaType(value, path) {{
  if (
    typeof value !== "string" ||
    utf8.encode(value).length > 255 ||
    !isMimeValue(value)
  ) {{
    fail(path, "a MIME value of at most 255 UTF-8 bytes");
  }}
}}

function assertBlobDigest(value, path) {{
  if (!/^sha256:[0-9a-f]{{64}}$/.test(value)) {{
    fail(path, "a tagged lowercase SHA-256 digest");
  }}
}}

function assertUuid(value, path) {{
  if (!/^[0-9a-f]{{8}}-[0-9a-f]{{4}}-[0-9a-f]{{4}}-[0-9a-f]{{4}}-[0-9a-f]{{12}}$/.test(value)) {{
    fail(path, "a canonical lowercase UUID");
  }}
}}

function assertSameOriginBlobUrl(value, path) {{
  const base = "http://signalbox.invalid";
  if (typeof value !== "string" || !value.startsWith("/") || value.startsWith("//")) {{
    fail(path, "a root-relative blob API path");
  }}
  const parsed = new URL(value, base);
  if (parsed.origin !== base || !parsed.pathname.startsWith("/api/blobs/") || parsed.hash !== "") {{
    fail(path, "a same-origin blob API path");
  }}
  const route = /^\/api\/blobs\/(sha256:[0-9a-f]{{64}})\/(download|content\/(?:image-png|image-jpeg|image-gif|image-webp))$/u.exec(parsed.pathname);
  if (route === null) {{
    fail(path, "a canonical blob API route");
  }}
  let mediaType;
  if (route[2] === "download") {{
    const mediaTypes = parsed.searchParams.getAll("media_type");
    const filenames = parsed.searchParams.getAll("display_filename");
    const known = [...parsed.searchParams.keys()].every((key) => key === "media_type" || key === "display_filename");
    if (mediaTypes.length !== 1 || mediaTypes[0] === "" || filenames.length > 1 || !known) {{
      fail(path, "a download route with required media type metadata");
    }}
    assertMediaType(mediaTypes[0], `${{path}} media_type`);
    mediaType = mediaTypes[0];
    if (filenames.length === 1) {{
      assertDisplayFilename(filenames[0], `${{path}} display_filename`);
    }}
  }} else if (parsed.search !== "") {{
    fail(path, "a content route without query metadata");
  }} else {{
    mediaType = {{
      "content/image-png": "image/png",
      "content/image-jpeg": "image/jpeg",
      "content/image-gif": "image/gif",
      "content/image-webp": "image/webp",
    }}[route[2]];
  }}
  return {{
    digest: route[1],
    kind: route[2],
    mediaType,
    displayFilename: route[2] === "download"
      ? parsed.searchParams.get("display_filename") ?? undefined
      : undefined,
  }};
}}

function u64Bytes(value) {{
  const output = new Uint8Array(8);
  let remaining = BigInt(value);
  for (let index = 7; index >= 0; index -= 1) {{ output[index] = Number(remaining & 255n); remaining >>= 8n; }}
  return output;
}}

function digestBytes(value) {{
  return Uint8Array.from(value.slice(7).match(/../gu), (pair) => Number.parseInt(pair, 16));
}}

function sha256(bytes) {{
  const constants = new Uint32Array([0x428a2f98,0x71374491,0xb5c0fbcf,0xe9b5dba5,0x3956c25b,0x59f111f1,0x923f82a4,0xab1c5ed5,0xd807aa98,0x12835b01,0x243185be,0x550c7dc3,0x72be5d74,0x80deb1fe,0x9bdc06a7,0xc19bf174,0xe49b69c1,0xefbe4786,0x0fc19dc6,0x240ca1cc,0x2de92c6f,0x4a7484aa,0x5cb0a9dc,0x76f988da,0x983e5152,0xa831c66d,0xb00327c8,0xbf597fc7,0xc6e00bf3,0xd5a79147,0x06ca6351,0x14292967,0x27b70a85,0x2e1b2138,0x4d2c6dfc,0x53380d13,0x650a7354,0x766a0abb,0x81c2c92e,0x92722c85,0xa2bfe8a1,0xa81a664b,0xc24b8b70,0xc76c51a3,0xd192e819,0xd6990624,0xf40e3585,0x106aa070,0x19a4c116,0x1e376c08,0x2748774c,0x34b0bcb5,0x391c0cb3,0x4ed8aa4a,0x5b9cca4f,0x682e6ff3,0x748f82ee,0x78a5636f,0x84c87814,0x8cc70208,0x90befffa,0xa4506ceb,0xbef9a3f7,0xc67178f2]);
  const paddedLength = Math.ceil((bytes.length + 9) / 64) * 64;
  const padded = new Uint8Array(paddedLength); padded.set(bytes); padded[bytes.length] = 0x80;
  let bits = BigInt(bytes.length) * 8n;
  for (let index = paddedLength - 1; index >= paddedLength - 8; index -= 1) {{ padded[index] = Number(bits & 255n); bits >>= 8n; }}
  const state = new Uint32Array([0x6a09e667,0xbb67ae85,0x3c6ef372,0xa54ff53a,0x510e527f,0x9b05688c,0x1f83d9ab,0x5be0cd19]);
  const words = new Uint32Array(64);
  const rotate = (value, count) => (value >>> count) | (value << (32 - count));
  for (let block = 0; block < paddedLength; block += 64) {{
    for (let index = 0; index < 16; index += 1) {{ const offset = block + index * 4; words[index] = (padded[offset] << 24) | (padded[offset + 1] << 16) | (padded[offset + 2] << 8) | padded[offset + 3]; }}
    for (let index = 16; index < 64; index += 1) {{ const x=words[index-15], y=words[index-2]; words[index]=(words[index-16]+(rotate(x,7)^rotate(x,18)^(x>>>3))+words[index-7]+(rotate(y,17)^rotate(y,19)^(y>>>10)))>>>0; }}
    let [a,b,c,d,e,f,g,h]=state;
    for (let index=0; index<64; index+=1) {{ const sum1=(h+(rotate(e,6)^rotate(e,11)^rotate(e,25))+((e&f)^(~e&g))+constants[index]+words[index])>>>0; const sum0=(rotate(a,2)^rotate(a,13)^rotate(a,22))>>>0; const majority=((a&b)^(a&c)^(b&c))>>>0; h=g;g=f;f=e;e=(d+sum1)>>>0;d=c;c=b;b=a;a=(sum1+sum0+majority)>>>0; }}
    for (const [index,value] of [a,b,c,d,e,f,g,h].entries()) state[index]=(state[index]+value)>>>0;
  }}
  return [...state].map((word) => word.toString(16).padStart(8, "0")).join("");
}}

function deterministicCacheKey(derivation) {{
  const name=utf8.encode(derivation.transformation_name), parameters=utf8.encode(derivation.parameters_json);
  const version=new Uint8Array(4); new DataView(version.buffer).setUint32(0, derivation.transformation_version);
  const pieces=[utf8.encode("signalbox.blob-derivation.v1\0"),u64Bytes(derivation.input_digests.length)];
  derivation.input_digests.forEach((digest)=>pieces.push(digestBytes(digest)));
  pieces.push(u64Bytes(name.length),name,version,u64Bytes(parameters.length),parameters,digestBytes(derivation.producer.implementation_digest));
  const framed=new Uint8Array(pieces.reduce((total,piece)=>total+piece.length,0)); let offset=0;
  pieces.forEach((piece)=>{{ framed.set(piece,offset); offset+=piece.length; }});
  return `sha256:${{sha256(framed)}}`;
}}

export function decodeWebBlobDescriptor(value) {{
  assertSchema(schemas.WebBlobDescriptor, schemas.WebBlobDescriptor, value, "blob_descriptor");
  assertBlobDigest(value.digest, "blob_descriptor.digest");
  assertCanonicalU64(value.byte_length, "blob_descriptor.byte_length");
  assertMediaType(value.declared_media_type, "blob_descriptor.declared_media_type");
  value.display_filename.forEach((filename, index) =>
    assertDisplayFilename(filename, `blob_descriptor.display_filename[${{index}}]`));
  value.available_views.forEach((view, index) => {{
    assertMediaType(view.media_type, `blob_descriptor.available_views[${{index}}].media_type`);
    assertCanonicalU64(view.byte_length, `blob_descriptor.available_views[${{index}}].byte_length`);
    const contentPath = `blob_descriptor.available_views[${{index}}].content_url`;
    const contentRoute = assertSameOriginBlobUrl(view.content_url, contentPath);
    const contentDigest = contentRoute.digest;
    if (view.media_type !== contentRoute.mediaType) {{
      fail(`blob_descriptor.available_views[${{index}}].media_type`, "the content route media type");
    }}
    if (view.kind === "download" || view.kind === "browser_native") {{
      if (view.derivations.length !== 0) {{
        fail(`blob_descriptor.available_views[${{index}}].derivations`, "empty for an original representation");
      }}
      if (contentDigest !== value.digest) {{
        fail(contentPath, "a route for the descriptor digest");
      }}
      if ((view.kind === "download") !== (contentRoute.kind === "download")) {{
        fail(contentPath, "a route matching the advertised view kind");
      }}
      if (view.byte_length !== value.byte_length) {{
        fail(`blob_descriptor.available_views[${{index}}].byte_length`, "the descriptor byte length for an original representation");
      }}
      if (view.kind === "download" && view.media_type !== value.declared_media_type) {{
        fail(`blob_descriptor.available_views[${{index}}].media_type`, "the descriptor declared media type for the download representation");
      }}
      if (
        view.kind === "download" &&
        contentRoute.displayFilename !== value.display_filename[0]
      ) {{
        fail(contentPath, "download filename metadata matching the descriptor");
      }}
      if (
        view.kind === "browser_native" &&
        value.declared_media_type.split(";", 1)[0].trim().toLowerCase() !== contentRoute.mediaType
      ) {{
        fail(contentPath, "an original-image route matching the descriptor declared media type");
      }}
    }} else {{
      if (contentRoute.kind !== "content/image-png") {{
        fail(contentPath, "an image-content route for a derivative view");
      }}
      if (!view.derivations.some((derivation) =>
        derivation.input_digests.includes(value.digest) &&
        derivation.output_digests.includes(contentDigest))) {{
        fail(contentPath, "a route for a derivation output bound to the descriptor input");
      }}
    }}
    view.derivations.forEach((derivation, derivationIndex) => {{
      const path = `blob_descriptor.available_views[${{index}}].derivations[${{derivationIndex}}]`;
      assertCanonicalParametersJson(derivation.parameters_json, `${{path}}.parameters_json`);
      assertUuid(derivation.derivation_id, `${{path}}.derivation_id`);
      derivation.input_digests.forEach((digest, digestIndex) =>
        assertBlobDigest(digest, `${{path}}.input_digests[${{digestIndex}}]`));
      derivation.output_digests.forEach((digest, digestIndex) =>
        assertBlobDigest(digest, `${{path}}.output_digests[${{digestIndex}}]`));
      if (derivation.producer.class === "deterministic") {{
        assertBlobDigest(derivation.producer.implementation_digest, `${{path}}.producer.implementation_digest`);
        assertBlobDigest(derivation.producer.cache_key, `${{path}}.producer.cache_key`);
        if (deterministicCacheKey(derivation) !== derivation.producer.cache_key) {{
          fail(`${{path}}.producer.cache_key`, "the deterministic key for the advertised provenance");
        }}
      }} else if (derivation.producer.class === "executed") {{
        assertUuid(derivation.producer.execution_id, `${{path}}.producer.execution_id`);
        assertBlobDigest(derivation.producer.implementation_digest, `${{path}}.producer.implementation_digest`);
      }} else {{
        assertUuid(derivation.producer.model_call_id, `${{path}}.producer.model_call_id`);
      }}
    }});
    if (view.kind === "thumbnail" || view.kind === "preview") {{
      const derivation = view.derivations[0];
      const expectedName = view.kind === "thumbnail" ? "image.thumbnail" : "image.preview";
      const expectedParameters = view.kind === "thumbnail"
        ? '{{"edge_px":256,"format":"image/png"}}'
        : '{{"edge_px":1600,"format":"image/png"}}';
      if (
        derivation === undefined ||
        derivation.transformation_name !== expectedName ||
        derivation.transformation_version !== 1 ||
        derivation.parameters_json !== expectedParameters ||
        derivation.producer.class !== "deterministic"
      ) {{
        fail(
          `blob_descriptor.available_views[${{index}}].derivations`,
          "the exact deterministic image transformation for the advertised view kind",
        );
      }}
    }}
  }});
  const downloadViews = value.available_views.filter((view) => view.kind === "download");
  if (downloadViews.length !== 1) {{
    fail("blob_descriptor.available_views", "exactly one download view");
  }}
  const representationKinds = value.available_views.map((view) => view.kind);
  if (new Set(representationKinds).size !== representationKinds.length) {{
    fail("blob_descriptor.available_views", "at most one view of each representation kind");
  }}
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
  }} else if (value.kind === "update") {{
    assertAttentionSummaries(value.summaries, "attention_event.summaries");
  }}
  return value;
}}

export function decodeWebSessionCatalogSnapshot(value) {{
  assertSchema(
    schemas.WebSessionCatalogSnapshot,
    schemas.WebSessionCatalogSnapshot,
    value,
    "session_catalog_snapshot",
  );
  assertSessionCatalogSnapshot(value, "session_catalog_snapshot");
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
  if (value.continuation != null) {{
    const last = value.results.at(-1);
    if (
      last === undefined ||
      value.continuation.address.event_sequence !== last.address.event_sequence ||
      value.continuation.projection_id !== last.projection_id
    ) {{
      fail("search_page.continuation", "the last result ordering key");
    }}
  }}
  return value;
}}

export function decodeWebUsageSummary(value) {{
  assertSchema(schemas.WebUsageSummary, schemas.WebUsageSummary, value, "usage_summary");
  const encoder = new TextEncoder();
  const compatibilityKeys = new Set();
  let totalCallCount = 0n;
  value.groups.forEach((group, index) => {{
    const callCount = BigInt(group.call_count);
    totalCallCount += callCount;
    if (totalCallCount > 10000n) {{
      fail("usage_summary.groups", "at most 10000 represented calls");
    }}
    assertUsageEvidence(
      group.input_semantics,
      group.tokens,
      group.cost,
      `usage_summary.groups[${{index}}]`,
      group.input_semantics === "cache_inclusive" &&
        callCount > 1n &&
        group.tokens.input !== null &&
        group.tokens.cache_creation_input !== null &&
        group.tokens.cache_read_input !== null,
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
  const encoder = new TextEncoder();
  let previousKey = null;
  const callIds = new Set();
  value.calls.forEach((call, index) => {{
    assertUsageEvidence(
      call.input_semantics,
      call.tokens,
      call.cost,
      `usage_call_page.calls[${{index}}]`,
      false,
    );
    const profileBytes = encoder.encode(call.profile_id).length;
    if (profileBytes === 0 || profileBytes > 256) {{
      fail(`usage_call_page.calls[${{index}}].profile_id`, "1 through 256 UTF-8 bytes");
    }}
    const isCompaction = call.call_kind === "context_compaction";
    if (!Object.hasOwn(call, "turn_id") || isCompaction !== (call.turn_id === null)) {{
      fail(
        `usage_call_page.calls[${{index}}].turn_id`,
        "null exactly for context compaction calls",
      );
    }}
    const key = {{ recordedAt: BigInt(call.recorded_at_micros), callId: call.call_id }};
    if (callIds.has(call.call_id)) {{
      fail(`usage_call_page.calls[${{index}}].call_id`, "unique within the page");
    }}
    callIds.add(call.call_id);
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
        max_attention_title_scalars = MAX_ATTENTION_TITLE_SCALARS,
        live_preview_limit = max_session_live_queued_turns(),
        max_search_snippet_bytes = max_search_snippet_bytes(),
    );
    for schema in schemas {
        // These decoders carry hand-written structural invariants beyond their
        // schema shape (blob view provenance, timeline-detail correlations,
        // attention state/action agreement, search highlight ranges) and are
        // emitted verbatim in the template above.
        if matches!(
            schema.name,
            "WebBlobDescriptor"
                | "WebSessionTimelineDetailPage"
                | "WebAttentionSnapshot"
                | "WebAttentionStreamEvent"
                | "WebSessionCatalogSnapshot"
                | "WebSearchPage"
                | "WebUsageSummary"
                | "WebUsageCallPage"
        ) {
            continue;
        }
        let path = schema.name.to_ascii_lowercase();
        output.push_str(&format!(
            "export function {decoder}(value) {{\n  assertSchema(schemas.{name}, schemas.{name}, value, {path:?});\n",
            decoder = schema.decoder,
            name = schema.name,
        ));
        if schema.name == "WebContractBootstrap" {
            output.push_str(&format!(
                "  if (value.contract.name !== {name:?} || value.contract.version !== {version:?} ||\n      value.capabilities.bounded_json !== {bounded_json} ||\n      value.capabilities.same_origin_json_mutations !== {same_origin_json_mutations} ||\n      value.capabilities.ndjson_streaming !== {ndjson_streaming} ||\n      value.capabilities.import_discovery !== {import_discovery} ||\n      value.capabilities.imported_continuations !== {imported_continuations} ||\n      value.capabilities.bounded_session_live !== {bounded_session_live} ||\n      value.limits.max_json_body_bytes !== {max_json_body_bytes} ||\n      value.limits.max_ndjson_item_bytes !== {max_ndjson_item_bytes} ||\n      value.limits.max_session_live_queued_turns !== {max_session_live_queued_turns}) {{\n    throw new TypeError(\"bootstrap carries an incompatible web contract\");\n  }}\n",
                name = WEB_CONTRACT_NAME,
                version = WEB_CONTRACT_VERSION,
                bounded_json = current_bootstrap.capabilities.bounded_json,
                same_origin_json_mutations = current_bootstrap.capabilities.same_origin_json_mutations,
                ndjson_streaming = current_bootstrap.capabilities.ndjson_streaming,
                import_discovery = current_bootstrap.capabilities.import_discovery,
                imported_continuations = current_bootstrap.capabilities.imported_continuations,
                bounded_session_live = current_bootstrap.capabilities.bounded_session_live,
                max_json_body_bytes = current_bootstrap.limits.max_json_body_bytes,
                max_ndjson_item_bytes = current_bootstrap.limits.max_ndjson_item_bytes,
                max_session_live_queued_turns = current_bootstrap.limits.max_session_live_queued_turns,
            ));
        }
        if schema.name == "WebSessionLiveSnapshot" {
            output.push_str("  assertLiveSnapshot(value, \"session_live_snapshot\");\n");
        }
        if schema.name == "WebSessionLiveStreamEvent" {
            output.push_str(
                "  if (value.kind === \"snapshot\") {\n    assertLiveSnapshot(value.snapshot, \"session_live_event.snapshot\");\n  }\n  if (value.kind === \"durable\" && value.cursor !== value.address.event_sequence) {\n    fail(\"session_live_event.address.event_sequence\", \"equal to cursor\");\n  }\n",
            );
            output.push_str(&format!(
                "  if (value.kind === \"provider_text_delta\" && new TextEncoder().encode(value.content).length > {MAX_WEB_PROVIDER_TEXT_FRAGMENT_BYTES}) {{\n    fail(\"session_live_event.content\", \"at most {MAX_WEB_PROVIDER_TEXT_FRAGMENT_BYTES} UTF-8 bytes\");\n  }}\n"
            ));
        }
        output.push_str("  return value;\n}\n\n");
    }
    output.truncate(output.trim_end_matches('\n').len());
    output.push('\n');
    Ok(output)
}

fn declaration_module(schemas: &[ContractSchema]) -> Result<String, GenerateWebContractError> {
    let mut definitions = BTreeMap::new();
    let mut roots = Vec::with_capacity(schemas.len());
    for schema in schemas {
        roots.push((
            schema,
            typescript_type(&schema.schema, &schema.schema, &mut definitions)?,
        ));
    }
    // A root DTO reached by `$ref` from another root (the attention stream event
    // embeds the attention snapshot) also lands in `definitions`. Its root
    // export below already declares the name, so keeping the definition too
    // would emit the same `export type` twice.
    for (schema, _) in &roots {
        definitions.remove(schema.name);
    }
    let mut output = String::from(
        "// @generated by `cargo run -p signalbox-web-contract --bin generate-web-contract`.\n// Do not edit by hand.\n\n",
    );
    for (name, definition) in definitions {
        output.push_str(&format!("export type {name} = {definition};\n\n"));
    }
    for (schema, root) in &roots {
        output.push_str(&format!("export type {} = {root};\n\n", schema.name));
    }
    for (schema, _) in roots {
        // The usage call page decoder also takes the page order its caller
        // requested, which is what lets it check descending call keys.
        let parameters = if schema.name == "WebUsageCallPage" {
            "value: unknown, order: \"newest\""
        } else {
            "value: unknown"
        };
        output.push_str(&format!(
            "export function {}({parameters}): {};\n",
            schema.decoder, schema.name
        ));
    }
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
    if let Some(variants) = schema
        .get("oneOf")
        .or_else(|| schema.get("anyOf"))
        .and_then(Value::as_array)
    {
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
        WebAttentionStreamEvent, WebContractBootstrap, WebContractExample, WebDollarAmount,
        WebSessionId, WebTimelineEventSequence, WebTimelineModelCallState, WebU64,
        WebUsageCallCount, WebUsageRateVersion, WebUsageTimestampMicros, generated_artifacts,
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
    fn checked_in_bootstrap_fixture_matches_rust_authority() {
        assert_generated_artifact_current("clients/web/src/generated/web-contract-bootstrap.json");
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
    fn blob_derivation_capability_requires_exposed_derivative_views() {
        let bootstrap = WebContractBootstrap::for_runtime(true, false);

        assert!(bootstrap.capabilities.immutable_blob_content);
        assert!(!bootstrap.capabilities.blob_derivations);
        assert!(!bootstrap.capabilities.image_derivatives);
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
    fn attention_stream_event_rejects_unknown_variant_fields() {
        let encoded = r#"{"kind":"resync_required","cursor":"1","unexpected":true}"#;

        assert!(serde_json::from_str::<WebAttentionStreamEvent>(encoded).is_err());
    }

    #[test]
    fn attention_runtime_decoder_enforces_string_patterns() {
        let runtime = generated_artifacts()
            .expect("the Rust schemas can generate browser artifacts")
            .into_iter()
            .find(|artifact| artifact.path == "clients/web/src/generated/web-contract.mjs")
            .expect("the runtime decoder artifact exists")
            .contents;

        assert!(runtime.contains("new RegExp(schema.pattern, \"u\").test(value)"));
        assert!(runtime.contains(r#""pattern": "^(0|[1-9][0-9]*)$""#));
        assert!(runtime.contains(
            r#""pattern": "^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$""#,
        ));
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
    fn model_call_state_rejects_contradictory_extra_fields() {
        assert!(
            serde_json::from_str::<WebTimelineModelCallState>(
                r#"{"type":"prepared","disposition":"completed"}"#,
            )
            .is_err()
        );
        assert!(
            serde_json::from_str::<WebTimelineModelCallState>(
                r#"{"type":"terminal","disposition":"completed"}"#,
            )
            .is_ok()
        );
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
