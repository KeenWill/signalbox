//! Rust-authoritative browser HTTP data-transfer contract.
//!
//! The generated TypeScript declarations and runtime decoders are derived from
//! these serde and JSON Schema definitions. Browser DTOs deliberately remain
//! distinct from domain, persistence, and local process-protocol values.

use std::{collections::BTreeMap, error::Error, fmt};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use signalbox_application::{max_timeline_window_bytes, max_timeline_window_items};

/// Exact browser HTTP contract version served by this daemon build.
pub const WEB_CONTRACT_VERSION: &str = "2";
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
    /// Immutable same-origin blob descriptors and byte delivery are available.
    pub immutable_blob_content: bool,
    /// Blob-to-blob provenance reads are present on derivative views.
    pub blob_derivations: bool,
    /// The daemon can lazily produce isolated deterministic image derivatives.
    pub image_derivatives: bool,
    /// Stable bounded session descriptors and historical windows are available.
    pub bounded_session_timeline: bool,
    /// Bounded imported-conversation discovery and entry windows are available.
    pub import_discovery: bool,
    /// Imported frontiers can seed a native session through an idempotent command.
    pub imported_continuations: bool,
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
        Self::for_runtime(false, false, false, false)
    }

    /// Describes this contract with deployment-bound capabilities.
    #[must_use]
    pub fn for_runtime(
        immutable_blob_content: bool,
        image_derivatives: bool,
        bounded_session_timeline: bool,
        imports_available: bool,
    ) -> Self {
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
                bounded_session_timeline,
                import_discovery: imports_available,
                imported_continuations: imports_available,
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

/// Stable browser-visible location of one durable session event.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WebTimelineAddress {
    /// Positive global durable event sequence encoded losslessly for JavaScript.
    pub event_sequence: String,
}

/// Explicit lifetime size facts used only for browser loading policy.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WebSessionTimelineSizeFacts {
    pub item_count: String,
    pub projected_text_bytes: String,
    pub projected_structured_bytes: String,
    pub referenced_blob_count: String,
    pub referenced_blob_bytes: String,
}

/// Current work facts carried by the lightweight session descriptor.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WebSessionWorkFacts {
    pub active_turn_count: String,
    pub queued_turn_count: String,
}

/// Browser descriptor for one authoritative bounded session projection.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WebSessionTimelineDescriptor {
    pub session_id: String,
    pub sizes: WebSessionTimelineSizeFacts,
    pub first_address: WebTimelineAddress,
    pub latest_address: WebTimelineAddress,
    pub work: WebSessionWorkFacts,
    pub observed_through: String,
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
    pub session_id: String,
    pub items: Vec<WebSessionTimelineItem>,
    pub projected_structured_bytes: u32,
    pub continuation_before: Option<WebTimelineAddress>,
    pub continuation_after: Option<WebTimelineAddress>,
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

/// Bounded imports catalog request carried as query parameters.
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
    /// Number of normalized imported entries encoded losslessly for JavaScript.
    pub entry_count: String,
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
    #[schemars(length(max = 100))]
    pub items: Vec<WebImportSummary>,
    /// Exclusive cursor for the next page, absent at the end.
    pub next_cursor: Option<String>,
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
    /// Sum of exact raw source-record occurrence bytes, encoded losslessly for JavaScript.
    pub raw_source_bytes: String,
    /// Sum of normalized source-record encoding bytes, encoded losslessly for JavaScript.
    pub normalized_source_record_bytes: String,
    /// Sum of normalized entry and source-metadata encoding bytes, encoded losslessly for JavaScript.
    pub normalized_entry_bytes: String,
}

/// One immutable imported frontier suitable for precise continuation.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WebImportContinuationReference {
    /// Owning imported-conversation UUID.
    pub imported_conversation_id: String,
    /// Exact imported-entry UUID at the inclusive frontier.
    pub imported_entry_id: String,
    /// One-based immutable imported position encoded losslessly for JavaScript.
    pub position: String,
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
    /// Number of exact raw source records encoded losslessly for JavaScript.
    pub raw_record_count: String,
    /// Number of normalized imported entries encoded losslessly for JavaScript.
    pub entry_count: String,
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
    /// Required only for the `position` anchor; encoded losslessly for JavaScript.
    pub position: Option<String>,
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
    /// One-based physical source-record occurrence encoded losslessly for JavaScript.
    pub raw_record_position: String,
    /// One-based normalized entry position within that source record, encoded losslessly for JavaScript.
    pub record_entry_position: String,
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
    /// Resolved immutable anchor position encoded losslessly for JavaScript.
    pub anchor_position: String,
    /// First position returned, encoded losslessly for JavaScript.
    pub first_position: String,
    /// Last position returned, encoded losslessly for JavaScript.
    pub last_position: String,
    /// Whether earlier entries exist.
    pub has_before: bool,
    /// Whether later entries exist.
    pub has_after: bool,
    /// Entries in ascending immutable position order.
    #[schemars(length(max = 101))]
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

