//! Browser-facing same-origin HTTP transport foundation.
//!
//! This boundary owns browser HTTP semantics and browser DTOs. It does not
//! expose local process-protocol messages, storage records, or application
//! authentication.

use std::{
    env,
    error::Error,
    ffi::OsString,
    fmt, io,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    num::NonZeroU64,
    path::PathBuf,
    str::FromStr as _,
};

use axum::{
    Json, Router,
    body::{Body, Bytes, to_bytes},
    extract::{DefaultBodyLimit, Path, Query, Request, State, rejection::QueryRejection},
    http::{
        HeaderMap, HeaderValue, Method, StatusCode,
        header::{
            ACCEPT_RANGES, CACHE_CONTROL, CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_RANGE,
            CONTENT_TYPE, ETAG, HOST, IF_NONE_MATCH, IF_RANGE, ORIGIN, RANGE,
            X_CONTENT_TYPE_OPTIONS,
        },
    },
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use futures_util::{Stream, StreamExt, stream};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use signalbox_domain::{BlobDerivation, BlobDerivationProducer, BlobDigest};
use signalbox_web_contract::{
    MAX_JSON_BODY_BYTES, MAX_NDJSON_ITEM_BYTES, WebApiError, WebApiErrorKind, WebApiErrorResponse,
    WebBlobAvailableView, WebBlobDerivation, WebBlobDerivationProducer, WebBlobDescriptor,
    WebBlobViewKind, WebContractBootstrap, WebContractExample,
};
use tokio::{io::AsyncReadExt as _, net::TcpListener, sync::watch};
use tower_http::services::{ServeDir, ServeFile};
use url::Url;

use crate::{
    WebBlobRuntime, WebImageDerivativeKind, blob_read_runtime::open_recorded_blob_range,
    web_blob_runtime::WebBlobRuntimeError,
};

/// Optional deployment override for the browser listener.
pub const WEB_BIND_ENVIRONMENT: &str = "SIGNALBOX_WEB_BIND";
/// Optional production web-build root served outside `/api/`.
pub const WEB_ASSET_ROOT_ENVIRONMENT: &str = "SIGNALBOX_WEB_ASSET_ROOT";
/// Conservative browser listener default: reachable only from this host.
pub const DEFAULT_WEB_BIND_ADDRESS: SocketAddr =
    SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 37_231);

const JSON_CONTENT_TYPE: &str = "application/json";
const NDJSON_CONTENT_TYPE: &str = "application/x-ndjson";
const HTTP_DEFAULT_PORT: u16 = 80;
const IMMUTABLE_CACHE_CONTROL: &str = "public, max-age=31536000, immutable";
const MAX_DISPLAY_FILENAME_BYTES: usize = 1024;
const BLOB_STREAM_CHUNK_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug)]
struct WebHttpState {
    blobs: Option<WebBlobRuntime>,
}

/// Deployment-owned browser listener and production assets configuration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WebHttpConfiguration {
    bind_address: SocketAddr,
    asset_root: Option<PathBuf>,
}

impl WebHttpConfiguration {
    /// Reads the two browser transport settings from the process environment.
    pub fn from_environment() -> Result<Self, WebHttpConfigurationError> {
        Self::from_values(
            env::var_os(WEB_BIND_ENVIRONMENT),
            env::var_os(WEB_ASSET_ROOT_ENVIRONMENT),
        )
    }

    fn from_values(
        bind_address: Option<OsString>,
        asset_root: Option<OsString>,
    ) -> Result<Self, WebHttpConfigurationError> {
        let bind_address = match bind_address {
            None => DEFAULT_WEB_BIND_ADDRESS,
            Some(value) => value
                .into_string()
                .map_err(|_| WebHttpConfigurationError::BindAddressNotUnicode)?
                .parse()
                .map_err(|_| WebHttpConfigurationError::InvalidBindAddress)?,
        };
        let asset_root = match asset_root {
            None => None,
            Some(value) if value.is_empty() => {
                return Err(WebHttpConfigurationError::EmptyAssetRoot);
            }
            Some(value) => Some(PathBuf::from(value)),
        };
        Ok(Self {
            bind_address,
            asset_root,
        })
    }

    /// Creates explicit configuration for a deterministic or embedded server.
    #[must_use]
    pub fn new(bind_address: SocketAddr, asset_root: Option<PathBuf>) -> Self {
        Self {
            bind_address,
            asset_root,
        }
    }

    /// Address the listener binds.
    #[must_use]
    pub const fn bind_address(&self) -> SocketAddr {
        self.bind_address
    }

    /// Optional root containing a static production web build.
    #[must_use]
    pub fn asset_root(&self) -> Option<&PathBuf> {
        self.asset_root.as_ref()
    }
}

/// Closed configuration failures that never expose rejected values.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WebHttpConfigurationError {
    /// Explicit listener setting was not Unicode.
    BindAddressNotUnicode,
    /// Explicit listener setting was not a socket address.
    InvalidBindAddress,
    /// Explicit production asset root was empty.
    EmptyAssetRoot,
}

impl fmt::Display for WebHttpConfigurationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BindAddressNotUnicode => {
                write!(
                    formatter,
                    "setting {WEB_BIND_ENVIRONMENT} is not valid Unicode"
                )
            }
            Self::InvalidBindAddress => {
                write!(
                    formatter,
                    "setting {WEB_BIND_ENVIRONMENT} is not a socket address"
                )
            }
            Self::EmptyAssetRoot => {
                write!(formatter, "setting {WEB_ASSET_ROOT_ENVIRONMENT} is empty")
            }
        }
    }
}

impl Error for WebHttpConfigurationError {}

/// Closed browser runtime failure classification.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WebHttpRuntimeError {
    /// The configured listener could not bind.
    Bind,
    /// The bound HTTP server failed before shutdown.
    Serve,
}

impl fmt::Display for WebHttpRuntimeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Bind => formatter.write_str("browser HTTP listener could not bind"),
            Self::Serve => formatter.write_str("browser HTTP listener failed"),
        }
    }
}

impl Error for WebHttpRuntimeError {}

/// Bound browser HTTP runtime.
pub struct WebHttpRuntime {
    listener: TcpListener,
    router: Router,
}

