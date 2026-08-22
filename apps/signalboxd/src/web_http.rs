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
    AttentionSummary, SessionLiveActiveState, SessionLiveReconciliation,
    SessionLiveRunnerConnectionHealth, SessionLiveRunnerState, SessionLiveSnapshot,
    SessionTimelineDescriptor, SessionTimelineEventKind, SessionTimelineWindow, TimelineAddress,
    TimelineContinuation, TimelineWindowAnchor, TimelineWindowLimits,
    max_attention_goal_summary_characters, max_attention_title_characters,
};
use signalbox_domain::SessionId;
use signalbox_persistence::attention::{AttentionRepository, AttentionRepositoryError};
use signalbox_persistence::session_live::{SessionLiveRepository, SessionLiveRepositoryError};
use signalbox_persistence::session_timeline::{
    SessionTimelineRepository, SessionTimelineRepositoryError,
};
use signalbox_web_contract::{
    MAX_JSON_BODY_BYTES, MAX_NDJSON_ITEM_BYTES, WebApiError, WebApiErrorKind, WebApiErrorResponse,
    WebAttentionAction, WebAttentionActivity, WebAttentionActivityKind, WebAttentionBlockedReason,
    WebAttentionContinuation, WebAttentionGoalBlock, WebAttentionJudgeFacts, WebAttentionSnapshot,
    WebAttentionSort, WebAttentionState, WebAttentionStreamEvent, WebAttentionSummary,
    WebContractBootstrap, WebContractExample, WebSessionLiveActiveState, WebSessionLiveActiveTurn,
    WebSessionLiveReconciliation, WebSessionLiveRunner, WebSessionLiveRunnerConnectionHealth,
    WebSessionLiveRunnerState, WebSessionLiveSnapshot, WebSessionLiveStreamEvent,
    WebSessionTimelineDescriptor, WebSessionTimelineEventKind, WebSessionTimelineItem,
    WebSessionTimelineSizeFacts, WebSessionTimelineWindow, WebSessionWorkFacts, WebTimelineAddress,
    WebTimelineEventSequence, WebU64,
};
use sqlx::{PgPool, types::Uuid};
use tokio::{net::TcpListener, sync::watch};
use tower_http::services::{ServeDir, ServeFile};
use url::Url;

use crate::{ProcessMonitor, ProcessMonitorReceiveError, ProcessMonitorUpdate};

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
// numeric-bound: hard safety - leaves room for worst-case JSON escaping and the event envelope
const MAX_WEB_PROVIDER_TEXT_FRAGMENT_BYTES: usize = 8_192;

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
        monitor: ProcessMonitor,
    ) -> Result<Self, WebHttpRuntimeError> {
        let router =
            production_router_with_monitor(configuration.asset_root, Some(pool), Some(monitor));
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
    production_router_with_monitor(asset_root, pool, None)
}

