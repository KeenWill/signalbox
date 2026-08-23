//! Browser-facing same-origin HTTP transport foundation.
//!
//! This boundary owns browser HTTP semantics and browser DTOs. It does not
//! expose local process-protocol messages, storage records, or application
//! authentication.

use std::{
    collections::BTreeSet,
    env,
    error::Error,
    ffi::OsString,
    fmt, io,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::PathBuf,
    sync::Arc,
    time::{Duration, UNIX_EPOCH},
};

use axum::{
    Json, Router,
    body::{Body, Bytes, to_bytes},
    extract::{DefaultBodyLimit, Query, Request, State, rejection::QueryRejection},
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
    AttentionSnapshot, AttentionState, AttentionSummary, max_attention_goal_summary_characters,
};
use signalbox_domain::SessionId;
use signalbox_persistence::attention::{AttentionRepository, AttentionRepositoryError};
use signalbox_web_contract::{
    MAX_JSON_BODY_BYTES, MAX_NDJSON_ITEM_BYTES, WebApiError, WebApiErrorKind, WebApiErrorResponse,
    WebAttentionAction, WebAttentionActivity, WebAttentionActivityKind, WebAttentionBlockedReason,
    WebAttentionGoalBlock, WebAttentionJudgeFacts, WebAttentionSnapshot, WebAttentionState,
    WebAttentionStreamEvent, WebAttentionSummary, WebContractBootstrap, WebContractExample,
};
use sqlx::{PgPool, types::Uuid};
use tokio::{
    net::TcpListener,
    sync::{Semaphore, watch},
};
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
    follow_shutdown: Option<watch::Sender<bool>>,
}

impl WebHttpRuntime {
    /// Binds the production same-origin router.
    pub async fn bind(
        configuration: WebHttpConfiguration,
        pool: PgPool,
    ) -> Result<Self, WebHttpRuntimeError> {
        let snapshot_reader_budget = super::process_runtime::shared_snapshot_reader_budget(
            pool.options().get_max_connections(),
        );
        Self::bind_production(configuration, pool, snapshot_reader_budget).await
    }

    /// Binds production HTTP reads to the daemon-wide snapshot-reader budget.
    pub async fn bind_with_snapshot_reader_budget(
        configuration: WebHttpConfiguration,
        pool: PgPool,
        snapshot_reader_budget: Arc<Semaphore>,
    ) -> Result<Self, WebHttpRuntimeError> {
        Self::bind_production(configuration, pool, Some(snapshot_reader_budget)).await
    }