impl WebHttpRuntime {
    /// Binds the production same-origin router.
    pub async fn bind(
        configuration: WebHttpConfiguration,
        blobs: Option<WebBlobRuntime>,
    ) -> Result<Self, WebHttpRuntimeError> {
        let router = production_router(configuration.asset_root, blobs);
        Self::bind_router(configuration.bind_address, router).await
    }

    /// Binds an explicit router, primarily for deterministic browser scenarios.
    pub async fn bind_router(
        bind_address: SocketAddr,
        router: Router,
    ) -> Result<Self, WebHttpRuntimeError> {
        let listener = TcpListener::bind(bind_address)
            .await
            .map_err(|_| WebHttpRuntimeError::Bind)?;
        Ok(Self { listener, router })
    }

    /// Actual address, including an operating-system-selected test port.
    pub fn local_address(&self) -> Result<SocketAddr, WebHttpRuntimeError> {
        self.listener
            .local_addr()
            .map_err(|_| WebHttpRuntimeError::Bind)
    }

    /// Serves until shutdown, then cancels requests by dropping their futures.
    pub async fn run(self, mut shutdown: watch::Receiver<bool>) -> Result<(), WebHttpRuntimeError> {
        let shutdown_requested = async move {
            if *shutdown.borrow() {
                return;
            }
            while shutdown.changed().await.is_ok() {
                if *shutdown.borrow() {
                    return;
                }
            }
        };
        axum::serve(self.listener, self.router)
            .with_graceful_shutdown(shutdown_requested)
            .await
            .map_err(|_| WebHttpRuntimeError::Serve)
    }
}

/// Builds the production router: `/api/` remains API-only and assets share its origin.
pub fn production_router(asset_root: Option<PathBuf>, blobs: Option<WebBlobRuntime>) -> Router {
    let api = Router::new()
        .route("/bootstrap", get(contract_bootstrap))
        .route("/blobs/{digest}/descriptor", get(blob_descriptor))
        .route(
            "/blobs/{digest}/content/{representation}",
            get(blob_content).head(blob_content),
        )
        .route(
            "/blobs/{digest}/download",
            get(blob_download).head(blob_download),
        )
        .fallback(api_not_found)
        .with_state(WebHttpState { blobs });
    let router = Router::new().nest("/api", api);
    match asset_root {
        Some(root) => router.fallback_service(
            ServeDir::new(root.clone())
                .append_index_html_on_directories(true)
                .fallback(ServeFile::new(root.join("index.html"))),
        ),
        None => router.fallback(static_assets_not_configured),
    }
}

/// Builds an in-memory deterministic server with no persistence dependency.
///
/// It uses the same guards, body decoder, generated DTOs, and NDJSON encoder as
/// production endpoints. The test-only surface is never mounted by
/// [`production_router`].
pub fn deterministic_test_router() -> Router {
    let mutation = Router::new()
        .route("/mutate", post(deterministic_mutation))
        .route_layer(middleware::from_fn(validate_json_mutation));
    let api = Router::new()
        .route("/bootstrap", get(deterministic_contract_bootstrap))
        .route("/test/read", get(deterministic_read))
        .route("/test/stream", get(deterministic_stream))
        .nest("/test", mutation)
        .fallback(api_not_found)
        .layer(DefaultBodyLimit::max(MAX_JSON_BODY_BYTES));
    Router::new()
        .route("/", get(deterministic_page))
        .nest("/api", api)
}

async fn contract_bootstrap(State(state): State<WebHttpState>) -> Json<WebContractBootstrap> {
    let image_derivatives = state
        .blobs
        .as_ref()
        .is_some_and(WebBlobRuntime::supports_image_derivatives);
    Json(WebContractBootstrap::for_runtime(
        state.blobs.is_some(),
        image_derivatives,
    ))
}

async fn deterministic_contract_bootstrap() -> Json<WebContractBootstrap> {
    Json(WebContractBootstrap::current())
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BlobUseQuery {
    media_type: String,
    display_filename: Option<String>,
}

async fn blob_descriptor(
    State(state): State<WebHttpState>,
    Path(digest): Path<String>,
    use_metadata: Result<Query<BlobUseQuery>, QueryRejection>,
) -> Response {
    let use_metadata = match use_metadata {
        Ok(Query(use_metadata)) => use_metadata,
        Err(_) => return invalid_blob_use_response(),
    };
    let Some(runtime) = state.blobs else {
        return application_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "blob_storage_unavailable",
            "blob storage is not configured",
        );
    };
    let digest = match BlobDigest::from_str(&digest) {
        Ok(digest) => digest,
        Err(_) => {
            return transport_error(
                StatusCode::BAD_REQUEST,
                "invalid_blob_digest",
                "blob digest is not canonical",
            );
        }
    };
    if !valid_blob_use(&use_metadata) {
        return transport_error(
            StatusCode::BAD_REQUEST,
            "invalid_blob_use",
            "blob media type or display filename is invalid",
        );
    }
    let entry = match runtime.entry(digest).await {
        Ok(entry) => entry,
        Err(error) => return runtime_error_response(error),
    };
    let query = blob_use_query(&use_metadata);
    let download_url = format!("/api/blobs/{digest}/download?{query}");
    let byte_length = entry.expected().byte_length().to_string();
    let mut available_views = vec![WebBlobAvailableView {
        kind: WebBlobViewKind::Download,
        media_type: use_metadata.media_type.clone(),
        byte_length: byte_length.clone(),
        content_url: download_url,
        derivations: Vec::new(),
    }];
    if let Some(representation) = image_representation(&use_metadata.media_type) {
        let Some(representation_media_type) = representation_media_type(representation) else {
            return runtime_error_response(WebBlobRuntimeError::Integrity);
        };
        available_views.push(WebBlobAvailableView {
            kind: WebBlobViewKind::BrowserNative,
            media_type: representation_media_type.to_owned(),
            byte_length: byte_length.clone(),
            content_url: format!("/api/blobs/{digest}/content/{representation}"),
            derivations: Vec::new(),
        });
        if runtime.supports_image_derivatives() {
            append_image_derivative_view(
                &runtime,
                digest,
                WebImageDerivativeKind::Thumbnail,
                WebBlobViewKind::Thumbnail,
                &mut available_views,
            )
            .await;
            append_image_derivative_view(
                &runtime,
                digest,
                WebImageDerivativeKind::Preview,
                WebBlobViewKind::Preview,
                &mut available_views,
            )
            .await;
        }
    }
    Json(WebBlobDescriptor {
        digest: digest.to_string(),
        byte_length,
        declared_media_type: use_metadata.media_type,
        display_filename: use_metadata.display_filename.into_iter().collect(),
        available_views,
    })
    .into_response()
}