fn production_router_with_monitor(
    asset_root: Option<PathBuf>,
    pool: Option<PgPool>,
    monitor: Option<ProcessMonitor>,
) -> Router {
    let state = WebApiState {
        timeline: pool.clone().map(SessionTimelineRepository::new),
        live: pool.clone().map(SessionLiveRepository::new),
        attention: pool.map(AttentionRepository::new),
        monitor,
    };
    let session_reads = Router::new()
        .route("/sessions/{session_id}", get(session_descriptor))
        .route(
            "/sessions/{session_id}/timeline",
            get(session_timeline_window),
        )
        .route("/sessions/{session_id}/live", get(session_live_snapshot))
        .route("/sessions/{session_id}/follow", get(session_live_follow))
        .route("/sessions", get(attention_snapshot))
        .route("/attention/follow", get(attention_follow))
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
    live: Option<SessionLiveRepository>,
    attention: Option<AttentionRepository>,
    monitor: Option<ProcessMonitor>,
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
        session_id: descriptor.session.into_uuid().to_string(),
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
        session_id: window.session.into_uuid().to_string(),
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

async fn session_live_snapshot(
    State(state): State<WebApiState>,
    Path(session_id): Path<String>,
) -> Response {
    let session = match parse_session_id(&session_id) {
        Ok(session) => session,
        Err(error) => return error.into_response(),
    };
    let Some(repository) = state.live else {
        return live_projection_unavailable();
    };
    match repository.read_live_snapshot(session).await {
        Ok(Some(snapshot)) => Json(live_snapshot_dto(snapshot)).into_response(),
        Ok(None) => application_error(
            StatusCode::NOT_FOUND,
            "session_not_found",
            "the requested session does not exist",
        ),
        Err(error) => live_projection_error(error),
    }
}

async fn session_live_follow(
    State(state): State<WebApiState>,
    Path(session_id): Path<String>,
) -> Response {
    let session = match parse_session_id(&session_id) {
        Ok(session) => session,
        Err(error) => return error.into_response(),
    };
    let Some(repository) = state.live else {
        return live_projection_unavailable();
    };
    let Some(monitor) = state.monitor else {
        return application_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "session_monitor_unavailable",
            "the live session monitor is not configured",
        );
    };
    // Subscribe before the repeatable-read snapshot so every update after its
    // cursor is either observed or converted into an explicit resync.
    let subscription = monitor.subscribe();
    let snapshot = match repository.read_live_snapshot(session).await {
        Ok(Some(snapshot)) => snapshot,
        Ok(None) => {
            return application_error(
                StatusCode::NOT_FOUND,
                "session_not_found",
                "the requested session does not exist",
            );
        }
        Err(error) => return live_projection_error(error),
    };
    let observed_through = snapshot.observed_through;
    let queued_at_snapshot = subscription.queued_len();
    let mut pending = VecDeque::new();
    pending.push_back(WebSessionLiveStreamEvent::Snapshot {
        snapshot: live_snapshot_dto(snapshot),
    });
    let source = stream::unfold(
        LiveFollowState {
            subscription,
            session,
            observed_through,
            queued_at_snapshot,
            pending,
            ended: false,
        },
        live_follow_next,
    );
    ndjson_response(source)
}

struct LiveFollowState {
    subscription: crate::ProcessMonitorSubscription,
    session: SessionId,
    observed_through: u64,
    queued_at_snapshot: usize,
    pending: VecDeque<WebSessionLiveStreamEvent>,
    ended: bool,
}

async fn live_follow_next(
    mut state: LiveFollowState,
) -> Option<(WebSessionLiveStreamEvent, LiveFollowState)> {
    if let Some(event) = state.pending.pop_front() {
        return Some((event, state));
    }
    if state.ended {
        return None;
    }
    loop {
        let update = match state.subscription.recv().await {
            Ok(update) => update,
            Err(ProcessMonitorReceiveError::Lagged) => {
                state.ended = true;
                return Some((
                    WebSessionLiveStreamEvent::ResyncRequired {
                        cursor: WebU64::from_u64(state.observed_through),
                    },
                    state,
                ));
            }
            Err(ProcessMonitorReceiveError::Closed) => return None,
        };
        let queued_at_snapshot = if state.queued_at_snapshot == 0 {
            false
        } else {
            state.queued_at_snapshot -= 1;
            true
        };
        match update {
            ProcessMonitorUpdate::Durable {
                cursor,
                session,
                kind,
            } => {
                if cursor <= state.observed_through {
                    continue;
                }
                state.observed_through = cursor;
                if session != state.session {
                    continue;
                }
                let Some(sequence) = std::num::NonZeroU64::new(cursor) else {
                    state.ended = true;
                    return Some((
                        WebSessionLiveStreamEvent::ResyncRequired {
                            cursor: WebU64::from_u64(state.observed_through),
                        },
                        state,
                    ));
                };
                return Some((
                    WebSessionLiveStreamEvent::Durable {
                        cursor: WebU64::from_u64(cursor),
                        address: address_dto(TimelineAddress::new(sequence)),
                        event_kind: event_kind_dto(kind),
                    },
                    state,
                ));
            }
            ProcessMonitorUpdate::ProviderTextDelta {
                session,
                turn,
                call,
                part_index,
                text,
            } => {
                if queued_at_snapshot || session != state.session {
                    continue;
                }
                state
                    .pending
                    .extend(web_text_fragments(&text).map(|content| {
                        WebSessionLiveStreamEvent::ProviderTextDelta {
                            turn_id: turn.into_uuid().to_string(),
                            model_call_id: call.into_uuid().to_string(),
                            part_index,
                            content,
                        }
                    }));
                let event = state.pending.pop_front()?;
                return Some((event, state));
            }
        }
    }
}