/// Produces all checked-in browser contract artifacts.
///
/// # Errors
///
/// Returns a closed build-time error when serde cannot encode a generated value
/// or a DTO schema grows beyond the generator's focused supported shapes.
pub fn generated_artifacts() -> Result<Vec<GeneratedArtifact>, GenerateWebContractError> {
    let schemas = contract_schemas();
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

struct ContractSchema {
    name: &'static str,
    decoder: &'static str,
    schema: Value,
}

fn contract_schemas() -> Vec<ContractSchema> {
    vec![
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
            schema: canonical_schema(schemars::schema_for!(WebSessionTimelineWindow).to_value()),
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
    ]
}

fn canonical_schema(mut schema: Value) -> Value {
    // Schemars' default feature set preserves declaration order, while its
    // focused no-default build uses sorted maps. Workspace feature unification
    // must not change checked-in artifacts.
    schema.sort_all_objects();
    schema
}

fn runtime_module(schemas: &[ContractSchema]) -> Result<String, GenerateWebContractError> {
    let mut schema_values = serde_json::Map::new();
    for schema in schemas {
        schema_values.insert(schema.name.to_owned(), schema.schema.clone());
    }
    let mut schema_values = Value::Object(schema_values);
    schema_values.sort_all_objects();
    let schema_values = serde_json::to_string_pretty(&schema_values)
        .map_err(|_| GenerateWebContractError::Serialization)?;
    let mut output = format!(
        r##"// @generated by `cargo run -p signalbox-web-contract --bin generate-web-contract`.
// Do not edit by hand.

const schemas = {schema_values};

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
    if (schema.minLength !== undefined && value.length < schema.minLength) {{
      fail(path, `at least ${{schema.minLength}} characters`);
    }}
    if (schema.maxLength !== undefined && value.length > schema.maxLength) {{
      fail(path, `at most ${{schema.maxLength}} characters`);
    }}
    if (schema.pattern !== undefined && !new RegExp(schema.pattern, "u").test(value)) {{
      fail(path, `matching ${{schema.pattern}}`);
    }}
    return;
  }}
  if (typeof value !== schema.type) {{
    fail(path, schema.type);
  }}
}}

function assertCanonicalU64(value, path) {{
  if (!/^[1-9][0-9]{{0,19}}$/.test(value) || BigInt(value) > 18446744073709551615n) {{
    fail(path, "a positive canonical decimal u64 string");
  }}
}}

function assertCanonicalNonnegativeU64(value, path) {{
  if (!/^(?:0|[1-9][0-9]{{0,19}})$/.test(value) || BigInt(value) > 18446744073709551615n) {{
    fail(path, "a nonnegative canonical decimal u64 string");
  }}
}}

function assertSha256(value, path) {{
  if (!/^[0-9a-f]{{64}}$/.test(value)) {{
    fail(path, "a lowercase hexadecimal SHA-256 digest");
  }}
}}

function assertImportEvidenceBytes(value, path) {{
  if (utf8.encode(value).byteLength > 512) {{
    fail(path, "at most 512 UTF-8 bytes");
  }}
}}

function validateWebImportFrontier(value, path) {{
  assertUuid(value.imported_conversation_id, `${{path}}.imported_conversation_id`);
  assertUuid(value.imported_entry_id, `${{path}}.imported_entry_id`);
  assertCanonicalU64(value.position, `${{path}}.position`);
}}

function validateWebImportListPage(value) {{
  value.items.forEach((item, index) => {{
    const path = `import_list_page.items[${{index}}]`;
    assertCanonicalU64(item.entry_count, `${{path}}.entry_count`);
    if (item.source_session_id !== undefined && item.source_session_id !== null) {{
      assertImportEvidenceBytes(item.source_session_id.leading_text, `${{path}}.source_session_id.leading_text`);
    }}
  }});
}}

function validateWebImportDescriptor(value) {{
  assertSha256(value.source.source_digest_sha256, "import_descriptor.source.source_digest_sha256");
  assertCanonicalU64(value.raw_record_count, "import_descriptor.raw_record_count");
  assertCanonicalNonnegativeU64(value.entry_count, "import_descriptor.entry_count");
  assertCanonicalU64(value.sizes.raw_source_bytes, "import_descriptor.sizes.raw_source_bytes");
  assertCanonicalU64(value.sizes.normalized_source_record_bytes, "import_descriptor.sizes.normalized_source_record_bytes");
  assertCanonicalU64(value.sizes.normalized_entry_bytes, "import_descriptor.sizes.normalized_entry_bytes");
  validateWebImportFrontier(value.timeline.first, "import_descriptor.timeline.first");
  validateWebImportFrontier(value.timeline.latest, "import_descriptor.timeline.latest");
  if (value.source.source_session_id !== undefined && value.source.source_session_id !== null) {{
    assertImportEvidenceBytes(value.source.source_session_id.leading_text, "import_descriptor.source.source_session_id.leading_text");
  }}
}}