async fn append_image_derivative_view(
    runtime: &WebBlobRuntime,
    input: BlobDigest,
    kind: WebImageDerivativeKind,
    view_kind: WebBlobViewKind,
    views: &mut Vec<WebBlobAvailableView>,
) {
    let Ok(derivation) = runtime.derive_image(input, kind).await else {
        return;
    };
    let Some(output) = derivation.outputs().first().copied() else {
        return;
    };
    let Ok(entry) = runtime.entry(output).await else {
        return;
    };
    let Some(provenance) = project_derivation(&derivation) else {
        return;
    };
    views.push(WebBlobAvailableView {
        kind: view_kind,
        media_type: String::from("image/png"),
        byte_length: entry.expected().byte_length().to_string(),
        content_url: format!("/api/blobs/{output}/content/image-png"),
        derivations: vec![provenance],
    });
}

fn project_derivation(derivation: &BlobDerivation) -> Option<WebBlobDerivation> {
    let producer = match derivation.producer() {
        BlobDerivationProducer::Deterministic { implementation } => {
            WebBlobDerivationProducer::Deterministic {
                implementation_digest: implementation.to_string(),
                cache_key: derivation.deterministic_key()?.digest().to_string(),
            }
        }
        BlobDerivationProducer::Executed {
            execution_id,
            implementation,
        } => WebBlobDerivationProducer::Executed {
            execution_id: execution_id.to_string(),
            implementation_digest: implementation.to_string(),
        },
        BlobDerivationProducer::ModelDerived { model_call } => {
            WebBlobDerivationProducer::ModelDerived {
                model_call_id: model_call.into_uuid().to_string(),
            }
        }
    };
    Some(WebBlobDerivation {
        derivation_id: derivation.id().into_uuid().to_string(),
        input_digests: derivation
            .inputs()
            .iter()
            .map(ToString::to_string)
            .collect(),
        transformation_name: derivation.transformation().name().as_str().to_owned(),
        transformation_version: derivation.transformation().version().get(),
        parameters_json: derivation.transformation().parameters_json().to_owned(),
        producer,
        output_digests: derivation
            .outputs()
            .iter()
            .map(ToString::to_string)
            .collect(),
    })
}

fn valid_blob_use(value: &BlobUseQuery) -> bool {
    !value.media_type.is_empty()
        && value.media_type.len() <= 255
        && value.media_type.parse::<mime::Mime>().is_ok()
        && value.display_filename.as_ref().is_none_or(|filename| {
            !filename.is_empty()
                && filename.len() <= MAX_DISPLAY_FILENAME_BYTES
                && !filename.chars().any(char::is_control)
        })
}

fn invalid_blob_use_response() -> Response {
    transport_error(
        StatusCode::BAD_REQUEST,
        "invalid_blob_use",
        "blob media type or display filename is invalid",
    )
}

fn blob_use_query(value: &BlobUseQuery) -> String {
    let mut serializer = url::form_urlencoded::Serializer::new(String::new());
    serializer.append_pair("media_type", &value.media_type);
    if let Some(filename) = &value.display_filename {
        serializer.append_pair("display_filename", filename);
    }
    serializer.finish()
}

fn image_representation(media_type: &str) -> Option<&'static str> {
    let media_type = media_type.parse::<mime::Mime>().ok()?;
    match (media_type.type_().as_str(), media_type.subtype().as_str()) {
        ("image", "png") => Some("image-png"),
        ("image", "jpeg") => Some("image-jpeg"),
        ("image", "gif") => Some("image-gif"),
        ("image", "webp") => Some("image-webp"),
        _ => None,
    }
}

fn representation_media_type(representation: &str) -> Option<&'static str> {
    match representation {
        "image-png" => Some("image/png"),
        "image-jpeg" => Some("image/jpeg"),
        "image-gif" => Some("image/gif"),
        "image-webp" => Some("image/webp"),
        _ => None,
    }
}

async fn blob_content(
    State(state): State<WebHttpState>,
    Path((digest, representation)): Path<(String, String)>,
    request: Request,
) -> Response {
    let Some(media_type) = representation_media_type(&representation) else {
        return api_not_found().await;
    };
    serve_blob(state, digest, media_type, None, request).await
}

async fn blob_download(
    State(state): State<WebHttpState>,
    Path(digest): Path<String>,
    use_metadata: Result<Query<BlobUseQuery>, QueryRejection>,
    request: Request,
) -> Response {
    let use_metadata = match use_metadata {
        Ok(Query(use_metadata)) => use_metadata,
        Err(_) => return invalid_blob_use_response(),
    };
    if !valid_blob_use(&use_metadata) {
        return transport_error(
            StatusCode::BAD_REQUEST,
            "invalid_blob_use",
            "blob media type or display filename is invalid",
        );
    }
    let filename = use_metadata
        .display_filename
        .as_deref()
        .unwrap_or("download");
    serve_blob(
        state,
        digest,
        &use_metadata.media_type,
        Some(content_disposition(filename)),
        request,
    )
    .await
}