    async fn bind_production(
        configuration: WebHttpConfiguration,
        pool: PgPool,
        snapshot_reader_budget: Option<Arc<Semaphore>>,
    ) -> Result<Self, WebHttpRuntimeError> {
        let (follow_shutdown, follow_shutdown_receiver) = watch::channel(false);
        let router = production_router_with_budget(
            configuration.asset_root,
            Some(pool),
            snapshot_reader_budget,
            Some(follow_shutdown_receiver),
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
pub fn production_router(asset_root: Option<PathBuf>, pool: Option<PgPool>) -> Router {
    let snapshot_reader_budget = pool.as_ref().and_then(|pool| {
        super::process_runtime::shared_snapshot_reader_budget(pool.options().get_max_connections())
    });
    production_router_with_budget(asset_root, pool, snapshot_reader_budget, None)
}

fn production_router_with_budget(
    asset_root: Option<PathBuf>,
    pool: Option<PgPool>,
    snapshot_reader_budget: Option<Arc<Semaphore>>,
    shutdown: Option<watch::Receiver<bool>>,
) -> Router {
    let state = WebApiState {
        attention: pool.clone().map(AttentionRepository::new),
        snapshot_reader_budget: snapshot_reader_budget.clone(),
        shutdown,
    };
    let api = Router::new()
        .route("/bootstrap", get(contract_bootstrap))
        .route("/attention", get(attention_snapshot))
        .route("/attention/follow", get(attention_follow))
        .with_state(state)
        .merge(crate::web_repo_watch::router(pool, snapshot_reader_budget))
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
    attention: Option<AttentionRepository>,
    snapshot_reader_budget: Option<Arc<Semaphore>>,
    shutdown: Option<watch::Receiver<bool>>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AttentionSnapshotQuery {
    after_session_id: Option<String>,
}

async fn attention_snapshot(
    State(state): State<WebApiState>,
    query: Result<Query<AttentionSnapshotQuery>, QueryRejection>,
) -> Response {
    let Query(query) = match query {
        Ok(query) => query,
        Err(_) => {
            return transport_error(
                StatusCode::BAD_REQUEST,
                "invalid_query_parameters",
                "attention query parameters are invalid",
            );
        }
    };
    let Some(repository) = state.attention else {
        return application_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "attention_projection_unavailable",
            "attention projection is not configured",
        );
    };
    let after = match query.after_session_id {
        Some(value) => match value.parse::<Uuid>() {
            Ok(value) => Some(SessionId::from_uuid(value)),
            Err(_) => {
                return application_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_session_id",
                    "attention continuation is not a UUID",
                );
            }
        },
        None => None,
    };
    let Some(budget) = state.snapshot_reader_budget else {
        return attention_projection_error(None);
    };
    let Ok(_permit) = budget.acquire().await else {
        return attention_projection_error(None);
    };
    match repository.snapshot(after).await {
        Ok(snapshot) => match attention_snapshot_dto(snapshot) {
            Ok(snapshot) => Json(snapshot).into_response(),
            Err(()) => attention_projection_error(None),
        },
        Err(error) => attention_projection_error(Some(error)),
    }
}

async fn attention_follow(State(state): State<WebApiState>) -> Response {
    let mut shutdown = state.shutdown;
    let Some(repository) = state.attention else {
        return application_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "attention_projection_unavailable",
            "attention projection is not configured",
        );
    };
    let Some(budget) = state.snapshot_reader_budget else {
        return attention_projection_error(None);
    };
    let snapshot_permit = tokio::select! {
        () = wait_for_web_shutdown(&mut shutdown) => return empty_ndjson_response(),
        permit = Arc::clone(&budget).acquire_owned() => permit,
    };
    let Ok(snapshot_permit) = snapshot_permit else {
        return attention_projection_error(None);
    };
    let snapshot = match tokio::select! {
        () = wait_for_web_shutdown(&mut shutdown) => return empty_ndjson_response(),
        snapshot = repository.snapshot(None) => snapshot,
    } {
        Ok(snapshot) => snapshot,
        Err(error) => return attention_projection_error(Some(error)),
    };
    drop(snapshot_permit);
    let cursor = snapshot.cursor;
    let live_page_has_capacity = snapshot.continuation_after.is_none();
    let visible_sessions = snapshot
        .summaries
        .iter()
        .map(|summary| summary.session)
        .collect::<BTreeSet<_>>();
    let snapshot = match attention_snapshot_dto(snapshot) {
        Ok(snapshot) => snapshot,
        Err(()) => return attention_projection_error(None),
    };
    let source = stream::unfold(
        (
            repository,
            Some(WebAttentionStreamEvent::Snapshot { snapshot }),
            cursor,
            visible_sessions,
            live_page_has_capacity,
            budget,
            shutdown,
            AttentionFollowDisposition::Continue,
        ),
        |(
            repository,
            pending,
            cursor,
            visible_sessions,
            live_page_has_capacity,
            budget,
            mut shutdown,
            disposition,
        )| async move {
            if shutdown.as_ref().is_some_and(|shutdown| *shutdown.borrow()) {
                return None;
            }
            if let Some(event) = pending {
                return Some((
                    event,
                    (
                        repository,
                        None,
                        cursor,
                        visible_sessions,
                        live_page_has_capacity,
                        budget,
                        shutdown,
                        AttentionFollowDisposition::Continue,
                    ),
                ));
            }
            if disposition == AttentionFollowDisposition::End {
                return None;
            }
            let mut cursor = cursor;
            let mut delay = Duration::from_millis(250);
            loop {
                tokio::select! {
                    () = wait_for_web_shutdown(&mut shutdown) => return None,
                    () = tokio::time::sleep(delay) => {}
                }
                let permit = tokio::select! {
                    () = wait_for_web_shutdown(&mut shutdown) => return None,
                    permit = Arc::clone(&budget).acquire_owned() => permit,
                };
                let Ok(_permit) = permit else {
                    return None;
                };
                let changes = tokio::select! {
                    () = wait_for_web_shutdown(&mut shutdown) => return None,
                    changes = repository.changes_after(cursor) => changes,
                };
                match changes {
                    Ok(AttentionChanges::Updated {
                        cursor: next,
                        summaries,
                    }) if summaries.is_empty() => {
                        cursor = next;
                        delay = delay.saturating_mul(2).min(Duration::from_secs(4));
                    }
                    Ok(AttentionChanges::Updated {
                        cursor: next,
                        summaries,
                    }) => {
                        if attention_changes_require_resync(
                            &summaries,
                            &visible_sessions,
                            live_page_has_capacity,
                        ) {
                            return Some((
                                WebAttentionStreamEvent::ResyncRequired {
                                    cursor: next.value().to_string(),
                                },
                                (
                                    repository,
                                    None,
                                    next,
                                    visible_sessions,
                                    live_page_has_capacity,
                                    budget,
                                    shutdown,
                                    AttentionFollowDisposition::End,
                                ),
                            ));
                        }
                        let summaries =
                            page_scoped_attention_summaries(summaries, &visible_sessions)
                                .into_iter()
                                .map(attention_summary_dto)
                                .collect::<Result<Vec<_>, _>>()
                                .ok()?;
                        if summaries.is_empty() {
                            cursor = next;
                            continue;
                        }
                        return Some((
                            WebAttentionStreamEvent::Update {
                                cursor: next.value().to_string(),
                                summaries,
                            },
                            (
                                repository,
                                None,
                                next,
                                visible_sessions,
                                live_page_has_capacity,
                                budget,
                                shutdown,
                                AttentionFollowDisposition::Continue,
                            ),
                        ));
                    }
                    Ok(AttentionChanges::ResyncRequired { cursor: next }) => {
                        return Some((
                            WebAttentionStreamEvent::ResyncRequired {
                                cursor: next.value().to_string(),
                            },
                            (
                                repository,
                                None,
                                next,
                                visible_sessions,
                                live_page_has_capacity,
                                budget,
                                shutdown,
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

fn page_scoped_attention_summaries(
    summaries: Vec<AttentionSummary>,
    visible_sessions: &BTreeSet<SessionId>,
) -> Vec<AttentionSummary> {
    summaries
        .into_iter()
        .filter(|summary| visible_sessions.contains(&summary.session))
        .collect()
}

fn attention_changes_require_resync(
    summaries: &[AttentionSummary],
    visible_sessions: &BTreeSet<SessionId>,
    live_page_has_capacity: bool,
) -> bool {
    live_page_has_capacity
        && summaries
            .iter()
            .any(|summary| !visible_sessions.contains(&summary.session))
}

fn empty_ndjson_response() -> Response {
    ndjson_response(stream::empty::<WebAttentionStreamEvent>())
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AttentionFollowDisposition {
    Continue,
    End,
}

fn attention_snapshot_dto(snapshot: AttentionSnapshot) -> Result<WebAttentionSnapshot, ()> {
    Ok(WebAttentionSnapshot {
        cursor: snapshot.cursor.value().to_string(),
        summaries: snapshot
            .summaries
            .into_iter()
            .map(attention_summary_dto)
            .collect::<Result<Vec<_>, _>>()?,
        continuation_after_session_id: snapshot
            .continuation_after
            .map(|session| session.into_uuid().to_string()),
    })
}

pub(crate) fn attention_summary_dto(summary: AttentionSummary) -> Result<WebAttentionSummary, ()> {
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
        current_turn_id: summary
            .current_turn
            .map(|turn| turn.into_uuid().to_string()),
        state: match summary.state {
            AttentionState::Active => WebAttentionState::Active,
            AttentionState::Queued => WebAttentionState::Queued,
            AttentionState::Blocked => WebAttentionState::Blocked,
            AttentionState::AwaitingApproval => WebAttentionState::AwaitingApproval,
            AttentionState::Ambiguous => WebAttentionState::Ambiguous,
            AttentionState::AwaitingToolRecovery => WebAttentionState::AwaitingToolRecovery,
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

pub(crate) fn application_error(
    status: StatusCode,
    code: &'static str,
    message: &'static str,
) -> Response {
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
        collections::BTreeSet,
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
        AttentionCursor, AttentionGoalBlock, AttentionJudgeFacts, AttentionSnapshot,
        AttentionState, AttentionSummary, max_attention_change_items,
        max_attention_goal_summary_characters, max_attention_snapshot_items,
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
        DEFAULT_WEB_BIND_ADDRESS, WebHttpConfiguration, WebHttpConfigurationError, WebHttpRuntime,
        attention_snapshot_dto, deterministic_test_router, ndjson_response, production_router,
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
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://signalbox:signalbox@localhost/signalbox")
            .expect("the unused fixture pool URL is valid");
        let runtime = WebHttpRuntime::bind(
            WebHttpConfiguration::new(loopback_ephemeral(), Some(assets.path().to_path_buf())),
            pool,
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
    async fn attention_snapshot_requires_projection_configuration() {
        let request = Request::get("/api/attention")
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

    #[tokio::test]
    async fn attention_snapshot_query_rejection_uses_typed_transport_error() {
        let request = Request::get("/api/attention?unexpected=true")
            .body(Body::empty())
            .expect("the request is valid");
        let response = production_router(None, None)
            .oneshot(request)
            .await
            .expect("the production router responds");
        let status = response.status();
        let body: serde_json::Value = serde_json::from_slice(&response_body(response).await)
            .expect("the typed transport failure is JSON");

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"]["kind"], "transport");
        assert_eq!(body["error"]["code"], "invalid_query_parameters");
    }

    #[tokio::test]
    async fn attention_follow_requires_projection_configuration() {
        let request = Request::get("/api/attention/follow")
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

    #[tokio::test]
    async fn attention_follower_wait_stops_when_web_shutdown_begins() {
        let (shutdown, shutdown_receiver) = watch::channel(false);
        let mut shutdown_receiver = Some(shutdown_receiver);
        let waiting = tokio::spawn(async move {
            super::wait_for_web_shutdown(&mut shutdown_receiver).await;
        });

        shutdown
            .send(true)
            .expect("the follower still observes web shutdown");
        tokio::time::timeout(Duration::from_secs(1), waiting)
            .await
            .expect("the follower wait exits promptly on shutdown")
            .expect("the follower wait task completes cleanly");
    }

    #[test]
    fn attention_follow_filters_changes_to_the_visible_snapshot_page() {
        let visible = SessionId::from_uuid(Uuid::from_u128(1));
        let off_page = SessionId::from_uuid(Uuid::from_u128(2));
        let summary = |session| AttentionSummary {
            session,
            current_turn: None,
            state: AttentionState::Idle,
            action: None,
            goal_block: None,
            judge: AttentionJudgeFacts {
                actionable: 0,
                completed: 0,
                escalated: 0,
                failed: 0,
            },
            last_activity: AttentionActivity {
                recorded_at: UNIX_EPOCH,
                kind: AttentionActivityKind::Session,
            },
        };
        let visible_sessions = BTreeSet::from([visible]);

        let scoped = super::page_scoped_attention_summaries(
            vec![summary(off_page), summary(visible)],
            &visible_sessions,
        );

        assert_eq!(scoped.len(), 1);
        assert_eq!(scoped[0].session, visible);
    }

    #[test]
    fn attention_follow_resyncs_for_a_new_identity_on_a_partial_live_page() {
        let visible = SessionId::from_uuid(Uuid::from_u128(1));
        let new_session = SessionId::from_uuid(Uuid::from_u128(2));
        let summary = AttentionSummary {
            session: new_session,
            current_turn: None,
            state: AttentionState::Idle,
            action: None,
            goal_block: None,
            judge: AttentionJudgeFacts {
                actionable: 0,
                completed: 0,
                escalated: 0,
                failed: 0,
            },
            last_activity: AttentionActivity {
                recorded_at: UNIX_EPOCH,
                kind: AttentionActivityKind::Session,
            },
        };
        let visible_sessions = BTreeSet::from([visible]);

        assert!(super::attention_changes_require_resync(
            std::slice::from_ref(&summary),
            &visible_sessions,
            true,
        ));
        assert!(!super::attention_changes_require_resync(
            &[summary],
            &visible_sessions,
            false,
        ));
    }

    #[test]
    fn attention_summary_bound_is_enforced_and_maximum_snapshot_fits_one_ndjson_item() {
        let session = SessionId::from_uuid(Uuid::from_u128(u128::MAX));
        let summary = AttentionSummary {
            session,
            current_turn: Some(TurnId::from_uuid(Uuid::from_u128(u128::MAX))),
            state: AttentionState::Blocked,
            action: Some(AttentionAction::ProvideGoalNeed),
            goal_block: Some(AttentionGoalBlock {
                generation: u64::MAX,
                reason: AttentionBlockedReason::ExternalChangeRequired,
                need_summary: String::from('\u{1}')
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
            summaries: vec![summary; usize::from(max_attention_snapshot_items())],
            continuation_after: Some(session),
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

    #[test]
    fn maximum_attention_update_fits_one_ndjson_item() {
        let session = SessionId::from_uuid(Uuid::from_u128(u128::MAX));
        let summary = AttentionSummary {
            session,
            current_turn: Some(TurnId::from_uuid(Uuid::from_u128(u128::MAX))),
            state: AttentionState::Blocked,
            action: Some(AttentionAction::ProvideGoalNeed),
            goal_block: Some(AttentionGoalBlock {
                generation: u64::MAX,
                reason: AttentionBlockedReason::ExternalChangeRequired,
                need_summary: String::from('\u{1}')
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
        let summaries = vec![summary; usize::from(max_attention_change_items())]
            .into_iter()
            .map(super::attention_summary_dto)
            .collect::<Result<Vec<_>, _>>()
            .expect("maximum summaries are representable");
        let event = WebAttentionStreamEvent::Update {
            cursor: u64::MAX.to_string(),
            summaries,
        };
        let mut writer = super::NdjsonItemWriter::new();

        serde_json::to_writer(&mut writer, &event)
            .expect("the maximum update serializes within one item");
        writer
            .write_all(b"\n")
            .expect("the NDJSON terminator fits the item");

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