fn web_text_fragments(value: &str) -> impl Iterator<Item = String> + '_ {
    let mut remaining = value;
    let mut emitted_empty = !value.is_empty();
    std::iter::from_fn(move || {
        if remaining.is_empty() {
            if emitted_empty {
                return None;
            }
            emitted_empty = true;
            return Some(String::new());
        }
        let mut end = remaining.len().min(MAX_WEB_PROVIDER_TEXT_FRAGMENT_BYTES);
        while !remaining.is_char_boundary(end) {
            end -= 1;
        }
        let (fragment, rest) = remaining.split_at(end);
        remaining = rest;
        Some(fragment.to_owned())
    })
}

fn live_snapshot_dto(snapshot: SessionLiveSnapshot) -> WebSessionLiveSnapshot {
    WebSessionLiveSnapshot {
        session_id: snapshot.session.into_uuid().to_string(),
        observed_through: WebU64::from_u64(snapshot.observed_through),
        active: snapshot.active.map(|active| WebSessionLiveActiveTurn {
            turn_id: active.turn.into_uuid().to_string(),
            state: match active.state {
                SessionLiveActiveState::Running { model_call } => {
                    WebSessionLiveActiveState::Running {
                        model_call_id: model_call.map(|call| call.into_uuid().to_string()),
                    }
                }
                SessionLiveActiveState::AwaitingModelCallRecovery { call } => {
                    WebSessionLiveActiveState::AwaitingModelCallRecovery {
                        model_call_id: call.into_uuid().to_string(),
                    }
                }
                SessionLiveActiveState::AwaitingToolApproval { request } => {
                    WebSessionLiveActiveState::AwaitingToolApproval {
                        tool_request_id: request.into_uuid().to_string(),
                    }
                }
                SessionLiveActiveState::AwaitingChild { request, child } => {
                    WebSessionLiveActiveState::AwaitingChild {
                        tool_request_id: request.into_uuid().to_string(),
                        child_session_id: child.into_uuid().to_string(),
                    }
                }
                SessionLiveActiveState::AwaitingToolRecovery { attempt } => {
                    WebSessionLiveActiveState::AwaitingToolRecovery {
                        tool_attempt_id: attempt.into_uuid().to_string(),
                    }
                }
                SessionLiveActiveState::AwaitingRunnerRecovery {
                    runner,
                    placement_revision,
                } => WebSessionLiveActiveState::AwaitingRunnerRecovery {
                    runner_id: runner.into_uuid().to_string(),
                    placement_revision: WebU64::from_u64(placement_revision),
                },
            },
        }),
        queued_turn_count: WebU64::from_u64(snapshot.queued_turn_count),
        queued_turn_ids: snapshot
            .queued_turns
            .into_iter()
            .map(|turn| turn.into_uuid().to_string())
            .collect(),
        reconciliation: snapshot
            .reconciliation
            .map(|reconciliation| match reconciliation {
                SessionLiveReconciliation::ModelCall { turn, call } => {
                    WebSessionLiveReconciliation::ModelCall {
                        turn_id: turn.into_uuid().to_string(),
                        model_call_id: call.into_uuid().to_string(),
                    }
                }
                SessionLiveReconciliation::ToolAttempt { turn, attempt } => {
                    WebSessionLiveReconciliation::ToolAttempt {
                        turn_id: turn.into_uuid().to_string(),
                        tool_attempt_id: attempt.into_uuid().to_string(),
                    }
                }
            }),
        runner: snapshot.runner.map(|runner| WebSessionLiveRunner {
            runner_id: runner.runner.map(|runner| runner.into_uuid().to_string()),
            placement_revision: WebU64::from_u64(runner.placement_revision),
            state: match runner.state {
                SessionLiveRunnerState::Unpinned => WebSessionLiveRunnerState::Unpinned,
                SessionLiveRunnerState::Pinned => WebSessionLiveRunnerState::Pinned,
                SessionLiveRunnerState::RunnerLostBeforePin => {
                    WebSessionLiveRunnerState::RunnerLostBeforePin
                }
                SessionLiveRunnerState::RunnerLost => WebSessionLiveRunnerState::RunnerLost,
                SessionLiveRunnerState::RunnerAbandoned => {
                    WebSessionLiveRunnerState::RunnerAbandoned
                }
            },
            connection_health: runner.connection_health.map(|health| match health {
                SessionLiveRunnerConnectionHealth::Connected => {
                    WebSessionLiveRunnerConnectionHealth::Connected
                }
                SessionLiveRunnerConnectionHealth::Suspect => {
                    WebSessionLiveRunnerConnectionHealth::Suspect
                }
                SessionLiveRunnerConnectionHealth::Shutdown => {
                    WebSessionLiveRunnerConnectionHealth::Shutdown
                }
                SessionLiveRunnerConnectionHealth::Lost => {
                    WebSessionLiveRunnerConnectionHealth::Lost
                }
            }),
        }),
    }
}