async fn serve_blob(
    state: WebHttpState,
    digest: String,
    media_type: &str,
    disposition: Option<String>,
    request: Request,
) -> Response {
    let Some(runtime) = state.blobs else {
        return application_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "blob_storage_unavailable",
            "blob storage is not configured",
        );
    };
    let digest = match BlobDigest::from_str(&digest) {
        Ok(digest) => digest,
        Err(_) => {
            return transport_error(
                StatusCode::BAD_REQUEST,
                "invalid_blob_digest",
                "blob digest is not canonical",
            );
        }
    };
    let entry = match runtime.entry(digest).await {
        Ok(entry) => entry,
        Err(error) => return runtime_error_response(error),
    };
    let etag = format!("\"{digest}\"");
    if if_none_match(request.headers(), &etag) {
        return not_modified_response(&etag);
    }
    let total = entry.expected().byte_length();
    let requested_range = request
        .headers()
        .get(RANGE)
        .filter(|_| if_range_matches(request.headers(), &etag));
    let (offset, length, partial) = match requested_range {
        Some(range) => match parse_byte_range(range, total) {
            Ok(range) => range,
            Err(()) => return range_not_satisfiable(total, &etag),
        },
        None => (0, total, false),
    };
    let content_type = match HeaderValue::from_str(media_type) {
        Ok(value) => value,
        Err(_) => {
            return transport_error(
                StatusCode::BAD_REQUEST,
                "invalid_blob_media_type",
                "blob media type is not an HTTP field value",
            );
        }
    };
    let method = request.method().clone();
    let body = if method == Method::HEAD {
        Body::empty()
    } else {
        let Some(length) = NonZeroU64::new(length) else {
            return range_not_satisfiable(total, &etag);
        };
        let reader =
            match open_recorded_blob_range(runtime.registry(), &entry, offset, length).await {
                Ok(reader) => reader,
                Err(error) => return blob_read_error_response(error),
            };
        reader_body(reader, length.get())
    };
    let mut response = Response::new(body);
    *response.status_mut() = if partial {
        StatusCode::PARTIAL_CONTENT
    } else {
        StatusCode::OK
    };
    response.headers_mut().insert(CONTENT_TYPE, content_type);
    insert_static_blob_headers(response.headers_mut(), &etag, length);
    if partial {
        let end = offset + length - 1;
        insert_header(
            response.headers_mut(),
            CONTENT_RANGE,
            format!("bytes {offset}-{end}/{total}"),
        );
    }
    if let Some(disposition) = disposition {
        insert_header(response.headers_mut(), CONTENT_DISPOSITION, disposition);
    }
    response
}

fn reader_body(reader: signalbox_blob_store::BlobReader, length: u64) -> Body {
    let source = stream::try_unfold((reader, length), |(mut reader, remaining)| async move {
        if remaining == 0 {
            return Ok(None);
        }
        let capacity = usize::try_from(remaining.min(BLOB_STREAM_CHUNK_BYTES as u64))
            .map_err(|_| io::Error::other("blob response length is invalid"))?;
        let mut buffer = vec![0_u8; capacity];
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            return Err(io::Error::other(
                "blob response ended before its declared length",
            ));
        }
        buffer.truncate(read);
        let read = u64::try_from(read).map_err(|_| io::Error::other("blob read is invalid"))?;
        Ok(Some((Bytes::from(buffer), (reader, remaining - read))))
    });
    Body::from_stream(source)
}

fn parse_byte_range(value: &HeaderValue, total: u64) -> Result<(u64, u64, bool), ()> {
    let value = value.to_str().map_err(|_| ())?;
    let range = value.strip_prefix("bytes=").ok_or(())?;
    if range.contains(',') {
        return Err(());
    }
    let (start, end) = range.split_once('-').ok_or(())?;
    if start.is_empty() {
        let suffix = parse_canonical_u64(end)?.min(total);
        if suffix == 0 {
            return Err(());
        }
        return Ok((total - suffix, suffix, true));
    }
    let start = parse_canonical_u64(start)?;
    if start >= total {
        return Err(());
    }
    let end = if end.is_empty() {
        total - 1
    } else {
        parse_canonical_u64(end)?.min(total - 1)
    };
    if end < start {
        return Err(());
    }
    Ok((start, end - start + 1, true))
}

fn parse_canonical_u64(value: &str) -> Result<u64, ()> {
    if value.is_empty() || (value.starts_with('0') && value.len() > 1) {
        return Err(());
    }
    value.parse().map_err(|_| ())
}

fn if_none_match(headers: &HeaderMap, etag: &str) -> bool {
    headers
        .get(IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value.split(',').any(|candidate| {
                let candidate = candidate.trim();
                candidate == "*" || candidate == etag || candidate.strip_prefix("W/") == Some(etag)
            })
        })
}

fn if_range_matches(headers: &HeaderMap, etag: &str) -> bool {
    headers
        .get(IF_RANGE)
        .and_then(|value| value.to_str().ok())
        .is_none_or(|value| value.trim() == etag)
}

fn not_modified_response(etag: &str) -> Response {
    let mut response = StatusCode::NOT_MODIFIED.into_response();
    insert_header(response.headers_mut(), ETAG, etag.to_owned());
    response.headers_mut().insert(
        CACHE_CONTROL,
        HeaderValue::from_static(IMMUTABLE_CACHE_CONTROL),
    );
    response
}

fn range_not_satisfiable(total: u64, etag: &str) -> Response {
    let mut response = StatusCode::RANGE_NOT_SATISFIABLE.into_response();
    insert_header(
        response.headers_mut(),
        CONTENT_RANGE,
        format!("bytes */{total}"),
    );
    insert_header(response.headers_mut(), ETAG, etag.to_owned());
    response
}