function validateWebImportEntryWindowRequest(value) {{
  if (value.position !== undefined && value.position !== null) {{
    assertCanonicalU64(value.position, "import_entry_window_request.position");
  }}
}}

function validateWebImportEntryWindow(value) {{
  assertCanonicalU64(value.anchor_position, "import_entry_window.anchor_position");
  assertCanonicalU64(value.first_position, "import_entry_window.first_position");
  assertCanonicalU64(value.last_position, "import_entry_window.last_position");
  value.items.forEach((item, index) => {{
    const path = `import_entry_window.items[${{index}}]`;
    validateWebImportFrontier(item.frontier, `${{path}}.frontier`);
    assertCanonicalU64(item.raw_record_position, `${{path}}.raw_record_position`);
    assertCanonicalU64(item.record_entry_position, `${{path}}.record_entry_position`);
    const hasText = item.text !== undefined && item.text !== null;
    if ((item.content_kind === "text") !== hasText) {{
      throw new TypeError(`${{path}}.text must be present exactly for text content`);
    }}
    if (item.text?.kind === "attested") {{
      assertImportEvidenceBytes(item.text.leading_text, `${{path}}.text.leading_text`);
    }}
  }});
}}

function validateWebImportContinuation(value, path) {{
  assertUuid(value.command_id, `${{path}}.command_id`);
  validateWebImportFrontier(value.frontier, `${{path}}.frontier`);
  if (path === "import_continuation_request") {{
    const selection = value.initial_model_selection;
    if (selection.kind === "direct") {{
      assertUuid(selection.selection_id, `${{path}}.initial_model_selection.selection_id`);
    }} else {{
      assertUuid(selection.alias_id, `${{path}}.initial_model_selection.alias_id`);
    }}
  }}
  if (path === "import_continuation_response") {{
    assertUuid(value.session_id, `${{path}}.session_id`);
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

function validateWebBlobDescriptor(value) {{
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

"##,
    );
    for schema in schemas {
        let path = schema.name.to_ascii_lowercase();
        output.push_str(&format!(
            "export function {decoder}(value) {{\n  assertSchema(schemas.{name}, schemas.{name}, value, {path:?});\n",
            decoder = schema.decoder,
            name = schema.name,
        ));
        if schema.name == "WebContractBootstrap" {
            output.push_str(&format!(
                "  if (value.contract.name !== {name:?} || value.contract.version !== {version:?}) {{\n    throw new TypeError(\"bootstrap carries an incompatible web contract\");\n  }}\n",
                name = WEB_CONTRACT_NAME,
                version = WEB_CONTRACT_VERSION,
            ));
        }
        if schema.name == "WebBlobDescriptor" {
            output.push_str("  validateWebBlobDescriptor(value);\n");
        }
        match schema.name {
            "WebImportListPage" => output.push_str("  validateWebImportListPage(value);\n"),
            "WebImportDescriptor" => output.push_str("  validateWebImportDescriptor(value);\n"),
            "WebImportEntryWindowRequest" => {
                output.push_str("  validateWebImportEntryWindowRequest(value);\n");
            }
            "WebImportEntryWindow" => {
                output.push_str("  validateWebImportEntryWindow(value);\n");
            }
            "WebImportContinuationRequest" => output.push_str(
                "  validateWebImportContinuation(value, \"import_continuation_request\");\n",
            ),
            "WebImportContinuationResponse" => output.push_str(
                "  validateWebImportContinuation(value, \"import_continuation_response\");\n",
            ),
            _ => {}
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
        output.push_str(&format!(
            "export function {}(value: unknown): {};\n",
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
    fn runtime_bootstrap_reports_only_configured_capabilities() {
        let unavailable = WebContractBootstrap::for_runtime(false, false, false, false);
        let available = WebContractBootstrap::for_runtime(true, true, true, true);

        assert!(!unavailable.capabilities.immutable_blob_content);
        assert!(!unavailable.capabilities.blob_derivations);
        assert!(!unavailable.capabilities.image_derivatives);
        assert!(!unavailable.capabilities.bounded_session_timeline);
        assert!(!unavailable.capabilities.import_discovery);
        assert!(!unavailable.capabilities.imported_continuations);
        assert!(available.capabilities.immutable_blob_content);
        assert!(available.capabilities.blob_derivations);
        assert!(available.capabilities.image_derivatives);
        assert!(available.capabilities.bounded_session_timeline);
        assert!(available.capabilities.import_discovery);
        assert!(available.capabilities.imported_continuations);
    }

    #[test]
    fn blob_derivation_capability_requires_exposed_derivative_views() {
        let bootstrap = WebContractBootstrap::for_runtime(true, false, true, false);

        assert!(bootstrap.capabilities.immutable_blob_content);
        assert!(!bootstrap.capabilities.blob_derivations);
        assert!(!bootstrap.capabilities.image_derivatives);
        assert!(bootstrap.capabilities.bounded_session_timeline);
        assert!(!bootstrap.capabilities.import_discovery);
        assert!(!bootstrap.capabilities.imported_continuations);
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
