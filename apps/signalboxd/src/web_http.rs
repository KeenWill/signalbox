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
    path::PathBuf,
    sync::Arc,
};

use axum::{
    Json, Router,
    body::{Body, Bytes, to_bytes},
    extract::{DefaultBodyLimit, Path, Query, Request, State, rejection::QueryRejection},
    http::{
        HeaderMap, HeaderValue, Method, StatusCode,
        header::{CONTENT_TYPE, HOST, ORIGIN},
    },
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use futures_util::{Stream, StreamExt, stream};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use signalbox_application::{
    SearchContentClass, SearchCursor, SearchPageLimit, SearchQuery, SearchResultSource,
    SearchScope, SearchStrategy, SearchText, SessionTimelineDescriptor, SessionTimelineEventKind,
    SessionTimelineWindow, TimelineAddress, TimelineContinuation, TimelineWindowAnchor,
    TimelineWindowLimits, UsageAggregateGroup, UsageAggregateTokenAxes, UsageCallCursor,
    UsageCallEvidence, UsageCallKind, UsageCallOrder, UsageCallPageLimit, UsageCallQuery,
    UsageInputTokenSemantics, UsageProvenance, UsageQuery, UsageSelection, UsageTimeRange,
    UsageTimestampMicros, UsageTokenAxes, UsageTokenPresence,
};
use signalbox_domain::{
    ModelCallId, ProviderModelIdentity, ResolvedProviderTarget, SessionId, TurnId,
};
use signalbox_persistence::process_read::ProcessModelCallInputTokenSemantics;
use signalbox_persistence::search::{SearchRepository, SearchRepositoryError};
use signalbox_persistence::session_timeline::{
    SessionTimelineRepository, SessionTimelineRepositoryError,
};
use signalbox_persistence::usage::{
    UsageRepository, UsageRepositoryError, usage_timestamp_is_representable,
};
use signalbox_web_contract::{
    MAX_JSON_BODY_BYTES, MAX_NDJSON_ITEM_BYTES, WebApiError, WebApiErrorKind, WebApiErrorResponse,
    WebContractBootstrap, WebContractExample, WebDollarAmount, WebNullableU64, WebNullableU128,
    WebSearchContentClass, WebSearchCursor, WebSearchHighlight, WebSearchPage,
    WebSearchProjectionId, WebSearchResult, WebSearchResultSource, WebSessionId,
    WebSessionTimelineDescriptor, WebSessionTimelineEventKind, WebSessionTimelineItem,
    WebSessionTimelineSizeFacts, WebSessionTimelineWindow, WebSessionWorkFacts, WebTimelineAddress,
    WebTimelineEventSequence, WebU64, WebUsageAggregateGroup, WebUsageAggregateTokenAxes,
    WebUsageCall, WebUsageCallCount, WebUsageCallCursor, WebUsageCallKind, WebUsageCallPage,
    WebUsageCost, WebUsageCostLabel, WebUsageCostUnavailableReason, WebUsageInputSemantics,
    WebUsageProvenance, WebUsageRateVersion, WebUsageSummary, WebUsageTokenAxes,
    WebUsageTokenCoverage, WebUuid,
};
use sqlx::PgPool;
use tokio::{net::TcpListener, sync::watch};
use tower_http::services::{ServeDir, ServeFile};
use url::Url;

use crate::{BillingKind, HubModelConfiguration, configuration::ModelCallInputUsage};

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
        if !bind_address.ip().is_loopback() {
            return Err(WebHttpConfigurationError::NonLoopbackBindAddress);
        }
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

    /// Creates explicit loopback configuration for an embedded production server.
    pub fn new(
        bind_address: SocketAddr,
        asset_root: Option<PathBuf>,
    ) -> Result<Self, WebHttpConfigurationError> {
        if !bind_address.ip().is_loopback() {
            return Err(WebHttpConfigurationError::NonLoopbackBindAddress);
        }
        Ok(Self {
            bind_address,
            asset_root,
        })
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
    /// Explicit listener setting would expose unauthenticated routes off-host.
    NonLoopbackBindAddress,
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
            Self::NonLoopbackBindAddress => write!(
                formatter,
                "setting {WEB_BIND_ENVIRONMENT} must use a loopback address"
            ),
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
        pool: PgPool,
        model_configuration: HubModelConfiguration,
    ) -> Result<Self, WebHttpRuntimeError> {
        let router = production_router_with_model_configuration(
            configuration.asset_root,
            Some(pool),
            Some(model_configuration),
        );
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
pub fn production_router(asset_root: Option<PathBuf>, pool: Option<PgPool>) -> Router {
    production_router_with_model_configuration(asset_root, pool, None)
}

fn production_router_with_model_configuration(
    asset_root: Option<PathBuf>,
    pool: Option<PgPool>,
    model_configuration: Option<HubModelConfiguration>,
) -> Router {
    let state = WebApiState {
        timeline: pool.clone().map(SessionTimelineRepository::new),
        search: pool.clone().map(SearchRepository::new),
        usage: pool.map(UsageRepository::new),
        model_configuration: model_configuration.map(Arc::new),
    };
    let session_reads = Router::new()
        .route("/sessions/{session_id}", get(session_descriptor))
        .route(
            "/sessions/{session_id}/timeline",
            get(session_timeline_window),
        )
        .route("/search", get(search))
        .route("/usage/summary", get(usage_summary))
        .route("/usage/calls", get(usage_calls))
        .route_layer(middleware::from_fn(validate_loopback_host));
    let api = Router::new()
        .route("/bootstrap", get(contract_bootstrap))
        .merge(session_reads)
        .with_state(state)
        .fallback(api_not_found);
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

#[derive(Clone, Debug)]
struct WebApiState {
    timeline: Option<SessionTimelineRepository>,
    search: Option<SearchRepository>,
    usage: Option<UsageRepository>,
    model_configuration: Option<Arc<HubModelConfiguration>>,
}

async fn validate_loopback_host(request: Request, next: Next) -> Response {
    let loopback = request
        .headers()
        .get(HOST)
        .and_then(|host| host.to_str().ok())
        .and_then(|host| host.parse::<axum::http::uri::Authority>().ok())
        .is_some_and(|authority| {
            let host = authority.host();
            host.eq_ignore_ascii_case("localhost")
                || host
                    .strip_prefix('[')
                    .and_then(|host| host.strip_suffix(']'))
                    .unwrap_or(host)
                    .parse::<IpAddr>()
                    .is_ok_and(|address| address.is_loopback())
        });
    if !loopback {
        return transport_error(
            StatusCode::FORBIDDEN,
            "non_loopback_host_rejected",
            "session reads require a loopback request authority",
        );
    }
    next.run(request).await
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TimelineWindowQuery {
    anchor: String,
    address: Option<String>,
    max_items: Option<String>,
    max_bytes: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchHttpQuery {
    strategy: String,
    q: String,
    session_id: Option<String>,
    max_items: String,
    after_address: Option<String>,
    after_projection: Option<String>,
}

async fn search(
    State(state): State<WebApiState>,
    query: Result<Query<SearchHttpQuery>, QueryRejection>,
) -> Response {
    let Query(query) = match query {
        Ok(query) => query,
        Err(_) => return invalid_search_query(),
    };
    let Some(request) = parse_search_query(query) else {
        return invalid_search_query();
    };
    let Some(repository) = state.search else {
        return application_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "search_projection_unavailable",
            "search projection is not configured",
        );
    };
    match repository.search(request).await {
        Ok(page) => Json(search_page_dto(page)).into_response(),
        Err(error) => search_repository_error(error),
    }
}

fn parse_search_query(query: SearchHttpQuery) -> Option<SearchQuery> {
    if query.strategy != "lexical" {
        return None;
    }
    let text = SearchText::try_new(query.q).ok()?;
    let limit = query
        .max_items
        .parse::<u16>()
        .ok()
        .and_then(|value| SearchPageLimit::new(value).ok())?;
    let scope = match query.session_id {
        Some(value) => SearchScope::Session(parse_session_id(&value).ok()?),
        None => SearchScope::Global,
    };
    let after = match (query.after_address, query.after_projection) {
        (None, None) => None,
        (Some(address), Some(projection)) => Some(SearchCursor::new(
            TimelineAddress::new(parse_positive_u64(&address)?),
            parse_positive_i64(&projection)?,
        )),
        _ => return None,
    };
    Some(SearchQuery {
        strategy: SearchStrategy::Lexical,
        scope,
        text,
        limit,
        after,
    })
}

fn parse_positive_u64(value: &str) -> Option<std::num::NonZeroU64> {
    if value.is_empty()
        || value.starts_with('0')
        || !value.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    value
        .parse::<u64>()
        .ok()
        .and_then(std::num::NonZeroU64::new)
}

fn parse_positive_i64(value: &str) -> Option<std::num::NonZeroU64> {
    let value = parse_positive_u64(value)?;
    i64::try_from(value.get()).ok()?;
    Some(value)
}

fn invalid_search_query() -> Response {
    application_error(
        StatusCode::BAD_REQUEST,
        "invalid_search_query",
        "search parameters are malformed or outside the contract bounds",
    )
}

fn search_repository_error(error: SearchRepositoryError) -> Response {
    let failure_class = match &error {
        SearchRepositoryError::Database(_) => "infrastructure",
        SearchRepositoryError::Corruption(_) => "fail_closed_corruption",
    };
    tracing::error!(failure_class, cause = %error, "lexical search projection read failed");
    application_error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "search_projection_failed",
        "the durable search projection could not be read",
    )
}

fn search_page_dto(page: signalbox_application::SearchPage) -> WebSearchPage {
    WebSearchPage {
        results: page.results.into_iter().map(search_result_dto).collect(),
        continuation: page.next.map(|cursor| WebSearchCursor {
            address: address_dto(cursor.address()),
            projection_id: WebSearchProjectionId::from_nonzero(cursor.projection()),
        }),
    }
}

fn search_result_dto(result: signalbox_application::SearchResult) -> WebSearchResult {
    WebSearchResult {
        session_id: WebSessionId::from_validated_uuid(result.session.into_uuid().to_string()),
        address: address_dto(result.address),
        projection_id: WebSearchProjectionId::from_nonzero(result.projection),
        source: search_source_dto(result.source),
        content_class: search_content_class_dto(result.content_class),
        snippet: result.snippet,
        highlights: result
            .highlights
            .into_iter()
            .map(|highlight| WebSearchHighlight {
                start_byte: u32::from(highlight.start_byte),
                end_byte: u32::from(highlight.end_byte),
            })
            .collect(),
    }
}

fn search_source_dto(source: SearchResultSource) -> WebSearchResultSource {
    match source {
        SearchResultSource::Session(session) => WebSearchResultSource::Session {
            session_id: WebSessionId::from_validated_uuid(session.into_uuid().to_string()),
        },
        SearchResultSource::AcceptedInput { input, turn } => WebSearchResultSource::AcceptedInput {
            accepted_input_id: web_uuid(input.into_uuid()),
            turn_id: web_uuid(turn.into_uuid()),
        },
        SearchResultSource::SteeringInput { input, source_turn } => {
            WebSearchResultSource::SteeringInput {
                accepted_input_id: web_uuid(input.into_uuid()),
                source_turn_id: web_uuid(source_turn.into_uuid()),
            }
        }
        SearchResultSource::TurnTranscriptEntry { entry, turn } => {
            WebSearchResultSource::TurnTranscriptEntry {
                semantic_entry_id: web_uuid(entry.into_uuid()),
                turn_id: web_uuid(turn.into_uuid()),
            }
        }
        SearchResultSource::SessionTranscriptEntry { entry } => {
            WebSearchResultSource::SessionTranscriptEntry {
                semantic_entry_id: web_uuid(entry.into_uuid()),
            }
        }
        SearchResultSource::ToolRequest { request, turn } => WebSearchResultSource::ToolRequest {
            tool_request_id: web_uuid(request.into_uuid()),
            turn_id: web_uuid(turn.into_uuid()),
        },
        SearchResultSource::ToolAttempt { attempt, turn } => WebSearchResultSource::ToolAttempt {
            tool_attempt_id: web_uuid(attempt.into_uuid()),
            turn_id: web_uuid(turn.into_uuid()),
        },
        SearchResultSource::Attachment { attachment } => WebSearchResultSource::Attachment {
            attachment_id: web_uuid(attachment.into_uuid()),
        },
        SearchResultSource::DerivedArtifact { artifact } => {
            WebSearchResultSource::DerivedArtifact {
                artifact_id: web_uuid(artifact.into_uuid()),
            }
        }
    }
}

fn web_uuid(value: uuid::Uuid) -> WebUuid {
    WebUuid::from_validated_uuid(value.to_string())
}

fn search_content_class_dto(content: SearchContentClass) -> WebSearchContentClass {
    match content {
        SearchContentClass::UserTranscript => WebSearchContentClass::UserTranscript,
        SearchContentClass::AssistantTranscript => WebSearchContentClass::AssistantTranscript,
        SearchContentClass::ToolArguments => WebSearchContentClass::ToolArguments,
        SearchContentClass::ToolResult => WebSearchContentClass::ToolResult,
        SearchContentClass::SessionMetadata => WebSearchContentClass::SessionMetadata,
        SearchContentClass::AttachmentFilename => WebSearchContentClass::AttachmentFilename,
        SearchContentClass::AttachmentMediaMetadata => {
            WebSearchContentClass::AttachmentMediaMetadata
        }
        SearchContentClass::DerivedTextArtifact => WebSearchContentClass::DerivedTextArtifact,
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct UsageSummaryHttpQuery {
    from_micros: Option<String>,
    to_micros: Option<String>,
    session_id: Option<String>,
    turn_id: Option<String>,
    model_id: Option<String>,
    provenance: Option<String>,
    call_kind: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct UsageCallsHttpQuery {
    from_micros: Option<String>,
    to_micros: Option<String>,
    session_id: Option<String>,
    turn_id: Option<String>,
    model_id: Option<String>,
    provenance: Option<String>,
    call_kind: Option<String>,
    order: String,
    max_items: String,
    after_recorded_at_micros: Option<String>,
    after_call_id: Option<String>,
}

async fn usage_summary(
    State(state): State<WebApiState>,
    query: Result<Query<UsageSummaryHttpQuery>, QueryRejection>,
) -> Response {
    let Query(query) = match query {
        Ok(query) => query,
        Err(_) => return invalid_usage_query(),
    };
    let Some(query) = parse_usage_query(
        query.from_micros,
        query.to_micros,
        query.session_id,
        query.turn_id,
        query.model_id,
        query.provenance,
        query.call_kind,
    ) else {
        return invalid_usage_query();
    };
    let (Some(repository), Some(configuration)) = (state.usage, state.model_configuration) else {
        return usage_unavailable();
    };
    match repository.aggregate(query).await {
        Ok(report) => Json(WebUsageSummary {
            groups: report
                .groups
                .into_iter()
                .map(|group| usage_aggregate_dto(group, &configuration))
                .collect(),
            truncated: report.truncated,
        })
        .into_response(),
        Err(error) => usage_repository_error(error),
    }
}

async fn usage_calls(
    State(state): State<WebApiState>,
    query: Result<Query<UsageCallsHttpQuery>, QueryRejection>,
) -> Response {
    let Query(query) = match query {
        Ok(query) => query,
        Err(_) => return invalid_usage_query(),
    };
    let scope = parse_usage_query(
        query.from_micros,
        query.to_micros,
        query.session_id,
        query.turn_id,
        query.model_id,
        query.provenance,
        query.call_kind,
    );
    let order = match query.order.as_str() {
        "newest" => Some(UsageCallOrder::NewestFirst),
        "oldest" => Some(UsageCallOrder::OldestFirst),
        _ => None,
    };
    let limit = query
        .max_items
        .parse::<u16>()
        .ok()
        .and_then(|value| UsageCallPageLimit::new(value).ok());
    let after = match (query.after_recorded_at_micros, query.after_call_id) {
        (None, None) => Some(None),
        (Some(recorded_at), Some(call)) => parse_usage_timestamp(&recorded_at)
            .zip(parse_model_call_id(&call))
            .map(|(recorded_at, call)| Some(UsageCallCursor { recorded_at, call })),
        _ => None,
    };
    let (Some(scope), Some(order), Some(limit), Some(after)) = (scope, order, limit, after) else {
        return invalid_usage_query();
    };
    let (Some(repository), Some(configuration)) = (state.usage, state.model_configuration) else {
        return usage_unavailable();
    };
    match repository
        .calls(UsageCallQuery {
            scope,
            order,
            limit,
            after,
        })
        .await
    {
        Ok(page) => Json(WebUsageCallPage {
            calls: page
                .calls
                .into_iter()
                .map(|call| usage_call_dto(call, &configuration))
                .collect(),
            continuation: page.next.map(|cursor| WebUsageCallCursor {
                recorded_at_micros: WebU64::from_u64(cursor.recorded_at.get()),
                call_id: web_uuid(cursor.call.into_uuid()),
            }),
        })
        .into_response(),
        Err(error) => usage_repository_error(error),
    }
}

#[allow(clippy::too_many_arguments)]
fn parse_usage_query(
    from_micros: Option<String>,
    to_micros: Option<String>,
    session_id: Option<String>,
    turn_id: Option<String>,
    model_id: Option<String>,
    provenance: Option<String>,
    call_kind: Option<String>,
) -> Option<UsageQuery> {
    let from_inclusive = parse_optional(from_micros, parse_usage_timestamp)?;
    let to_exclusive = parse_optional(to_micros, parse_usage_timestamp)?;
    let time = UsageTimeRange::new(from_inclusive, to_exclusive).ok()?;
    let selection = UsageSelection {
        session: parse_optional(session_id, |value| {
            uuid::Uuid::parse_str(value).ok().map(SessionId::from_uuid)
        })?,
        turn: parse_optional(turn_id, |value| {
            uuid::Uuid::parse_str(value).ok().map(TurnId::from_uuid)
        })?,
        model: parse_optional(model_id, |value| {
            uuid::Uuid::parse_str(value).ok().map(|identity| {
                ResolvedProviderTarget::naming(ProviderModelIdentity::from_uuid(identity))
            })
        })?,
        provenance: parse_optional(provenance, parse_usage_provenance)?,
        call_kind: parse_optional(call_kind, parse_usage_call_kind)?,
    };
    Some(UsageQuery { time, selection })
}

fn parse_optional<T>(
    value: Option<String>,
    parser: impl FnOnce(&str) -> Option<T>,
) -> Option<Option<T>> {
    match value {
        None => Some(None),
        Some(value) => parser(&value).map(Some),
    }
}

fn parse_usage_timestamp(value: &str) -> Option<UsageTimestampMicros> {
    if value.is_empty()
        || (value.starts_with('0') && value != "0")
        || !value.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    let timestamp = UsageTimestampMicros::new(value.parse().ok()?).ok()?;
    usage_timestamp_is_representable(timestamp).then_some(timestamp)
}

fn parse_model_call_id(value: &str) -> Option<ModelCallId> {
    uuid::Uuid::parse_str(value)
        .ok()
        .map(ModelCallId::from_uuid)
}

fn parse_usage_provenance(value: &str) -> Option<UsageProvenance> {
    match value {
        "reported" => Some(UsageProvenance::Reported),
        "estimated" => Some(UsageProvenance::Estimated),
        _ => None,
    }
}

fn parse_usage_call_kind(value: &str) -> Option<UsageCallKind> {
    match value {
        "model_call" => Some(UsageCallKind::ModelCall),
        "approval_judge" => Some(UsageCallKind::ApprovalJudge),
        _ => None,
    }
}

fn invalid_usage_query() -> Response {
    application_error(
        StatusCode::BAD_REQUEST,
        "invalid_usage_query",
        "usage parameters are malformed or outside the contract bounds",
    )
}

fn usage_unavailable() -> Response {
    application_error(
        StatusCode::SERVICE_UNAVAILABLE,
        "usage_projection_unavailable",
        "usage projection or configured rates are not available",
    )
}

fn usage_repository_error(error: UsageRepositoryError) -> Response {
    let failure_class = match &error {
        UsageRepositoryError::Database(_) => "infrastructure",
        UsageRepositoryError::Corruption(_) => "fail_closed_corruption",
    };
    tracing::error!(failure_class, cause = %error, "usage projection read failed");
    application_error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "usage_projection_failed",
        "the durable usage projection could not be read",
    )
}

fn usage_aggregate_dto(
    group: UsageAggregateGroup,
    configuration: &HubModelConfiguration,
) -> WebUsageAggregateGroup {
    WebUsageAggregateGroup {
        call_kind: usage_call_kind_dto(group.key.call_kind),
        model_id: web_uuid(group.key.model.identity().into_uuid()),
        profile_id: signalbox_web_contract::WebUsageProfileId::from_bounded(
            group.key.web_profile.clone(),
        ),
        provenance: usage_provenance_dto(group.key.provenance),
        input_semantics: usage_input_semantics_dto(group.key.input_semantics),
        coverage: WebUsageTokenCoverage {
            input: group.key.coverage.input == UsageTokenPresence::Present,
            output: group.key.coverage.output == UsageTokenPresence::Present,
            cache_creation_input: group.key.coverage.cache_creation_input
                == UsageTokenPresence::Present,
            cache_read_input: group.key.coverage.cache_read_input == UsageTokenPresence::Present,
        },
        call_count: WebUsageCallCount::from_positive(group.call_count),
        tokens: usage_aggregate_tokens_dto(group.tokens),
        cost: usage_aggregate_cost_dto(configuration, &group),
    }
}

fn usage_call_dto(call: UsageCallEvidence, configuration: &HubModelConfiguration) -> WebUsageCall {
    WebUsageCall {
        call_kind: usage_call_kind_dto(call.call_kind),
        call_id: web_uuid(call.call.into_uuid()),
        session_id: WebSessionId::from_uuid_bytes(*call.session.into_uuid().as_bytes()),
        turn_id: call.turn.map(|turn| web_uuid(turn.into_uuid())),
        model_id: web_uuid(call.model.identity().into_uuid()),
        provenance: usage_provenance_dto(call.provenance),
        input_semantics: usage_input_semantics_dto(call.input_semantics),
        tokens: usage_tokens_dto(call.tokens),
        recorded_at_micros: WebU64::from_u64(call.recorded_at.get()),
        cost: usage_cost_dto(
            configuration,
            call.model,
            &call.credential_profile,
            call.input_semantics,
            call.tokens,
            true,
        ),
    }
}

fn usage_cost_dto(
    configuration: &HubModelConfiguration,
    model: ResolvedProviderTarget,
    credential_profile: &str,
    input_semantics: UsageInputTokenSemantics,
    tokens: UsageTokenAxes,
    cost_derivation_safe: bool,
) -> WebUsageCost {
    let unavailable = |reason| WebUsageCost::Unavailable { reason };
    if tokens.coverage()
        == (signalbox_application::UsageTokenCoverage {
            input: UsageTokenPresence::Absent,
            output: UsageTokenPresence::Absent,
            cache_creation_input: UsageTokenPresence::Absent,
            cache_read_input: UsageTokenPresence::Absent,
        })
    {
        return unavailable(WebUsageCostUnavailableReason::NoTokenEvidence);
    }
    let semantics = match input_semantics {
        UsageInputTokenSemantics::Unknown => {
            return unavailable(WebUsageCostUnavailableReason::UnknownInputSemantics);
        }
        UsageInputTokenSemantics::CacheExclusive => {
            ProcessModelCallInputTokenSemantics::CacheExclusive
        }
        UsageInputTokenSemantics::CacheInclusive => {
            if tokens.output.is_none()
                && tokens.cache_creation_input.is_none()
                && tokens.cache_read_input.is_none()
            {
                return unavailable(WebUsageCostUnavailableReason::IncompleteCacheAxes);
            }
            let cache_total = tokens.cache_creation_input.zip(tokens.cache_read_input);
            if cache_total.is_some_and(|(creation, read)| {
                creation
                    .checked_add(read)
                    .is_none_or(|cache| tokens.input.is_some_and(|input| input < cache))
            }) {
                return unavailable(WebUsageCostUnavailableReason::InvalidCacheBreakdown);
            }
            ProcessModelCallInputTokenSemantics::CacheInclusive
        }
    };
    if !cost_derivation_safe {
        return unavailable(WebUsageCostUnavailableReason::InvalidCacheBreakdown);
    }
    let Some(cost) = configuration.derive_model_call_cost(
        model,
        credential_profile,
        ModelCallInputUsage::from_persisted(tokens.input, Some(semantics)),
        tokens.output,
        tokens.cache_creation_input,
        tokens.cache_read_input,
    ) else {
        return unavailable(WebUsageCostUnavailableReason::ConfigurationUnavailable);
    };
    WebUsageCost::Derived {
        amount_usd: WebDollarAmount::from_derived(cost.amount_usd().normalize().to_string()),
        rate_version: WebUsageRateVersion::from_configured(cost.rate_version().to_owned()),
        label: match cost.billing_kind() {
            BillingKind::ApiMetered => WebUsageCostLabel::Real,
            BillingKind::Subscription => WebUsageCostLabel::MeteredEquivalent,
        },
    }
}

fn usage_aggregate_cost_dto(
    configuration: &HubModelConfiguration,
    group: &UsageAggregateGroup,
) -> WebUsageCost {
    let unavailable = |reason| WebUsageCost::Unavailable { reason };
    if group.tokens.input.is_none()
        && group.tokens.output.is_none()
        && group.tokens.cache_creation_input.is_none()
        && group.tokens.cache_read_input.is_none()
    {
        return unavailable(WebUsageCostUnavailableReason::NoTokenEvidence);
    }
    let semantics = match group.key.input_semantics {
        UsageInputTokenSemantics::Unknown => {
            return unavailable(WebUsageCostUnavailableReason::UnknownInputSemantics);
        }
        UsageInputTokenSemantics::CacheExclusive => {
            ProcessModelCallInputTokenSemantics::CacheExclusive
        }
        UsageInputTokenSemantics::CacheInclusive => {
            if group.tokens.output.is_none()
                && group.tokens.cache_creation_input.is_none()
                && group.tokens.cache_read_input.is_none()
            {
                return unavailable(WebUsageCostUnavailableReason::IncompleteCacheAxes);
            }
            if group
                .tokens
                .cache_creation_input
                .zip(group.tokens.cache_read_input)
                .is_some_and(|(creation, read)| {
                    creation
                        .checked_add(read)
                        .is_none_or(|cache| group.tokens.input.is_some_and(|input| input < cache))
                })
            {
                return unavailable(WebUsageCostUnavailableReason::InvalidCacheBreakdown);
            }
            ProcessModelCallInputTokenSemantics::CacheInclusive
        }
    };
    if !group.cost_derivation_safe {
        return unavailable(WebUsageCostUnavailableReason::InvalidCacheBreakdown);
    }
    let Some(cost) = configuration.derive_usage_aggregate_cost(
        group.key.model,
        &group.key.credential_profile,
        semantics,
        group.tokens.input,
        group.tokens.output,
        group.tokens.cache_creation_input,
        group.tokens.cache_read_input,
    ) else {
        return unavailable(WebUsageCostUnavailableReason::ConfigurationUnavailable);
    };
    WebUsageCost::Derived {
        amount_usd: WebDollarAmount::from_derived(cost.amount_usd().normalize().to_string()),
        rate_version: WebUsageRateVersion::from_configured(cost.rate_version().to_owned()),
        label: match cost.billing_kind() {
            BillingKind::ApiMetered => WebUsageCostLabel::Real,
            BillingKind::Subscription => WebUsageCostLabel::MeteredEquivalent,
        },
    }
}

const fn usage_call_kind_dto(kind: UsageCallKind) -> WebUsageCallKind {
    match kind {
        UsageCallKind::ModelCall => WebUsageCallKind::ModelCall,
        UsageCallKind::ApprovalJudge => WebUsageCallKind::ApprovalJudge,
        UsageCallKind::ContextCompaction => WebUsageCallKind::ContextCompaction,
    }
}

const fn usage_provenance_dto(provenance: UsageProvenance) -> WebUsageProvenance {
    match provenance {
        UsageProvenance::Reported => WebUsageProvenance::Reported,
        UsageProvenance::Estimated => WebUsageProvenance::Estimated,
    }
}

const fn usage_input_semantics_dto(semantics: UsageInputTokenSemantics) -> WebUsageInputSemantics {
    match semantics {
        UsageInputTokenSemantics::Unknown => WebUsageInputSemantics::Unknown,
        UsageInputTokenSemantics::CacheExclusive => WebUsageInputSemantics::CacheExclusive,
        UsageInputTokenSemantics::CacheInclusive => WebUsageInputSemantics::CacheInclusive,
    }
}

fn usage_tokens_dto(tokens: UsageTokenAxes) -> WebUsageTokenAxes {
    WebUsageTokenAxes {
        input: WebNullableU64::from_option(tokens.input),
        output: WebNullableU64::from_option(tokens.output),
        cache_creation_input: WebNullableU64::from_option(tokens.cache_creation_input),
        cache_read_input: WebNullableU64::from_option(tokens.cache_read_input),
    }
}

fn usage_aggregate_tokens_dto(tokens: UsageAggregateTokenAxes) -> WebUsageAggregateTokenAxes {
    WebUsageAggregateTokenAxes {
        input: WebNullableU128::from_option(tokens.input),
        output: WebNullableU128::from_option(tokens.output),
        cache_creation_input: WebNullableU128::from_option(tokens.cache_creation_input),
        cache_read_input: WebNullableU128::from_option(tokens.cache_read_input),
    }
}

#[derive(Clone, Copy, Debug)]
enum SessionTimelineRequestError {
    InvalidSessionId,
    InvalidAddress,
    InvalidAnchor,
    MissingBounds,
}

impl SessionTimelineRequestError {
    fn into_response(self) -> Response {
        match self {
            Self::InvalidSessionId => application_error(
                StatusCode::BAD_REQUEST,
                "invalid_session_id",
                "session id is not a UUID",
            ),
            Self::InvalidAddress => application_error(
                StatusCode::BAD_REQUEST,
                "invalid_timeline_address",
                "this anchor requires one positive decimal timeline address",
            ),
            Self::InvalidAnchor => application_error(
                StatusCode::BAD_REQUEST,
                "invalid_timeline_anchor",
                "timeline anchor and address do not form a recognized request",
            ),
            Self::MissingBounds => {
                tracing::error!(
                    failure_class = "fail_closed_corruption",
                    "session timeline projection has missing bounds"
                );
                application_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "session_projection_failed",
                    "an existing session has no durable timeline bound",
                )
            }
        }
    }
}

async fn session_descriptor(
    State(state): State<WebApiState>,
    Path(session_id): Path<String>,
) -> Response {
    let Some(repository) = state.timeline else {
        return application_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "session_projection_unavailable",
            "session projection is not configured",
        );
    };
    let session = match parse_session_id(&session_id) {
        Ok(session) => session,
        Err(error) => return error.into_response(),
    };
    match repository.read_descriptor(session).await {
        Ok(Some(descriptor)) => match descriptor_dto(descriptor) {
            Ok(descriptor) => Json(descriptor).into_response(),
            Err(error) => error.into_response(),
        },
        Ok(None) => application_error(
            StatusCode::NOT_FOUND,
            "session_not_found",
            "the requested session does not exist",
        ),
        Err(error) => repository_projection_error(error),
    }
}

async fn session_timeline_window(
    State(state): State<WebApiState>,
    Path(session_id): Path<String>,
    query: Result<Query<TimelineWindowQuery>, QueryRejection>,
) -> Response {
    let Query(query) = match query {
        Ok(query) => query,
        Err(_) => return invalid_timeline_query(),
    };
    let limits = (|| {
        let max_items = query
            .max_items
            .as_deref()
            .and_then(|value| value.parse::<u16>().ok())?;
        let max_bytes = query
            .max_bytes
            .as_deref()
            .and_then(|value| value.parse::<u32>().ok())?;
        TimelineWindowLimits::new(max_items, max_bytes).ok()
    })();
    let Some(limits) = limits else {
        return invalid_timeline_query();
    };
    let Some(repository) = state.timeline else {
        return application_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "session_projection_unavailable",
            "session projection is not configured",
        );
    };
    let session = match parse_session_id(&session_id) {
        Ok(session) => session,
        Err(error) => return error.into_response(),
    };
    let anchor = match parse_window_anchor(&query.anchor, query.address.as_deref()) {
        Ok(anchor) => anchor,
        Err(error) => return error.into_response(),
    };
    match repository.read_window(session, anchor, limits).await {
        Ok(Some(window)) => Json(window_dto(window)).into_response(),
        Ok(None) => application_error(
            StatusCode::NOT_FOUND,
            "session_not_found",
            "the requested session does not exist",
        ),
        Err(error) => repository_projection_error(error),
    }
}

fn invalid_timeline_query() -> Response {
    application_error(
        StatusCode::BAD_REQUEST,
        "invalid_timeline_limits",
        "timeline query parameters are malformed or outside the contract bounds",
    )
}

fn repository_projection_error(error: SessionTimelineRepositoryError) -> Response {
    let failure_class = match &error {
        SessionTimelineRepositoryError::Database(_) => "infrastructure",
        SessionTimelineRepositoryError::Corruption(_) => "fail_closed_corruption",
    };
    tracing::error!(
        failure_class,
        cause = %error,
        "session timeline projection read failed"
    );
    application_error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "session_projection_failed",
        "the durable session projection could not be read",
    )
}

fn parse_session_id(value: &str) -> Result<SessionId, SessionTimelineRequestError> {
    uuid::Uuid::parse_str(value)
        .map(SessionId::from_uuid)
        .map_err(|_| SessionTimelineRequestError::InvalidSessionId)
}

fn parse_window_anchor(
    anchor: &str,
    address: Option<&str>,
) -> Result<TimelineWindowAnchor, SessionTimelineRequestError> {
    let parsed_address = || {
        address
            .filter(|value| {
                !value.is_empty()
                    && value.bytes().all(|byte| byte.is_ascii_digit())
                    && !value.starts_with('0')
            })
            .and_then(|value| value.parse::<u64>().ok())
            .and_then(std::num::NonZeroU64::new)
            .map(TimelineAddress::new)
            .ok_or(SessionTimelineRequestError::InvalidAddress)
    };
    match (anchor, address) {
        ("first", None) => Ok(TimelineWindowAnchor::First),
        ("latest", None) => Ok(TimelineWindowAnchor::Latest),
        ("before", _) => parsed_address().map(TimelineWindowAnchor::Before),
        ("after", _) => parsed_address().map(TimelineWindowAnchor::After),
        ("around", _) => parsed_address().map(TimelineWindowAnchor::Around),
        _ => Err(SessionTimelineRequestError::InvalidAnchor),
    }
}

fn address_dto(address: TimelineAddress) -> WebTimelineAddress {
    WebTimelineAddress {
        event_sequence: WebTimelineEventSequence::from_nonzero(address.sequence()),
    }
}

fn descriptor_dto(
    descriptor: SessionTimelineDescriptor,
) -> Result<WebSessionTimelineDescriptor, SessionTimelineRequestError> {
    let Some(first_address) = descriptor.bounds.first else {
        return Err(SessionTimelineRequestError::MissingBounds);
    };
    let Some(latest_address) = descriptor.bounds.latest else {
        return Err(SessionTimelineRequestError::MissingBounds);
    };
    Ok(WebSessionTimelineDescriptor {
        session_id: WebSessionId::from_uuid_bytes(*descriptor.session.into_uuid().as_bytes()),
        sizes: WebSessionTimelineSizeFacts {
            item_count: WebU64::from_u64(descriptor.sizes.item_count),
            projected_text_bytes: WebU64::from_u64(descriptor.sizes.projected_text_bytes),
            projected_structured_bytes: WebU64::from_u64(
                descriptor.sizes.projected_structured_bytes,
            ),
            referenced_blob_count: WebU64::from_u64(descriptor.sizes.referenced_blob_count),
            referenced_blob_bytes: WebU64::from_u64(descriptor.sizes.referenced_blob_bytes),
        },
        first_address: address_dto(first_address),
        latest_address: address_dto(latest_address),
        work: WebSessionWorkFacts {
            active_turn_count: WebU64::from_u64(descriptor.work.active_turn_count),
            queued_turn_count: WebU64::from_u64(descriptor.work.queued_turn_count),
        },
        observed_through: WebU64::from_u64(descriptor.observed_through),
    })
}

fn window_dto(window: SessionTimelineWindow) -> WebSessionTimelineWindow {
    let continuation_before = match window.continuation_before {
        TimelineContinuation::Exhausted => None,
        TimelineContinuation::MoreAt(address) => Some(address_dto(address)),
    };
    let continuation_after = match window.continuation_after {
        TimelineContinuation::Exhausted => None,
        TimelineContinuation::MoreAt(address) => Some(address_dto(address)),
    };
    WebSessionTimelineWindow {
        session_id: WebSessionId::from_uuid_bytes(*window.session.into_uuid().as_bytes()),
        items: window
            .items
            .into_iter()
            .map(|item| WebSessionTimelineItem {
                address: address_dto(item.address),
                kind: event_kind_dto(item.kind),
                projected_structured_bytes: item.projected_structured_bytes,
            })
            .collect(),
        projected_structured_bytes: window.projected_structured_bytes,
        continuation_before,
        continuation_after,
    }
}

fn event_kind_dto(kind: SessionTimelineEventKind) -> WebSessionTimelineEventKind {
    match kind {
        SessionTimelineEventKind::SessionCreated => WebSessionTimelineEventKind::SessionCreated,
        SessionTimelineEventKind::SessionModelSettingsChanged => {
            WebSessionTimelineEventKind::SessionModelSettingsChanged
        }
        SessionTimelineEventKind::TurnModelSettingsResolved => {
            WebSessionTimelineEventKind::TurnModelSettingsResolved
        }
        SessionTimelineEventKind::InputAccepted => WebSessionTimelineEventKind::InputAccepted,
        SessionTimelineEventKind::GoalTurnRetired => WebSessionTimelineEventKind::GoalTurnRetired,
        SessionTimelineEventKind::TurnActivated => WebSessionTimelineEventKind::TurnActivated,
        SessionTimelineEventKind::TurnFailed => WebSessionTimelineEventKind::TurnFailed,
        SessionTimelineEventKind::ModelCallTransition => {
            WebSessionTimelineEventKind::ModelCallTransition
        }
        SessionTimelineEventKind::ToolBatchTransition => {
            WebSessionTimelineEventKind::ToolBatchTransition
        }
        SessionTimelineEventKind::ToolApprovalDecided => {
            WebSessionTimelineEventKind::ToolApprovalDecided
        }
        SessionTimelineEventKind::ContextCompacted => WebSessionTimelineEventKind::ContextCompacted,
        SessionTimelineEventKind::TurnCompleted => WebSessionTimelineEventKind::TurnCompleted,
        SessionTimelineEventKind::TurnRefused => WebSessionTimelineEventKind::TurnRefused,
        SessionTimelineEventKind::TurnCancelled => WebSessionTimelineEventKind::TurnCancelled,
        SessionTimelineEventKind::TurnReconciliationRequired => {
            WebSessionTimelineEventKind::TurnReconciliationRequired
        }
        SessionTimelineEventKind::RunnerStateTransition => {
            WebSessionTimelineEventKind::RunnerStateTransition
        }
        SessionTimelineEventKind::DelegationUpdate => WebSessionTimelineEventKind::DelegationUpdate,
        SessionTimelineEventKind::DelegationWake => WebSessionTimelineEventKind::DelegationWake,
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
        .route("/bootstrap", get(contract_bootstrap))
        .route("/test/read", get(deterministic_read))
        .route("/test/stream", get(deterministic_stream))
        .nest("/test", mutation)
        .fallback(api_not_found)
        .layer(DefaultBodyLimit::max(MAX_JSON_BODY_BYTES));
    Router::new()
        .route("/", get(deterministic_page))
        .nest("/api", api)
}

async fn contract_bootstrap() -> Json<WebContractBootstrap> {
    Json(WebContractBootstrap::current())
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
    let body = Json(WebApiErrorResponse {
        error: WebApiError {
            kind: WebApiErrorKind::Transport,
            code: code.to_owned(),
            message: message.to_owned(),
        },
    });
    (status, body).into_response()
}

fn application_error(status: StatusCode, code: &'static str, message: &'static str) -> Response {
    (
        status,
        Json(WebApiErrorResponse {
            error: WebApiError {
                kind: WebApiErrorKind::Application,
                code: code.to_owned(),
                message: message.to_owned(),
            },
        }),
    )
        .into_response()
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
    use signalbox_application::{UsageInputTokenSemantics, UsageTokenAxes};
    use signalbox_domain::{ProviderModelIdentity, ResolvedProviderTarget};
    use signalbox_web_contract::{
        MAX_JSON_BODY_BYTES, MAX_NDJSON_ITEM_BYTES, WebContractBootstrap, WebContractExample,
        WebUsageCost, WebUsageCostUnavailableReason,
    };
    use tokio::sync::{mpsc, watch};
    use tower::ServiceExt as _;
    use url::Url;

    use super::{
        DEFAULT_WEB_BIND_ADDRESS, WebHttpConfiguration, WebHttpConfigurationError, WebHttpRuntime,
        deterministic_test_router, ndjson_response, production_router, usage_cost_dto,
    };
    use crate::HubModelConfiguration;

    fn loopback_ephemeral() -> SocketAddr {
        "127.0.0.1:0"
            .parse()
            .expect("the test listener address is valid")
    }

    fn example() -> WebContractExample {
        WebContractExample {
            request_id: "transport-test".to_owned(),
            message: "bounded payload".to_owned(),
        }
    }

    fn example_model_configuration() -> HubModelConfiguration {
        HubModelConfiguration::parse(crate::configuration::tests::CONFIGURATION)
            .expect("the shared model configuration fixture is valid")
    }

    fn rated_example_target() -> ResolvedProviderTarget {
        ResolvedProviderTarget::naming(ProviderModelIdentity::from_uuid(uuid::uuid!(
            "20000000-0000-4000-8000-000000000001"
        )))
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
    fn explicit_loopback_deployment_configuration_is_admitted() {
        let bind_address: SocketAddr = "127.0.0.1:8080"
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
    fn non_loopback_bind_is_rejected_without_authentication() {
        let error = WebHttpConfiguration::from_values(Some(OsString::from("0.0.0.0:8080")), None)
            .expect_err("unauthenticated browser routes remain loopback-only");

        assert_eq!(error, WebHttpConfigurationError::NonLoopbackBindAddress);
    }

    #[test]
    fn explicit_non_loopback_configuration_is_rejected() {
        let bind_address = "0.0.0.0:8080"
            .parse()
            .expect("the fixture address is valid");
        let error = WebHttpConfiguration::new(bind_address, None)
            .expect_err("every production configuration remains loopback-only");

        assert_eq!(error, WebHttpConfigurationError::NonLoopbackBindAddress);
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
        let runtime = WebHttpRuntime::bind_router(
            loopback_ephemeral(),
            production_router(Some(assets.path().to_path_buf()), None),
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
        assert_eq!(decoded, WebContractBootstrap::current());
        assert_eq!(runtime_outcome, Ok(()));
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
    async fn malformed_timeline_query_uses_the_structured_error_envelope() {
        let request = Request::get(
            "/api/sessions/00000000-0000-0000-0000-000000000991/timeline?max_items=nope",
        )
        .header(header::HOST, "localhost")
        .body(Body::empty())
        .expect("the request is valid");
        let response = production_router(None, None)
            .oneshot(request)
            .await
            .expect("the production router responds");
        let status = response.status();
        let body: serde_json::Value = serde_json::from_slice(&response_body(response).await)
            .expect("the rejection is structured JSON");

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"]["kind"], "application");
        assert_eq!(body["error"]["code"], "invalid_timeline_limits");
    }

    #[tokio::test]
    async fn missing_timeline_ceiling_uses_the_structured_error_envelope() {
        let request = Request::get(
            "/api/sessions/00000000-0000-0000-0000-000000000991/timeline?anchor=first&max_items=1",
        )
        .header(header::HOST, "localhost")
        .body(Body::empty())
        .expect("the request is valid");
        let response = production_router(None, None)
            .oneshot(request)
            .await
            .expect("the production router responds");
        let status = response.status();
        let body: serde_json::Value = serde_json::from_slice(&response_body(response).await)
            .expect("the rejection is structured JSON");

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"]["code"], "invalid_timeline_limits");
    }

    #[tokio::test]
    async fn search_rejects_a_non_product_strategy() {
        let unsupported = Request::get("/api/search?strategy=postgres&q=term&max_items=10")
            .header(header::HOST, "localhost")
            .body(Body::empty())
            .expect("the request is valid");
        let unsupported = production_router(None, None)
            .oneshot(unsupported)
            .await
            .expect("the production router responds");
        let unsupported_status = unsupported.status();
        let unsupported_body: serde_json::Value =
            serde_json::from_slice(&response_body(unsupported).await)
                .expect("the rejection is structured JSON");

        assert_eq!(unsupported_status, StatusCode::BAD_REQUEST);
        assert_eq!(unsupported_body["error"]["code"], "invalid_search_query");
    }

    #[tokio::test]
    async fn search_rejects_a_partial_cursor() {
        let partial =
            Request::get("/api/search?strategy=lexical&q=term&max_items=10&after_address=5")
                .header(header::HOST, "localhost")
                .body(Body::empty())
                .expect("the request is valid");
        let partial = production_router(None, None)
            .oneshot(partial)
            .await
            .expect("the production router responds");
        let partial_status = partial.status();
        let partial_body: serde_json::Value = serde_json::from_slice(&response_body(partial).await)
            .expect("the rejection is structured JSON");

        assert_eq!(partial_status, StatusCode::BAD_REQUEST);
        assert_eq!(partial_body["error"]["code"], "invalid_search_query");
    }

    #[tokio::test]
    async fn search_rejects_an_oversized_projection_cursor() {
        let oversized = Request::get(
            "/api/search?strategy=lexical&q=term&max_items=10&after_address=5&after_projection=9223372036854775808",
        )
        .header(header::HOST, "localhost")
        .body(Body::empty())
        .expect("the request is valid");
        let oversized = production_router(None, None)
            .oneshot(oversized)
            .await
            .expect("the production router responds");
        let oversized_status = oversized.status();

        assert_eq!(oversized_status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn valid_search_is_parsed_before_repository_availability_is_reported() {
        let request = Request::get(
            "/api/search?strategy=lexical&q=natural%20terms&max_items=100&after_address=5&after_projection=7",
        )
        .header(header::HOST, "localhost")
        .body(Body::empty())
        .expect("the request is valid");
        let response = production_router(None, None)
            .oneshot(request)
            .await
            .expect("the production router responds");
        let status = response.status();
        let body: serde_json::Value = serde_json::from_slice(&response_body(response).await)
            .expect("the response is structured JSON");

        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["error"]["code"], "search_projection_unavailable");
    }

    #[tokio::test]
    async fn representable_usage_filters_are_parsed_before_projection_availability_is_reported() {
        let request = Request::get(
            "/api/usage/calls?from_micros=0&to_micros=1777777777123456&provenance=estimated&call_kind=approval_judge&order=oldest&max_items=100",
        )
        .header(header::HOST, "localhost")
        .body(Body::empty())
        .expect("the request is valid");
        let response = production_router(None, None)
            .oneshot(request)
            .await
            .expect("the production router responds");
        let status = response.status();
        let body: serde_json::Value = serde_json::from_slice(&response_body(response).await)
            .expect("the response is structured JSON");

        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["error"]["code"], "usage_projection_unavailable");
    }

    #[tokio::test]
    async fn usage_filters_reject_timestamps_outside_persistence_range() {
        let request = Request::get(
            "/api/usage/calls?to_micros=9223372036854775807&order=oldest&max_items=100",
        )
        .header(header::HOST, "localhost")
        .body(Body::empty())
        .expect("the request is valid");
        let response = production_router(None, None)
            .oneshot(request)
            .await
            .expect("the production router responds");
        let status = response.status();
        let body: serde_json::Value = serde_json::from_slice(&response_body(response).await)
            .expect("the rejection is structured JSON");

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"]["code"], "invalid_usage_query");
    }

    #[tokio::test]
    async fn usage_detail_rejects_a_partial_keyset_cursor() {
        let request =
            Request::get("/api/usage/calls?order=newest&max_items=10&after_recorded_at_micros=7")
                .header(header::HOST, "localhost")
                .body(Body::empty())
                .expect("the request is valid");
        let response = production_router(None, None)
            .oneshot(request)
            .await
            .expect("the production router responds");
        let status = response.status();
        let body: serde_json::Value = serde_json::from_slice(&response_body(response).await)
            .expect("the rejection is structured JSON");

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"]["code"], "invalid_usage_query");
    }

    #[test]
    fn configured_usage_cost_keeps_rate_version_and_billing_label_separate() {
        let configuration = example_model_configuration();
        let tokens = UsageTokenAxes {
            input: Some(1_000_000),
            output: None,
            cache_creation_input: None,
            cache_read_input: None,
        };
        let real = usage_cost_dto(
            &configuration,
            rated_example_target(),
            "anthropic-primary",
            UsageInputTokenSemantics::CacheExclusive,
            tokens,
            true,
        );
        let metered_equivalent = usage_cost_dto(
            &configuration,
            rated_example_target(),
            "codex-subscription-primary",
            UsageInputTokenSemantics::CacheExclusive,
            tokens,
            true,
        );
        let real = serde_json::to_value(real).expect("real cost serializes");
        let metered_equivalent =
            serde_json::to_value(metered_equivalent).expect("equivalent cost serializes");

        assert_eq!(real["status"], "derived");
        assert_eq!(real["label"], "real");
        assert_eq!(metered_equivalent["status"], "derived");
        assert_eq!(metered_equivalent["label"], "metered_equivalent");
        assert_eq!(real["rate_version"], metered_equivalent["rate_version"]);
        assert_eq!(real["amount_usd"], metered_equivalent["amount_usd"]);
    }

    #[test]
    fn configured_usage_cost_prices_independent_axes_with_incomplete_cache_coverage() {
        let configuration = example_model_configuration();
        let cost = usage_cost_dto(
            &configuration,
            rated_example_target(),
            "anthropic-primary",
            UsageInputTokenSemantics::CacheInclusive,
            UsageTokenAxes {
                input: Some(10),
                output: Some(2),
                cache_creation_input: None,
                cache_read_input: Some(3),
            },
            true,
        );
        let cost = serde_json::to_value(cost).expect("cost serializes");

        assert_eq!(cost["status"], "derived");
        assert_ne!(cost["amount_usd"], "0");
    }

    #[test]
    fn configured_usage_cost_rejects_an_overflowing_cache_total() {
        let configuration = example_model_configuration();
        let cost = usage_cost_dto(
            &configuration,
            rated_example_target(),
            "anthropic-primary",
            UsageInputTokenSemantics::CacheInclusive,
            UsageTokenAxes {
                input: Some(u64::MAX),
                output: None,
                cache_creation_input: Some(u64::MAX),
                cache_read_input: Some(1),
            },
            true,
        );

        assert_eq!(
            cost,
            WebUsageCost::Unavailable {
                reason: WebUsageCostUnavailableReason::InvalidCacheBreakdown,
            }
        );
    }

    #[test]
    fn configured_usage_cost_reports_unpriceable_incomplete_cache_evidence() {
        let configuration = example_model_configuration();
        let cost = usage_cost_dto(
            &configuration,
            rated_example_target(),
            "anthropic-primary",
            UsageInputTokenSemantics::CacheInclusive,
            UsageTokenAxes {
                input: Some(10),
                output: None,
                cache_creation_input: None,
                cache_read_input: None,
            },
            true,
        );

        assert_eq!(
            cost,
            WebUsageCost::Unavailable {
                reason: WebUsageCostUnavailableReason::IncompleteCacheAxes,
            }
        );
    }

    #[tokio::test]
    async fn session_reads_reject_non_loopback_host_authorities() {
        let request = Request::get("/api/sessions/00000000-0000-0000-0000-000000000991")
            .header(header::HOST, "attacker.example")
            .body(Body::empty())
            .expect("the request is valid");
        let response = production_router(None, None)
            .oneshot(request)
            .await
            .expect("the production router responds");
        let status = response.status();
        let body: serde_json::Value = serde_json::from_slice(&response_body(response).await)
            .expect("the rejection is structured JSON");

        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body["error"]["kind"], "transport");
        assert_eq!(body["error"]["code"], "non_loopback_host_rejected");
    }

    #[test]
    fn timeline_addresses_require_canonical_positive_decimal() {
        assert!(super::parse_window_anchor("after", Some("+5")).is_err());
        assert!(super::parse_window_anchor("after", Some("05")).is_err());
        assert!(super::parse_window_anchor("after", Some("0")).is_err());
        assert!(super::parse_window_anchor("after", Some("-5")).is_err());
        assert!(super::parse_window_anchor("after", Some(" 5")).is_err());
        assert!(super::parse_window_anchor("after", Some("5 ")).is_err());
        assert!(super::parse_window_anchor("after", Some("5")).is_ok());
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