fn insert_static_blob_headers(headers: &mut HeaderMap, etag: &str, length: u64) {
    headers.insert(ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    headers.insert(
        CACHE_CONTROL,
        HeaderValue::from_static(IMMUTABLE_CACHE_CONTROL),
    );
    headers.insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    insert_header(headers, ETAG, etag.to_owned());
    insert_header(headers, CONTENT_LENGTH, length.to_string());
}

fn insert_header(headers: &mut HeaderMap, name: axum::http::HeaderName, value: String) {
    if let Ok(value) = HeaderValue::from_str(&value) {
        headers.insert(name, value);
    }
}

fn content_disposition(filename: &str) -> String {
    let mut encoded = String::new();
    for byte in filename.bytes() {
        if byte.is_ascii_alphanumeric() || b"!#$&+-.^_`|~".contains(&byte) {
            encoded.push(char::from(byte));
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    format!("attachment; filename=\"download\"; filename*=UTF-8''{encoded}")
}

fn runtime_error_response(error: WebBlobRuntimeError) -> Response {
    match error {
        WebBlobRuntimeError::NotFound => application_error(
            StatusCode::NOT_FOUND,
            "blob_not_found",
            "blob does not exist",
        ),
        WebBlobRuntimeError::Busy => application_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "blob_derivation_busy",
            "blob derivative capacity is busy",
        ),
        WebBlobRuntimeError::Corrupt
        | WebBlobRuntimeError::Unavailable
        | WebBlobRuntimeError::IsolationUnavailable
        | WebBlobRuntimeError::ProducerFailed
        | WebBlobRuntimeError::Integrity => application_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "blob_unavailable",
            "blob content is temporarily unavailable",
        ),
    }
}

fn blob_read_error_response(error: crate::blob_read_runtime::BlobReadError) -> Response {
    use crate::blob_read_runtime::BlobReadError;
    match error {
        BlobReadError::NotFound => runtime_error_response(WebBlobRuntimeError::NotFound),
        BlobReadError::RangeOutOfBounds { .. } => application_error(
            StatusCode::RANGE_NOT_SATISFIABLE,
            "blob_range_not_satisfiable",
            "blob byte range is not satisfiable",
        ),
        BlobReadError::Missing
        | BlobReadError::Corrupt
        | BlobReadError::Unavailable
        | BlobReadError::Integrity => runtime_error_response(WebBlobRuntimeError::Unavailable),
    }
}

fn deterministic_example() -> WebContractExample {
    WebContractExample {
        request_id: "deterministic-request".to_owned(),
        message: "deterministic response".to_owned(),
    }
}

async fn deterministic_read() -> Json<WebContractExample> {
    Json(deterministic_example())
}

async fn deterministic_mutation(request: Request) -> Response {
    match decode_bounded_json::<WebContractExample>(request).await {
        Ok(value) => Json(value).into_response(),
        Err(error) => error,
    }
}

async fn deterministic_stream() -> Response {
    let first = deterministic_example();
    let second = WebContractExample {
        request_id: "deterministic-request-next".to_owned(),
        message: "incremental response".to_owned(),
    };
    ndjson_response(stream::iter([first, second]))
}

async fn deterministic_page() -> Response {
    const PAGE: &str = r##"<!doctype html>
<html lang="en"><meta charset="utf-8"><title>Signalbox transport scenario</title>
<body><main><h1>Signalbox transport scenario</h1><output id="status">loading</output></main>
<script type="module">
const bootstrap = await fetch("/api/bootstrap").then((response) => response.json());
const read = await fetch("/api/test/read").then((response) => response.json());
const stream = await fetch("/api/test/stream").then((response) => response.text());
document.querySelector("#status").textContent = `${bootstrap.contract.name}:${read.request_id}:${stream.trim().split("\n").length}`;
</script></body></html>"##;
    (
        [(
            CONTENT_TYPE,
            HeaderValue::from_static("text/html; charset=utf-8"),
        )],
        PAGE,
    )
        .into_response()
}

/// Decodes one JSON request after enforcing the contract's byte ceiling.
pub async fn decode_bounded_json<T>(request: Request) -> Result<T, Response>
where
    T: DeserializeOwned,
{
    let bytes = to_bytes(request.into_body(), MAX_JSON_BODY_BYTES)
        .await
        .map_err(|error| {
            if error_chain_contains_length_limit(&error) {
                transport_error(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "json_body_too_large",
                    "JSON request body exceeds the contract limit",
                )
            } else {
                transport_error(
                    StatusCode::BAD_REQUEST,
                    "json_body_read_failed",
                    "JSON request body could not be read",
                )
            }
        })?;
    serde_json::from_slice(&bytes).map_err(|_| {
        transport_error(
            StatusCode::BAD_REQUEST,
            "invalid_json",
            "request body is not the expected JSON value",
        )
    })
}

fn error_chain_contains_length_limit(error: &axum::Error) -> bool {
    let mut current: Option<&(dyn Error + 'static)> = Some(error);
    while let Some(error) = current {
        if error.is::<http_body_util::LengthLimitError>() {
            return true;
        }
        current = error.source();
    }
    false
}

/// Encodes an incrementally polled stream as fetch-compatible NDJSON.
///
/// Dropping the response body drops the source stream. A producer waiting on a
/// bounded channel therefore observes receiver closure when the browser
/// disconnects.
pub fn ndjson_response<S, T>(source: S) -> Response
where
    S: Stream<Item = T> + Send + 'static,
    T: Serialize + Send + 'static,
{
    let encoded = source.map(encode_ndjson_item);
    let mut response = Response::new(Body::from_stream(encoded));
    response
        .headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static(NDJSON_CONTENT_TYPE));
    response
}

fn encode_ndjson_item<T>(item: T) -> Result<Bytes, io::Error>
where
    T: Serialize,
{
    let mut writer = NdjsonItemWriter::new();
    if serde_json::to_writer(&mut writer, &item).is_err() {
        let message = if writer.limit_exceeded {
            "NDJSON item exceeds the contract limit"
        } else {
            "NDJSON item could not be encoded"
        };
        return Err(io::Error::other(message));
    }
    let mut encoded = writer.encoded;
    encoded.push(b'\n');
    Ok(Bytes::from(encoded))
}

struct NdjsonItemWriter {
    encoded: Vec<u8>,
    limit_exceeded: bool,
}

impl NdjsonItemWriter {
    fn new() -> Self {
        Self {
            encoded: Vec::with_capacity(MAX_NDJSON_ITEM_BYTES + 1),
            limit_exceeded: false,
        }
    }
}

impl io::Write for NdjsonItemWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let Some(new_length) = self.encoded.len().checked_add(buffer.len()) else {
            self.limit_exceeded = true;
            return Err(io::Error::other("NDJSON item exceeds the contract limit"));
        };
        if new_length > MAX_NDJSON_ITEM_BYTES {
            self.limit_exceeded = true;
            return Err(io::Error::other("NDJSON item exceeds the contract limit"));
        }
        self.encoded.extend_from_slice(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

async fn validate_json_mutation(request: Request, next: Next) -> Response {
    if request.method() != Method::POST {
        return transport_error(
            StatusCode::METHOD_NOT_ALLOWED,
            "mutation_method_not_allowed",
            "browser mutations use POST with JSON",
        );
    }
    if !has_json_content_type(request.headers()) {
        return transport_error(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "json_content_type_required",
            "browser mutations require application/json",
        );
    }
    if validate_supplied_origin(request.headers()).is_err() {
        return transport_error(
            StatusCode::FORBIDDEN,
            "cross_origin_mutation_rejected",
            "mutation origin does not match request authority",
        );
    }
    next.run(request).await
}

fn has_json_content_type(headers: &HeaderMap) -> bool {
    headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .is_some_and(|value| value.trim().eq_ignore_ascii_case(JSON_CONTENT_TYPE))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OriginValidationError {
    Mismatch,
}

fn validate_supplied_origin(headers: &HeaderMap) -> Result<(), OriginValidationError> {
    let Some(origin) = headers.get(ORIGIN) else {
        return Ok(());
    };
    let origin = origin
        .to_str()
        .ok()
        .and_then(|origin| Url::parse(origin).ok())
        .filter(|origin| matches!(origin.scheme(), "http" | "https"))
        .filter(|origin| origin.path() == "/")
        .filter(|origin| origin.query().is_none() && origin.fragment().is_none())
        .filter(|origin| origin.username().is_empty() && origin.password().is_none());
    let authority = headers
        .get(HOST)
        .and_then(|host| host.to_str().ok())
        .and_then(|host| host.parse::<axum::http::uri::Authority>().ok());
    let matching = origin.zip(authority).is_some_and(|(origin, authority)| {
        let authority_port = authority.port_u16().unwrap_or(HTTP_DEFAULT_PORT);
        origin
            .host_str()
            .is_some_and(|host| host.eq_ignore_ascii_case(authority.host()))
            && origin.port_or_known_default() == Some(authority_port)
    });
    if matching {
        Ok(())
    } else {
        Err(OriginValidationError::Mismatch)
    }
}

fn transport_error(status: StatusCode, code: &'static str, message: &'static str) -> Response {
    api_error(status, WebApiErrorKind::Transport, code, message)
}

fn application_error(status: StatusCode, code: &'static str, message: &'static str) -> Response {
    api_error(status, WebApiErrorKind::Application, code, message)
}

fn api_error(
    status: StatusCode,
    kind: WebApiErrorKind,
    code: &'static str,
    message: &'static str,
) -> Response {
    let body = Json(WebApiErrorResponse {
        error: WebApiError {
            kind,
            code: code.to_owned(),
            message: message.to_owned(),
        },
    });
    (status, body).into_response()
}

async fn api_not_found() -> Response {
    transport_error(
        StatusCode::NOT_FOUND,
        "api_route_not_found",
        "API route does not exist in this contract",
    )
}

async fn static_assets_not_configured() -> Response {
    StatusCode::NOT_FOUND.into_response()
}

#[cfg(test)]
mod tests {
    use std::{
        ffi::OsString,
        io::{self, Write as _},
        net::SocketAddr,
        path::PathBuf,
        time::Duration,
    };

    use axum::{
        body::{Body, Bytes},
        http::{Request, StatusCode, header},
    };
    use http_body_util::BodyExt as _;
    use signalbox_web_contract::{
        MAX_JSON_BODY_BYTES, MAX_NDJSON_ITEM_BYTES, WebContractBootstrap, WebContractExample,
    };
    use tokio::sync::{mpsc, watch};
    use tower::ServiceExt as _;
    use url::Url;

    use super::{
        DEFAULT_WEB_BIND_ADDRESS, WebHttpConfiguration, WebHttpConfigurationError, WebHttpRuntime,
        content_disposition, deterministic_test_router, ndjson_response, parse_byte_range,
        production_router,
    };

    fn loopback_ephemeral() -> SocketAddr {
        "127.0.0.1:0"
            .parse()
            .expect("the test listener address is valid")
    }

    #[test]
    fn http_byte_ranges_cover_closed_open_and_suffix_forms() {
        let closed = parse_byte_range(&header::HeaderValue::from_static("bytes=2-5"), 10);
        let open = parse_byte_range(&header::HeaderValue::from_static("bytes=7-"), 10);
        let suffix = parse_byte_range(&header::HeaderValue::from_static("bytes=-4"), 10);

        assert_eq!(closed, Ok((2, 4, true)));
        assert_eq!(open, Ok((7, 3, true)));
        assert_eq!(suffix, Ok((6, 4, true)));
    }

    #[test]
    fn http_byte_ranges_reject_multiple_noncanonical_and_unsatisfied_forms() {
        let multiple = parse_byte_range(&header::HeaderValue::from_static("bytes=0-1,4-5"), 10);
        let noncanonical = parse_byte_range(&header::HeaderValue::from_static("bytes=01-2"), 10);
        let unsatisfied = parse_byte_range(&header::HeaderValue::from_static("bytes=10-"), 10);

        assert_eq!(multiple, Err(()));
        assert_eq!(noncanonical, Err(()));
        assert_eq!(unsatisfied, Err(()));
    }

    #[test]
    fn download_disposition_keeps_filename_data_out_of_header_syntax() {
        let disposition = content_disposition("report \"final\".csv");

        assert_eq!(
            disposition,
            "attachment; filename=\"download\"; filename*=UTF-8''report%20%22final%22.csv"
        );
    }

    fn example() -> WebContractExample {
        WebContractExample {
            request_id: "transport-test".to_owned(),
            message: "bounded payload".to_owned(),
        }
    }

    const STATIC_INDEX: &str = "signalbox-static-build";

    async fn response_body(response: axum::response::Response) -> Vec<u8> {
        axum::body::to_bytes(response.into_body(), MAX_JSON_BODY_BYTES)
            .await
            .expect("the response body stays within the JSON ceiling")
            .to_vec()
    }

    #[test]
    fn absent_configuration_uses_loopback_and_no_asset_root() {
        let configuration = WebHttpConfiguration::from_values(None, None)
            .expect("absent browser settings use conservative defaults");

        assert_eq!(configuration.bind_address(), DEFAULT_WEB_BIND_ADDRESS);
        assert_eq!(configuration.asset_root(), None);
    }

    #[test]
    fn explicit_deployment_configuration_is_admitted() {
        let bind_address: SocketAddr = "0.0.0.0:8080"
            .parse()
            .expect("the fixture address is valid");
        let asset_root = PathBuf::from("web-dist");
        let configuration = WebHttpConfiguration::from_values(
            Some(OsString::from(bind_address.to_string())),
            Some(asset_root.clone().into_os_string()),
        )
        .expect("explicit deployment settings are valid");

        assert_eq!(configuration.bind_address(), bind_address);
        assert_eq!(configuration.asset_root(), Some(&asset_root));
    }

    #[test]
    fn malformed_bind_address_fails_closed_without_echoing_the_value() {
        let error =
            WebHttpConfiguration::from_values(Some(OsString::from("not a socket address")), None)
                .expect_err("a malformed listener must fail configuration");

        assert_eq!(error, WebHttpConfigurationError::InvalidBindAddress);
        assert_eq!(
            error.to_string(),
            "setting SIGNALBOX_WEB_BIND is not a socket address"
        );
    }

    #[tokio::test]
    async fn production_server_serves_assets_and_bootstrap_on_one_origin() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let assets = tempfile::tempdir().expect("the static asset directory exists");
        std::fs::write(assets.path().join("index.html"), STATIC_INDEX)
            .expect("the static index exists");
        let runtime = WebHttpRuntime::bind(
            WebHttpConfiguration::new(loopback_ephemeral(), Some(assets.path().to_path_buf())),
            None,
        )
        .await
        .expect("the production test server binds");
        let address = runtime
            .local_address()
            .expect("the listener has an address");
        let (shutdown_sender, shutdown_receiver) = watch::channel(false);
        let task = tokio::spawn(runtime.run(shutdown_receiver));

        let asset = reqwest::get(format!("http://{address}/"))
            .await
            .expect("the static fetch completes");
        let bootstrap = reqwest::get(format!("http://{address}/api/bootstrap"))
            .await
            .expect("the bootstrap fetch completes");
        let bootstrap_origin = bootstrap.url().origin();
        let bootstrap_bytes = bootstrap.bytes().await.expect("the bootstrap body arrives");
        let decoded: WebContractBootstrap = serde_json::from_slice(&bootstrap_bytes)
            .expect("the bootstrap body matches the Rust contract");
        shutdown_sender
            .send(true)
            .expect("the browser server still observes shutdown");
        let runtime_outcome = task.await.expect("the browser server task joins");

        assert_eq!(asset.status(), StatusCode::OK);
        assert_eq!(
            asset.text().await.expect("the static body is text"),
            STATIC_INDEX
        );
        assert_eq!(
            bootstrap_origin,
            format!("http://{address}")
                .parse::<Url>()
                .expect("fixture URL is valid")
                .origin()
        );
        assert_eq!(decoded, WebContractBootstrap::for_runtime(false, false));
        assert_eq!(runtime_outcome, Ok(()));
    }

    #[tokio::test]
    async fn malformed_blob_query_is_a_structured_transport_error() {
        let request = Request::get("/api/blobs/not-a-digest/descriptor")
            .body(Body::empty())
            .expect("the request is valid");
        let response = production_router(None, None)
            .oneshot(request)
            .await
            .expect("the production router responds");
        let status = response.status();
        let body: serde_json::Value = serde_json::from_slice(&response_body(response).await)
            .expect("the query rejection is JSON");

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"]["kind"], "transport");
        assert_eq!(body["error"]["code"], "invalid_blob_use");
    }

    #[tokio::test]
    async fn mutation_with_matching_origin_round_trips_bounded_json() {
        let request = Request::post("/api/test/mutate")
            .header(header::HOST, "signalbox.test")
            .header(header::ORIGIN, "http://signalbox.test")
            .header(header::CONTENT_TYPE, "application/json; charset=utf-8")
            .body(Body::from(
                serde_json::to_vec(&example()).expect("the fixture serializes"),
            ))
            .expect("the request is valid");
        let response = deterministic_test_router()
            .oneshot(request)
            .await
            .expect("the deterministic router responds");
        let status = response.status();
        let decoded: WebContractExample = serde_json::from_slice(&response_body(response).await)
            .expect("the response is the example DTO");

        assert_eq!(status, StatusCode::OK);
        assert_eq!(decoded, example());
    }

    #[tokio::test]
    async fn responses_do_not_emit_permissive_cors() {
        let request = Request::get("/api/bootstrap")
            .body(Body::empty())
            .expect("the request is valid");
        let response = deterministic_test_router()
            .oneshot(request)
            .await
            .expect("the deterministic router responds");

        assert_eq!(
            response.headers().get(header::ACCESS_CONTROL_ALLOW_ORIGIN),
            None
        );
    }

    #[tokio::test]
    async fn mutation_without_browser_origin_is_admitted() {
        let request = Request::post("/api/test/mutate")
            .header(header::HOST, "signalbox.test")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                serde_json::to_vec(&example()).expect("the fixture serializes"),
            ))
            .expect("the request is valid");
        let response = deterministic_test_router()
            .oneshot(request)
            .await
            .expect("the deterministic router responds");

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn mutation_without_json_content_type_is_rejected() {
        let request = Request::post("/api/test/mutate")
            .header(header::HOST, "signalbox.test")
            .body(Body::from(
                serde_json::to_vec(&example()).expect("the fixture serializes"),
            ))
            .expect("the request is valid");
        let response = deterministic_test_router()
            .oneshot(request)
            .await
            .expect("the deterministic router responds");
        let status = response.status();
        let body: serde_json::Value =
            serde_json::from_slice(&response_body(response).await).expect("the rejection is JSON");

        assert_eq!(status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
        assert_eq!(body["error"]["code"], "json_content_type_required");
    }

    #[tokio::test]
    async fn mutation_with_cross_origin_is_rejected_as_transport_error() {
        let request = Request::post("/api/test/mutate")
            .header(header::HOST, "signalbox.test")
            .header(header::ORIGIN, "https://outside.example")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                serde_json::to_vec(&example()).expect("the fixture serializes"),
            ))
            .expect("the request is valid");
        let response = deterministic_test_router()
            .oneshot(request)
            .await
            .expect("the deterministic router responds");
        let status = response.status();
        let body: serde_json::Value =
            serde_json::from_slice(&response_body(response).await).expect("the rejection is JSON");

        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body["error"]["kind"], "transport");
        assert_eq!(body["error"]["code"], "cross_origin_mutation_rejected");
    }

    #[tokio::test]
    async fn mutation_with_implicit_host_port_rejects_cross_port_origin() {
        let request = Request::post("/api/test/mutate")
            .header(header::HOST, "signalbox.test")
            .header(header::ORIGIN, "http://signalbox.test:8080")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                serde_json::to_vec(&example()).expect("the fixture serializes"),
            ))
            .expect("the request is valid");
        let response = deterministic_test_router()
            .oneshot(request)
            .await
            .expect("the deterministic router responds");

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn mutation_with_implicit_host_port_rejects_https_default_port() {
        let request = Request::post("/api/test/mutate")
            .header(header::HOST, "signalbox.test")
            .header(header::ORIGIN, "https://signalbox.test")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                serde_json::to_vec(&example()).expect("the fixture serializes"),
            ))
            .expect("the request is valid");
        let response = deterministic_test_router()
            .oneshot(request)
            .await
            .expect("the deterministic router responds");

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn mutation_over_json_limit_is_rejected_before_decode() {
        let request = Request::post("/api/test/mutate")
            .header(header::HOST, "signalbox.test")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(vec![b' '; MAX_JSON_BODY_BYTES + 1]))
            .expect("the oversized request is valid HTTP");
        let response = deterministic_test_router()
            .oneshot(request)
            .await
            .expect("the deterministic router responds");

        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[tokio::test]
    async fn mutation_with_body_read_failure_is_bad_request() {
        let failing_body = futures_util::stream::once(async {
            Err::<Bytes, io::Error>(io::Error::other("fixture body read failure"))
        });
        let request = Request::post("/api/test/mutate")
            .header(header::HOST, "signalbox.test")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from_stream(failing_body))
            .expect("the request is valid");
        let response = deterministic_test_router()
            .oneshot(request)
            .await
            .expect("the deterministic router responds");
        let status = response.status();
        let body: serde_json::Value =
            serde_json::from_slice(&response_body(response).await).expect("the rejection is JSON");

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"]["code"], "json_body_read_failed");
    }

    #[tokio::test]
    async fn api_paths_never_fall_through_to_static_assets() {
        let assets = tempfile::tempdir().expect("the static asset directory exists");
        std::fs::write(assets.path().join("index.html"), "static fallback")
            .expect("the static index exists");
        let request = Request::get("/api/not-a-route")
            .body(Body::empty())
            .expect("the request is valid");
        let response = production_router(Some(assets.path().to_path_buf()), None)
            .oneshot(request)
            .await
            .expect("the production router responds");
        let status = response.status();
        let body: serde_json::Value =
            serde_json::from_slice(&response_body(response).await).expect("the API miss is JSON");

        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["error"]["code"], "api_route_not_found");
    }

    #[tokio::test]
    async fn ndjson_stream_yields_one_complete_item_before_the_next_exists() {
        let (sender, receiver) = mpsc::channel(1);
        let source = stream_from_receiver(receiver);
        let response = ndjson_response(source);
        let content_type = response.headers()[header::CONTENT_TYPE].clone();
        let mut body = response.into_body();
        let first = example();
        sender
            .send(first.clone())
            .await
            .expect("the receiver is open");
        let frame = body
            .frame()
            .await
            .expect("the first item arrives")
            .expect("the first item is encoded");
        let bytes = frame.into_data().expect("the first frame carries data");

        assert_eq!(content_type, "application/x-ndjson");
        assert_eq!(bytes.last(), Some(&b'\n'));
        assert_eq!(
            serde_json::from_slice::<WebContractExample>(&bytes[..bytes.len() - 1])
                .expect("the NDJSON item decodes"),
            first
        );
    }

    #[tokio::test]
    async fn dropping_ndjson_body_cancels_its_bounded_source() {
        let (sender, receiver) = mpsc::channel::<WebContractExample>(1);
        let response = ndjson_response(stream_from_receiver(receiver));

        drop(response);
        tokio::time::timeout(Duration::from_secs(1), sender.closed())
            .await
            .expect("dropping the body closes its source within the test bound");
    }

    #[tokio::test]
    async fn bounded_ndjson_source_applies_backpressure_before_body_poll() {
        let (sender, receiver) = mpsc::channel(1);
        let _response = ndjson_response(stream_from_receiver(receiver));
        let first = example();
        let second = WebContractExample {
            request_id: "transport-test-second".to_owned(),
            message: "waits for capacity".to_owned(),
        };

        sender
            .try_send(first)
            .expect("the first bounded slot is available");
        let error = sender
            .try_send(second.clone())
            .expect_err("the second item waits until the body consumes the first");

        assert_bounded_channel_full(error, second);
    }

    #[track_caller]
    fn assert_bounded_channel_full(
        error: tokio::sync::mpsc::error::TrySendError<WebContractExample>,
        expected: WebContractExample,
    ) {
        match error {
            tokio::sync::mpsc::error::TrySendError::Full(actual) => {
                assert_eq!(actual, expected);
            }
            tokio::sync::mpsc::error::TrySendError::Closed(_) => {
                panic!("the response still owns the bounded receiver");
            }
        }
    }

    #[tokio::test]
    async fn ndjson_item_over_hard_ceiling_fails_the_stream() {
        let oversized = WebContractExample {
            request_id: "transport-test".to_owned(),
            message: "x".repeat(MAX_NDJSON_ITEM_BYTES),
        };
        let response = ndjson_response(futures_util::stream::iter([oversized]));
        let mut body = response.into_body();
        let frame = body
            .frame()
            .await
            .expect("the oversized item produces a terminal frame result");

        assert!(frame.is_err());
    }

    #[test]
    fn ndjson_writer_refuses_overflow_without_appending_it() {
        let mut writer = super::NdjsonItemWriter::new();
        writer
            .write_all(&vec![b'x'; MAX_NDJSON_ITEM_BYTES])
            .expect("the exact item ceiling fits");
        let length_at_ceiling = writer.encoded.len();
        let error = writer
            .write_all(b"x")
            .expect_err("the next byte crosses the item ceiling");

        assert_eq!(length_at_ceiling, MAX_NDJSON_ITEM_BYTES);
        assert_eq!(writer.encoded.len(), length_at_ceiling);
        assert_eq!(error.to_string(), "NDJSON item exceeds the contract limit");
    }

    fn stream_from_receiver<T>(
        receiver: mpsc::Receiver<T>,
    ) -> impl futures_util::Stream<Item = T> + Send + 'static
    where
        T: Send + 'static,
    {
        futures_util::stream::unfold(receiver, |mut receiver| async move {
            receiver.recv().await.map(|item| (item, receiver))
        })
    }

    #[tokio::test]
    async fn deterministic_page_uses_real_transport_routes() {
        let request = Request::get("/")
            .body(Body::empty())
            .expect("the request is valid");
        let response = deterministic_test_router()
            .oneshot(request)
            .await
            .expect("the deterministic router responds");
        let status = response.status();
        let body = String::from_utf8(response_body(response).await)
            .expect("the deterministic page is UTF-8");

        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("fetch(\"/api/bootstrap\")"));
        assert!(body.contains("fetch(\"/api/test/stream\")"));
    }
}