fn live_projection_unavailable() -> Response {
    application_error(
        StatusCode::SERVICE_UNAVAILABLE,
        "session_live_projection_unavailable",
        "the live session projection is not configured",
    )
}

fn live_projection_error(error: SessionLiveRepositoryError) -> Response {
    tracing::error!(cause = %error, "session live projection read failed");
    application_error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "session_live_projection_failed",
        "the durable live session projection could not be read",
    )
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
    for (key, value) in url::form_urlencoded::parse(raw.unwrap_or_default().as_bytes()) {
        match key.as_ref() {
            "search" => set_once(&mut query.search, value.into_owned())?,
            "required_tag" => query.required_tag.push(value.into_owned()),
            "include_archived" => set_once(&mut query.include_archived, value.into_owned())?,
            "sort" => set_once(&mut query.sort, value.into_owned())?,
            "after_session_id" => set_once(&mut query.after_session_id, value.into_owned())?,
            "after_activity_unix_microseconds" => set_once(
                &mut query.after_activity_unix_microseconds,
                value.into_owned(),
            )?,
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

fn parse_attention_query(query: AttentionSnapshotQuery) -> Result<AttentionQuery, ()> {
    let sort = match query.sort.as_deref() {
        None | Some("last_activity_desc") => AttentionSort::LastActivityDescending,
        Some("session_id_asc") => AttentionSort::SessionIdentityAscending,
        Some(_) => return Err(()),
    };
    let include_archived = match query.include_archived.as_deref() {
        None | Some("false") => false,
        Some("true") => true,
        Some(_) => return Err(()),
    };
    let after_session = query
        .after_session_id
        .map(|value| value.parse::<Uuid>().map(SessionId::from_uuid))
        .transpose()
        .map_err(|_| ())?;
    let after_activity_micros = query
        .after_activity_unix_microseconds
        .map(|value| value.parse::<u64>())
        .transpose()
        .map_err(|_| ())?;
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
            Some(WebAttentionStreamEvent::Snapshot { snapshot }),
            cursor,
            AttentionFollowDisposition::Continue,
        ),
        |(repository, pending, cursor, disposition)| async move {
            if let Some(event) = pending {
                return Some((
                    event,
                    (
                        repository,
                        None,
                        cursor,
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
                    Ok(AttentionChanges::Updated { summaries, .. }) if summaries.is_empty() => {}
                    Ok(AttentionChanges::Updated {
                        cursor: next,
                        summaries,
                    }) => {
                        let summaries = summaries
                            .into_iter()
                            .map(attention_summary_dto)
                            .collect::<Result<Vec<_>, _>>()
                            .ok()?;
                        return Some((
                            WebAttentionStreamEvent::Update {
                                cursor: next.value().to_string(),
                                summaries,
                            },
                            (repository, None, next, AttentionFollowDisposition::Continue),
                        ));
                    }
                    Ok(AttentionChanges::ResyncRequired { cursor: next }) => {
                        return Some((
                            WebAttentionStreamEvent::ResyncRequired {
                                cursor: next.value().to_string(),
                            },
                            (repository, None, next, AttentionFollowDisposition::End),
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

fn attention_snapshot_dto(snapshot: AttentionSnapshot) -> Result<WebAttentionSnapshot, ()> {
    let continuation = snapshot
        .continuation
        .map(|continuation| match continuation {
            AttentionContinuation::LastActivity {
                recorded_at,
                session,
            } => Ok(WebAttentionContinuation::LastActivity {
                unix_microseconds: recorded_at
                    .duration_since(UNIX_EPOCH)
                    .map_err(|_| ())?
                    .as_micros()
                    .to_string(),
                session_id: session.into_uuid().to_string(),
            }),
            AttentionContinuation::SessionIdentity(session) => {
                Ok(WebAttentionContinuation::SessionIdentity {
                    session_id: session.into_uuid().to_string(),
                })
            }
        })
        .transpose()?;
    Ok(WebAttentionSnapshot {
        cursor: snapshot.cursor.value().to_string(),
        total: snapshot.total.to_string(),
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
    let unix_milliseconds = summary
        .last_activity
        .recorded_at
        .duration_since(UNIX_EPOCH)
        .map_err(|_| ())?
        .as_millis()
        .to_string();
    let goal_block = summary
        .goal_block
        .map(|goal| {
            if goal.need_summary.chars().count()
                > usize::from(max_attention_goal_summary_characters())
            {
                return Err(());
            }
            Ok(WebAttentionGoalBlock {
                generation: goal.generation.to_string(),
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
        session_id: summary.session.into_uuid().to_string(),
        title_summary: summary.title_summary,
        title_truncated: summary.title_truncated,
        archived: summary.archived,
        current_turn_id: summary
            .current_turn
            .map(|turn| turn.into_uuid().to_string()),
        active_turn_count: summary.active_turn_count.to_string(),
        queued_turn_count: summary.queued_turn_count.to_string(),
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
            actionable: summary.judge.actionable.to_string(),
            completed: summary.judge.completed.to_string(),
            escalated: summary.judge.escalated.to_string(),
            failed: summary.judge.failed.to_string(),
        },
        last_activity: WebAttentionActivity {
            unix_milliseconds,
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
        AttentionSnapshot, AttentionSort, AttentionState, AttentionSummary,
        max_attention_goal_summary_characters, max_attention_snapshot_items,
        max_attention_title_characters,
    };
    use signalbox_domain::{ModelCallId, SessionId, TurnId};
    use signalbox_web_contract::{
        MAX_JSON_BODY_BYTES, MAX_NDJSON_ITEM_BYTES, WebAttentionStreamEvent, WebContractBootstrap,
        WebContractExample, WebSessionLiveStreamEvent, WebSessionTimelineEventKind,
        WebTimelineAddress, WebTimelineEventSequence, WebU64,
    };
    use sqlx::types::Uuid;
    use tokio::sync::{mpsc, watch};
    use tower::ServiceExt as _;
    use url::Url;

    use super::{
        DEFAULT_WEB_BIND_ADDRESS, LiveFollowState, WebHttpConfiguration, WebHttpConfigurationError,
        WebHttpRuntime, attention_snapshot_dto, deterministic_test_router, live_follow_next,
        ndjson_response, production_router, web_text_fragments,
    };
    use crate::{ProcessMonitor, ProcessMonitorUpdate};

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

    fn live_session() -> SessionId {
        SessionId::from_uuid(Uuid::from_u128(0x991))
    }

    fn live_turn() -> TurnId {
        TurnId::from_uuid(Uuid::from_u128(0x992))
    }

    fn live_call() -> ModelCallId {
        ModelCallId::from_uuid(Uuid::from_u128(0x993))
    }

    fn live_follow_state(
        monitor: &ProcessMonitor,
        observed_through: u64,
        queued_at_snapshot: usize,
    ) -> LiveFollowState {
        LiveFollowState {
            subscription: monitor.subscribe(),
            session: live_session(),
            observed_through,
            queued_at_snapshot,
            pending: std::collections::VecDeque::new(),
            ended: false,
        }
    }

    #[tokio::test]
    async fn live_follow_orders_provider_draft_before_later_durable_header() {
        let monitor = ProcessMonitor::test_channel();
        let state = live_follow_state(&monitor, 7, 0);
        monitor.publish_for_test(ProcessMonitorUpdate::ProviderTextDelta {
            session: live_session(),
            turn: live_turn(),
            call: live_call(),
            part_index: 2,
            text: "draft".to_owned(),
        });
        let (draft, state) = live_follow_next(state)
            .await
            .expect("the provider draft is delivered");
        monitor.publish_for_test(ProcessMonitorUpdate::Durable {
            cursor: 8,
            session: live_session(),
            kind: signalbox_application::SessionTimelineEventKind::ModelCallTransition,
        });
        let (durable, _) = live_follow_next(state)
            .await
            .expect("the durable header follows the draft");

        assert_eq!(
            draft,
            WebSessionLiveStreamEvent::ProviderTextDelta {
                turn_id: live_turn().into_uuid().to_string(),
                model_call_id: live_call().into_uuid().to_string(),
                part_index: 2,
                content: "draft".to_owned(),
            }
        );
        assert_eq!(
            durable,
            WebSessionLiveStreamEvent::Durable {
                cursor: WebU64::from_u64(8),
                address: WebTimelineAddress {
                    event_sequence: WebTimelineEventSequence::from_nonzero(
                        std::num::NonZeroU64::new(8).expect("the fixture cursor is positive"),
                    ),
                },
                event_kind: WebSessionTimelineEventKind::ModelCallTransition,
            }
        );
    }

    #[tokio::test]
    async fn live_follow_discards_provider_draft_queued_before_snapshot() {
        let monitor = ProcessMonitor::test_channel();
        let mut state = live_follow_state(&monitor, 7, 0);
        monitor.publish_for_test(ProcessMonitorUpdate::ProviderTextDelta {
            session: live_session(),
            turn: live_turn(),
            call: live_call(),
            part_index: 0,
            text: "stale draft".to_owned(),
        });
        state.queued_at_snapshot = state.subscription.queued_len();
        monitor.publish_for_test(ProcessMonitorUpdate::Durable {
            cursor: 8,
            session: live_session(),
            kind: signalbox_application::SessionTimelineEventKind::TurnCompleted,
        });
        let (event, _) = live_follow_next(state)
            .await
            .expect("the post-snapshot durable event is delivered");

        assert_eq!(
            event,
            WebSessionLiveStreamEvent::Durable {
                cursor: WebU64::from_u64(8),
                address: WebTimelineAddress {
                    event_sequence: WebTimelineEventSequence::from_nonzero(
                        std::num::NonZeroU64::new(8).expect("the fixture cursor is positive"),
                    ),
                },
                event_kind: WebSessionTimelineEventKind::TurnCompleted,
            }
        );
    }

    #[tokio::test]
    async fn live_follow_lag_requires_transient_presentation_resync() {
        let monitor = ProcessMonitor::test_channel();
        let state = live_follow_state(&monitor, 7, 0);
        monitor.fill_for_test(ProcessMonitorUpdate::Durable {
            cursor: 8,
            session: live_session(),
            kind: signalbox_application::SessionTimelineEventKind::TurnActivated,
        });
        let (event, _) = live_follow_next(state)
            .await
            .expect("lag produces one explicit terminal event");

        assert_eq!(
            event,
            WebSessionLiveStreamEvent::ResyncRequired {
                cursor: WebU64::from_u64(7),
            }
        );
    }

    #[test]
    fn provider_text_fragment_fits_after_worst_case_json_escaping() {
        let source = "\u{0001}".repeat(super::MAX_WEB_PROVIDER_TEXT_FRAGMENT_BYTES);
        let content = web_text_fragments(&source)
            .next()
            .expect("nonempty provider text has a first fragment");
        let encoded = super::encode_ndjson_item(WebSessionLiveStreamEvent::ProviderTextDelta {
            turn_id: live_turn().into_uuid().to_string(),
            model_call_id: live_call().into_uuid().to_string(),
            part_index: u32::MAX,
            content,
        })
        .expect("the worst-case escaped fragment remains below the NDJSON ceiling");

        assert!(encoded.len() <= MAX_NDJSON_ITEM_BYTES + 1);
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
        let snapshot = attention_snapshot_dto(AttentionSnapshot {
            cursor: AttentionCursor::new(u64::MAX),
            total: u64::MAX,
            sort: AttentionSort::LastActivityDescending,
            summaries: vec![summary; usize::from(max_attention_snapshot_items())],
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
