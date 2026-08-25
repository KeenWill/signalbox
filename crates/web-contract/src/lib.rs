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
    let bootstrap_schema = canonical_schema(schemars::schema_for!(WebContractBootstrap).to_value());
    let example_schema = canonical_schema(schemars::schema_for!(WebContractExample).to_value());
    let error_schema = canonical_schema(schemars::schema_for!(WebApiErrorResponse).to_value());
    let blob_schema = canonical_schema(schemars::schema_for!(WebBlobDescriptor).to_value());
    let descriptor_schema =
        canonical_schema(schemars::schema_for!(WebSessionTimelineDescriptor).to_value());
    let mut window_schema =
        canonical_schema(schemars::schema_for!(WebSessionTimelineWindow).to_value());
    make_property_nullable(&mut window_schema, "continuation_before")?;
    make_property_nullable(&mut window_schema, "continuation_after")?;
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
                &blob_schema,
                &descriptor_schema,
                &window_schema,
            )?,
        },
        GeneratedArtifact {
            path: "clients/web/src/generated/web-contract.d.mts",
            contents: declaration_module(
                &bootstrap_schema,
                &example_schema,
                &error_schema,
                &blob_schema,
                &descriptor_schema,
                &window_schema,
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
    let property = schema
        .pointer_mut(&format!("/properties/{property_name}"))
        .ok_or(GenerateWebContractError::UnsupportedSchema)?;
    let concrete = property.take();
    *property = json!({ "anyOf": [concrete, { "type": "null" }] });
    Ok(())
}

fn runtime_module(
    bootstrap_schema: &Value,
    example_schema: &Value,
    error_schema: &Value,
    blob_schema: &Value,
    descriptor_schema: &Value,
    window_schema: &Value,
) -> Result<String, GenerateWebContractError> {
    let mut schemas = json!({
        "WebContractBootstrap": bootstrap_schema,
        "WebContractExample": example_schema,
        "WebApiErrorResponse": error_schema,
        "WebBlobDescriptor": blob_schema,
        "WebSessionTimelineDescriptor": descriptor_schema,
        "WebSessionTimelineWindow": window_schema,
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
    if (
      (schema.pattern === "^[1-9][0-9]*$" || schema.pattern === "^(0|[1-9][0-9]*)$") &&
      (value.length > 20 || BigInt(value) > 18446744073709551615n)
    ) {{
      fail(path, "an unsigned 64-bit integer");
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

export function decodeWebSessionTimelineDescriptor(value) {{
  assertSchema(schemas.WebSessionTimelineDescriptor, schemas.WebSessionTimelineDescriptor, value, "session_descriptor");
  return value;
}}

export function decodeWebSessionTimelineWindow(value) {{
  assertSchema(schemas.WebSessionTimelineWindow, schemas.WebSessionTimelineWindow, value, "timeline_window");
  return value;
}}
"##,
        contract_name = WEB_CONTRACT_NAME,
        contract_version = WEB_CONTRACT_VERSION,
    ))
}

fn declaration_module(
    bootstrap_schema: &Value,
    example_schema: &Value,
    error_schema: &Value,
    blob_schema: &Value,
    descriptor_schema: &Value,
    window_schema: &Value,
) -> Result<String, GenerateWebContractError> {
    let mut definitions = BTreeMap::new();
    let bootstrap = typescript_type(bootstrap_schema, bootstrap_schema, &mut definitions)?;
    let example = typescript_type(example_schema, example_schema, &mut definitions)?;
    let error = typescript_type(error_schema, error_schema, &mut definitions)?;
    let blob = typescript_type(blob_schema, blob_schema, &mut definitions)?;
    let descriptor = typescript_type(descriptor_schema, descriptor_schema, &mut definitions)?;
    let window = typescript_type(window_schema, window_schema, &mut definitions)?;
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
    output.push_str(&format!("export type WebBlobDescriptor = {blob};\n\n"));
    output.push_str(&format!(
        "export type WebSessionTimelineDescriptor = {descriptor};\n\n"
    ));
    output.push_str(&format!(
        "export type WebSessionTimelineWindow = {window};\n\n"
    ));
    output.push_str(
        "export function decodeWebContractBootstrap(value: unknown): WebContractBootstrap;\nexport function decodeWebContractExample(value: unknown): WebContractExample;\nexport function decodeWebApiErrorResponse(value: unknown): WebApiErrorResponse;\nexport function decodeWebBlobDescriptor(value: unknown): WebBlobDescriptor;\nexport function decodeWebSessionTimelineDescriptor(value: unknown): WebSessionTimelineDescriptor;\nexport function decodeWebSessionTimelineWindow(value: unknown): WebSessionTimelineWindow;\n",
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
