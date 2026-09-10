//! Browser-facing same-origin HTTP transport foundation.
//!
//! This boundary owns browser HTTP semantics and browser DTOs. It does not
//! expose local process-protocol messages, storage records, or application
//! authentication.

use std::{
    collections::{BTreeSet, VecDeque},
    env,
    error::Error,
    ffi::OsString,
    fmt, io,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    num::NonZeroU64,
    path::PathBuf,
    sync::Arc,
    time::{Duration, UNIX_EPOCH},
};

use axum::{
    Json, Router,
    body::{Body, Bytes, to_bytes},
    extract::{DefaultBodyLimit, Path, Query, RawQuery, Request, State, rejection::QueryRejection},
    http::{
        HeaderMap, HeaderValue, Method, StatusCode,
        header::{
            ACCEPT_RANGES, ALLOW, CACHE_CONTROL, CONTENT_DISPOSITION, CONTENT_LENGTH,
            CONTENT_RANGE, CONTENT_TYPE, ETAG, HOST, IF_RANGE, ORIGIN, RANGE,
            X_CONTENT_TYPE_OPTIONS,
        },
    },
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use futures_util::{Stream, StreamExt, stream};
use headers::{
    ETag as TypedEtag, IfNoneMatch as TypedIfNoneMatch, IfRange as TypedIfRange,
    Range as TypedRange,
};
use percent_encoding::{AsciiSet, CONTROLS, utf8_percent_encode};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use signalbox_application::{
    AttentionAction, AttentionActivityKind, AttentionBlockedReason, AttentionChanges,
    AttentionContinuation, AttentionGoalBlock, AttentionLifecycleState, AttentionQuery,
    AttentionSnapshot, AttentionSort, AttentionState, AttentionSummary, SearchContentClass,
    SearchCursor, SearchPageLimit, SearchQuery, SearchResultSource, SearchScope, SearchStrategy,
    SearchText, SessionLiveActiveState, SessionLiveReconciliation,
    SessionLiveRunnerConnectionHealth, SessionLiveRunnerState, SessionLiveSnapshot,
    SessionTimelineDescriptor, SessionTimelineDetailBody, SessionTimelineDetailPage,
    SessionTimelineEventKind, SessionTimelineWindow, TimelineAddress, TimelineBodyContinuation,
    TimelineBodyField, TimelineContinuation, TimelineDetailContinuation, TimelineDetailCursor,
    TimelineDetailLimits, TimelineModelCallDisposition, TimelineModelCallState,
    TimelineTextExcerpt, TimelineTurnLifecycleKind, TimelineWindowAnchor, TimelineWindowLimits,
    UsageAggregateCompleteness, UsageAggregateGroup, UsageAggregateTokenAxes,
    UsageCacheNormalization, UsageCallCursor, UsageCallEvidence, UsageCallKind, UsageCallOrder,
    UsageCallPageLimit, UsageCallQuery, UsageInputTokenSemantics, UsageProvenance, UsageQuery,
    UsageSelection, UsageTimeFromInclusive, UsageTimeRange, UsageTimeToExclusive,
    UsageTimestampMicros, UsageTokenAxes, UsageTokenPresence, max_attention_filter_tags,
    max_attention_filter_utf8_bytes, max_attention_goal_summary_characters,
    max_attention_title_characters,
};
use signalbox_blob_store::MAX_BLOB_RANGE_BYTES;
use signalbox_domain::{
    BlobDerivation, BlobDerivationProducer, BlobDigest, ModelCallId, ProviderModelCallFailureCause,
    ProviderModelIdentity, ResolvedProviderTarget, SessionId, TurnId,
};
use signalbox_persistence::attention::{
    AttentionPage, AttentionRepository, AttentionRepositoryError, AutomaticResumeAttemptBounds,
};
use signalbox_persistence::outbox::OutboxDispatchError;
use signalbox_persistence::process_read::ProcessModelCallInputTokenSemantics;
use signalbox_persistence::search::{SearchRepository, SearchRepositoryError};
use signalbox_persistence::session_live::{SessionLiveRepository, SessionLiveRepositoryError};
use signalbox_persistence::session_timeline::{
    SessionTimelineRepository, SessionTimelineRepositoryError,
};
use signalbox_persistence::usage::{UsageRepository, UsageRepositoryError};
use signalbox_web_contract::{
    MAX_JSON_BODY_BYTES, MAX_NDJSON_ITEM_BYTES, MAX_WEB_PROVIDER_TEXT_FRAGMENT_BYTES, WebApiError,
    WebApiErrorKind, WebApiErrorResponse, WebAttentionAction, WebAttentionActivity,
    WebAttentionActivityKind, WebAttentionBlockedReason, WebAttentionGoalBlock,
    WebAttentionJudgeFacts, WebAttentionLifecycleState, WebAttentionSnapshot, WebAttentionState,
    WebAttentionStreamEvent, WebAttentionSummary, WebBlobAvailableView, WebBlobDerivation,
    WebBlobDerivationProducer, WebBlobDescriptor, WebBlobId, WebBlobViewKind, WebContractBootstrap,
    WebContractExample, WebDollarAmount, WebLiveResourceId, WebNullableU64, WebNullableU128,
    WebPositiveU64, WebProviderModelCallFailureCause, WebSearchContentClass, WebSearchCursor,
    WebSearchHighlight, WebSearchPage, WebSearchProjectionId, WebSearchResult,
    WebSearchResultSource, WebSessionCatalogActivity, WebSessionCatalogContinuation,
    WebSessionCatalogSnapshot, WebSessionCatalogSort, WebSessionCatalogSummary, WebSessionId,
    WebSessionLiveActiveState, WebSessionLiveActiveTurn, WebSessionLiveReconciliation,
    WebSessionLiveRunner, WebSessionLiveRunnerConnectionHealth, WebSessionLiveSnapshot,
    WebSessionLiveStreamEvent, WebSessionRate, WebSessionRates, WebSessionTimelineDescriptor,
    WebSessionTimelineDetail, WebSessionTimelineDetailBody, WebSessionTimelineDetailPage,
    WebSessionTimelineEventKind, WebSessionTimelineItem, WebSessionTimelineSizeFacts,
    WebSessionTimelineWindow, WebSessionWorkFacts, WebTimelineAddress, WebTimelineBlobReference,
    WebTimelineBodyContinuation, WebTimelineBodyField, WebTimelineDetailContinuation,
    WebTimelineEventSequence, WebTimelineModelCallDisposition, WebTimelineModelCallState,
    WebTimelineModelUsage, WebTimelineTextExcerpt, WebTimelineTurnLifecycleKind, WebTurnId, WebU64,
    WebUsageAggregateGroup, WebUsageAggregateTokenAxes, WebUsageCall, WebUsageCallCount,
    WebUsageCallCursor, WebUsageCallKind, WebUsageCallPage, WebUsageCost, WebUsageCostLabel,
    WebUsageCostUnavailableReason, WebUsageInputSemantics, WebUsageProvenance, WebUsageRateVersion,
    WebUsageSummary, WebUsageTimestampMicros, WebUsageTokenAxes, WebUsageTokenCoverage, WebUuid,
};
use sqlx::{PgPool, Row as _, types::Uuid};
use tokio::{
    net::TcpListener,
    sync::{OwnedSemaphorePermit, Semaphore, mpsc, watch},
    time::{Instant, timeout_at},
};
use tower_http::services::{ServeDir, ServeFile};
use url::Url;

use crate::{
    BillingKind, BlobStoreRegistry, HubModelConfiguration, ProcessMonitor,
    ProcessMonitorReceiveError, ProcessMonitorUpdate, WebBlobRuntime, WebImageDerivativeKind,
    blob_read_runtime::{open_recorded_blob_range, open_recorded_blob_verified},
    configuration::ModelCallInputUsage,
    web_blob_runtime::WebBlobRuntimeError,
    web_imports,
};

/// Optional deployment override for the browser listener.
pub const WEB_BIND_ENVIRONMENT: &str = "SIGNALBOX_WEB_BIND";
/// Optional production web-build root served outside `/api/`.
pub const WEB_ASSET_ROOT_ENVIRONMENT: &str = "SIGNALBOX_WEB_ASSET_ROOT";
/// Conservative browser listener default: reachable only from this host.
pub const DEFAULT_WEB_BIND_ADDRESS: SocketAddr =
    SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 37_231);

