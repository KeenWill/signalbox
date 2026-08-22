//! Rust-authoritative browser HTTP data-transfer contract.
//!
//! The generated TypeScript declarations and runtime decoders are derived from
//! these serde and JSON Schema definitions. Browser DTOs deliberately remain
//! distinct from domain, persistence, and local process-protocol values.

use std::{collections::BTreeMap, error::Error, fmt};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

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
                import_discovery: true,
                imported_continuations: true,
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
    /// Bounded evidence for a non-text value too large to classify without full decoding.
    OpaqueNonText,
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
    const accepted = schema.type.some((candidate) => {{
      try {{
        assertSchema(root, {{ ...schema, type: candidate }}, value, path);
        return true;
      }} catch {{
        return false;
      }}
    }});
    if (!accepted) {{
      fail(path, "one recognized type");
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
    if let Some(kinds) = schema.get("type").and_then(Value::as_array) {
        return Ok(kinds
            .iter()
            .map(|kind| {
                let mut variant = schema.clone();
                variant["type"] = kind.clone();
                typescript_type(root, &variant, definitions)
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
        Some("string") => Ok("string".to_owned()),
        Some("null") => Ok("null".to_owned()),
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
