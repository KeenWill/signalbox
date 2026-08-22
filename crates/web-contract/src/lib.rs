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
                blob_derivations: immutable_blob_content,
                image_derivatives,
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
    pub declared_media_type: String,
    #[schemars(length(max = 1))]
    pub display_filename: Vec<String>,
    #[schemars(length(max = 4))]
    pub available_views: Vec<WebBlobAvailableView>,
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
            )?,
        },
        GeneratedArtifact {
            path: "clients/web/src/generated/web-contract.d.mts",
            contents: declaration_module(
                &bootstrap_schema,
                &example_schema,
                &error_schema,
                &blob_schema,
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

fn runtime_module(
    bootstrap_schema: &Value,
    example_schema: &Value,
    error_schema: &Value,
    blob_schema: &Value,
) -> Result<String, GenerateWebContractError> {
    let mut schemas = json!({
        "WebContractBootstrap": bootstrap_schema,
        "WebContractExample": example_schema,
        "WebApiErrorResponse": error_schema,
        "WebBlobDescriptor": blob_schema,
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

const rawJsonInteger = Symbol("rawJsonInteger");

function canonicalJson(value) {{
  if (value !== null && typeof value === "object" && Object.hasOwn(value, rawJsonInteger)) {{
    return value[rawJsonInteger];
  }}
  if (Array.isArray(value)) {{
    return `[${{value.map(canonicalJson).join(",")}}]`;
  }}
  if (value !== null && typeof value === "object") {{
    return `{{${{Object.keys(value)
      .sort()
      .map((key) => `${{JSON.stringify(key)}}:${{canonicalJson(value[key])}}`)
      .join(",")}}}}`;
  }}
  return JSON.stringify(value);
}}

function assertCanonicalParametersJson(value, path) {{
  if (new TextEncoder().encode(value).length > 4096) {{
    fail(path, "canonical JSON of at most 4096 UTF-8 bytes");
  }}
  let parsed;
  try {{
    parsed = JSON.parse(value, (_key, parsedValue, context) => {{
      if (
        typeof parsedValue === "number" &&
        Number.isInteger(parsedValue) &&
        !Number.isSafeInteger(parsedValue)
      ) {{
        return {{ [rawJsonInteger]: context.source }};
      }}
      return parsedValue;
    }});
  }} catch {{
    fail(path, "canonical JSON");
  }}
  if (canonicalJson(parsed) !== value) {{
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
  const route = /^\/api\/blobs\/(sha256:[0-9a-f]{{64}})\/(?:download|content\/[a-z0-9-]+)$/u.exec(parsed.pathname);
  if (route === null) {{
    fail(path, "a canonical blob API route");
  }}
  return route[1];
}}

export function decodeWebBlobDescriptor(value) {{
  assertSchema(schemas.WebBlobDescriptor, schemas.WebBlobDescriptor, value, "blob_descriptor");
  assertBlobDigest(value.digest, "blob_descriptor.digest");
  assertCanonicalU64(value.byte_length, "blob_descriptor.byte_length");
  value.display_filename.forEach((filename, index) =>
    assertDisplayFilename(filename, `blob_descriptor.display_filename[${{index}}]`));
  value.available_views.forEach((view, index) => {{
    assertCanonicalU64(view.byte_length, `blob_descriptor.available_views[${{index}}].byte_length`);
    const contentPath = `blob_descriptor.available_views[${{index}}].content_url`;
    const contentDigest = assertSameOriginBlobUrl(view.content_url, contentPath);
    if (view.kind === "download" || view.kind === "browser_native") {{
      if (contentDigest !== value.digest) {{
        fail(contentPath, "a route for the descriptor digest");
      }}
    }} else if (!view.derivations.some((derivation) =>
      derivation.input_digests.includes(value.digest) &&
      derivation.output_digests.includes(contentDigest))) {{
      fail(contentPath, "a route for a derivation output bound to the descriptor input");
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
      }} else if (derivation.producer.class === "executed") {{
        assertUuid(derivation.producer.execution_id, `${{path}}.producer.execution_id`);
        assertBlobDigest(derivation.producer.implementation_digest, `${{path}}.producer.implementation_digest`);
      }} else {{
        assertUuid(derivation.producer.model_call_id, `${{path}}.producer.model_call_id`);
      }}
    }});
  }});
  const downloadViews = value.available_views.filter((view) => view.kind === "download");
  if (downloadViews.length !== 1) {{
    fail("blob_descriptor.available_views", "exactly one download view");
  }}
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
) -> Result<String, GenerateWebContractError> {
    let mut definitions = BTreeMap::new();
    let bootstrap = typescript_type(bootstrap_schema, bootstrap_schema, &mut definitions)?;
    let example = typescript_type(example_schema, example_schema, &mut definitions)?;
    let error = typescript_type(error_schema, error_schema, &mut definitions)?;
    let blob = typescript_type(blob_schema, blob_schema, &mut definitions)?;
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
    output.push_str(
        "export function decodeWebContractBootstrap(value: unknown): WebContractBootstrap;\nexport function decodeWebContractExample(value: unknown): WebContractExample;\nexport function decodeWebApiErrorResponse(value: unknown): WebApiErrorResponse;\nexport function decodeWebBlobDescriptor(value: unknown): WebBlobDescriptor;\n",
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