const JSON_CONTENT_TYPE: &str = "application/json";
const TEXT_CONTENT_TYPE: &str = "text/plain";
const NDJSON_CONTENT_TYPE: &str = "application/x-ndjson";
const HTTP_DEFAULT_PORT: u16 = 80;
const IMMUTABLE_CACHE_CONTROL: &str = "public, max-age=31536000, immutable";
const MAX_DISPLAY_FILENAME_BYTES: usize = 1024;
const BLOB_STREAM_CHUNK_BYTES: usize = 64 * 1024;
const MAX_CONCURRENT_WEB_BLOB_READS: usize = 4;
const BLOB_RESPONSE_TIMEOUT_SECONDS: u64 = 120;

#[derive(Clone, Debug)]
struct WebHttpState {
    blobs: Option<WebBlobRuntime>,
    blob_read_budget: Arc<Semaphore>,
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
        validate_loopback_bind_address(bind_address)?;
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

    /// Creates explicit loopback-only configuration for a deterministic or embedded server.
    pub fn new(
        bind_address: SocketAddr,
        asset_root: Option<PathBuf>,
    ) -> Result<Self, WebHttpConfigurationError> {
        validate_loopback_bind_address(bind_address)?;
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

fn validate_loopback_bind_address(
    bind_address: SocketAddr,
) -> Result<(), WebHttpConfigurationError> {
    if bind_address.ip().is_loopback() {
        Ok(())
    } else {
        Err(WebHttpConfigurationError::NonLoopbackBindAddress)
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
    follow_shutdown: Option<watch::Sender<bool>>,
}

/// Browser listener bound before runtime-owned monitor construction.
pub struct BoundWebHttpListener {
    listener: TcpListener,
    asset_root: Option<PathBuf>,
    pool: PgPool,
    blobs: Option<WebBlobRuntime>,
    model_configuration: HubModelConfiguration,
    blob_store_registry: Option<Arc<BlobStoreRegistry>>,
    snapshot_reader_budget: Arc<Semaphore>,
}

#[derive(Clone, Debug)]
struct ProductionReadRuntime {
    snapshot_reader_budget: Option<Arc<Semaphore>>,
    shutdown: Option<watch::Receiver<bool>>,
    monitor: Option<ProcessMonitor>,
}

impl BoundWebHttpListener {
    /// Attaches the daemon's one bounded monitor and builds the production router.
    pub fn into_runtime(
        self,
        monitor: ProcessMonitor,
        eligibility_nudge: signalbox_application::InProcessEligibilityNudge,
    ) -> WebHttpRuntime {
        let (follow_shutdown, follow_shutdown_receiver) = watch::channel(false);
        let router = production_router_with_budget(
            self.asset_root,
            Some(self.pool),
            self.blobs,
            Some(self.model_configuration),
            self.blob_store_registry,
            ProductionReadRuntime {
                snapshot_reader_budget: Some(self.snapshot_reader_budget),
                shutdown: Some(follow_shutdown_receiver),
                monitor: Some(monitor),
            },
            Some(eligibility_nudge),
        );
        WebHttpRuntime {
            listener: self.listener,
            router,
            follow_shutdown: Some(follow_shutdown),
        }
    }
}

impl WebHttpRuntime {
    /// Supplies one current catalog snapshot to each browser request.
    pub fn with_configuration_reload(
        mut self,
        reload: crate::configuration_reload::ConfigurationReload,
    ) -> Self {
        self.router = self.router.layer(axum::Extension(reload));
        self
    }

    /// Binds the production same-origin router.
    ///
    /// Fails construction when the pool cannot fund the shared snapshot
    /// reader budget, mirroring the daemon entry point's own startup
    /// rejection (`main.rs`'s `insufficient_snapshot_reader_pool_capacity`
    /// failure) instead of returning a runtime whose session-read routes
    /// can never obtain a reader permit.
    pub async fn bind(
        configuration: WebHttpConfiguration,
        pool: PgPool,
        blobs: Option<WebBlobRuntime>,
        model_configuration: HubModelConfiguration,
        blob_store_registry: Option<Arc<BlobStoreRegistry>>,
    ) -> Result<Self, WebHttpRuntimeError> {
        let snapshot_reader_budget = crate::process_runtime::shared_snapshot_reader_budget(
            pool.options().get_max_connections(),
            Some(&model_configuration),
        )
        .ok_or(WebHttpRuntimeError::Bind)?;
        Self::bind_with_snapshot_reader_budget(
            configuration,
            pool,
            blobs,
            model_configuration,
            blob_store_registry,
            snapshot_reader_budget,
        )
        .await
    }

    /// Binds production HTTP reads to the daemon-wide snapshot-reader budget.
    pub async fn bind_with_snapshot_reader_budget(
        configuration: WebHttpConfiguration,
        pool: PgPool,
        blobs: Option<WebBlobRuntime>,
        model_configuration: HubModelConfiguration,
        blob_store_registry: Option<Arc<BlobStoreRegistry>>,
        snapshot_reader_budget: Arc<Semaphore>,
    ) -> Result<Self, WebHttpRuntimeError> {
        Self::bind_production(
            configuration,
            pool,
            blobs,
            model_configuration,
            blob_store_registry,
            Some(snapshot_reader_budget),
        )
        .await
    }

    /// Binds the production socket while deferring monitor-dependent router composition.
    pub async fn bind_listener_with_snapshot_reader_budget(
        configuration: WebHttpConfiguration,
        pool: PgPool,
        blobs: Option<WebBlobRuntime>,
        model_configuration: HubModelConfiguration,
        blob_store_registry: Option<Arc<BlobStoreRegistry>>,
        snapshot_reader_budget: Arc<Semaphore>,
    ) -> Result<BoundWebHttpListener, WebHttpRuntimeError> {
        let listener = TcpListener::bind(configuration.bind_address)
            .await
            .map_err(|_| WebHttpRuntimeError::Bind)?;
        Ok(BoundWebHttpListener {
            listener,
            asset_root: configuration.asset_root,
            pool,
            blobs,
            model_configuration,
            blob_store_registry,
            snapshot_reader_budget,
        })
    }

    async fn bind_production(
        configuration: WebHttpConfiguration,
        pool: PgPool,
        blobs: Option<WebBlobRuntime>,
        model_configuration: HubModelConfiguration,
        blob_store_registry: Option<Arc<BlobStoreRegistry>>,
        snapshot_reader_budget: Option<Arc<Semaphore>>,
    ) -> Result<Self, WebHttpRuntimeError> {
        let (follow_shutdown, follow_shutdown_receiver) = watch::channel(false);
        let router = production_router_with_budget(
            configuration.asset_root,
            Some(pool),
            blobs,
            Some(model_configuration),
            blob_store_registry,
            ProductionReadRuntime {
                snapshot_reader_budget,
                shutdown: Some(follow_shutdown_receiver),
                monitor: None,
            },
            None,
        );
        Self::bind_router_with_follow_shutdown(
            configuration.bind_address,
            router,
            Some(follow_shutdown),
        )
        .await
    }

    /// Binds an explicit router, primarily for deterministic browser scenarios.
    pub async fn bind_router(
        bind_address: SocketAddr,
        router: Router,
    ) -> Result<Self, WebHttpRuntimeError> {
        Self::bind_router_with_follow_shutdown(bind_address, router, None).await
    }

    async fn bind_router_with_follow_shutdown(
        bind_address: SocketAddr,
        router: Router,
        follow_shutdown: Option<watch::Sender<bool>>,
    ) -> Result<Self, WebHttpRuntimeError> {
        let listener = TcpListener::bind(bind_address)
            .await
            .map_err(|_| WebHttpRuntimeError::Bind)?;
        Ok(Self {
            listener,
            router,
            follow_shutdown,
        })
    }

    /// Actual address, including an operating-system-selected test port.
    pub fn local_address(&self) -> Result<SocketAddr, WebHttpRuntimeError> {
        self.listener
            .local_addr()
            .map_err(|_| WebHttpRuntimeError::Bind)
    }

    /// Serves until shutdown, then cancels requests by dropping their futures.
    pub async fn run(self, mut shutdown: watch::Receiver<bool>) -> Result<(), WebHttpRuntimeError> {
        let Self {
            listener,
            router,
            follow_shutdown,
        } = self;
        let shutdown_requested = async move {
            if !*shutdown.borrow() {
                while shutdown.changed().await.is_ok() {
                    if *shutdown.borrow() {
                        break;
                    }
                }
            }
            if let Some(follow_shutdown) = follow_shutdown {
                let _ = follow_shutdown.send(true);
            }
        };
        axum::serve(listener, router)
            .with_graceful_shutdown(shutdown_requested)
            .await
            .map_err(|_| WebHttpRuntimeError::Serve)
    }
}

/// Builds the production router: `/api/` remains API-only and assets share its origin.
///
/// `shutdown` must be driven before an enclosing Axum graceful shutdown waits
/// for requests, so live snapshot and follow reads can release their database
/// and reader-budget waits.
pub fn production_router(
    asset_root: Option<PathBuf>,
    pool: Option<PgPool>,
    blobs: Option<WebBlobRuntime>,
    model_configuration: Option<HubModelConfiguration>,
    blob_store_registry: Option<Arc<BlobStoreRegistry>>,
    shutdown: Option<watch::Receiver<bool>>,
    eligibility_nudge: Option<signalbox_application::InProcessEligibilityNudge>,
) -> Router {
    let snapshot_reader_budget = pool.as_ref().and_then(|pool| {
        crate::process_runtime::shared_snapshot_reader_budget(
            pool.options().get_max_connections(),
            model_configuration.as_ref(),
        )
    });
    production_router_with_budget(
        asset_root,
        pool,
        blobs,
        model_configuration,
        blob_store_registry,
        ProductionReadRuntime {
            snapshot_reader_budget,
            shutdown,
            monitor: None,
        },
        eligibility_nudge,
    )
}

fn production_router_with_budget(
    asset_root: Option<PathBuf>,
    pool: Option<PgPool>,
    blobs: Option<WebBlobRuntime>,
    model_configuration: Option<HubModelConfiguration>,
    _blob_store_registry: Option<Arc<BlobStoreRegistry>>,
    read_runtime: ProductionReadRuntime,
    eligibility_nudge: Option<signalbox_application::InProcessEligibilityNudge>,
) -> Router {
    let http_state = WebHttpState {
        blobs,
        blob_read_budget: Arc::new(Semaphore::new(MAX_CONCURRENT_WEB_BLOB_READS)),
    };
    let automatic_resume_attempts =
        configured_automatic_resume_attempts(model_configuration.as_ref());
    let state = WebApiState {
        pool: pool.clone(),
        attention: pool
            .clone()
            .map(|pool| AttentionRepository::new(pool, automatic_resume_attempts)),
        timeline: pool.clone().map(SessionTimelineRepository::new),
        live: pool.clone().map(SessionLiveRepository::new),
        search: pool.clone().map(SearchRepository::new),
        usage: pool.clone().map(UsageRepository::new),
        model_configuration: model_configuration.clone().map(Arc::new),
        snapshot_reader_budget: read_runtime.snapshot_reader_budget.clone(),
        shutdown: read_runtime.shutdown,
        monitor: read_runtime.monitor,
        eligibility_nudge,
    };
    // Every route that reads session data sits behind the loopback authority
    // gate. The attention projection returns session identities, goal-need
    // summaries, and operator state, so it belongs here for the same reason the
    // descriptor and timeline reads do: the listener is unauthenticated, and a
    // rebound origin must not reach session data with an attacker's authority.
    // `same_origin_router` additionally gates the whole listener, `/bootstrap`
    // and the static assets included, so this route layer is the inner of two.
    let session_inputs = Router::new()
        .route("/sessions/{session_id}/input", post(session_submit_input))
        .route_layer(middleware::from_fn(validate_json_mutation))
        .route_layer(middleware::from_fn(validate_loopback_host))
        .with_state(state.clone());
    let session_reads = Router::new()
        .route("/sessions/{session_id}", get(session_descriptor))
        .route(
            "/sessions/{session_id}/timeline",
            get(session_timeline_window),
        )
        .route("/sessions/{session_id}/live", get(session_live_snapshot))
        .route("/sessions/{session_id}/follow", get(session_live_follow))
        .route("/sessions", get(session_catalog))
        .route("/sessions/rates", get(session_rates))
        .route("/search", get(search))
        .route("/usage/summary", get(usage_summary))
        .route("/usage/calls", get(usage_calls))
        .route("/attention", get(attention_snapshot))
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
        .route_layer(middleware::from_fn(validate_loopback_host))
        .with_state(state);
    // Every route that reads session-attached content sits behind the
    // loopback authority gate. Blob descriptors and bytes are reachable by
    // digest alone and a descriptor read can start isolated derivation work,
    // so they belong here for the same reason the session reads do: the
    // listener is unauthenticated, and a rebound origin must not reach blob
    // content or trigger derivations with an attacker's authority.
    let blob_reads = Router::new()
        .route(
            "/blobs/{digest}/descriptor",
            get(blob_descriptor).head(blob_descriptor_head),
        )
        .route(
            "/blobs/{digest}/content/{representation}",
            get(blob_content).head(blob_content),
        )
        .route(
            "/blobs/{digest}/download",
            get(blob_download).head(blob_download),
        )
        .route_layer(middleware::from_fn(validate_loopback_host))
        .with_state(http_state.clone());
    let api = Router::new()
        .route("/bootstrap", get(contract_bootstrap))
        .with_state(http_state)
        .merge(session_reads)
        .merge(session_inputs)
        .merge(blob_reads);
    // Imported-conversation reads need both a pool and hub model settings; the
    // bootstrap and session surfaces stay routable without either.
    let api = match (pool, model_configuration) {
        (Some(pool), Some(model_configuration)) => {
            api.nest("/imports", web_imports::router(pool, model_configuration))
        }
        _ => api,
    };
    let api = api.fallback(api_not_found);
    same_origin_router(asset_root, api)
}

/// Reads the deployment's automatic-resume attempt limits for the attention
/// projection.
///
/// The projection reports a blocked goal as still owed automatic resumption
/// until one of these limits ends its run, so both must be the configured
/// numbers the daemon's resume planner reads
/// (`goal_mode::GoalModeNumericBounds`). An absent or unbounded setting leaves
/// that limit unbounded there as well.
fn configured_automatic_resume_attempts(
    model_configuration: Option<&HubModelConfiguration>,
) -> AutomaticResumeAttemptBounds {
    AutomaticResumeAttemptBounds::new(
        configured_automatic_resume_limit(model_configuration, "automatic_resume_attempt_budget"),
        configured_automatic_resume_limit(model_configuration, "automatic_resume_attempt_ceiling"),
    )
}

fn configured_automatic_resume_limit(
    model_configuration: Option<&HubModelConfiguration>,
    field: &'static str,
) -> Option<u32> {
    model_configuration
        .and_then(|configuration| configuration.numeric_bounds().integer(field).flatten())
        .and_then(|limit| u32::try_from(limit).ok())
}

fn same_origin_router(asset_root: Option<PathBuf>, api: Router) -> Router {
    let router = Router::new().nest("/api", api);
    let router = match asset_root {
        Some(root) => router.fallback_service(
            ServeDir::new(root.clone())
                .append_index_html_on_directories(true)
                .fallback(ServeFile::new(root.join("index.html"))),
        ),
        None => router.fallback(static_assets_not_configured),
    };
    router
        .layer(middleware::from_fn(validate_loopback_host))
        .layer(middleware::from_fn(origin::validate_api_fetch_site))
}

#[derive(Clone, Debug)]
struct WebApiState {
    pool: Option<PgPool>,
    attention: Option<AttentionRepository>,
    timeline: Option<SessionTimelineRepository>,
    live: Option<SessionLiveRepository>,
    search: Option<SearchRepository>,
    usage: Option<UsageRepository>,
    model_configuration: Option<Arc<HubModelConfiguration>>,
    snapshot_reader_budget: Option<Arc<Semaphore>>,
    shutdown: Option<watch::Receiver<bool>>,
    monitor: Option<ProcessMonitor>,
    eligibility_nudge: Option<signalbox_application::InProcessEligibilityNudge>,
}

async fn session_submit_input(
    State(mut state): State<WebApiState>,
    reload: Option<axum::Extension<crate::configuration_reload::ConfigurationReload>>,
    Path(session_id): Path<String>,
    request: Request,
) -> Response {
    if let Some(axum::Extension(reload)) = reload {
        state.model_configuration = Some(reload.catalogs().models);
    }

    use signalbox_application::{
        EligibilityNudge as _, SubmitInputOutcome, SubmitInputRequest, SubmitInputService,
        UuidV7SubmitInputIdGenerator,
    };
    use signalbox_domain::{
        DeliveryRequest, DurableCommandId, ModelSelectionOverride, PerInputConfigurationChoices,
        UserContent,
    };
    use signalbox_persistence::{
        session::SessionRepository,
        submit_input::{SubmitInputRepository, SubmitInputRepositoryError},
    };
    let request =
        match decode_bounded_json::<signalbox_web_contract::WebSubmitInputRequest>(request).await {
            Ok(request) => request,
            Err(response) => return response,
        };
    let Ok(session) = parse_canonical_session_id(&session_id) else {
        return application_error(
            StatusCode::BAD_REQUEST,
            "invalid_session_id",
            "session identity is not canonical",
        );
    };
    let Some(command_id) = Uuid::parse_str(&request.command_id)
        .ok()
        .filter(|identity| identity.to_string() == request.command_id)
    else {
        return application_error(
            StatusCode::BAD_REQUEST,
            "invalid_command_id",
            "command identity is not a canonical UUID",
        );
    };
    let command_id = DurableCommandId::from_uuid(command_id);
    let Ok(content) = UserContent::try_text(request.message) else {
        return application_error(
            StatusCode::BAD_REQUEST,
            "invalid_input",
            "message must be nonempty, NUL-free text within the input limit",
        );
    };
    let (Some(pool), Some(configuration), Some(eligibility_nudge)) = (
        state.pool,
        state.model_configuration,
        state.eligibility_nudge,
    ) else {
        return application_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "input_unavailable",
            "session input is not configured",
        );
    };
    let repository = SubmitInputRepository::with_model_capabilities(
        pool.clone(),
        configuration.model_capability_catalog(),
    );
    match repository.load(command_id).await {
        Ok(Some(recorded)) => {
            if recorded.command().session() != session
                || recorded.command().content() != &content
                || !matches!(
                    recorded.command().delivery(),
                    DeliveryRequest::StartWhenNoActiveTurn { configuration }
                        if configuration == PerInputConfigurationChoices::new(
                            configuration.expected_session_defaults_version(),
                            ModelSelectionOverride::UseSessionDefault,
                        )
                )
            {
                return web_input_conflict();
            }
            if matches!(
                recorded.result(),
                signalbox_domain::SubmitInputResult::Applied(
                    signalbox_domain::SubmitInputAppliedResult::TurnOrigin(_)
                )
            ) {
                let _ = eligibility_nudge.nudge(session);
            }
            return web_input_result(recorded.result());
        }
        Ok(None) => {}
        Err(SubmitInputRepositoryError::DifferentCommandKind { .. }) => {
            return web_input_conflict();
        }
        Err(_) => return web_input_unconfirmed(),
    }
    let current = match SessionRepository::new(pool).load_session(session).await {
        Ok(Some(current)) => current,
        Ok(None) => {
            return application_error(
                StatusCode::NOT_FOUND,
                "session_not_found",
                "the requested session does not exist",
            );
        }
        Err(_) => return web_input_unconfirmed(),
    };
    let delivery = DeliveryRequest::StartWhenNoActiveTurn {
        configuration: PerInputConfigurationChoices::new(
            current.current_configuration_defaults().version(),
            ModelSelectionOverride::UseSessionDefault,
        ),
    };
    let maximum = configuration
        .numeric_bounds()
        .integer("max_message_utf8_bytes")
        .flatten()
        .and_then(|value| usize::try_from(value).ok());
    let request = match SubmitInputRequest::try_new_with_content_limit(
        command_id, session, content, delivery, maximum,
    ) {
        Ok(request) => request,
        Err(_) => {
            return application_error(
                StatusCode::BAD_REQUEST,
                "invalid_input",
                "command identity or message exceeds input admission bounds",
            );
        }
    };
    let mut service = SubmitInputService::new(
        UuidV7SubmitInputIdGenerator,
        crate::process_runtime::ConfiguredSubmitInputTransaction {
            repository,
            model_configuration: &configuration,
            principal: signalbox_domain::CommandPrincipal::Operator,
            cascade_root_kind: signalbox_domain::ParentTerminationKind::Cancelled,
        },
        eligibility_nudge,
        signalbox_application::InProcessToolDispatchGate::default(),
    );
    match service.execute(request).await {
        Ok(SubmitInputOutcome::Recorded(result)) => web_input_result(&result),
        // A concurrent request may have resolved different session defaults.
        // Retry reads its recorded command before resolving defaults again.
        Ok(SubmitInputOutcome::ConflictingReuse { .. }) => web_input_unconfirmed(),
        Err(SubmitInputRepositoryError::UnsupportedModelSetting(_)) => application_error(
            StatusCode::CONFLICT,
            "unsupported_model_setting",
            "the selected model does not support an explicitly requested setting",
        ),
        Err(_) => web_input_unconfirmed(),
    }
}

fn web_input_conflict() -> Response {
    application_error(
        StatusCode::CONFLICT,
        "conflicting_command_reuse",
        "command identity already names a different request",
    )
}

fn web_input_unconfirmed() -> Response {
    application_error(
        StatusCode::SERVICE_UNAVAILABLE,
        "input_outcome_unconfirmed",
        "input outcome is unconfirmed; retry the same command and message",
    )
}

fn web_input_result(result: &signalbox_domain::SubmitInputResult) -> Response {
    use signalbox_domain::{SubmitInputRejectedResult as Rejected, SubmitInputResult};
    let SubmitInputResult::Rejected(rejection) = result else {
        return StatusCode::NO_CONTENT.into_response();
    };
    let (code, reason) = match rejection {
        Rejected::ActiveTurnPresent { .. } => (
            "active_turn_present",
            "input cannot start a turn while another turn is active",
        ),
        Rejected::SessionNotFound { .. } => {
            ("session_not_found", "the requested session does not exist")
        }
        Rejected::SessionDefaultsVersionMismatch { .. } => (
            "session_defaults_changed",
            "session defaults changed during submission; submit again with a new command",
        ),
        Rejected::UnknownModelAlias { .. } => (
            "unknown_model_alias",
            "the session model alias has no selectable definition",
        ),
        Rejected::AcceptancePositionExhausted { .. } => (
            "acceptance_position_exhausted",
            "the session cannot accept more input positions",
        ),
        Rejected::AttachmentBlobNotFound { .. } => {
            ("attachment_not_found", "an attachment is unavailable")
        }
        Rejected::AttachmentByteBudgetExceeded { .. } => (
            "attachment_budget_exceeded",
            "attachments exceed the byte budget",
        ),
        Rejected::NoActiveTurn { .. } => {
            ("no_active_turn", "the expected active turn does not exist")
        }
        Rejected::ActiveTurnMismatch { .. } => (
            "active_turn_mismatch",
            "the active turn does not match the request",
        ),
        Rejected::SafePointUnavailableWhileStopping { .. } => (
            "safe_point_unavailable",
            "the stopping turn cannot accept this input",
        ),
        Rejected::InterruptAlreadyApplied { .. } => (
            "interrupt_already_applied",
            "an interrupt already owns this turn",
        ),
        Rejected::InterruptUnavailableWhileAwaitingApproval { .. } => (
            "awaiting_approval",
            "the turn is waiting for an approval decision",
        ),
    };
    application_error(StatusCode::CONFLICT, code, reason)
}

async fn session_rates(State(state): State<WebApiState>, RawQuery(query): RawQuery) -> Response {
    let mut sessions = Vec::new();
    for (key, value) in url::form_urlencoded::parse(query.as_deref().unwrap_or("").as_bytes()) {
        if key != "session_id"
            || sessions.len() == usize::from(signalbox_application::max_attention_snapshot_items())
        {
            return invalid_attention_query();
        }
        let Ok(session) = parse_canonical_session_id(&value) else {
            return invalid_attention_query();
        };
        let id = session.into_uuid();
        if sessions.contains(&id) {
            return invalid_attention_query();
        }
        sessions.push(id);
    }
    let (Some(pool), Some(budget)) = (state.pool, state.snapshot_reader_budget) else {
        return attention_projection_error(None);
    };
    let Ok(_permit) = budget.acquire().await else {
        return attention_projection_error(None);
    };
    match read_session_rates(&pool, &sessions).await {
        Ok(sessions) => Json(WebSessionRates { sessions }).into_response(),
        Err(_) => attention_projection_error(None),
    }
}

async fn read_session_rates(
    pool: &PgPool,
    sessions: &[Uuid],
) -> Result<Vec<WebSessionRate>, sqlx::Error> {
    let rows = sqlx::query(
        r#"SELECT lifecycle.session_id, lifecycle.state_kind,
                  counts.turn_count, counts.failed_turn_count,
                  counts.retired_turn_count, counts.completed_turn_count,
                  failure.event_sequence::text AS last_failure_sequence,
                  cause.terminal_provider_failure_cause AS last_provider_cause,
                  goal.event_kind AS goal_disposition
             FROM session_lifecycle AS lifecycle
             CROSS JOIN LATERAL (
                 SELECT count(*)::text AS turn_count,
                        count(*) FILTER (WHERE terminal_disposition_kind = 'failed')::text AS failed_turn_count,
                        count(*) FILTER (WHERE terminal_disposition_kind = 'retired')::text AS retired_turn_count,
                        count(*) FILTER (WHERE terminal_disposition_kind = 'completed')::text AS completed_turn_count
                   FROM turn_lifecycle WHERE session_id = lifecycle.session_id
             ) AS counts
             LEFT JOIN LATERAL (
                 SELECT event_sequence, turn_id FROM turn_terminal_outbox_event
                  WHERE session_id = lifecycle.session_id AND disposition_kind = 'failed'
                  ORDER BY event_sequence DESC LIMIT 1
             ) AS failure ON true
             LEFT JOIN LATERAL (
                 SELECT call.terminal_provider_failure_cause FROM model_call AS call
                   JOIN model_call_transition_outbox_event AS transition USING (model_call_id)
                  WHERE call.session_id = lifecycle.session_id AND call.turn_id = failure.turn_id
                    AND transition.call_state_kind = 'terminal'
                    AND transition.event_sequence < failure.event_sequence
                  ORDER BY transition.event_sequence DESC LIMIT 1
             ) AS cause ON true
             LEFT JOIN LATERAL (
                 SELECT event_kind FROM goal_event WHERE session_id = lifecycle.session_id
                  ORDER BY event_ordinal DESC LIMIT 1
             ) AS goal ON true
            WHERE lifecycle.session_id = ANY($1)
            ORDER BY lifecycle.session_id"#,
    ).bind(sessions).fetch_all(pool).await?;
    rows.iter()
        .map(|row| {
            let decode_error = |error| sqlx::Error::Decode(Box::new(error));
            let count = |name| -> Result<WebU64, sqlx::Error> {
                Ok(WebU64::from_u64(
                    row.try_get::<String, _>(name)?
                        .parse()
                        .map_err(decode_error)?,
                ))
            };
            let variant = |name| -> Result<Option<serde_json::Value>, sqlx::Error> {
                Ok(row
                    .try_get::<Option<String>, _>(name)?
                    .map(serde_json::Value::String))
            };
            Ok(WebSessionRate {
                session_id: WebSessionId::from_uuid_bytes(
                    row.try_get::<Uuid, _>("session_id")?.into_bytes(),
                ),
                lifecycle_state: serde_json::from_value(serde_json::Value::String(
                    row.try_get("state_kind")?,
                ))
                .map_err(|error| sqlx::Error::Decode(Box::new(error)))?,
                turn_count: count("turn_count")?,
                failed_turn_count: count("failed_turn_count")?,
                retired_turn_count: count("retired_turn_count")?,
                completed_turn_count: count("completed_turn_count")?,
                last_failure_sequence: row
                    .try_get::<Option<String>, _>("last_failure_sequence")?
                    .map(|value| value.parse().map(WebU64::from_u64).map_err(decode_error))
                    .transpose()?,
                last_provider_cause: variant("last_provider_cause")?
                    .map(serde_json::from_value)
                    .transpose()
                    .map_err(|error| sqlx::Error::Decode(Box::new(error)))?,
                goal_disposition: variant("goal_disposition")?
                    .map(serde_json::from_value)
                    .transpose()
                    .map_err(|error| sqlx::Error::Decode(Box::new(error)))?,
            })
        })
        .collect()
}

async fn wait_for_web_shutdown(shutdown: &mut Option<watch::Receiver<bool>>) {
    let Some(shutdown) = shutdown else {
        std::future::pending::<()>().await;
        return;
    };
    while !*shutdown.borrow() {
        if shutdown.changed().await.is_err() {
            return;
        }
    }
}

mod response;
use response::{api_not_found, static_assets_not_configured};
pub(crate) use response::{application_error, transport_error};

mod origin;
#[cfg(test)]
use origin::has_loopback_host;
use origin::{
    has_content_type, has_json_content_type, validate_loopback_host, validate_supplied_origin,
};

mod body;
#[cfg(test)]
use body::{NdjsonItemWriter, encode_ndjson_item};
pub use body::{decode_bounded_json, ndjson_response};
pub(crate) use body::{decode_bounded_utf8, validate_json_mutation, validate_text_mutation};

mod test_routers;
use test_routers::contract_bootstrap;
pub use test_routers::deterministic_test_router;

mod blob;
#[cfg(test)]
use blob::{
    applicable_range_header, content_disposition, if_none_match, if_range_matches,
    parse_byte_range, reader_body_until, single_range_header, try_acquire_web_blob_read_permit,
};
use blob::{blob_content, blob_descriptor, blob_descriptor_head, blob_download};

mod search;
use search::{search, web_uuid};

mod usage;
#[cfg(test)]
use usage::{usage_aggregate_cost_dto, usage_cost_dto};
use usage::{usage_calls, usage_summary};

mod timeline;
#[cfg(test)]
use timeline::{TimelineDetailQuery, parse_detail_query};
use timeline::{
    address_dto, event_kind_dto, parse_session_id, session_descriptor,
    session_timeline_item_detail, session_timeline_region_detail, session_timeline_turn_detail,
    session_timeline_window,
};

mod session_live;
use session_live::{session_live_follow, session_live_snapshot};

mod session_catalog;
#[cfg(test)]
use session_catalog::parse_session_catalog_query;
use session_catalog::{
    SessionCatalogQuery, parse_canonical_session_id, parse_catalog_canonical_u64, session_catalog,
};

mod attention;
#[cfg(test)]
use attention::attention_summary_dto;
#[cfg(test)]
use attention::{
    attention_changes_require_resync, attention_snapshot_dto, page_scoped_attention_summaries,
};
use attention::{
    attention_follow, attention_goal_block_dto, attention_projection_error, attention_snapshot,
    empty_ndjson_response, invalid_attention_query, parse_attention_query, web_attention_action,
    web_attention_activity_kind, web_attention_state,
};

#[cfg(test)]
mod tests;
