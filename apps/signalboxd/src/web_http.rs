//! Browser-facing same-origin HTTP transport foundation.
//!
//! This boundary owns browser HTTP semantics and browser DTOs. It does not
//! expose local process-protocol messages, storage records, or application
//! authentication.

use std::{
    collections::VecDeque,
    env,
    error::Error,
    ffi::OsString,
    fmt, io,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::PathBuf,
    time::{Duration, UNIX_EPOCH},
};

use axum::{
    Json, Router,
    body::{Body, Bytes, to_bytes},
    extract::{DefaultBodyLimit, Path, Query, RawQuery, Request, State, rejection::QueryRejection},
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
    AttentionAction, AttentionActivityKind, AttentionBlockedReason, AttentionChanges,
    AttentionContinuation, AttentionQuery, AttentionSnapshot, AttentionSort, AttentionState,
    AttentionSummary, SessionTimelineDescriptor, SessionTimelineDetailBody,
    SessionTimelineDetailPage, SessionTimelineEventKind, SessionTimelineWindow, TimelineAddress,
    TimelineApprovalActor, TimelineApprovalDecision, TimelineBodyContinuation, TimelineBodyField,
    TimelineBoundChildAction, TimelineContinuation, TimelineDelegationDetail,
    TimelineDelegationOutcome, TimelineDelegationPolicy, TimelineDelegationProvenance,
    TimelineDelegationReason, TimelineDelegationWaitMode, TimelineDetailContinuation,
    TimelineDetailCursor, TimelineDetailLimits, TimelineGoalBlockedReason, TimelineGoalEvent,
    TimelineModelCallDisposition, TimelineModelCallState, TimelineModelSettingsDetail,
    TimelineReconciliationOperation, TimelineRunnerSandboxPosture, TimelineRunnerState,
    TimelineTextExcerpt, TimelineToolApprovalPosture, TimelineToolAttempt, TimelineToolBatchState,
    TimelineToolEffectPosture, TimelineToolSandboxPosture, TimelineToolState,
    TimelineTurnLifecycleKind, TimelineWindowAnchor, TimelineWindowLimits,
    max_attention_filter_tags, max_attention_filter_utf8_bytes,
    max_attention_goal_summary_characters, max_attention_snapshot_items,
    max_attention_title_characters,
};
use signalbox_domain::{
    ImportedSessionRelationship, ProviderModelCallFailureCause, SessionId, TurnId,
};
use signalbox_persistence::attention::{AttentionRepository, AttentionRepositoryError};
use signalbox_persistence::outbox::OutboxDispatchError;
use signalbox_persistence::session_timeline::{
    SessionTimelineRepository, SessionTimelineRepositoryError,
};
use signalbox_web_contract::{
    MAX_JSON_BODY_BYTES, MAX_NDJSON_ITEM_BYTES, WebApiError, WebApiErrorKind, WebApiErrorResponse,
    WebAttentionAction, WebAttentionActivity, WebAttentionActivityKind, WebAttentionBlockedReason,
    WebAttentionContinuation, WebAttentionGoalBlock, WebAttentionJudgeFacts, WebAttentionSnapshot,
    WebAttentionSort, WebAttentionState, WebAttentionStreamEvent, WebAttentionSummary, WebBlobId,
    WebContractBootstrap, WebContractExample, WebPositiveU64, WebProviderModelCallFailureCause,
    WebRunnerWorkingDirectory, WebSessionId, WebSessionTimelineDescriptor,
    WebSessionTimelineDetail, WebSessionTimelineDetailBody, WebSessionTimelineDetailPage,
    WebSessionTimelineEventKind, WebSessionTimelineItem, WebSessionTimelineSizeFacts,
    WebSessionTimelineWindow, WebSessionWorkFacts, WebTimelineAddress, WebTimelineApprovalActor,
    WebTimelineApprovalDecision, WebTimelineBlobReference, WebTimelineBodyContinuation,
    WebTimelineBodyField, WebTimelineBoundChildAction, WebTimelineDelegationDetail,
    WebTimelineDelegationOutcome, WebTimelineDelegationPolicy, WebTimelineDelegationProvenance,
    WebTimelineDelegationReason, WebTimelineDelegationWaitMode, WebTimelineDetailContinuation,
    WebTimelineEffectiveModelSettings, WebTimelineEventSequence, WebTimelineFastMode,
    WebTimelineFastModeOverlay, WebTimelineGoalBlockedReason, WebTimelineGoalEvent,
    WebTimelineImportedEvidence, WebTimelineImportedRelationship, WebTimelineModelCallDisposition,
    WebTimelineModelCallState, WebTimelineModelChangeAdjustment, WebTimelineModelSelection,
    WebTimelineModelSettingSource, WebTimelineModelSettingsDetail, WebTimelineModelSettingsOverlay,
    WebTimelineModelSettingsPrecedence, WebTimelineModelSettingsSnapshot, WebTimelineModelUsage,
    WebTimelineReasoningLevel, WebTimelineReconciliationOperation, WebTimelineRunnerSandboxPosture,
    WebTimelineRunnerState, WebTimelineServiceTier, WebTimelineSettingOverlay,
    WebTimelineTextExcerpt, WebTimelineToolApprovalPosture, WebTimelineToolAttempt,
    WebTimelineToolAttemptEvidence, WebTimelineToolBatchState, WebTimelineToolEffectPosture,
    WebTimelineToolFailureCause, WebTimelineToolSandboxPosture, WebTimelineToolState,
    WebTimelineTurnLifecycleKind, WebToolName, WebTurnId, WebU64,
};
use sqlx::{PgPool, types::Uuid};
use tokio::{net::TcpListener, sync::watch};
use tower_http::services::{ServeDir, ServeFile};
use url::Url;

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
    ) -> Result<Self, WebHttpRuntimeError> {
        let router = production_router(configuration.asset_root, Some(pool));
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
    let state = WebApiState {
        timeline: pool.clone().map(SessionTimelineRepository::new),
        attention: pool.map(AttentionRepository::new),
    };
    let session_reads = Router::new()
        .route("/sessions/{session_id}", get(session_descriptor))
        .route(
            "/sessions/{session_id}/timeline",
            get(session_timeline_window),
        )
        .route("/sessions", get(attention_snapshot))
        .route("/attention/follow", get(attention_follow))
        .route(
            "/sessions/{session_id}/timeline/{address}/detail",
            get(session_timeline_item_detail),
        )
        .route(
            "/sessions/{session_id}/turns/{turn_id}/timeline-detail",
            get(session_timeline_turn_detail),
        )
        .route(
            "/sessions/{session_id}/timeline-detail",
            get(session_timeline_region_detail),
        )
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
    attention: Option<AttentionRepository>,
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
struct TimelineDetailQuery {
    max_items: Option<String>,
    max_bytes: Option<String>,
    cursor_address: Option<String>,
    cursor_field: Option<String>,
    cursor_member: Option<String>,
    cursor_offset: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TimelineRegionDetailQuery {
    first: Option<String>,
    through: Option<String>,
    max_items: Option<String>,
    max_bytes: Option<String>,
    cursor_address: Option<String>,
    cursor_field: Option<String>,
    cursor_member: Option<String>,
    cursor_offset: Option<String>,
}

#[derive(Clone, Copy, Debug)]
enum SessionTimelineRequestError {
    InvalidSessionId,
    InvalidAddress,
    InvalidAnchor,
    MissingBounds,
    InvalidProjectedSessionId,
    InvalidProjectedToolAttempt,
    InvalidProjectedOrdinal,
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
            Self::InvalidProjectedSessionId => {
                tracing::error!(
                    failure_class = "fail_closed_corruption",
                    "session timeline projection has an invalid session identity"
                );
                application_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "session_projection_failed",
                    "an existing session has an invalid durable identity",
                )
            }
            Self::InvalidProjectedOrdinal => {
                tracing::error!(
                    failure_class = "fail_closed_corruption",
                    "session timeline projection has an invalid positive ordinal"
                );
                application_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "session_projection_failed",
                    "an existing session has an invalid durable ordinal",
                )
            }
            Self::InvalidProjectedToolAttempt => {
                tracing::error!(
                    failure_class = "fail_closed_corruption",
                    "session timeline projection has an invalid tool-attempt shape"
                );
                application_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "session_projection_failed",
                    "an existing session has invalid durable tool-attempt evidence",
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
        Ok(Some(window)) => match window_dto(window) {
            Ok(window) => Json(window).into_response(),
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

async fn session_timeline_item_detail(
    State(state): State<WebApiState>,
    Path((session_id, address)): Path<(String, String)>,
    query: Result<Query<TimelineDetailQuery>, QueryRejection>,
) -> Response {
    let Query(query) = match query {
        Ok(query) => query,
        Err(_) => return invalid_timeline_detail_query(),
    };
    let session = match parse_session_id(&session_id) {
        Ok(session) => session,
        Err(error) => return error.into_response(),
    };
    let address = match parse_timeline_address(&address) {
        Ok(address) => address,
        Err(error) => return error.into_response(),
    };
    let (limits, cursor) = match parse_detail_query(&query) {
        Some(parsed) => parsed,
        None => return invalid_timeline_detail_query(),
    };
    let Some(repository) = state.timeline else {
        return session_projection_unavailable();
    };
    match repository
        .read_item_details(session, address, cursor, limits)
        .await
    {
        Ok(Some(page)) => match detail_page_dto(page) {
            Ok(page) => Json(page).into_response(),
            Err(error) => error.into_response(),
        },
        Ok(None) => timeline_detail_not_found(),
        Err(error) => repository_projection_error(error),
    }
}

async fn session_timeline_turn_detail(
    State(state): State<WebApiState>,
    Path((session_id, turn_id)): Path<(String, String)>,
    query: Result<Query<TimelineDetailQuery>, QueryRejection>,
) -> Response {
    let Query(query) = match query {
        Ok(query) => query,
        Err(_) => return invalid_timeline_detail_query(),
    };
    let session = match parse_session_id(&session_id) {
        Ok(session) => session,
        Err(error) => return error.into_response(),
    };
    let turn = match uuid::Uuid::parse_str(&turn_id) {
        Ok(turn) => TurnId::from_uuid(turn),
        Err(_) => {
            return application_error(
                StatusCode::BAD_REQUEST,
                "invalid_turn_id",
                "turn id is not a UUID",
            );
        }
    };
    let (limits, cursor) = match parse_detail_query(&query) {
        Some(parsed) => parsed,
        None => return invalid_timeline_detail_query(),
    };
    let Some(repository) = state.timeline else {
        return session_projection_unavailable();
    };
    match repository
        .read_turn_details(session, turn, cursor, limits)
        .await
    {
        Ok(Some(page)) => match detail_page_dto(page) {
            Ok(page) => Json(page).into_response(),
            Err(error) => error.into_response(),
        },
        Ok(None) => timeline_detail_not_found(),
        Err(error) => repository_projection_error(error),
    }
}

async fn session_timeline_region_detail(
    State(state): State<WebApiState>,
    Path(session_id): Path<String>,
    query: Result<Query<TimelineRegionDetailQuery>, QueryRejection>,
) -> Response {
    let Query(query) = match query {
        Ok(query) => query,
        Err(_) => return invalid_timeline_detail_query(),
    };
    let session = match parse_session_id(&session_id) {
        Ok(session) => session,
        Err(error) => return error.into_response(),
    };
    let first = query
        .first
        .as_deref()
        .and_then(|value| parse_timeline_address(value).ok());
    let through = query
        .through
        .as_deref()
        .and_then(|value| parse_timeline_address(value).ok());
    let detail_query = TimelineDetailQuery {
        max_items: query.max_items,
        max_bytes: query.max_bytes,
        cursor_address: query.cursor_address,
        cursor_field: query.cursor_field,
        cursor_member: query.cursor_member,
        cursor_offset: query.cursor_offset,
    };
    let (Some(first), Some(through), Some((limits, cursor))) =
        (first, through, parse_detail_query(&detail_query))
    else {
        return invalid_timeline_detail_query();
    };
    let Some(repository) = state.timeline else {
        return session_projection_unavailable();
    };
    match repository
        .read_region_details(session, first, through, cursor, limits)
        .await
    {
        Ok(Some(page)) => match detail_page_dto(page) {
            Ok(page) => Json(page).into_response(),
            Err(error) => error.into_response(),
        },
        Ok(None) => timeline_detail_not_found(),
        Err(error) => repository_projection_error(error),
    }
}

fn parse_detail_query(
    query: &TimelineDetailQuery,
) -> Option<(TimelineDetailLimits, Option<TimelineDetailCursor>)> {
    let max_items = query.max_items.as_deref()?.parse::<u16>().ok()?;
    let max_bytes = query.max_bytes.as_deref()?.parse::<u32>().ok()?;
    let limits = TimelineDetailLimits::new(max_items, max_bytes).ok()?;
    let cursor = match (
        query.cursor_address.as_deref(),
        query.cursor_field.as_deref(),
        query.cursor_member.as_deref(),
        query.cursor_offset.as_deref(),
    ) {
        (None, None, None, None) => None,
        (Some(address), None, None, None) => Some(TimelineDetailCursor {
            address: parse_timeline_address(address).ok()?,
            field: None,
            member_index: 0,
            offset_bytes: 0,
        }),
        (Some(address), Some(field), Some(member), Some(offset)) => Some(TimelineDetailCursor {
            address: parse_timeline_address(address).ok()?,
            field: Some(parse_body_field(field).ok()?),
            member_index: member.parse().ok()?,
            offset_bytes: offset.parse().ok()?,
        }),
        _ => return None,
    };
    Some((limits, cursor))
}

fn parse_body_field(value: &str) -> Result<TimelineBodyField, ()> {
    match value {
        "input_text" => Ok(TimelineBodyField::InputText),
        "model_response" => Ok(TimelineBodyField::ModelResponse),
        "tool_arguments" => Ok(TimelineBodyField::ToolArguments),
        "tool_result" => Ok(TimelineBodyField::ToolResult),
        "tool_failure" => Ok(TimelineBodyField::ToolFailure),
        "approval_rationale" => Ok(TimelineBodyField::ApprovalRationale),
        "goal_text" => Ok(TimelineBodyField::GoalText),
        "compaction_summary" => Ok(TimelineBodyField::CompactionSummary),
        "delegation_content" => Ok(TimelineBodyField::DelegationContent),
        _ => Err(()),
    }
}

fn invalid_timeline_detail_query() -> Response {
    application_error(
        StatusCode::BAD_REQUEST,
        "invalid_timeline_detail_limits",
        "timeline detail parameters are malformed or outside the contract bounds",
    )
}

fn timeline_detail_not_found() -> Response {
    application_error(
        StatusCode::NOT_FOUND,
        "timeline_detail_not_found",
        "the requested session timeline detail does not exist",
    )
}

fn session_projection_unavailable() -> Response {
    application_error(
        StatusCode::SERVICE_UNAVAILABLE,
        "session_projection_unavailable",
        "session projection is not configured",
    )
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
        SessionTimelineRepositoryError::InvalidDetailQuery => {
            return invalid_timeline_detail_query();
        }
        SessionTimelineRepositoryError::Database(_) => "infrastructure",
        SessionTimelineRepositoryError::Corruption(_) => "fail_closed_corruption",
        SessionTimelineRepositoryError::Outbox(OutboxDispatchError::Database(_)) => {
            "infrastructure"
        }
        SessionTimelineRepositoryError::Outbox(OutboxDispatchError::Corruption(_)) => {
            "fail_closed_corruption"
        }
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

fn parse_timeline_address(value: &str) -> Result<TimelineAddress, SessionTimelineRequestError> {
    if value.is_empty()
        || !value.bytes().all(|byte| byte.is_ascii_digit())
        || value.starts_with('0')
    {
        return Err(SessionTimelineRequestError::InvalidAddress);
    }
    value
        .parse::<u64>()
        .ok()
        .and_then(std::num::NonZeroU64::new)
        .map(TimelineAddress::new)
        .ok_or(SessionTimelineRequestError::InvalidAddress)
}

fn parse_window_anchor(
    anchor: &str,
    address: Option<&str>,
) -> Result<TimelineWindowAnchor, SessionTimelineRequestError> {
    let parsed_address = || {
        address
            .ok_or(SessionTimelineRequestError::InvalidAddress)
            .and_then(parse_timeline_address)
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

fn window_dto(
    window: SessionTimelineWindow,
) -> Result<WebSessionTimelineWindow, SessionTimelineRequestError> {
    let continuation_before = match window.continuation_before {
        TimelineContinuation::Exhausted => None,
        TimelineContinuation::MoreAt(address) => Some(address_dto(address)),
    };
    let continuation_after = match window.continuation_after {
        TimelineContinuation::Exhausted => None,
        TimelineContinuation::MoreAt(address) => Some(address_dto(address)),
    };
    Ok(WebSessionTimelineWindow {
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
    })
}

fn detail_page_dto(
    page: SessionTimelineDetailPage,
) -> Result<WebSessionTimelineDetailPage, SessionTimelineRequestError> {
    Ok(WebSessionTimelineDetailPage {
        session_id: WebSessionId::from_uuid_bytes(*page.session.into_uuid().as_bytes()),
        items: page
            .items
            .into_iter()
            .map(|item| {
                Ok(WebSessionTimelineDetail {
                    address: address_dto(item.address),
                    kind: event_kind_dto(item.kind),
                    body: detail_body_dto(item.body)?,
                    projected_body_bytes: item.projected_body_bytes,
                })
            })
            .collect::<Result<Vec<_>, SessionTimelineRequestError>>()?,
        projected_body_bytes: page.projected_body_bytes,
        continuation: page.continuation.map(|continuation| match continuation {
            TimelineDetailContinuation::MoreAt(address) => WebTimelineDetailContinuation::MoreAt {
                address: address_dto(address),
            },
            TimelineDetailContinuation::MoreBody(body) => WebTimelineDetailContinuation::MoreBody {
                body: body_continuation_dto(body),
            },
        }),
    })
}

fn detail_body_dto(
    body: SessionTimelineDetailBody,
) -> Result<WebSessionTimelineDetailBody, SessionTimelineRequestError> {
    Ok(match body {
        SessionTimelineDetailBody::SessionCreated { imported_evidence } => {
            WebSessionTimelineDetailBody::SessionCreated {
                imported_evidence: imported_evidence.map(|evidence| WebTimelineImportedEvidence {
                    imported_conversation_id: web_uuid(
                        evidence.imported_conversation_id.into_uuid(),
                    ),
                    imported_entry_id: web_uuid(evidence.imported_entry_id.into_uuid()),
                    imported_position: WebU64::from_u64(evidence.imported_position),
                    relationship: match evidence.relationship {
                        ImportedSessionRelationship::Resume => {
                            WebTimelineImportedRelationship::Resume
                        }
                        ImportedSessionRelationship::Fork => WebTimelineImportedRelationship::Fork,
                    },
                }),
            }
        }
        SessionTimelineDetailBody::ModelSettings { detail } => {
            WebSessionTimelineDetailBody::ModelSettings {
                detail: model_settings_detail_dto(detail),
            }
        }
        SessionTimelineDetailBody::UserInput {
            turn_id,
            text,
            attachments,
        } => WebSessionTimelineDetailBody::UserInput {
            turn_id: web_uuid(turn_id.into_uuid()),
            text: text_excerpt_dto(text),
            attachments: attachments
                .into_iter()
                .map(|reference| {
                    Ok(WebTimelineBlobReference {
                        blob_id: WebBlobId::from_canonical(reference.blob_id.to_string())
                            .ok_or(SessionTimelineRequestError::InvalidProjectedSessionId)?,
                        length_bytes: WebU64::from_u64(reference.length_bytes),
                        media_type: reference.media_type,
                    })
                })
                .collect::<Result<Vec<_>, SessionTimelineRequestError>>()?,
        },
        SessionTimelineDetailBody::ModelCall {
            turn_id,
            model_call_id,
            state,
            model_identity_id,
            request_context_items,
            response,
            usage,
            provider_failure_cause,
        } => WebSessionTimelineDetailBody::ModelCall {
            turn_id: web_uuid(turn_id.into_uuid()),
            model_call_id: web_uuid(model_call_id.into_uuid()),
            state: model_call_state_dto(state),
            model_identity_id: web_uuid(model_identity_id.into_uuid()),
            request_context_items: WebU64::from_u64(request_context_items),
            response: response.map(text_excerpt_dto),
            usage: WebTimelineModelUsage {
                input_tokens: usage.input_tokens.map(WebU64::from_u64),
                output_tokens: usage.output_tokens.map(WebU64::from_u64),
                cache_creation_input_tokens: usage
                    .cache_creation_input_tokens
                    .map(WebU64::from_u64),
                cache_read_input_tokens: usage.cache_read_input_tokens.map(WebU64::from_u64),
            },
            provider_failure_cause: provider_failure_cause.map(provider_failure_cause_dto),
        },
        SessionTimelineDetailBody::ToolBatch {
            turn_id,
            producing_model_call_id,
            state,
            projected_member_index,
            tools,
            goal_events,
        } => WebSessionTimelineDetailBody::ToolBatch {
            turn_id: web_uuid(turn_id.into_uuid()),
            producing_model_call_id: web_uuid(producing_model_call_id.into_uuid()),
            state: match state {
                TimelineToolBatchState::Proposed { frontier_id } => {
                    WebTimelineToolBatchState::Proposed {
                        frontier_id: web_uuid(frontier_id.into_uuid()),
                    }
                }
                TimelineToolBatchState::ResultsProjected { frontier_id } => {
                    WebTimelineToolBatchState::ResultsProjected {
                        frontier_id: web_uuid(frontier_id.into_uuid()),
                    }
                }
                TimelineToolBatchState::RecoveryRequired { attempt_id } => {
                    WebTimelineToolBatchState::RecoveryRequired {
                        tool_attempt_id: web_uuid(attempt_id.into_uuid()),
                    }
                }
            },
            projected_member_index,
            tools: tools
                .into_iter()
                .map(tool_attempt_dto)
                .collect::<Result<Vec<_>, SessionTimelineRequestError>>()?,
            goal_events: goal_events
                .into_iter()
                .map(goal_event_dto)
                .collect::<Result<Vec<_>, SessionTimelineRequestError>>()?,
        },
        SessionTimelineDetailBody::ToolApprovalDecision {
            turn_id,
            request_id,
            tool_name,
            decision,
            actor,
            rationale,
            approval_judge_escalated,
        } => WebSessionTimelineDetailBody::ToolApprovalDecision {
            turn_id: web_uuid(turn_id.into_uuid()),
            request_id: web_uuid(request_id.into_uuid()),
            tool_name: WebToolName::from_checked(tool_name.into_string()),
            decision: match decision {
                TimelineApprovalDecision::Approve => WebTimelineApprovalDecision::Approve,
                TimelineApprovalDecision::Deny => WebTimelineApprovalDecision::Deny,
            },
            actor: match actor {
                TimelineApprovalActor::Policy => WebTimelineApprovalActor::Policy {},
                TimelineApprovalActor::User { command_id } => WebTimelineApprovalActor::User {
                    command_id: web_uuid(command_id.into_uuid()),
                },
                TimelineApprovalActor::Delegate {
                    model_selection_id,
                    model_call_id,
                } => WebTimelineApprovalActor::Delegate {
                    model_selection_id: web_uuid(model_selection_id.into_uuid()),
                    model_call_id: web_uuid(model_call_id.into_uuid()),
                },
            },
            rationale: rationale.map(text_excerpt_dto),
            approval_judge_escalated,
        },
        SessionTimelineDetailBody::GoalEvent { turn_id, event } => {
            WebSessionTimelineDetailBody::GoalEvent {
                turn_id: web_uuid(turn_id.into_uuid()),
                event: goal_event_dto(event)?,
            }
        }
        SessionTimelineDetailBody::ContextCompaction {
            compaction_id,
            model_call_id,
            through_position,
            summary_entry_id,
            result_frontier_id,
            summary,
        } => WebSessionTimelineDetailBody::ContextCompaction {
            compaction_id: web_uuid(compaction_id.into_uuid()),
            model_call_id: web_uuid(model_call_id.into_uuid()),
            through_position: WebU64::from_u64(through_position),
            summary_entry_id: web_uuid(summary_entry_id.into_uuid()),
            result_frontier_id: web_uuid(result_frontier_id.into_uuid()),
            summary: text_excerpt_dto(summary),
        },
        SessionTimelineDetailBody::TurnLifecycle {
            turn_id,
            lifecycle,
            cause_code,
        } => WebSessionTimelineDetailBody::TurnLifecycle {
            turn_id: web_uuid(turn_id.into_uuid()),
            lifecycle: match lifecycle {
                TimelineTurnLifecycleKind::Activated => WebTimelineTurnLifecycleKind::Activated,
                TimelineTurnLifecycleKind::Terminalized => {
                    WebTimelineTurnLifecycleKind::Terminalized
                }
            },
            cause_code,
        },
        SessionTimelineDetailBody::Reconciliation {
            turn_id,
            operation,
            terminal_frontier_id,
            attempt_count,
            exhausted,
            operator_required,
            cause_code,
        } => {
            let operation = match operation {
                TimelineReconciliationOperation::ModelCall(call) => {
                    WebTimelineReconciliationOperation::ModelCall {
                        model_call_id: web_uuid(call.into_uuid()),
                    }
                }
                TimelineReconciliationOperation::ToolAttempt(attempt) => {
                    WebTimelineReconciliationOperation::ToolAttempt {
                        tool_attempt_id: web_uuid(attempt.into_uuid()),
                    }
                }
            };
            WebSessionTimelineDetailBody::Reconciliation {
                turn_id: web_uuid(turn_id.into_uuid()),
                operation,
                terminal_frontier_id: web_uuid(terminal_frontier_id.into_uuid()),
                attempt_count: WebU64::from_u64(attempt_count),
                exhausted,
                operator_required,
                cause_code,
            }
        }
        SessionTimelineDetailBody::Runner {
            runner_id,
            placement_revision,
            sandbox_posture,
            working_directory,
            state,
        } => WebSessionTimelineDetailBody::Runner {
            runner_id: web_uuid(runner_id.into_uuid()),
            placement_revision: web_positive(placement_revision)?,
            sandbox_posture: match sandbox_posture {
                TimelineRunnerSandboxPosture::Unsandboxed => {
                    WebTimelineRunnerSandboxPosture::Unsandboxed
                }
                TimelineRunnerSandboxPosture::Sandboxed => {
                    WebTimelineRunnerSandboxPosture::Sandboxed
                }
            },
            working_directory: working_directory.map(WebRunnerWorkingDirectory::from_checked),
            state: match state {
                TimelineRunnerState::Pinned => WebTimelineRunnerState::Pinned,
                TimelineRunnerState::Suspect => WebTimelineRunnerState::Suspect,
                TimelineRunnerState::Connected => WebTimelineRunnerState::Connected,
                TimelineRunnerState::RunnerLostBeforePin => {
                    WebTimelineRunnerState::RunnerLostBeforePin
                }
                TimelineRunnerState::RunnerLost => WebTimelineRunnerState::RunnerLost,
                TimelineRunnerState::Replaced => WebTimelineRunnerState::Replaced,
                TimelineRunnerState::WorkingDirectoryChanged => {
                    WebTimelineRunnerState::WorkingDirectoryChanged
                }
                TimelineRunnerState::Abandoned => WebTimelineRunnerState::Abandoned,
            },
        },
        SessionTimelineDetailBody::Delegation(detail) => WebSessionTimelineDetailBody::Delegation {
            detail: delegation_detail_dto(detail)?,
        },
    })
}

fn goal_event_dto(
    event: TimelineGoalEvent,
) -> Result<WebTimelineGoalEvent, SessionTimelineRequestError> {
    let reason_dto = |reason| match reason {
        TimelineGoalBlockedReason::UserInputRequired => {
            WebTimelineGoalBlockedReason::UserInputRequired
        }
        TimelineGoalBlockedReason::ExternalChangeRequired => {
            WebTimelineGoalBlockedReason::ExternalChangeRequired
        }
        TimelineGoalBlockedReason::AuthorizationRequired => {
            WebTimelineGoalBlockedReason::AuthorizationRequired
        }
        TimelineGoalBlockedReason::ExecutionFailure => {
            WebTimelineGoalBlockedReason::ExecutionFailure
        }
    };
    Ok(match event {
        TimelineGoalEvent::Commissioned { generation, text } => {
            WebTimelineGoalEvent::Commissioned {
                generation: web_positive(generation)?,
                text: text_excerpt_dto(text),
            }
        }
        TimelineGoalEvent::Blocked {
            generation,
            reason,
            text,
        } => WebTimelineGoalEvent::Blocked {
            generation: web_positive(generation)?,
            reason: reason_dto(reason),
            text: text_excerpt_dto(text),
        },
        TimelineGoalEvent::Resumed { generation, text } => WebTimelineGoalEvent::Resumed {
            generation: web_positive(generation)?,
            text: text.map(text_excerpt_dto),
        },
        TimelineGoalEvent::Achieved { generation, text } => WebTimelineGoalEvent::Achieved {
            generation: web_positive(generation)?,
            text: text_excerpt_dto(text),
        },
        TimelineGoalEvent::UserStopped { generation } => WebTimelineGoalEvent::UserStopped {
            generation: web_positive(generation)?,
        },
        TimelineGoalEvent::Superseded { generation, text } => WebTimelineGoalEvent::Superseded {
            generation: web_positive(generation)?,
            text: text_excerpt_dto(text),
        },
    })
}

fn model_settings_detail_dto(
    detail: TimelineModelSettingsDetail,
) -> WebTimelineModelSettingsDetail {
    match detail {
        TimelineModelSettingsDetail::SessionDefaultsChanged {
            command_id,
            prior_defaults_version,
            installed_defaults_version,
            prior_model,
            installed_model,
            prior_settings,
            installed_settings,
            caller_override,
            adjustments,
        } => WebTimelineModelSettingsDetail::SessionDefaultsChanged {
            command_id: web_uuid(command_id.into_uuid()),
            prior_defaults_version: WebU64::from_u64(prior_defaults_version.as_u64()),
            installed_defaults_version: WebU64::from_u64(installed_defaults_version.as_u64()),
            prior_model: model_selection_request_dto(prior_model),
            installed_model: model_selection_request_dto(installed_model),
            prior_settings: model_settings_snapshot_dto(prior_settings),
            installed_settings: model_settings_snapshot_dto(installed_settings),
            caller_override: model_settings_overlay_dto(caller_override),
            adjustments: adjustments
                .into_iter()
                .map(model_change_adjustment_dto)
                .collect(),
        },
        TimelineModelSettingsDetail::TurnResolved {
            accepted_input_id,
            turn_id,
            defaults_version,
            selection,
            per_call_override,
            settings,
            adjusted_from_selection_id,
            adjustments,
        } => WebTimelineModelSettingsDetail::TurnResolved {
            accepted_input_id: web_uuid(accepted_input_id.into_uuid()),
            turn_id: web_uuid(turn_id.into_uuid()),
            defaults_version: WebU64::from_u64(defaults_version.as_u64()),
            requested_model: frozen_model_selection_dto(selection),
            selected_direct_id: web_uuid(selection.selected_direct().into_uuid()),
            per_call_override: model_settings_overlay_dto(per_call_override),
            settings: model_settings_snapshot_dto(settings),
            adjusted_from_selection_id: adjusted_from_selection_id
                .map(|selection| web_uuid(selection.into_uuid())),
            adjustments: adjustments
                .into_iter()
                .map(model_change_adjustment_dto)
                .collect(),
        },
    }
}

fn model_selection_request_dto(
    selection: signalbox_domain::ModelSelectionRequest,
) -> WebTimelineModelSelection {
    match selection {
        signalbox_domain::ModelSelectionRequest::Direct(selection) => {
            WebTimelineModelSelection::Direct {
                selection_id: web_uuid(selection.into_uuid()),
            }
        }
        signalbox_domain::ModelSelectionRequest::Alias(alias) => WebTimelineModelSelection::Alias {
            alias_id: web_uuid(alias.into_uuid()),
        },
    }
}

fn frozen_model_selection_dto(
    selection: signalbox_domain::FrozenModelSelection,
) -> WebTimelineModelSelection {
    match selection {
        signalbox_domain::FrozenModelSelection::Direct(selection) => {
            WebTimelineModelSelection::Direct {
                selection_id: web_uuid(selection.into_uuid()),
            }
        }
        signalbox_domain::FrozenModelSelection::FrozenAlias { alias, .. } => {
            WebTimelineModelSelection::Alias {
                alias_id: web_uuid(alias.into_uuid()),
            }
        }
    }
}

fn model_settings_snapshot_dto(
    settings: signalbox_domain::ValidatedModelSettings,
) -> WebTimelineModelSettingsSnapshot {
    let precedence = settings.precedence();
    let resolved = settings.resolved();
    let effective = resolved.effective();
    WebTimelineModelSettingsSnapshot {
        precedence: WebTimelineModelSettingsPrecedence {
            per_call: model_settings_overlay_dto(precedence.per_call()),
            session: model_settings_overlay_dto(precedence.session()),
            profile: model_settings_overlay_dto(precedence.profile()),
            global_default: model_settings_overlay_dto(precedence.global_default()),
        },
        effective: WebTimelineEffectiveModelSettings {
            reasoning_level: effective.reasoning_level().map(reasoning_level_dto),
            fast_mode: fast_mode_dto(effective.fast_mode()),
            service_tier: effective.service_tier().map(service_tier_dto),
        },
        reasoning_source: resolved.reasoning_source().map(model_setting_source_dto),
        fast_mode_source: resolved.fast_mode_source().map(model_setting_source_dto),
        service_tier_source: resolved.service_tier_source().map(model_setting_source_dto),
        validated_for_selection_id: settings
            .validated_for()
            .map(|selection| web_uuid(selection.into_uuid())),
    }
}

fn model_settings_overlay_dto(
    overlay: signalbox_domain::ModelSettingsOverlay,
) -> WebTimelineModelSettingsOverlay {
    WebTimelineModelSettingsOverlay {
        reasoning_level: setting_overlay_dto(overlay.reasoning_level(), reasoning_level_dto),
        fast_mode: match overlay.fast_mode() {
            signalbox_domain::FastModeOverlay::Inherit => WebTimelineFastModeOverlay::Inherit,
            signalbox_domain::FastModeOverlay::Value(value) => {
                WebTimelineFastModeOverlay::Value(fast_mode_dto(value))
            }
        },
        service_tier: setting_overlay_dto(overlay.service_tier(), service_tier_dto),
    }
}

fn setting_overlay_dto<DomainT, WebT>(
    value: signalbox_domain::SettingOverlay<DomainT>,
    map: impl FnOnce(DomainT) -> WebT,
) -> WebTimelineSettingOverlay<WebT> {
    match value {
        signalbox_domain::SettingOverlay::Inherit => WebTimelineSettingOverlay::Inherit,
        signalbox_domain::SettingOverlay::ProviderDefault => {
            WebTimelineSettingOverlay::ProviderDefault
        }
        signalbox_domain::SettingOverlay::Value(value) => {
            WebTimelineSettingOverlay::Value(map(value))
        }
    }
}

const fn reasoning_level_dto(value: signalbox_domain::ReasoningLevel) -> WebTimelineReasoningLevel {
    match value {
        signalbox_domain::ReasoningLevel::None => WebTimelineReasoningLevel::None,
        signalbox_domain::ReasoningLevel::Minimal => WebTimelineReasoningLevel::Minimal,
        signalbox_domain::ReasoningLevel::Low => WebTimelineReasoningLevel::Low,
        signalbox_domain::ReasoningLevel::Medium => WebTimelineReasoningLevel::Medium,
        signalbox_domain::ReasoningLevel::High => WebTimelineReasoningLevel::High,
        signalbox_domain::ReasoningLevel::XHigh => WebTimelineReasoningLevel::Xhigh,
        signalbox_domain::ReasoningLevel::Max => WebTimelineReasoningLevel::Max,
        signalbox_domain::ReasoningLevel::Ultra => WebTimelineReasoningLevel::Ultra,
    }
}

const fn fast_mode_dto(value: signalbox_domain::FastMode) -> WebTimelineFastMode {
    match value {
        signalbox_domain::FastMode::Disabled => WebTimelineFastMode::Disabled,
        signalbox_domain::FastMode::Enabled => WebTimelineFastMode::Enabled,
    }
}

const fn model_setting_source_dto(
    value: signalbox_domain::ModelSettingSource,
) -> WebTimelineModelSettingSource {
    match value {
        signalbox_domain::ModelSettingSource::PerCall => WebTimelineModelSettingSource::PerCall,
        signalbox_domain::ModelSettingSource::Session => WebTimelineModelSettingSource::Session,
        signalbox_domain::ModelSettingSource::Profile => WebTimelineModelSettingSource::Profile,
        signalbox_domain::ModelSettingSource::GlobalDefault => {
            WebTimelineModelSettingSource::GlobalDefault
        }
    }
}

const fn service_tier_dto(value: signalbox_domain::ServiceTier) -> WebTimelineServiceTier {
    match value {
        signalbox_domain::ServiceTier::Anthropic(value) => {
            WebTimelineServiceTier::Anthropic(match value {
                signalbox_domain::AnthropicServiceTier::Auto => {
                    signalbox_web_contract::WebTimelineAnthropicServiceTier::Auto
                }
                signalbox_domain::AnthropicServiceTier::StandardOnly => {
                    signalbox_web_contract::WebTimelineAnthropicServiceTier::StandardOnly
                }
            })
        }
        signalbox_domain::ServiceTier::OpenAi(value) => {
            WebTimelineServiceTier::OpenAi(match value {
                signalbox_domain::OpenAiServiceTier::Auto => {
                    signalbox_web_contract::WebTimelineOpenAiServiceTier::Auto
                }
                signalbox_domain::OpenAiServiceTier::Default => {
                    signalbox_web_contract::WebTimelineOpenAiServiceTier::Default
                }
                signalbox_domain::OpenAiServiceTier::Flex => {
                    signalbox_web_contract::WebTimelineOpenAiServiceTier::Flex
                }
                signalbox_domain::OpenAiServiceTier::Scale => {
                    signalbox_web_contract::WebTimelineOpenAiServiceTier::Scale
                }
                signalbox_domain::OpenAiServiceTier::Priority => {
                    signalbox_web_contract::WebTimelineOpenAiServiceTier::Priority
                }
                signalbox_domain::OpenAiServiceTier::Fast => {
                    signalbox_web_contract::WebTimelineOpenAiServiceTier::Fast
                }
            })
        }
        signalbox_domain::ServiceTier::CodexCli(value) => {
            WebTimelineServiceTier::CodexCli(match value {
                signalbox_domain::CodexCliServiceTier::Default => {
                    signalbox_web_contract::WebTimelineCodexCliServiceTier::Default
                }
                signalbox_domain::CodexCliServiceTier::Priority => {
                    signalbox_web_contract::WebTimelineCodexCliServiceTier::Priority
                }
                signalbox_domain::CodexCliServiceTier::Flex => {
                    signalbox_web_contract::WebTimelineCodexCliServiceTier::Flex
                }
            })
        }
    }
}

fn model_change_adjustment_dto(
    adjustment: signalbox_domain::ModelChangeAdjustment,
) -> WebTimelineModelChangeAdjustment {
    match adjustment {
        signalbox_domain::ModelChangeAdjustment::ReasoningLevelClamped { from, to } => {
            WebTimelineModelChangeAdjustment::ReasoningLevelClamped {
                from: reasoning_level_dto(from),
                to: reasoning_level_dto(to),
            }
        }
        signalbox_domain::ModelChangeAdjustment::ReasoningLevelCleared { from } => {
            WebTimelineModelChangeAdjustment::ReasoningLevelCleared {
                from: reasoning_level_dto(from),
            }
        }
        signalbox_domain::ModelChangeAdjustment::FastModeDisabled => {
            WebTimelineModelChangeAdjustment::FastModeDisabled {}
        }
        signalbox_domain::ModelChangeAdjustment::ServiceTierCleared { from } => {
            WebTimelineModelChangeAdjustment::ServiceTierCleared {
                from: service_tier_dto(from),
            }
        }
    }
}

fn delegation_policy_dto(policy: TimelineDelegationPolicy) -> WebTimelineDelegationPolicy {
    match policy {
        TimelineDelegationPolicy::Background => WebTimelineDelegationPolicy::Background,
        TimelineDelegationPolicy::Bound {
            on_parent_stopped,
            on_parent_cancelled,
        } => WebTimelineDelegationPolicy::Bound {
            on_parent_stopped: bound_child_action_dto(on_parent_stopped),
            on_parent_cancelled: bound_child_action_dto(on_parent_cancelled),
        },
    }
}

fn delegation_detail_dto(
    detail: TimelineDelegationDetail,
) -> Result<WebTimelineDelegationDetail, SessionTimelineRequestError> {
    Ok(match detail {
        TimelineDelegationDetail::ChildSpawned {
            relationship_id,
            child,
            policy,
        } => WebTimelineDelegationDetail::ChildSpawned {
            relationship_id: web_uuid(relationship_id.into_uuid()),
            child_session_id: WebSessionId::from_uuid_bytes(*child.into_uuid().as_bytes()),
            policy: delegation_policy_dto(policy),
        },
        TimelineDelegationDetail::ChildWaiting {
            relationship_id,
            child,
            awaiting_request,
            mode,
        } => WebTimelineDelegationDetail::ChildWaiting {
            relationship_id: web_uuid(relationship_id.into_uuid()),
            child_session_id: WebSessionId::from_uuid_bytes(*child.into_uuid().as_bytes()),
            awaiting_request_id: web_uuid(awaiting_request.into_uuid()),
            mode: match mode {
                TimelineDelegationWaitMode::Foreground => WebTimelineDelegationWaitMode::Foreground,
                TimelineDelegationWaitMode::Background => WebTimelineDelegationWaitMode::Background,
            },
        },
        TimelineDelegationDetail::ChildLifecycleDisposition {
            relationship_id,
            child,
            event_ordinal,
            outcome,
            reason,
            provenance,
        } => WebTimelineDelegationDetail::ChildLifecycleDisposition {
            relationship_id: web_uuid(relationship_id.into_uuid()),
            child_session_id: WebSessionId::from_uuid_bytes(*child.into_uuid().as_bytes()),
            event_ordinal: web_positive(event_ordinal)?,
            outcome: delegation_outcome_dto(outcome),
            reason: delegation_reason_dto(reason),
            provenance: delegation_provenance_dto(provenance)?,
        },
        TimelineDelegationDetail::ChildResult {
            relationship_id,
            child,
            outcome,
            reason,
            provenance,
            content,
        } => WebTimelineDelegationDetail::ChildResult {
            relationship_id: web_uuid(relationship_id.into_uuid()),
            child_session_id: WebSessionId::from_uuid_bytes(*child.into_uuid().as_bytes()),
            outcome: delegation_outcome_dto(outcome),
            reason: delegation_reason_dto(reason),
            provenance: delegation_provenance_dto(provenance)?,
            content: content.map(text_excerpt_dto),
        },
        TimelineDelegationDetail::SessionMessage {
            relationship_id,
            message,
            sender,
            recipient,
            message_ordinal,
            delivery_sequence,
            content,
        } => WebTimelineDelegationDetail::SessionMessage {
            relationship_id: web_uuid(relationship_id.into_uuid()),
            message_id: web_uuid(message.into_uuid()),
            sender_session_id: WebSessionId::from_uuid_bytes(*sender.into_uuid().as_bytes()),
            recipient_session_id: WebSessionId::from_uuid_bytes(*recipient.into_uuid().as_bytes()),
            message_ordinal: web_positive(message_ordinal)?,
            delivery_sequence: web_positive(delivery_sequence)?,
            content: text_excerpt_dto(content),
        },
        TimelineDelegationDetail::ResultWake {
            relationship_id,
            awaiting_request,
        } => WebTimelineDelegationDetail::ResultWake {
            relationship_id: web_uuid(relationship_id.into_uuid()),
            awaiting_request_id: awaiting_request.map(|request| web_uuid(request.into_uuid())),
        },
        TimelineDelegationDetail::MessageWake {
            relationship_id,
            message,
        } => WebTimelineDelegationDetail::MessageWake {
            relationship_id: web_uuid(relationship_id.into_uuid()),
            message_id: web_uuid(message.into_uuid()),
        },
    })
}

const fn delegation_outcome_dto(
    outcome: TimelineDelegationOutcome,
) -> WebTimelineDelegationOutcome {
    match outcome {
        TimelineDelegationOutcome::ResultReturned => WebTimelineDelegationOutcome::ResultReturned,
        TimelineDelegationOutcome::ChildFailed => WebTimelineDelegationOutcome::ChildFailed,
        TimelineDelegationOutcome::ChildStopped => WebTimelineDelegationOutcome::ChildStopped,
        TimelineDelegationOutcome::ChildCancelled => WebTimelineDelegationOutcome::ChildCancelled,
        TimelineDelegationOutcome::ContinueRunning => WebTimelineDelegationOutcome::ContinueRunning,
        TimelineDelegationOutcome::AlreadyTerminal => WebTimelineDelegationOutcome::AlreadyTerminal,
    }
}

const fn delegation_reason_dto(reason: TimelineDelegationReason) -> WebTimelineDelegationReason {
    match reason {
        TimelineDelegationReason::ChildCompleted => WebTimelineDelegationReason::ChildCompleted,
        TimelineDelegationReason::ChildExecutionFailed => {
            WebTimelineDelegationReason::ChildExecutionFailed
        }
        TimelineDelegationReason::ChildResultUnavailable => {
            WebTimelineDelegationReason::ChildResultUnavailable
        }
        TimelineDelegationReason::ChildCancelled => WebTimelineDelegationReason::ChildCancelled,
        TimelineDelegationReason::ParentStoppedWithDescendants => {
            WebTimelineDelegationReason::ParentStoppedWithDescendants
        }
        TimelineDelegationReason::ParentCancelledWithDescendants => {
            WebTimelineDelegationReason::ParentCancelledWithDescendants
        }
    }
}

fn delegation_provenance_dto(
    provenance: TimelineDelegationProvenance,
) -> Result<WebTimelineDelegationProvenance, SessionTimelineRequestError> {
    Ok(match provenance {
        TimelineDelegationProvenance::ChildTurn { session, turn } => {
            WebTimelineDelegationProvenance::ChildTurn {
                session_id: WebSessionId::from_uuid_bytes(*session.into_uuid().as_bytes()),
                turn_id: web_uuid(turn.into_uuid()),
            }
        }
        TimelineDelegationProvenance::ParentTurnCommand {
            session,
            turn,
            command,
        } => WebTimelineDelegationProvenance::ParentTurnCommand {
            session_id: WebSessionId::from_uuid_bytes(*session.into_uuid().as_bytes()),
            turn_id: web_uuid(turn.into_uuid()),
            command_id: web_uuid(command.into_uuid()),
        },
        TimelineDelegationProvenance::ParentGoalCommand {
            session,
            goal_generation,
            command,
        } => WebTimelineDelegationProvenance::ParentGoalCommand {
            session_id: WebSessionId::from_uuid_bytes(*session.into_uuid().as_bytes()),
            goal_generation: web_positive(goal_generation)?,
            command_id: web_uuid(command.into_uuid()),
        },
    })
}

const fn bound_child_action_dto(action: TimelineBoundChildAction) -> WebTimelineBoundChildAction {
    match action {
        TimelineBoundChildAction::KeepRunning => WebTimelineBoundChildAction::KeepRunning,
        TimelineBoundChildAction::Stop => WebTimelineBoundChildAction::Stop,
        TimelineBoundChildAction::Cancel => WebTimelineBoundChildAction::Cancel,
    }
}

fn tool_attempt_dto(
    attempt: TimelineToolAttempt,
) -> Result<WebTimelineToolAttempt, SessionTimelineRequestError> {
    let evidence = match (
        attempt.attempt_id,
        attempt.effect_posture,
        attempt.state,
        attempt.cause_code.as_deref(),
    ) {
        (None, None, None, None) => WebTimelineToolAttemptEvidence::RequestOnly {},
        (Some(attempt_id), Some(effect_posture), Some(state), cause) => {
            WebTimelineToolAttemptEvidence::PhysicalAttempt {
                attempt_id: web_uuid(attempt_id.into_uuid()),
                result: attempt.result.map(text_excerpt_dto),
                failure: attempt.failure.map(text_excerpt_dto),
                result_present: attempt.has_result,
                failure_present: attempt.has_failure,
                effect_posture: match effect_posture {
                    TimelineToolEffectPosture::EffectFree => {
                        WebTimelineToolEffectPosture::EffectFree
                    }
                    TimelineToolEffectPosture::ExternalEffect => {
                        WebTimelineToolEffectPosture::ExternalEffect
                    }
                },
                sandbox_posture: attempt.sandbox_posture.map(|posture| match posture {
                    TimelineToolSandboxPosture::Unsandboxed => {
                        WebTimelineToolSandboxPosture::Unsandboxed
                    }
                    TimelineToolSandboxPosture::Sandboxed => {
                        WebTimelineToolSandboxPosture::Sandboxed
                    }
                }),
                state: match state {
                    TimelineToolState::Prepared => WebTimelineToolState::Prepared,
                    TimelineToolState::InFlight => WebTimelineToolState::InFlight,
                    TimelineToolState::AwaitingChild => WebTimelineToolState::AwaitingChild,
                    TimelineToolState::Completed => WebTimelineToolState::Completed,
                    TimelineToolState::KnownFailed => WebTimelineToolState::KnownFailed,
                    TimelineToolState::Ambiguous => WebTimelineToolState::Ambiguous,
                },
                cause: match cause {
                    None => None,
                    Some("unknown_tool") => Some(WebTimelineToolFailureCause::UnknownTool),
                    Some("invalid_arguments") => {
                        Some(WebTimelineToolFailureCause::InvalidArguments)
                    }
                    Some("execution_failed") => Some(WebTimelineToolFailureCause::ExecutionFailed),
                    Some("result_too_large") => Some(WebTimelineToolFailureCause::ResultTooLarge),
                    Some("crash_lost") => Some(WebTimelineToolFailureCause::CrashLost),
                    Some(_) => {
                        return Err(SessionTimelineRequestError::InvalidProjectedToolAttempt);
                    }
                },
            }
        }
        _ => return Err(SessionTimelineRequestError::InvalidProjectedToolAttempt),
    };
    Ok(WebTimelineToolAttempt {
        request_id: web_uuid(attempt.request_id.into_uuid()),
        tool_name: WebToolName::from_checked(attempt.tool_name.into_string()),
        arguments: attempt.arguments.map(text_excerpt_dto),
        approval_posture: match attempt.approval_posture {
            TimelineToolApprovalPosture::Auto => WebTimelineToolApprovalPosture::Auto,
            TimelineToolApprovalPosture::Delegated => WebTimelineToolApprovalPosture::Delegated,
            TimelineToolApprovalPosture::Human => WebTimelineToolApprovalPosture::Human,
        },
        approval_judge_escalated: attempt.approval_judge_escalated,
        operator_required: attempt.operator_required,
        evidence,
    })
}

fn model_call_state_dto(state: TimelineModelCallState) -> WebTimelineModelCallState {
    match state {
        TimelineModelCallState::Prepared => WebTimelineModelCallState::Prepared {},
        TimelineModelCallState::InFlight => WebTimelineModelCallState::InFlight {},
        TimelineModelCallState::CancellationRequested => {
            WebTimelineModelCallState::CancellationRequested {}
        }
        TimelineModelCallState::Terminal(disposition) => WebTimelineModelCallState::Terminal {
            disposition: match disposition {
                TimelineModelCallDisposition::Completed => {
                    WebTimelineModelCallDisposition::Completed
                }
                TimelineModelCallDisposition::KnownFailed => {
                    WebTimelineModelCallDisposition::KnownFailed
                }
                TimelineModelCallDisposition::Refused => WebTimelineModelCallDisposition::Refused,
                TimelineModelCallDisposition::Cancelled => {
                    WebTimelineModelCallDisposition::Cancelled
                }
                TimelineModelCallDisposition::Ambiguous => {
                    WebTimelineModelCallDisposition::Ambiguous
                }
            },
        },
    }
}

fn provider_failure_cause_dto(
    cause: ProviderModelCallFailureCause,
) -> WebProviderModelCallFailureCause {
    match cause {
        ProviderModelCallFailureCause::CredentialRejected => {
            WebProviderModelCallFailureCause::CredentialRejected
        }
        ProviderModelCallFailureCause::PermissionDenied => {
            WebProviderModelCallFailureCause::PermissionDenied
        }
        ProviderModelCallFailureCause::InvalidRequest => {
            WebProviderModelCallFailureCause::InvalidRequest
        }
        ProviderModelCallFailureCause::TargetNotFound => {
            WebProviderModelCallFailureCause::TargetNotFound
        }
        ProviderModelCallFailureCause::RequestTooLarge => {
            WebProviderModelCallFailureCause::RequestTooLarge
        }
        ProviderModelCallFailureCause::RateLimited => WebProviderModelCallFailureCause::RateLimited,
        ProviderModelCallFailureCause::QuotaExhausted => {
            WebProviderModelCallFailureCause::QuotaExhausted
        }
        ProviderModelCallFailureCause::Overloaded => WebProviderModelCallFailureCause::Overloaded,
        ProviderModelCallFailureCause::ProviderInternal => {
            WebProviderModelCallFailureCause::ProviderInternal
        }
        ProviderModelCallFailureCause::Unrecognized => {
            WebProviderModelCallFailureCause::Unrecognized
        }
    }
}

fn web_uuid(value: uuid::Uuid) -> WebSessionId {
    WebSessionId::from_uuid_bytes(*value.as_bytes())
}

fn web_positive(value: u64) -> Result<WebPositiveU64, SessionTimelineRequestError> {
    std::num::NonZeroU64::new(value)
        .map(WebPositiveU64::from_nonzero)
        .ok_or(SessionTimelineRequestError::InvalidProjectedOrdinal)
}

fn text_excerpt_dto(excerpt: TimelineTextExcerpt) -> WebTimelineTextExcerpt {
    WebTimelineTextExcerpt {
        text: excerpt.text,
        offset_bytes: WebU64::from_u64(excerpt.offset_bytes),
        total_bytes: WebU64::from_u64(excerpt.total_bytes),
        continuation: excerpt.continuation.map(body_continuation_dto),
    }
}

fn body_continuation_dto(continuation: TimelineBodyContinuation) -> WebTimelineBodyContinuation {
    WebTimelineBodyContinuation {
        address: address_dto(continuation.address),
        field: match continuation.field {
            TimelineBodyField::InputText => WebTimelineBodyField::InputText,
            TimelineBodyField::ModelResponse => WebTimelineBodyField::ModelResponse,
            TimelineBodyField::ToolArguments => WebTimelineBodyField::ToolArguments,
            TimelineBodyField::ToolResult => WebTimelineBodyField::ToolResult,
            TimelineBodyField::ToolFailure => WebTimelineBodyField::ToolFailure,
            TimelineBodyField::ApprovalRationale => WebTimelineBodyField::ApprovalRationale,
            TimelineBodyField::GoalText => WebTimelineBodyField::GoalText,
            TimelineBodyField::CompactionSummary => WebTimelineBodyField::CompactionSummary,
            TimelineBodyField::DelegationContent => WebTimelineBodyField::DelegationContent,
        },
        member_index: continuation.member_index,
        offset_bytes: WebU64::from_u64(continuation.offset_bytes),
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

#[derive(Debug, Default)]
struct AttentionSnapshotQuery {
    search: Option<String>,
    required_tag: Vec<String>,
    include_archived: Option<String>,
    sort: Option<String>,
    after_session_id: Option<String>,
    after_activity_unix_microseconds: Option<String>,
}

async fn attention_snapshot(
    State(state): State<WebApiState>,
    RawQuery(query): RawQuery,
) -> Response {
    let query = match parse_attention_snapshot_query(query.as_deref()) {
        Ok(query) => query,
        Err(()) => return invalid_attention_query(),
    };
    let Some(repository) = state.attention else {
        return application_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "session_catalog_unavailable",
            "session catalog projection is not configured",
        );
    };
    let query = match parse_attention_query(query) {
        Ok(query) => query,
        Err(()) => return invalid_attention_query(),
    };
    match repository.snapshot(query).await {
        Ok(snapshot) => match attention_snapshot_dto(snapshot) {
            Ok(snapshot) => Json(snapshot).into_response(),
            Err(()) => attention_projection_error(None),
        },
        Err(error) => attention_projection_error(Some(error)),
    }
}

fn parse_attention_snapshot_query(raw: Option<&str>) -> Result<AttentionSnapshotQuery, ()> {
    let mut query = AttentionSnapshotQuery::default();
    let mut filter_bytes = 0_usize;
    for pair in raw.unwrap_or_default().split('&') {
        if pair.is_empty() {
            continue;
        }
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        let key = decode_query_component(key)?;
        let value = decode_query_component(value)?;
        match key.as_str() {
            "search" => {
                filter_bytes = filter_bytes.checked_add(value.len()).ok_or(())?;
                if filter_bytes > usize::from(max_attention_filter_utf8_bytes()) {
                    return Err(());
                }
                set_once(&mut query.search, value)?;
            }
            "required_tag" => {
                if query.required_tag.len() >= usize::from(max_attention_filter_tags()) {
                    return Err(());
                }
                filter_bytes = filter_bytes.checked_add(value.len()).ok_or(())?;
                if filter_bytes > usize::from(max_attention_filter_utf8_bytes()) {
                    return Err(());
                }
                query.required_tag.push(value);
            }
            "include_archived" => set_once(&mut query.include_archived, value)?,
            "sort" => set_once(&mut query.sort, value)?,
            "after_session_id" => set_once(&mut query.after_session_id, value)?,
            "after_activity_unix_microseconds" => {
                set_once(&mut query.after_activity_unix_microseconds, value)?;
            }
            _ => return Err(()),
        }
    }
    Ok(query)
}

fn set_once(target: &mut Option<String>, value: String) -> Result<(), ()> {
    if target.replace(value).is_some() {
        return Err(());
    }
    Ok(())
}

/// Strict `application/x-www-form-urlencoded` component decoding: `+` is a
/// space, `%XX` escapes must be complete hex pairs, and the decoded bytes must
/// be valid UTF-8. A lossy decoder would silently rewrite invalid bytes to
/// U+FFFD and execute a different exact filter than the caller sent.
fn decode_query_component(raw: &str) -> Result<String, ()> {
    let bytes = raw.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => {
                decoded.push(b' ');
                index += 1;
            }
            b'%' => {
                let high = bytes.get(index + 1).copied().and_then(hex_digit_value);
                let low = bytes.get(index + 2).copied().and_then(hex_digit_value);
                let (Some(high), Some(low)) = (high, low) else {
                    return Err(());
                };
                decoded.push(high * 16 + low);
                index += 3;
            }
            byte => {
                decoded.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8(decoded).map_err(|_| ())
}

const fn hex_digit_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Accepts only the canonical unsigned decimal spelling the contract emits:
/// digits only, no sign, and no leading zero. `u64::from_str` alone would
/// admit `+1` and `01` as extra wire spellings of one typed keyset.
fn parse_canonical_u64(value: &str) -> Result<u64, ()> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(());
    }
    if value != "0" && value.starts_with('0') {
        return Err(());
    }
    value.parse::<u64>().map_err(|_| ())
}

/// Accepts only the canonical lowercase hyphenated UUID spelling the contract
/// emits; the permissive UUID parser would also admit uppercase, braced,
/// simple, and URN spellings of the same keyset session.
fn parse_canonical_session_id(value: &str) -> Result<SessionId, ()> {
    let parsed = value.parse::<Uuid>().map_err(|_| ())?;
    if value != parsed.hyphenated().to_string() {
        return Err(());
    }
    Ok(SessionId::from_uuid(parsed))
}

fn parse_attention_query(query: AttentionSnapshotQuery) -> Result<AttentionQuery, ()> {
    let sort = match query.sort.as_deref() {
        None | Some("last_activity_descending") => AttentionSort::LastActivityDescending,
        Some("session_identity_ascending") => AttentionSort::SessionIdentityAscending,
        Some(_) => return Err(()),
    };
    let include_archived = match query.include_archived.as_deref() {
        None | Some("false") => false,
        Some("true") => true,
        Some(_) => return Err(()),
    };
    let after_session = query
        .after_session_id
        .map(|value| parse_canonical_session_id(&value))
        .transpose()?;
    let after_activity_micros = query
        .after_activity_unix_microseconds
        .map(|value| parse_canonical_u64(&value))
        .transpose()?;
    if after_activity_micros.is_some_and(|value| {
        sqlx::types::time::OffsetDateTime::from_unix_timestamp_nanos(i128::from(value) * 1_000)
            .is_err()
    }) {
        return Err(());
    }
    let after_activity = after_activity_micros
        .map(|value| {
            UNIX_EPOCH
                .checked_add(Duration::from_micros(value))
                .ok_or(())
        })
        .transpose()?;
    let continuation = match (sort, after_session, after_activity) {
        (AttentionSort::LastActivityDescending, None, None)
        | (AttentionSort::SessionIdentityAscending, None, None) => None,
        (AttentionSort::LastActivityDescending, Some(session), Some(recorded_at)) => {
            Some(AttentionContinuation::LastActivity {
                recorded_at,
                session,
            })
        }
        (AttentionSort::SessionIdentityAscending, Some(session), None) => {
            Some(AttentionContinuation::SessionIdentity(session))
        }
        _ => return Err(()),
    };
    AttentionQuery::try_new(
        query.search,
        query.required_tag,
        include_archived,
        sort,
        continuation,
    )
    .map_err(|_| ())
}

fn invalid_attention_query() -> Response {
    application_error(
        StatusCode::BAD_REQUEST,
        "invalid_session_catalog_query",
        "session catalog query parameters are malformed or outside the contract bounds",
    )
}

async fn attention_follow(State(state): State<WebApiState>) -> Response {
    let Some(repository) = state.attention else {
        return application_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "attention_projection_unavailable",
            "attention projection is not configured",
        );
    };
    let snapshot = match repository.snapshot(AttentionQuery::hot_page()).await {
        Ok(snapshot) => snapshot,
        Err(error) => return attention_projection_error(Some(error)),
    };
    let cursor = snapshot.cursor;
    let snapshot = match attention_snapshot_dto(snapshot) {
        Ok(snapshot) => snapshot,
        Err(()) => return attention_projection_error(None),
    };
    let source = stream::unfold(
        (
            repository,
            VecDeque::from([(WebAttentionStreamEvent::Snapshot { snapshot }, cursor)]),
            cursor,
            AttentionFollowDisposition::Continue,
        ),
        |(repository, mut pending, mut cursor, disposition)| async move {
            if let Some((event, emitted_cursor)) = pending.pop_front() {
                return Some((
                    event,
                    (
                        repository,
                        pending,
                        emitted_cursor,
                        AttentionFollowDisposition::Continue,
                    ),
                ));
            }
            if disposition == AttentionFollowDisposition::End {
                return None;
            }
            loop {
                tokio::time::sleep(Duration::from_millis(250)).await;
                match repository.changes_after(cursor).await {
                    Ok(AttentionChanges::Updated {
                        cursor: next,
                        summaries,
                    }) if summaries.is_empty() => {
                        cursor = next;
                    }
                    Ok(AttentionChanges::Updated {
                        cursor: next,
                        summaries,
                    }) => {
                        let summaries = summaries
                            .into_iter()
                            .map(attention_summary_dto)
                            .collect::<Result<Vec<_>, _>>()
                            .ok()?;
                        let mut updates = attention_update_events(cursor, next, summaries);
                        let (event, emitted_cursor) = updates.pop_front()?;
                        return Some((
                            event,
                            (
                                repository,
                                updates,
                                emitted_cursor,
                                AttentionFollowDisposition::Continue,
                            ),
                        ));
                    }
                    Ok(AttentionChanges::ResyncRequired { cursor: next }) => {
                        return Some((
                            WebAttentionStreamEvent::ResyncRequired {
                                cursor: WebU64::from_u64(next.value()),
                            },
                            (
                                repository,
                                VecDeque::new(),
                                next,
                                AttentionFollowDisposition::End,
                            ),
                        ));
                    }
                    Err(error) => {
                        log_attention_projection_error(&error);
                        return None;
                    }
                }
            }
        },
    );
    ndjson_response(source)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AttentionFollowDisposition {
    Continue,
    End,
}

fn attention_update_events(
    prior: signalbox_application::AttentionCursor,
    next: signalbox_application::AttentionCursor,
    summaries: Vec<WebAttentionSummary>,
) -> VecDeque<(
    WebAttentionStreamEvent,
    signalbox_application::AttentionCursor,
)> {
    let chunk_size = usize::from(max_attention_snapshot_items());
    let chunk_count = summaries.len().div_ceil(chunk_size);
    summaries
        .chunks(chunk_size)
        .enumerate()
        .map(|(index, chunk)| {
            let emitted_cursor = if index + 1 == chunk_count {
                next
            } else {
                prior
            };
            (
                WebAttentionStreamEvent::Update {
                    cursor: WebU64::from_u64(emitted_cursor.value()),
                    summaries: chunk.to_vec(),
                },
                emitted_cursor,
            )
        })
        .collect()
}

fn attention_snapshot_dto(snapshot: AttentionSnapshot) -> Result<WebAttentionSnapshot, ()> {
    let continuation = snapshot
        .continuation
        .map(|continuation| match continuation {
            AttentionContinuation::LastActivity {
                recorded_at,
                session,
            } => Ok(WebAttentionContinuation::LastActivity {
                unix_microseconds: WebU64::from_u64(
                    recorded_at
                        .duration_since(UNIX_EPOCH)
                        .map_err(|_| ())?
                        .as_micros()
                        .try_into()
                        .map_err(|_| ())?,
                ),
                session_id: WebSessionId::from_uuid_bytes(session.into_uuid().into_bytes()),
            }),
            AttentionContinuation::SessionIdentity(session) => {
                Ok(WebAttentionContinuation::SessionIdentity {
                    session_id: WebSessionId::from_uuid_bytes(session.into_uuid().into_bytes()),
                })
            }
        })
        .transpose()?;
    Ok(WebAttentionSnapshot {
        cursor: WebU64::from_u64(snapshot.cursor.value()),
        total: WebU64::from_u64(snapshot.total),
        sort: match snapshot.sort {
            AttentionSort::LastActivityDescending => WebAttentionSort::LastActivityDescending,
            AttentionSort::SessionIdentityAscending => WebAttentionSort::SessionIdentityAscending,
        },
        summaries: snapshot
            .summaries
            .into_iter()
            .map(attention_summary_dto)
            .collect::<Result<Vec<_>, _>>()?,
        continuation,
    })
}

fn attention_summary_dto(summary: AttentionSummary) -> Result<WebAttentionSummary, ()> {
    if summary
        .title_summary
        .as_ref()
        .is_some_and(|title| title.chars().count() > usize::from(max_attention_title_characters()))
    {
        return Err(());
    }
    let unix_microseconds = summary
        .last_activity
        .recorded_at
        .duration_since(UNIX_EPOCH)
        .map_err(|_| ())?
        .as_micros()
        .try_into()
        .map_err(|_| ())?;
    let goal_block = summary
        .goal_block
        .map(|goal| {
            if goal.need_summary.chars().count()
                > usize::from(max_attention_goal_summary_characters())
            {
                return Err(());
            }
            Ok(WebAttentionGoalBlock {
                generation: WebPositiveU64::from_nonzero(
                    std::num::NonZeroU64::new(goal.generation).ok_or(())?,
                ),
                reason: match goal.reason {
                    AttentionBlockedReason::UserInputRequired => {
                        WebAttentionBlockedReason::UserInputRequired
                    }
                    AttentionBlockedReason::ExternalChangeRequired => {
                        WebAttentionBlockedReason::ExternalChangeRequired
                    }
                    AttentionBlockedReason::AuthorizationRequired => {
                        WebAttentionBlockedReason::AuthorizationRequired
                    }
                    AttentionBlockedReason::ExecutionFailure => {
                        WebAttentionBlockedReason::ExecutionFailure
                    }
                },
                need_summary: goal.need_summary,
            })
        })
        .transpose()?;
    Ok(WebAttentionSummary {
        session_id: WebSessionId::from_uuid_bytes(summary.session.into_uuid().into_bytes()),
        title_summary: summary.title_summary,
        title_truncated: summary.title_truncated,
        archived: summary.archived,
        current_turn_id: summary
            .current_turn
            .map(|turn| WebTurnId::from_uuid_bytes(turn.into_uuid().into_bytes())),
        active_turn_count: WebU64::from_u64(summary.active_turn_count),
        queued_turn_count: WebU64::from_u64(summary.queued_turn_count),
        state: match summary.state {
            AttentionState::Active => WebAttentionState::Active,
            AttentionState::Queued => WebAttentionState::Queued,
            AttentionState::Blocked => WebAttentionState::Blocked,
            AttentionState::AwaitingApproval => WebAttentionState::AwaitingApproval,
            AttentionState::Ambiguous => WebAttentionState::Ambiguous,
            AttentionState::AwaitingReconciliation => WebAttentionState::AwaitingReconciliation,
            AttentionState::RunnerLost => WebAttentionState::RunnerLost,
            AttentionState::Idle => WebAttentionState::Idle,
        },
        action: summary.action.map(|action| match action {
            AttentionAction::ProvideGoalNeed => WebAttentionAction::ProvideGoalNeed,
            AttentionAction::DecideApproval => WebAttentionAction::DecideApproval,
            AttentionAction::ReconcileTurn => WebAttentionAction::ReconcileTurn,
            AttentionAction::RestoreRunner => WebAttentionAction::RestoreRunner,
        }),
        goal_block,
        judge: WebAttentionJudgeFacts {
            actionable: WebU64::from_u64(summary.judge.actionable),
            completed: WebU64::from_u64(summary.judge.completed),
            escalated: WebU64::from_u64(summary.judge.escalated),
            failed: WebU64::from_u64(summary.judge.failed),
        },
        last_activity: WebAttentionActivity {
            unix_microseconds: WebU64::from_u64(unix_microseconds),
            kind: match summary.last_activity.kind {
                AttentionActivityKind::Session => WebAttentionActivityKind::Session,
                AttentionActivityKind::Turn => WebAttentionActivityKind::Turn,
                AttentionActivityKind::Goal => WebAttentionActivityKind::Goal,
                AttentionActivityKind::ApprovalJudge => WebAttentionActivityKind::ApprovalJudge,
                AttentionActivityKind::Runner => WebAttentionActivityKind::Runner,
            },
        },
    })
}

fn attention_projection_error(error: Option<AttentionRepositoryError>) -> Response {
    if let Some(error) = error.as_ref() {
        log_attention_projection_error(error);
    }
    application_error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "attention_projection_failed",
        "the attention projection could not be read",
    )
}

fn log_attention_projection_error(error: &AttentionRepositoryError) {
    let failure_class = match error {
        AttentionRepositoryError::Database(_) => "infrastructure",
        AttentionRepositoryError::Corruption(_) => "fail_closed_corruption",
    };
    tracing::error!(failure_class, cause = %error, "attention projection read failed");
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

async fn contract_bootstrap() -> Json<WebContractBootstrap> {
    Json(WebContractBootstrap::current())
}

async fn deterministic_contract_bootstrap() -> Json<WebContractBootstrap> {
    let mut bootstrap = WebContractBootstrap::current();
    bootstrap.capabilities.bounded_session_timeline = false;
    Json(bootstrap)
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
    let body = Json(WebApiErrorResponse {
        error: WebApiError {
            kind: WebApiErrorKind::Application,
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
        time::{Duration, UNIX_EPOCH},
    };

    use axum::{
        body::{Body, Bytes},
        http::{Request, StatusCode, header},
    };
    use http_body_util::BodyExt as _;
    use signalbox_application::{
        AttentionAction, AttentionActivity, AttentionActivityKind, AttentionBlockedReason,
        AttentionContinuation, AttentionCursor, AttentionGoalBlock, AttentionJudgeFacts,
        AttentionSnapshot, AttentionSort, AttentionState, AttentionSummary, TimelineAddress,
        TimelineBodyField, TimelineDetailCursor, max_attention_change_items,
        max_attention_filter_utf8_bytes, max_attention_goal_summary_characters,
        max_attention_snapshot_items, max_attention_title_characters,
    };
    use signalbox_domain::{SessionId, TurnId};
    use signalbox_web_contract::{
        MAX_JSON_BODY_BYTES, MAX_NDJSON_ITEM_BYTES, WebAttentionStreamEvent, WebContractBootstrap,
        WebContractExample,
    };
    use sqlx::types::Uuid;
    use tokio::sync::{mpsc, watch};
    use tower::ServiceExt as _;
    use url::Url;

    use super::{
        DEFAULT_WEB_BIND_ADDRESS, TimelineDetailQuery, WebHttpConfiguration,
        WebHttpConfigurationError, WebHttpRuntime, attention_snapshot_dto,
        deterministic_test_router, ndjson_response, parse_detail_query, production_router,
    };

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

    const STATIC_INDEX: &str = "signalbox-static-build";
    const LARGE_REPRESENTATIVE_UNIX_MILLISECONDS: u64 = 9_999_999_999_999;

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
    async fn missing_detail_ceiling_uses_the_structured_error_envelope() {
        let request = Request::get(
            "/api/sessions/00000000-0000-0000-0000-000000000991/timeline/1/detail?max_items=1",
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
        assert_eq!(body["error"]["code"], "invalid_timeline_detail_limits");
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

    /// Drives a session read at the loopback gate and reports only the status.
    ///
    /// The query is deliberately malformed, which separates the gate from
    /// everything behind it: `FORBIDDEN` means the gate rejected the
    /// authority, while `BAD_REQUEST` comes from the handler and is therefore
    /// reachable only once the gate has admitted the request.
    async fn session_read_status_for_host(host: &str) -> StatusCode {
        let request = Request::get(
            "/api/sessions/00000000-0000-0000-0000-000000000991/timeline?max_items=nope",
        )
        .header(header::HOST, host)
        .body(Body::empty())
        .expect("the request is valid");
        production_router(None, None)
            .oneshot(request)
            .await
            .expect("the production router responds")
            .status()
    }

    #[tokio::test]
    async fn session_reads_admit_loopback_authorities_including_ip_literals() {
        // `127.0.0.1` is the daemon's own DEFAULT_WEB_BIND_ADDRESS, so a
        // regression that tightened this branch would 403 the default
        // deployment. `[::1]` exercises the bracket strip that precedes the
        // parse, and `127.5.6.7` covers the whole 127.0.0.0/8 loopback range
        // rather than only the canonical address.
        for host in [
            "localhost",
            "localhost:37231",
            "LocalHost",
            "127.0.0.1",
            "127.0.0.1:37231",
            "127.5.6.7",
            "[::1]",
            "[::1]:37231",
        ] {
            assert_eq!(
                session_read_status_for_host(host).await,
                StatusCode::BAD_REQUEST,
                "`{host}` is a loopback authority and must reach the handler",
            );
        }
    }

    #[tokio::test]
    async fn session_reads_reject_non_loopback_ip_literal_authorities() {
        // Every authority here parses as an address, so `is_loopback` — not
        // the `parse::<IpAddr>()` that already turns hostnames away — is what
        // has to reject them. A regression that loosened the branch to accept
        // any parseable address would expose session history to any host that
        // can reach the port.
        for host in [
            "10.0.0.5",
            "10.0.0.5:37231",
            "192.168.1.20",
            "[2001:db8::1]",
            "[2001:db8::1]:37231",
        ] {
            assert_eq!(
                session_read_status_for_host(host).await,
                StatusCode::FORBIDDEN,
                "`{host}` parses as a non-loopback address and must be rejected",
            );
        }
    }

    #[tokio::test]
    async fn session_reads_reject_authorities_that_are_neither_localhost_nor_literals() {
        for host in ["attacker.example", "localhost.attacker.example"] {
            assert_eq!(
                session_read_status_for_host(host).await,
                StatusCode::FORBIDDEN,
                "`{host}` is neither localhost nor a loopback literal",
            );
        }
    }

    #[test]
    fn detail_cursors_require_closed_fields_and_canonical_addresses() {
        let query = TimelineDetailQuery {
            max_items: Some(String::from("1")),
            max_bytes: Some(String::from("256")),
            cursor_address: Some(String::from("7")),
            cursor_field: Some(String::from("model_response")),
            cursor_member: Some(String::from("0")),
            cursor_offset: Some(String::from("31")),
        };
        let parsed = parse_detail_query(&query).expect("the closed cursor is valid");

        assert_eq!(
            parsed.1,
            Some(TimelineDetailCursor {
                address: TimelineAddress::new(
                    std::num::NonZeroU64::new(7).expect("fixture address is positive")
                ),
                field: Some(TimelineBodyField::ModelResponse),
                member_index: 0,
                offset_bytes: 31,
            })
        );
    }

    #[test]
    fn detail_cursors_accept_address_only_item_continuations() {
        let query = TimelineDetailQuery {
            max_items: Some(String::from("1")),
            max_bytes: Some(String::from("256")),
            cursor_address: Some(String::from("7")),
            cursor_field: None,
            cursor_member: None,
            cursor_offset: None,
        };
        let parsed = parse_detail_query(&query).expect("the item cursor is valid");

        assert_eq!(
            parsed.1,
            Some(TimelineDetailCursor {
                address: TimelineAddress::new(
                    std::num::NonZeroU64::new(7).expect("fixture address is positive")
                ),
                field: None,
                member_index: 0,
                offset_bytes: 0,
            })
        );
    }

    #[test]
    fn detail_cursors_reject_incomplete_body_continuations() {
        let query = TimelineDetailQuery {
            max_items: Some(String::from("1")),
            max_bytes: Some(String::from("256")),
            cursor_address: Some(String::from("7")),
            cursor_field: Some(String::from("model_response")),
            cursor_member: None,
            cursor_offset: Some(String::from("31")),
        };

        assert!(parse_detail_query(&query).is_none());
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
    async fn session_catalog_requires_projection_configuration() {
        let request = Request::get("/api/sessions")
            .header(header::HOST, "localhost")
            .body(Body::empty())
            .expect("the request is valid");
        let response = production_router(None, None)
            .oneshot(request)
            .await
            .expect("the production router responds");
        let status = response.status();
        let body: serde_json::Value = serde_json::from_slice(&response_body(response).await)
            .expect("the typed application failure is JSON");

        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["error"]["code"], "session_catalog_unavailable");
    }

    #[test]
    fn session_catalog_query_accepts_repeated_exact_tags() {
        let query =
            super::parse_attention_snapshot_query(Some("required_tag=rust&required_tag=postgres"))
                .expect("repeated exact tags are one bounded catalog filter");

        assert_eq!(query.required_tag, ["rust", "postgres"]);
    }

    #[test]
    fn session_catalog_query_rejects_the_ninth_tag_while_decoding() {
        let raw = concat!(
            "required_tag=one&required_tag=two&required_tag=three",
            "&required_tag=four&required_tag=five&required_tag=six",
            "&required_tag=seven&required_tag=eight&required_tag=nine"
        );

        assert!(super::parse_attention_snapshot_query(Some(raw)).is_err());
    }

    #[test]
    fn session_catalog_query_rejects_excess_filter_bytes_while_decoding() {
        let raw = format!(
            "search={}",
            "x".repeat(usize::from(max_attention_filter_utf8_bytes()) + 1)
        );

        assert!(super::parse_attention_snapshot_query(Some(&raw)).is_err());
    }

    #[test]
    fn session_catalog_query_rejects_noncanonical_activity_cursors() {
        for cursor in ["01", "+1", " 1", "1_0"] {
            let identity_query = super::parse_attention_snapshot_query(Some(&format!(
                "sort=last_activity_descending\
                 &after_session_id=00000000-0000-0000-0000-000000000001\
                 &after_activity_unix_microseconds={cursor}"
            )))
            .expect("the query shape itself decodes");

            assert!(super::parse_attention_query(identity_query).is_err());
        }
    }

    #[test]
    fn session_catalog_query_rejects_noncanonical_session_cursors() {
        let identity_query = super::parse_attention_snapshot_query(Some(
            "sort=session_identity_ascending\
             &after_session_id=00000000-0000-0000-0000-0000000000AB",
        ))
        .expect("the query shape itself decodes");

        assert!(super::parse_attention_query(identity_query).is_err());
    }

    #[test]
    fn session_catalog_query_rejects_invalid_percent_encoded_utf8() {
        assert!(super::parse_attention_snapshot_query(Some("search=%FF")).is_err());
    }

    #[test]
    fn session_catalog_query_rejects_incomplete_percent_escapes() {
        assert!(super::parse_attention_snapshot_query(Some("search=%F")).is_err());
        assert!(super::parse_attention_snapshot_query(Some("search=%zz")).is_err());
    }

    #[test]
    fn session_catalog_query_decodes_escapes_and_plus_exactly() {
        let query = super::parse_attention_snapshot_query(Some("search=a+b%2Bc%C3%A9"))
            .expect("valid percent-encoded UTF-8 decodes");

        assert_eq!(query.search.as_deref(), Some("a b+c\u{e9}"));
    }

    #[test]
    fn session_catalog_query_accepts_emitted_sort_tokens() {
        let activity_query =
            super::parse_attention_snapshot_query(Some("sort=last_activity_descending"))
                .expect("the emitted activity sort token has a valid query shape");
        super::parse_attention_query(activity_query)
            .expect("the emitted activity sort token round trips through the request parser");

        let identity_query =
            super::parse_attention_snapshot_query(Some("sort=session_identity_ascending"))
                .expect("the emitted identity sort token has a valid query shape");
        super::parse_attention_query(identity_query)
            .expect("the emitted identity sort token round trips through the request parser");
    }

    #[test]
    fn session_catalog_query_rejects_timestamp_outside_database_range() {
        let raw = concat!(
            "sort=last_activity_descending",
            "&after_session_id=00000000-0000-0000-0000-000000000991",
            "&after_activity_unix_microseconds=18446744073709551615"
        );
        let query = super::parse_attention_snapshot_query(Some(raw))
            .expect("the query envelope itself is syntactically valid");

        assert!(super::parse_attention_query(query).is_err());
    }

    #[tokio::test]
    async fn attention_follow_requires_projection_configuration() {
        let request = Request::get("/api/attention/follow")
            .header(header::HOST, "localhost")
            .body(Body::empty())
            .expect("the request is valid");
        let response = production_router(None, None)
            .oneshot(request)
            .await
            .expect("the production router responds");
        let status = response.status();
        let body: serde_json::Value = serde_json::from_slice(&response_body(response).await)
            .expect("the typed application failure is JSON");

        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["error"]["code"], "attention_projection_unavailable");
    }

    #[test]
    fn attention_summary_bound_is_enforced_and_maximum_snapshot_fits_one_ndjson_item() {
        let session = SessionId::from_uuid(Uuid::from_u128(u128::MAX));
        let summary = AttentionSummary {
            session,
            title_summary: Some(
                String::from("x").repeat(usize::from(max_attention_title_characters())),
            ),
            title_truncated: true,
            archived: false,
            current_turn: Some(TurnId::from_uuid(Uuid::from_u128(u128::MAX))),
            active_turn_count: u64::MAX,
            queued_turn_count: u64::MAX,
            state: AttentionState::Blocked,
            action: Some(AttentionAction::ProvideGoalNeed),
            goal_block: Some(AttentionGoalBlock {
                generation: u64::MAX,
                reason: AttentionBlockedReason::ExternalChangeRequired,
                need_summary: String::from("🦀")
                    .repeat(usize::from(max_attention_goal_summary_characters())),
            }),
            judge: AttentionJudgeFacts {
                actionable: u64::MAX,
                completed: u64::MAX,
                escalated: u64::MAX,
                failed: u64::MAX,
            },
            last_activity: AttentionActivity {
                recorded_at: UNIX_EPOCH
                    + Duration::from_millis(LARGE_REPRESENTATIVE_UNIX_MILLISECONDS),
                kind: AttentionActivityKind::ApprovalJudge,
            },
        };
        let mut oversized_summary = summary.clone();
        oversized_summary
            .goal_block
            .as_mut()
            .expect("the maximum summary carries a goal block")
            .need_summary
            .push('x');

        let mut precision_summary = summary.clone();
        precision_summary.last_activity.recorded_at = UNIX_EPOCH + Duration::from_micros(1_234_567);
        let precision_dto = super::attention_summary_dto(precision_summary)
            .expect("the exact microsecond timestamp is representable");
        assert_eq!(
            precision_dto.last_activity.unix_microseconds.as_str(),
            "1234567"
        );

        let mut escaped_summary = summary.clone();
        escaped_summary.title_summary =
            Some(String::from('\u{1}').repeat(usize::from(max_attention_title_characters())));
        escaped_summary
            .goal_block
            .as_mut()
            .expect("the maximum summary carries a goal block")
            .need_summary =
            String::from('\u{1}').repeat(usize::from(max_attention_goal_summary_characters()));
        let maximal_update_summary = super::attention_summary_dto(escaped_summary.clone())
            .expect("the maximal summary maps to the web contract");
        let updates = super::attention_update_events(
            AttentionCursor::new(7),
            AttentionCursor::new(9),
            vec![maximal_update_summary; usize::from(max_attention_change_items())],
        );
        assert_eq!(updates.len(), 8);
        assert_attention_update_chunk(&updates[0], 7);
        assert_attention_update_chunk(&updates[1], 7);
        assert_attention_update_chunk(&updates[2], 7);
        assert_attention_update_chunk(&updates[3], 7);
        assert_attention_update_chunk(&updates[4], 7);
        assert_attention_update_chunk(&updates[5], 7);
        assert_attention_update_chunk(&updates[6], 7);
        assert_attention_update_chunk(&updates[7], 9);

        let snapshot = attention_snapshot_dto(AttentionSnapshot {
            cursor: AttentionCursor::new(u64::MAX),
            total: u64::MAX,
            sort: AttentionSort::LastActivityDescending,
            summaries: vec![escaped_summary; usize::from(max_attention_snapshot_items())],
            continuation: Some(AttentionContinuation::LastActivity {
                recorded_at: UNIX_EPOCH
                    + Duration::from_millis(LARGE_REPRESENTATIVE_UNIX_MILLISECONDS),
                session,
            }),
        })
        .expect("the maximum snapshot timestamp is representable");
        let mut writer = super::NdjsonItemWriter::new();
        serde_json::to_writer(&mut writer, &WebAttentionStreamEvent::Snapshot { snapshot })
            .expect("the maximum snapshot serializes within one item");
        writer
            .write_all(b"\n")
            .expect("the NDJSON terminator fits the item");

        assert!(super::attention_summary_dto(oversized_summary).is_err());
        assert!(writer.encoded.len() <= MAX_NDJSON_ITEM_BYTES);
    }

    fn assert_attention_update_chunk(
        update: &(WebAttentionStreamEvent, AttentionCursor),
        expected_cursor: u64,
    ) {
        let mut writer = super::NdjsonItemWriter::new();
        serde_json::to_writer(&mut writer, &update.0)
            .expect("the byte-safe update chunk serializes within one item");
        writer
            .write_all(b"\n")
            .expect("the NDJSON terminator fits the update chunk");

        assert!(writer.encoded.len() <= MAX_NDJSON_ITEM_BYTES);
        assert_eq!(update.1.value(), expected_cursor);
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
