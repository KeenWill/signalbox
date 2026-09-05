//! Authenticated, bounded GitHub webhook HTTP admission.

use std::{
    collections::HashMap,
    error::Error,
    fmt,
    future::Future,
    net::TcpListener as StdTcpListener,
    num::NonZeroU64,
    pin::Pin,
    sync::{Arc, Mutex, PoisonError},
    task::{Context, Poll},
    time::{Duration, Instant},
};

use axum::{
    Router,
    body::{Body, Bytes, to_bytes},
    extract::{DefaultBodyLimit, State},
    http::{HeaderMap, HeaderValue, Request, StatusCode},
    routing::post,
};
use hmac::{Hmac, KeyInit, Mac};
use hyper::server::conn::http1;
use hyper_util::{
    rt::{TokioIo, TokioTimer},
    server::graceful::GracefulShutdown,
    service::TowerToHyperService,
};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use signalbox_domain::RepositorySlug;
use signalbox_model_runtime::{CredentialAccess, CredentialReference};
use signalbox_persistence::repo_watch_webhook::{
    PostgresRepoWatchWebhookStore, RepoWatchWebhookAdmission, RepoWatchWebhookAdmissionOutcome,
    RepoWatchWebhookDeliveryKey,
};
use sqlx::PgPool;
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    net::{TcpListener, TcpStream},
    select,
    sync::{OwnedSemaphorePermit, Semaphore, watch},
    time::{Instant as TokioInstant, Sleep, sleep, timeout},
};
use uuid::Uuid;

use crate::configuration::{
    FileCredentialAccess, RepositoryWatchConfiguration, RepositoryWatchWebhookConfiguration,
};

const HEADER_CONTENT_ENCODING: &str = "content-encoding";
const HEADER_CONTENT_LENGTH: &str = "content-length";
const HEADER_CONTENT_TYPE: &str = "content-type";
const HEADER_DELIVERY: &str = "x-github-delivery";
const HEADER_EVENT: &str = "x-github-event";
const HEADER_HOOK_ID: &str = "x-github-hook-id";
const HEADER_SIGNATURE: &str = "x-hub-signature-256";
const HEADER_TRANSFER_ENCODING: &str = "transfer-encoding";
const JSON_CONTENT_TYPE: &str = "application/json";
const SHA256_SIGNATURE_PREFIX: &str = "sha256=";
const SHA256_BYTES: usize = 32;
const MAX_EVENT_NAME_BYTES: usize = 64;
const MAX_ACTION_NAME_BYTES: usize = 64;
const MAX_WEBHOOK_SECRET_BYTES: usize = 64 * 1024;

/// Hard safety ceiling protecting admission memory from one GitHub request.
pub(crate) const MAX_WEBHOOK_BODY_BYTES: usize = 25 * 1024 * 1024;
/// Hard safety ceiling protecting the daemon from concurrent body retention.
pub(crate) const MAX_WEBHOOK_IN_FLIGHT: usize = 64;
/// Hard safety ceiling on what every in-flight body may retain together. The
/// per-request and concurrency ceilings alone would admit 1.5 GiB of buffered
/// request bodies from peers that have not yet proved the shared secret.
pub(crate) const MAX_WEBHOOK_IN_FLIGHT_BYTES: usize = 128 * 1024 * 1024;
/// Hard safety ceiling protecting one hook from sustained authenticated floods.
pub(crate) const MAX_WEBHOOK_DELIVERIES_PER_MINUTE: u32 = 3_000;
/// Hard safety ceiling on connections held at once, including those that have
/// sent no complete request yet. Nothing the handler bounds begins until whole
/// request headers arrive, so this is what keeps a peer that opens sockets and
/// withholds headers from exhausting daemon descriptors. It is taken at accept
/// time, before any router or handler work.
pub(crate) const MAX_WEBHOOK_CONNECTIONS: usize = 256;
/// Hard safety ceiling on the aggregate header-field bytes one request may
/// carry, counted across every name and value. GitHub's own delivery headers
/// are a small fraction of this.
pub(crate) const MAX_WEBHOOK_HEADER_BYTES: usize = 32 * 1024;
/// Hard safety ceiling on how many header fields one request head may carry,
/// which bounds head parsing independently of their total size.
pub(crate) const MAX_WEBHOOK_HEADER_COUNT: usize = 64;
/// Hard safety ceiling on the read buffer one connection may grow while its
/// head is still incomplete. This is the memory bound below the router, which
/// refuses a head that cannot fit it; the exact aggregate ceiling above is
/// enforced on the parsed head before any credential or body work.
const MAX_WEBHOOK_HEAD_BUFFER_BYTES: usize = 64 * 1024;
/// How long a graceful shutdown waits for connections already being served.
const WEBHOOK_SHUTDOWN_GRACE: Duration = Duration::from_secs(10);
/// Hard safety deadline for one connection read to make progress. It covers the
/// request line and headers, which are read before any handler runs.
pub(crate) const WEBHOOK_CONNECTION_READ_TIMEOUT: Duration = Duration::from_secs(15);
/// Hard safety deadline for reading one request body. A peer that opens a
/// request and then stalls its body would otherwise hold a concurrency permit
/// and its share of the memory budget indefinitely.
pub(crate) const WEBHOOK_BODY_READ_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a failed accept waits before the listener tries again, so a
/// persistent listener fault cannot become a busy loop.
const WEBHOOK_ACCEPT_RETRY_DELAY: Duration = Duration::from_secs(1);

const RATE_WINDOW: Duration = Duration::from_secs(60);
/// How finely the rolling window is counted. Admissions are attributed to the
/// bucket they land in, so a burst is counted where it actually happened rather
/// than smeared across the window it belongs to.
///
/// One bucket more than the window spans is kept, because the oldest bucket
/// straddles the window's edge: dropping it whole would discard admissions that
/// are still inside the trailing minute. Keeping it counts a few that have just
/// left instead, which errs toward refusing rather than admitting.
const WEBHOOK_RATE_BUCKETS: usize = 7;
/// One bucket's span, which is `RATE_WINDOW` divided by the bucket count.
const RATE_BUCKET: Duration = Duration::from_secs(10);
/// Granularity of the shared body-memory budget, so one request reserves close
/// to what it may actually buffer instead of one indivisible slot.
const WEBHOOK_BODY_BUDGET_GRANULE_BYTES: usize = 64 * 1024;
const WEBHOOK_BODY_BUDGET_GRANULES: usize =
    MAX_WEBHOOK_IN_FLIGHT_BYTES / WEBHOOK_BODY_BUDGET_GRANULE_BYTES;

/// Why the configured webhook listener could not be constructed.
#[derive(Debug)]
pub(crate) enum RepoWatchWebhookRuntimeConstructionError {
    Bind(std::io::Error),
    MissingRepositoryWorker,
    Socket(std::io::Error),
}

impl fmt::Display for RepoWatchWebhookRuntimeConstructionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Bind(error) => write!(formatter, "could not bind webhook listener: {error}"),
            Self::MissingRepositoryWorker => {
                formatter.write_str("webhook repository has no repository-watch worker")
            }
            Self::Socket(error) => write!(formatter, "could not configure webhook socket: {error}"),
        }
    }
}

impl Error for RepoWatchWebhookRuntimeConstructionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Bind(error) | Self::Socket(error) => Some(error),
            Self::MissingRepositoryWorker => None,
        }
    }
}

/// Why the webhook listener ended before daemon shutdown.
#[derive(Debug)]
pub(crate) struct RepoWatchWebhookRuntimeError(std::io::Error);

impl fmt::Display for RepoWatchWebhookRuntimeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "repository-watch webhook listener failed: {}",
            self.0
        )
    }
}

impl Error for RepoWatchWebhookRuntimeError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.0)
    }
}

pub(crate) struct RepoWatchWebhookRuntime {
    listener: StdTcpListener,
    path: Arc<str>,
    state: WebhookHttpState,
}

impl RepoWatchWebhookRuntime {
    pub(crate) fn try_new(
        pool: PgPool,
        configuration: &RepositoryWatchConfiguration,
        mut workers: HashMap<RepositorySlug, Arc<watch::Sender<()>>>,
    ) -> Result<Option<Self>, RepoWatchWebhookRuntimeConstructionError> {
        let Some(listener_configuration) = configuration.webhook() else {
            return Ok(None);
        };
        let listener = bind_listener(listener_configuration)?;
        let mut hooks = HashMap::new();
        for repository in configuration.repositories() {
            let Some(webhook) = repository.webhook() else {
                continue;
            };
            let nudge = workers
                .remove(repository.repository())
                .ok_or(RepoWatchWebhookRuntimeConstructionError::MissingRepositoryWorker)?;
            let secret_reference = repository
                .webhook_secret_reference()
                .ok_or(RepoWatchWebhookRuntimeConstructionError::MissingRepositoryWorker)?;
            hooks.insert(
                webhook.hook_id(),
                WebhookHookBinding {
                    repository: repository.repository().clone(),
                    secret: FileCredentialAccess::new_bounded(
                        webhook.secret_file().to_path_buf(),
                        secret_reference.clone(),
                        MAX_WEBHOOK_SECRET_BYTES,
                    ),
                    secret_reference,
                    nudge,
                },
            );
        }
        Ok(Some(Self {
            listener,
            path: Arc::from(listener_configuration.path()),
            state: WebhookHttpState {
                hooks: Arc::new(hooks),
                store: PostgresRepoWatchWebhookStore::new(pool),
                in_flight: Arc::new(Semaphore::new(MAX_WEBHOOK_IN_FLIGHT)),
                body_budget: Arc::new(Semaphore::new(WEBHOOK_BODY_BUDGET_GRANULES)),
                body_read_timeout: WEBHOOK_BODY_READ_TIMEOUT,
                rate_limiter: Arc::new(WebhookRateLimiter::new()),
            },
        }))
    }

    pub(crate) async fn run(
        self,
        shutdown: watch::Receiver<bool>,
    ) -> Result<(), RepoWatchWebhookRuntimeError> {
        if *shutdown.borrow() {
            return Ok(());
        }
        let listener =
            TcpListener::from_std(self.listener).map_err(RepoWatchWebhookRuntimeError)?;
        let listener = BoundedWebhookListener {
            listener,
            connections: Arc::new(Semaphore::new(MAX_WEBHOOK_CONNECTIONS)),
            read_timeout: WEBHOOK_CONNECTION_READ_TIMEOUT,
        };
        let router = Router::new()
            .route(&self.path, post(admit_webhook))
            .layer(DefaultBodyLimit::max(MAX_WEBHOOK_BODY_BYTES))
            .with_state(self.state);
        serve_bounded_webhook_connections(listener, router, shutdown).await;
        Ok(())
    }
}

fn bind_listener(
    configuration: &RepositoryWatchWebhookConfiguration,
) -> Result<StdTcpListener, RepoWatchWebhookRuntimeConstructionError> {
    let listener = StdTcpListener::bind(configuration.bind_address())
        .map_err(RepoWatchWebhookRuntimeConstructionError::Bind)?;
    listener
        .set_nonblocking(true)
        .map_err(RepoWatchWebhookRuntimeConstructionError::Socket)?;
    Ok(listener)
}

/// Accepts connections under a fixed budget and a read deadline.
///
/// Axum builds its Hyper connection without the timer Hyper's own header-read
/// deadline needs, so nothing bounds a peer between accept and the first
/// complete request. These two bounds do: the budget caps how many such peers
/// can exist, and the deadline retires any one that stops making progress.
pub(crate) struct BoundedWebhookListener {
    listener: TcpListener,
    connections: Arc<Semaphore>,
    read_timeout: Duration,
}

impl BoundedWebhookListener {
    async fn accept(&mut self) -> DeadlinedConnection<TcpStream> {
        loop {
            // Taken before accepting, so a peer beyond the budget waits in the
            // kernel backlog rather than holding a daemon descriptor. Nothing
            // router-side or handler-side has run at this point.
            let permit = match Arc::clone(&self.connections).acquire_owned().await {
                Ok(permit) => permit,
                // This listener owns the budget and never closes it. Were that
                // ever to change, admitting nothing is the safe reading.
                Err(_) => std::future::pending().await,
            };
            match self.listener.accept().await {
                Ok((stream, _address)) => {
                    return DeadlinedConnection::new(stream, self.read_timeout, permit);
                }
                Err(error) => {
                    drop(permit);
                    tracing::warn!(
                        cause_code = "webhook_accept_failed",
                        error = %error,
                        "repository-watch webhook listener could not accept a connection"
                    );
                    sleep(WEBHOOK_ACCEPT_RETRY_DELAY).await;
                }
            }
        }
    }
}

/// Serves accepted connections under the raw-head ceilings.
///
/// Hyper is driven directly because `axum::serve` builds its connection with
/// default HTTP/1 settings, and the head ceilings sit on that builder: a
/// request head that exceeds either is refused before the router exists.
pub(crate) async fn serve_bounded_webhook_connections(
    mut listener: BoundedWebhookListener,
    router: Router,
    shutdown: watch::Receiver<bool>,
) {
    let graceful = GracefulShutdown::new();
    let mut stopping = Box::pin(webhook_shutdown(shutdown));
    loop {
        let connection = select! {
            () = &mut stopping => break,
            connection = listener.accept() => connection,
        };
        let served = http1::Builder::new()
            .timer(TokioTimer::new())
            .max_headers(MAX_WEBHOOK_HEADER_COUNT)
            .max_buf_size(MAX_WEBHOOK_HEAD_BUFFER_BYTES)
            .header_read_timeout(WEBHOOK_CONNECTION_READ_TIMEOUT)
            .serve_connection(
                TokioIo::new(connection),
                TowerToHyperService::new(router.clone()),
            );
        let watched = graceful.watch(served);
        tokio::spawn(async move {
            if let Err(error) = watched.await {
                tracing::debug!(
                    cause_code = "webhook_connection_failed",
                    error = %error,
                    "repository-watch webhook connection ended before completing a request"
                );
            }
        });
    }
    // A connection that never finishes must not hold daemon shutdown open.
    if timeout(WEBHOOK_SHUTDOWN_GRACE, graceful.shutdown())
        .await
        .is_err()
    {
        tracing::warn!(
            cause_code = "webhook_shutdown_deadline",
            "repository-watch webhook connections outlived the shutdown grace period"
        );
    }
}

/// One accepted connection that fails once a read stops making progress.
///
/// The deadline restarts on every byte received, so it retires stalled peers
/// rather than capping a connection's lifetime; a peer that keeps dripping
/// bytes is bounded by the connection budget instead.
pub(crate) struct DeadlinedConnection<Stream> {
    stream: Stream,
    deadline: Pin<Box<Sleep>>,
    read_timeout: Duration,
    _permit: OwnedSemaphorePermit,
}

impl<Stream> DeadlinedConnection<Stream> {
    pub(crate) fn new(
        stream: Stream,
        read_timeout: Duration,
        permit: OwnedSemaphorePermit,
    ) -> Self {
        Self {
            stream,
            deadline: Box::pin(sleep(read_timeout)),
            read_timeout,
            _permit: permit,
        }
    }
}

impl<Stream: AsyncRead + Unpin> AsyncRead for DeadlinedConnection<Stream> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let connection = self.as_mut().get_mut();
        match Pin::new(&mut connection.stream).poll_read(context, buffer) {
            Poll::Ready(result) => {
                connection
                    .deadline
                    .as_mut()
                    .reset(TokioInstant::now() + connection.read_timeout);
                Poll::Ready(result)
            }
            Poll::Pending => match connection.deadline.as_mut().poll(context) {
                Poll::Ready(()) => Poll::Ready(Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "repository-watch webhook connection stalled before completing a request",
                ))),
                Poll::Pending => Poll::Pending,
            },
        }
    }
}

impl<Stream: AsyncWrite + Unpin> AsyncWrite for DeadlinedConnection<Stream> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.as_mut().get_mut().stream).poll_write(context, buffer)
    }

    fn poll_flush(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.as_mut().get_mut().stream).poll_flush(context)
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.as_mut().get_mut().stream).poll_shutdown(context)
    }
}

async fn webhook_shutdown(mut shutdown: watch::Receiver<bool>) {
    if *shutdown.borrow() {
        return;
    }
    while shutdown.changed().await.is_ok() {
        if *shutdown.borrow() {
            return;
        }
    }
}

#[derive(Clone)]
struct WebhookHttpState {
    hooks: Arc<HashMap<NonZeroU64, WebhookHookBinding>>,
    store: PostgresRepoWatchWebhookStore,
    in_flight: Arc<Semaphore>,
    body_budget: Arc<Semaphore>,
    body_read_timeout: Duration,
    rate_limiter: Arc<WebhookRateLimiter>,
}

#[derive(Clone)]
struct WebhookHookBinding {
    repository: RepositorySlug,
    secret: FileCredentialAccess,
    secret_reference: CredentialReference,
    nudge: Arc<watch::Sender<()>>,
}

#[derive(Deserialize)]
struct WebhookEnvelope {
    repository: WebhookEnvelopeRepository,
    action: Option<String>,
}

#[derive(Deserialize)]
struct WebhookEnvelopeRepository {
    full_name: String,
}

async fn admit_webhook(
    State(state): State<WebhookHttpState>,
    request: Request<Body>,
) -> StatusCode {
    let Ok(_permit) = Arc::clone(&state.in_flight).try_acquire_owned() else {
        return StatusCode::SERVICE_UNAVAILABLE;
    };
    let headers = match parse_github_headers(request.headers()) {
        Ok(headers) => headers,
        Err(error) => return rejected_http_status(error),
    };
    let Some(hook) = state.hooks.get(&headers.hook_id()).cloned() else {
        return StatusCode::UNAUTHORIZED;
    };
    // Nothing is charged before verification. A budget keyed on the hook a
    // request claims is a lever the attacker holds and GitHub does not: spending
    // it with forged signatures would reject the deliveries it exists to protect.
    // Unauthenticated cost is bounded by resources instead — the connection
    // budget, the concurrency permit, the shared body budget, and the deadlines.
    //
    // An undeclared length may buffer up to the per-request ceiling, so it
    // reserves that much of the shared budget until the body is read.
    let read_limit = headers
        .declared_body_bytes()
        .unwrap_or(MAX_WEBHOOK_BODY_BYTES);
    let Ok(_budget) =
        Arc::clone(&state.body_budget).try_acquire_many_owned(body_budget_granules(read_limit))
    else {
        return StatusCode::SERVICE_UNAVAILABLE;
    };
    let body = match read_body_within_deadline(
        to_bytes(request.into_body(), read_limit),
        state.body_read_timeout,
    )
    .await
    {
        Ok(body) => body,
        Err(WebhookBodyRejection::TooLarge) => return StatusCode::PAYLOAD_TOO_LARGE,
        Err(WebhookBodyRejection::Unreadable) => return StatusCode::SERVICE_UNAVAILABLE,
        Err(WebhookBodyRejection::Deadline) => return StatusCode::REQUEST_TIMEOUT,
    };
    if headers
        .declared_body_bytes()
        .is_some_and(|declared| declared != body.len())
    {
        return StatusCode::BAD_REQUEST;
    }
    let secret = match hook.secret.resolve(&hook.secret_reference).await {
        Ok(secret)
            if !secret.expose_bytes().is_empty()
                && secret.expose_bytes().len() <= MAX_WEBHOOK_SECRET_BYTES =>
        {
            secret
        }
        Ok(_) | Err(_) => return StatusCode::SERVICE_UNAVAILABLE,
    };
    if verify_github_signature(secret.expose_bytes(), &body, headers.signature()).is_err() {
        return StatusCode::UNAUTHORIZED;
    }
    // Only a delivery that proved the shared secret spends this allowance, which
    // is what the ceiling exists to bound.
    if !state.rate_limiter.admit(headers.hook_id()) {
        return StatusCode::TOO_MANY_REQUESTS;
    }
    let envelope: WebhookEnvelope = match serde_json::from_slice(&body) {
        Ok(envelope) => envelope,
        Err(_) => return StatusCode::BAD_REQUEST,
    };
    let repository = match RepositorySlug::try_new(envelope.repository.full_name) {
        Ok(repository) if repository == hook.repository => repository,
        Ok(_) | Err(_) => return StatusCode::FORBIDDEN,
    };
    if envelope.action.as_deref().is_some_and(|action| {
        action.is_empty()
            || action.len() > MAX_ACTION_NAME_BYTES
            || !action
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
    }) {
        return StatusCode::BAD_REQUEST;
    }
    let body_digest: [u8; 32] = Sha256::digest(&body).into();
    // The admission owns its own copy, so the received buffer is released before
    // the persistence await. The budget reserves both representations because
    // they coexist during this conversion even though no await separates them.
    let exact_body = body.to_vec();
    drop(body);
    let admission = match RepoWatchWebhookAdmission::try_new(
        RepoWatchWebhookDeliveryKey::new(headers.hook_id(), headers.delivery_id()),
        repository,
        headers.event().to_owned(),
        envelope.action,
        body_digest,
        exact_body,
    ) {
        Ok(admission) => admission,
        Err(_) => return StatusCode::BAD_REQUEST,
    };
    match state.store.admit(&admission).await {
        Ok(RepoWatchWebhookAdmissionOutcome::Admitted(receipt))
        | Ok(RepoWatchWebhookAdmissionOutcome::EqualDuplicate(receipt)) => {
            match hook.nudge.send(()) {
                Ok(()) => StatusCode::ACCEPTED,
                Err(_) => {
                    tracing::error!(
                        repository = %hook.repository.as_str(),
                        hook_id = headers.hook_id().get(),
                        delivery_id = %headers.delivery_id(),
                        receipt_sequence = receipt.sequence().get(),
                        cause_code = "webhook_drain_task_unavailable",
                        "durable webhook delivery could not wake its repository task"
                    );
                    StatusCode::SERVICE_UNAVAILABLE
                }
            }
        }
        Ok(RepoWatchWebhookAdmissionOutcome::Conflict) => {
            tracing::warn!(
                repository = %hook.repository.as_str(),
                hook_id = headers.hook_id().get(),
                delivery_id = %headers.delivery_id(),
                cause_code = "webhook_delivery_replay_conflict",
                "conflicting webhook delivery replay rejected"
            );
            StatusCode::CONFLICT
        }
        Err(_) => StatusCode::SERVICE_UNAVAILABLE,
    }
}

/// Why an admitted request never produced exact body bytes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WebhookBodyRejection {
    Deadline,
    TooLarge,
    Unreadable,
}

/// Reads the exact request body under the per-request deadline, so a peer that
/// stalls its body releases its concurrency permit and memory reservation
/// instead of holding both until it disconnects.
pub(crate) async fn read_body_within_deadline<Read, Failure>(
    read: Read,
    deadline: Duration,
) -> Result<Bytes, WebhookBodyRejection>
where
    Read: Future<Output = Result<Bytes, Failure>>,
    Failure: std::error::Error + 'static,
{
    match timeout(deadline, read).await {
        Ok(Ok(body)) => Ok(body),
        Ok(Err(error)) => Err(classify_body_read_failure(&error)),
        Err(_) => Err(WebhookBodyRejection::Deadline),
    }
}

/// Distinguishes the collector's length-limit rejection from transport failure,
/// so an oversized body is refused as the peer's fault while a read the server
/// could not complete is not misreported as one.
fn classify_body_read_failure(error: &(dyn std::error::Error + 'static)) -> WebhookBodyRejection {
    let mut source = Some(error);
    while let Some(current) = source {
        if current.is::<http_body_util::LengthLimitError>() {
            return WebhookBodyRejection::TooLarge;
        }
        source = current.source();
    }
    WebhookBodyRejection::Unreadable
}

/// How much of the shared body-memory budget one request must reserve.
///
/// The received `Bytes` and the admission-owned `Vec` coexist while exact bytes
/// are copied between them, so both representations count against the hard
/// aggregate ceiling.
fn body_budget_granules(bytes: usize) -> u32 {
    u32::try_from(
        bytes
            .div_ceil(WEBHOOK_BODY_BUDGET_GRANULE_BYTES)
            .max(1)
            .saturating_mul(2),
    )
    .unwrap_or(u32::MAX)
}

const fn rejected_http_status(error: WebhookHttpRejection) -> StatusCode {
    match error {
        WebhookHttpRejection::HeadTooLarge => StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE,
        WebhookHttpRejection::InvalidContentLength => StatusCode::PAYLOAD_TOO_LARGE,
        WebhookHttpRejection::InvalidSignature => StatusCode::UNAUTHORIZED,
        WebhookHttpRejection::DuplicateHeader
        | WebhookHttpRejection::InvalidContentEncoding
        | WebhookHttpRejection::InvalidContentType
        | WebhookHttpRejection::InvalidDelivery
        | WebhookHttpRejection::InvalidEvent
        | WebhookHttpRejection::InvalidHookId
        | WebhookHttpRejection::InvalidTransferEncoding
        | WebhookHttpRejection::MissingHeader => StatusCode::BAD_REQUEST,
    }
}

/// Selected and syntax-checked singleton GitHub headers.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct GitHubWebhookHeadersV1 {
    hook_id: NonZeroU64,
    delivery_id: Uuid,
    event: String,
    signature: [u8; SHA256_BYTES],
    declared_body_bytes: Option<usize>,
}

impl GitHubWebhookHeadersV1 {
    pub(crate) const fn hook_id(&self) -> NonZeroU64 {
        self.hook_id
    }

    pub(crate) const fn delivery_id(&self) -> Uuid {
        self.delivery_id
    }

    pub(crate) fn event(&self) -> &str {
        &self.event
    }

    pub(crate) const fn signature(&self) -> &[u8; SHA256_BYTES] {
        &self.signature
    }

    pub(crate) const fn declared_body_bytes(&self) -> Option<usize> {
        self.declared_body_bytes
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WebhookHttpRejection {
    DuplicateHeader,
    HeadTooLarge,
    InvalidContentEncoding,
    InvalidContentLength,
    InvalidContentType,
    InvalidDelivery,
    InvalidEvent,
    InvalidHookId,
    InvalidSignature,
    InvalidTransferEncoding,
    MissingHeader,
}

pub(crate) fn parse_github_headers(
    headers: &HeaderMap,
) -> Result<GitHubWebhookHeadersV1, WebhookHttpRejection> {
    require_bounded_head(headers)?;
    require_content_type(headers)?;
    require_supported_encoding(headers)?;
    require_supported_transfer_encoding(headers)?;
    let hook_id = parse_hook_id(required_header(headers, HEADER_HOOK_ID)?)?;
    let delivery_id = parse_delivery(required_header(headers, HEADER_DELIVERY)?)?;
    let event = parse_event(required_header(headers, HEADER_EVENT)?)?;
    let signature = parse_signature(required_header(headers, HEADER_SIGNATURE)?)?;
    let declared_body_bytes = parse_content_length(headers)?;
    Ok(GitHubWebhookHeadersV1 {
        hook_id,
        delivery_id,
        event,
        signature,
        declared_body_bytes,
    })
}

/// Refuses a request head beyond either hard ceiling.
///
/// This runs before any other header is read, so an oversized head costs one
/// pass over fields the connection had already been allowed to buffer.
fn require_bounded_head(headers: &HeaderMap) -> Result<(), WebhookHttpRejection> {
    if headers.len() > MAX_WEBHOOK_HEADER_COUNT {
        return Err(WebhookHttpRejection::HeadTooLarge);
    }
    let mut head_bytes = 0_usize;
    for (name, value) in headers {
        head_bytes = head_bytes
            .saturating_add(name.as_str().len())
            .saturating_add(value.len());
        if head_bytes > MAX_WEBHOOK_HEADER_BYTES {
            return Err(WebhookHttpRejection::HeadTooLarge);
        }
    }
    Ok(())
}

fn required_header<'headers>(
    headers: &'headers HeaderMap,
    name: &'static str,
) -> Result<&'headers HeaderValue, WebhookHttpRejection> {
    let mut values = headers.get_all(name).iter();
    let value = values.next().ok_or(WebhookHttpRejection::MissingHeader)?;
    if values.next().is_some() {
        return Err(WebhookHttpRejection::DuplicateHeader);
    }
    Ok(value)
}

fn optional_header<'headers>(
    headers: &'headers HeaderMap,
    name: &'static str,
) -> Result<Option<&'headers HeaderValue>, WebhookHttpRejection> {
    let mut values = headers.get_all(name).iter();
    let value = values.next();
    if values.next().is_some() {
        return Err(WebhookHttpRejection::DuplicateHeader);
    }
    Ok(value)
}

fn header_text(value: &HeaderValue) -> Result<&str, WebhookHttpRejection> {
    value
        .to_str()
        .map_err(|_| WebhookHttpRejection::MissingHeader)
}

fn require_content_type(headers: &HeaderMap) -> Result<(), WebhookHttpRejection> {
    let content_type = header_text(required_header(headers, HEADER_CONTENT_TYPE)?)?;
    let media_type = content_type
        .parse::<mime::Mime>()
        .map_err(|_| WebhookHttpRejection::InvalidContentType)?;
    if media_type.essence_str() != JSON_CONTENT_TYPE {
        return Err(WebhookHttpRejection::InvalidContentType);
    }
    Ok(())
}

fn require_supported_encoding(headers: &HeaderMap) -> Result<(), WebhookHttpRejection> {
    let Some(content_encoding) = optional_header(headers, HEADER_CONTENT_ENCODING)? else {
        return Ok(());
    };
    if header_text(content_encoding)? != "identity" {
        return Err(WebhookHttpRejection::InvalidContentEncoding);
    }
    Ok(())
}

fn require_supported_transfer_encoding(headers: &HeaderMap) -> Result<(), WebhookHttpRejection> {
    let Some(transfer_encoding) = optional_header(headers, HEADER_TRANSFER_ENCODING)? else {
        return Ok(());
    };
    if header_text(transfer_encoding)? != "chunked" {
        return Err(WebhookHttpRejection::InvalidTransferEncoding);
    }
    Ok(())
}

fn parse_hook_id(value: &HeaderValue) -> Result<NonZeroU64, WebhookHttpRejection> {
    let text = header_text(value)?;
    let hook_id = text
        .parse::<NonZeroU64>()
        .map_err(|_| WebhookHttpRejection::InvalidHookId)?;
    if hook_id.get().to_string() != text {
        return Err(WebhookHttpRejection::InvalidHookId);
    }
    Ok(hook_id)
}

fn parse_delivery(value: &HeaderValue) -> Result<Uuid, WebhookHttpRejection> {
    let text = header_text(value)?;
    let delivery = Uuid::parse_str(text).map_err(|_| WebhookHttpRejection::InvalidDelivery)?;
    if delivery.hyphenated().to_string() != text {
        return Err(WebhookHttpRejection::InvalidDelivery);
    }
    Ok(delivery)
}

fn parse_event(value: &HeaderValue) -> Result<String, WebhookHttpRejection> {
    let event = header_text(value)?;
    if event.is_empty()
        || event.len() > MAX_EVENT_NAME_BYTES
        || !event
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte == b'_')
    {
        return Err(WebhookHttpRejection::InvalidEvent);
    }
    Ok(event.to_owned())
}

fn parse_signature(value: &HeaderValue) -> Result<[u8; SHA256_BYTES], WebhookHttpRejection> {
    let signature = header_text(value)?;
    let hex = signature
        .strip_prefix(SHA256_SIGNATURE_PREFIX)
        .ok_or(WebhookHttpRejection::InvalidSignature)?;
    let mut decoded = [0_u8; SHA256_BYTES];
    if hex::decode_to_slice(hex, &mut decoded).is_ok() && hex::encode(decoded) == hex {
        Ok(decoded)
    } else {
        Err(WebhookHttpRejection::InvalidSignature)
    }
}

fn parse_content_length(headers: &HeaderMap) -> Result<Option<usize>, WebhookHttpRejection> {
    let Some(value) = optional_header(headers, HEADER_CONTENT_LENGTH)? else {
        return Ok(None);
    };
    let text = header_text(value)?;
    let body_bytes = text
        .parse::<usize>()
        .map_err(|_| WebhookHttpRejection::InvalidContentLength)?;
    if body_bytes.to_string() != text || body_bytes > MAX_WEBHOOK_BODY_BYTES {
        return Err(WebhookHttpRejection::InvalidContentLength);
    }
    Ok(Some(body_bytes))
}

pub(crate) fn verify_github_signature(
    secret: &[u8],
    body: &[u8],
    expected: &[u8; SHA256_BYTES],
) -> Result<(), WebhookHttpRejection> {
    let mut verifier = Hmac::<Sha256>::new_from_slice(secret)
        .map_err(|_| WebhookHttpRejection::InvalidSignature)?;
    verifier.update(body);
    verifier
        .verify_slice(expected)
        .map_err(|_| WebhookHttpRejection::InvalidSignature)
}

/// Per-hook admission windows for deliveries that have proved the shared
/// secret. Requests that have not are bounded by resources, not by a counter an
/// unauthenticated peer could spend.
#[derive(Debug)]
pub(crate) struct WebhookRateLimiter {
    verified: Mutex<HashMap<NonZeroU64, HookRateWindow>>,
}

/// One hook's admissions, counted per bucket across the trailing window.
///
/// A fixed window that simply resets admits a full allowance on either side of
/// its boundary. Carrying the preceding window in proportion fixes that only if
/// its admissions were spread evenly, which a burst is not. Counting buckets
/// attributes each admission to when it happened, so every rolling window is
/// bounded whatever the arrival shape. The oldest bucket is counted whole, which
/// makes the ceiling strict rather than permissive at the boundary.
#[derive(Clone, Copy, Debug)]
struct HookRateWindow {
    bucket_started: Instant,
    newest: usize,
    buckets: [u32; WEBHOOK_RATE_BUCKETS],
}

impl WebhookRateLimiter {
    pub(crate) fn new() -> Self {
        Self {
            verified: Mutex::new(HashMap::new()),
        }
    }

    pub(crate) fn admit(&self, hook_id: NonZeroU64) -> bool {
        Self::admit_at(
            &self.verified,
            hook_id,
            Instant::now(),
            MAX_WEBHOOK_DELIVERIES_PER_MINUTE,
        )
    }

    fn admit_at(
        windows: &Mutex<HashMap<NonZeroU64, HookRateWindow>>,
        hook_id: NonZeroU64,
        now: Instant,
        ceiling: u32,
    ) -> bool {
        let mut windows = windows.lock().unwrap_or_else(PoisonError::into_inner);
        let window = windows.entry(hook_id).or_insert(HookRateWindow {
            bucket_started: now,
            newest: 0,
            buckets: [0; WEBHOOK_RATE_BUCKETS],
        });
        advance_rate_buckets(window, now);
        let admitted: u64 = window.buckets.iter().copied().map(u64::from).sum();
        if admitted >= u64::from(ceiling) {
            return false;
        }
        window.buckets[window.newest] = window.buckets[window.newest].saturating_add(1);
        true
    }

    #[cfg(test)]
    fn saturate(
        windows: &Mutex<HashMap<NonZeroU64, HookRateWindow>>,
        hook_id: NonZeroU64,
        started: Instant,
        ceiling: u32,
    ) {
        windows
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(
                hook_id,
                HookRateWindow {
                    bucket_started: started,
                    newest: 0,
                    buckets: {
                        let mut buckets = [0; WEBHOOK_RATE_BUCKETS];
                        buckets[0] = ceiling;
                        buckets
                    },
                },
            );
    }
}

/// Retires whatever buckets have aged out of the trailing window by `now`.
fn advance_rate_buckets(window: &mut HookRateWindow, now: Instant) {
    let elapsed = now.duration_since(window.bucket_started);
    if elapsed >= RATE_WINDOW + RATE_BUCKET {
        // Every bucket, including the one straddling the window edge, is older
        // than the trailing window.
        *window = HookRateWindow {
            bucket_started: now,
            newest: 0,
            buckets: [0; WEBHOOK_RATE_BUCKETS],
        };
        return;
    }
    let steps =
        usize::try_from(elapsed.as_millis() / RATE_BUCKET.as_millis()).unwrap_or(usize::MAX);
    if steps >= WEBHOOK_RATE_BUCKETS {
        *window = HookRateWindow {
            bucket_started: now,
            newest: 0,
            buckets: [0; WEBHOOK_RATE_BUCKETS],
        };
        return;
    }
    for _ in 0..steps {
        window.newest = (window.newest + 1) % WEBHOOK_RATE_BUCKETS;
        window.buckets[window.newest] = 0;
    }
    window.bucket_started += RATE_BUCKET.saturating_mul(u32::try_from(steps).unwrap_or(u32::MAX));
}

#[cfg(test)]
mod tests {
    use std::{error::Error, fs, num::NonZeroU64, time::Duration};

    use axum::{
        body::Body,
        extract::State,
        http::{HeaderMap, HeaderValue, Request, StatusCode},
    };
    use hmac::{Hmac, KeyInit, Mac};
    use sha2::Sha256;
    use signalbox_domain::RepositorySlug;
    use signalbox_model_runtime::CredentialReference;
    use signalbox_persistence::{
        disposable_postgres_server_args, disposable_postgres_state_tmpfs_from_example,
        disposable_test_container_labels, local_test_connection_options, migrate,
        repo_watch_webhook::PostgresRepoWatchWebhookStore,
    };
    use sqlx::{PgPool, postgres::PgPoolOptions};
    use tempfile::TempDir;
    use testcontainers_modules::{
        postgres::Postgres,
        testcontainers::{ContainerAsync, ImageExt, runners::AsyncRunner},
    };
    use tokio::sync::{Semaphore, watch};

    use super::{
        FileCredentialAccess, GitHubWebhookHeadersV1, MAX_WEBHOOK_BODY_BYTES,
        MAX_WEBHOOK_CONNECTIONS, MAX_WEBHOOK_DELIVERIES_PER_MINUTE, MAX_WEBHOOK_HEADER_BYTES,
        MAX_WEBHOOK_HEADER_COUNT, MAX_WEBHOOK_IN_FLIGHT, MAX_WEBHOOK_SECRET_BYTES,
        WEBHOOK_BODY_BUDGET_GRANULES, WEBHOOK_BODY_READ_TIMEOUT, WEBHOOK_CONNECTION_READ_TIMEOUT,
        WebhookBodyRejection, WebhookHookBinding, WebhookHttpRejection, WebhookHttpState,
        WebhookRateLimiter, admit_webhook, body_budget_granules, parse_github_headers,
        read_body_within_deadline, verify_github_signature,
    };

    const FIXTURE_HOOK_ID: NonZeroU64 = NonZeroU64::new(4_242).expect("fixture is positive");
    const FIXTURE_DELIVERY: &str = "550e8400-e29b-41d4-a716-446655440000";
    const FIXTURE_BODY: &[u8] = br#"{"repository":{"full_name":"keenwill/signalbox"}}"#;
    const FIXTURE_SECRET: &[u8] = b"correct horse battery staple";
    const FIXTURE_REPOSITORY: &str = "keenwill/signalbox";
    const FIXTURE_FOREIGN_BODY: &[u8] = br#"{"repository":{"full_name":"someone/else"}}"#;
    const POSTGRES_IMAGE_TAG: &str = "18.4-alpine3.23";
    const DATABASE_NAME: &str = "signalbox_webhook_wake";
    const DATABASE_USER: &str = "signalbox";
    const DATABASE_PASSWORD: &str = "signalbox-test-only";
    const WAKE_TIMEOUT: Duration = Duration::from_secs(5);

    async fn migrated_postgres() -> Result<(ContainerAsync<Postgres>, PgPool), Box<dyn Error>> {
        let container = Postgres::default()
            .with_db_name(DATABASE_NAME)
            .with_user(DATABASE_USER)
            .with_password(DATABASE_PASSWORD)
            .with_cmd(disposable_postgres_server_args())
            .with_mount(disposable_postgres_state_tmpfs_from_example()?)
            .with_tag(POSTGRES_IMAGE_TAG)
            .with_labels(disposable_test_container_labels())
            .start()
            .await?;
        let host = container.get_host().await?;
        let port = container.get_host_port_ipv4(5432).await?;
        let database_url =
            format!("postgres://{DATABASE_USER}:{DATABASE_PASSWORD}@{host}:{port}/{DATABASE_NAME}");
        let pool = PgPoolOptions::new()
            .max_connections(4)
            .connect_with(local_test_connection_options(&database_url)?)
            .await?;
        migrate(&pool).await?;
        Ok((container, pool))
    }

    fn valid_headers() -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert("content-type", HeaderValue::from_static("application/json"));
        headers.insert("x-github-hook-id", HeaderValue::from_static("4242"));
        headers.insert(
            "x-github-delivery",
            HeaderValue::from_static(FIXTURE_DELIVERY),
        );
        headers.insert("x-github-event", HeaderValue::from_static("workflow_job"));
        headers.insert(
            "x-hub-signature-256",
            HeaderValue::from_static(
                "sha256=0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            ),
        );
        headers
    }

    fn parsed_valid_headers() -> GitHubWebhookHeadersV1 {
        parse_github_headers(&valid_headers()).expect("fixture headers are valid")
    }

    fn fixture_signature() -> [u8; 32] {
        signature_for(FIXTURE_BODY)
    }

    fn signature_for(body: &[u8]) -> [u8; 32] {
        let mut mac = Hmac::<Sha256>::new_from_slice(FIXTURE_SECRET)
            .expect("SHA-256 accepts the fixture secret");
        mac.update(body);
        mac.finalize().into_bytes().into()
    }

    fn signature_header(body: &[u8]) -> HeaderValue {
        use std::fmt::Write as _;

        let mut value = String::from("sha256=");
        for byte in signature_for(body) {
            write!(&mut value, "{byte:02x}").expect("writing to String cannot fail");
        }
        HeaderValue::from_str(&value).expect("fixture signature is an HTTP value")
    }

    fn fixture_state() -> (TempDir, WebhookHttpState) {
        let pool = PgPoolOptions::new()
            .connect_lazy("postgres://fixture:fixture@127.0.0.1/fixture")
            .expect("fixture PostgreSQL URL is valid");
        let (directory, state, _receiver) = fixture_state_with_pool(pool);
        (directory, state)
    }

    fn fixture_state_with_pool(pool: PgPool) -> (TempDir, WebhookHttpState, watch::Receiver<()>) {
        let directory = TempDir::new().expect("fixture directory is created");
        let secret_path = directory.path().join("webhook-secret");
        fs::write(&secret_path, FIXTURE_SECRET).expect("fixture secret is written");
        let secret_reference = CredentialReference::new("fixture-webhook");
        let (nudge, receiver) = watch::channel(());
        let nudge = std::sync::Arc::new(nudge);
        let repository = RepositorySlug::try_new(FIXTURE_REPOSITORY.to_owned())
            .expect("fixture repository is canonical");
        let hooks = std::collections::HashMap::from([(
            FIXTURE_HOOK_ID,
            WebhookHookBinding {
                repository,
                secret: FileCredentialAccess::new_bounded(
                    secret_path,
                    secret_reference.clone(),
                    MAX_WEBHOOK_SECRET_BYTES,
                ),
                secret_reference,
                nudge,
            },
        )]);
        (
            directory,
            WebhookHttpState {
                hooks: std::sync::Arc::new(hooks),
                store: PostgresRepoWatchWebhookStore::new(pool),
                in_flight: std::sync::Arc::new(Semaphore::new(MAX_WEBHOOK_IN_FLIGHT)),
                body_budget: std::sync::Arc::new(Semaphore::new(WEBHOOK_BODY_BUDGET_GRANULES)),
                body_read_timeout: WEBHOOK_BODY_READ_TIMEOUT,
                rate_limiter: std::sync::Arc::new(WebhookRateLimiter::new()),
            },
            receiver,
        )
    }

    fn fixture_request(body: Vec<u8>, signed_body: &[u8]) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri("/")
            .header("content-type", "application/json")
            .header("x-github-hook-id", "4242")
            .header("x-github-delivery", FIXTURE_DELIVERY)
            .header("x-github-event", "workflow_job")
            .header("x-hub-signature-256", signature_header(signed_body))
            .body(Body::from(body))
            .expect("fixture request is valid")
    }

    #[test]
    fn exact_github_headers_are_admitted() {
        let parsed = parsed_valid_headers();

        assert_eq!(parsed.hook_id(), FIXTURE_HOOK_ID);
        assert_eq!(parsed.delivery_id().to_string(), FIXTURE_DELIVERY);
        assert_eq!(parsed.event(), "workflow_job");
        assert_eq!(parsed.declared_body_bytes(), None);
    }

    #[test]
    fn duplicate_signature_header_is_rejected() {
        let mut headers = valid_headers();
        headers.append(
            "x-hub-signature-256",
            HeaderValue::from_static(
                "sha256=0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            ),
        );

        assert_eq!(
            parse_github_headers(&headers),
            Err(WebhookHttpRejection::DuplicateHeader)
        );
    }

    #[test]
    fn uppercase_signature_hex_is_rejected() {
        let mut headers = valid_headers();
        headers.insert(
            "x-hub-signature-256",
            HeaderValue::from_static(
                "sha256=0123456789ABCDEF0123456789abcdef0123456789abcdef0123456789abcdef",
            ),
        );

        assert_eq!(
            parse_github_headers(&headers),
            Err(WebhookHttpRejection::InvalidSignature)
        );
    }

    #[test]
    fn content_length_at_the_hard_ceiling_is_admitted() {
        let mut headers = valid_headers();
        let limit = HeaderValue::from_str(&MAX_WEBHOOK_BODY_BYTES.to_string())
            .expect("fixture limit is an HTTP value");
        headers.insert("content-length", limit);

        assert_eq!(
            parse_github_headers(&headers)
                .expect("the hard ceiling is inclusive")
                .declared_body_bytes(),
            Some(MAX_WEBHOOK_BODY_BYTES)
        );
    }

    #[test]
    fn content_length_above_the_hard_ceiling_is_rejected() {
        let mut headers = valid_headers();
        let oversized = HeaderValue::from_str(&(MAX_WEBHOOK_BODY_BYTES + 1).to_string())
            .expect("fixture limit is an HTTP value");
        headers.insert("content-length", oversized);

        assert_eq!(
            parse_github_headers(&headers),
            Err(WebhookHttpRejection::InvalidContentLength)
        );
    }

    #[test]
    fn exact_body_signature_is_verified() {
        assert_eq!(
            verify_github_signature(FIXTURE_SECRET, FIXTURE_BODY, &fixture_signature()),
            Ok(())
        );
    }

    #[test]
    fn altered_body_signature_is_rejected() {
        assert_eq!(
            verify_github_signature(FIXTURE_SECRET, b"{}", &fixture_signature()),
            Err(WebhookHttpRejection::InvalidSignature)
        );
    }

    #[tokio::test]
    async fn invalid_signature_is_rejected_before_malformed_json() {
        let (_directory, state) = fixture_state();
        let request = fixture_request(b"not-json".to_vec(), b"different-body");

        let status = admit_webhook(State(state), request).await;

        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn signature_valid_foreign_repository_is_rejected() {
        let (_directory, state) = fixture_state();
        let request = fixture_request(FIXTURE_FOREIGN_BODY.to_vec(), FIXTURE_FOREIGN_BODY);

        let status = admit_webhook(State(state), request).await;

        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn streamed_body_at_the_hard_ceiling_reaches_authentication() {
        let (_directory, state) = fixture_state();
        let body = vec![b'x'; MAX_WEBHOOK_BODY_BYTES];
        let request = fixture_request(body, b"different-body");

        let status = admit_webhook(State(state), request).await;

        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn streamed_body_above_the_hard_ceiling_is_rejected() {
        let (_directory, state) = fixture_state();
        let body = vec![b'x'; MAX_WEBHOOK_BODY_BYTES + 1];
        let request = fixture_request(body, b"different-body");

        let status = admit_webhook(State(state), request).await;

        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[tokio::test]
    #[ignore = "requires ephemeral PostgreSQL"]
    async fn durable_http_admission_directly_wakes_the_repository_task()
    -> Result<(), Box<dyn Error>> {
        let (_container, pool) = migrated_postgres().await?;
        let (_directory, state, mut receiver) = fixture_state_with_pool(pool);
        let request = fixture_request(FIXTURE_BODY.to_vec(), FIXTURE_BODY);

        let status = admit_webhook(State(state), request).await;
        tokio::time::timeout(WAKE_TIMEOUT, receiver.changed()).await??;

        assert_eq!(status, StatusCode::ACCEPTED);
        Ok(())
    }

    #[tokio::test]
    #[ignore = "requires ephemeral PostgreSQL"]
    async fn equal_durable_replay_rewakes_the_repository_task() -> Result<(), Box<dyn Error>> {
        let (_container, pool) = migrated_postgres().await?;
        let (_directory, state, mut receiver) = fixture_state_with_pool(pool);
        let first = fixture_request(FIXTURE_BODY.to_vec(), FIXTURE_BODY);
        let replay = fixture_request(FIXTURE_BODY.to_vec(), FIXTURE_BODY);

        let first_status = admit_webhook(State(state.clone()), first).await;
        tokio::time::timeout(WAKE_TIMEOUT, receiver.changed()).await??;
        let replay_status = admit_webhook(State(state), replay).await;
        tokio::time::timeout(WAKE_TIMEOUT, receiver.changed()).await??;

        assert_eq!(first_status, StatusCode::ACCEPTED);
        assert_eq!(replay_status, StatusCode::ACCEPTED);
        Ok(())
    }

    #[test]
    fn per_hook_rate_ceiling_rejects_the_next_delivery() {
        let limiter = WebhookRateLimiter::new();
        let started = std::time::Instant::now();
        WebhookRateLimiter::saturate(
            &limiter.verified,
            FIXTURE_HOOK_ID,
            started,
            MAX_WEBHOOK_DELIVERIES_PER_MINUTE,
        );

        assert!(!WebhookRateLimiter::admit_at(
            &limiter.verified,
            FIXTURE_HOOK_ID,
            started,
            MAX_WEBHOOK_DELIVERIES_PER_MINUTE
        ));
    }

    #[test]
    fn a_burst_still_counts_part_way_through_the_window() {
        let limiter = WebhookRateLimiter::new();
        let started = std::time::Instant::now();
        WebhookRateLimiter::saturate(
            &limiter.verified,
            FIXTURE_HOOK_ID,
            started,
            MAX_WEBHOOK_DELIVERIES_PER_MINUTE,
        );

        // A burst is counted where it happened, so half a window later the whole
        // of it still lies inside the trailing minute.
        assert!(!WebhookRateLimiter::admit_at(
            &limiter.verified,
            FIXTURE_HOOK_ID,
            started + Duration::from_secs(30),
            MAX_WEBHOOK_DELIVERIES_PER_MINUTE
        ));
    }

    #[test]
    fn a_burst_at_the_window_edge_still_counts_a_minute_later() {
        let limiter = WebhookRateLimiter::new();
        let started = std::time::Instant::now();
        WebhookRateLimiter::saturate(
            &limiter.verified,
            FIXTURE_HOOK_ID,
            started,
            MAX_WEBHOOK_DELIVERIES_PER_MINUTE,
        );

        // The bucket anchored here can hold admissions arriving almost a bucket
        // later, so at one window it is still partly inside the trailing minute.
        assert!(!WebhookRateLimiter::admit_at(
            &limiter.verified,
            FIXTURE_HOOK_ID,
            started + super::RATE_WINDOW,
            MAX_WEBHOOK_DELIVERIES_PER_MINUTE
        ));
    }

    #[test]
    fn a_burst_older_than_the_window_releases_its_allowance() {
        let limiter = WebhookRateLimiter::new();
        let started = std::time::Instant::now();
        WebhookRateLimiter::saturate(
            &limiter.verified,
            FIXTURE_HOOK_ID,
            started,
            MAX_WEBHOOK_DELIVERIES_PER_MINUTE,
        );

        assert!(WebhookRateLimiter::admit_at(
            &limiter.verified,
            FIXTURE_HOOK_ID,
            started + super::RATE_WINDOW + super::RATE_BUCKET,
            MAX_WEBHOOK_DELIVERIES_PER_MINUTE
        ));
    }

    #[tokio::test]
    async fn a_forged_signature_never_spends_the_authenticated_allowance() {
        let (_directory, state) = fixture_state();
        let limiter = std::sync::Arc::clone(&state.rate_limiter);
        let request = fixture_request(FIXTURE_BODY.to_vec(), b"different-body");

        let status = admit_webhook(State(state), request).await;

        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert!(
            limiter
                .verified
                .lock()
                .expect("fixture limiter is uncontended")
                .is_empty()
        );
    }

    /// Serves a trivial router behind the real bounded listener.
    async fn bounded_listener_fixture() -> (
        std::net::SocketAddr,
        TempDir,
        tokio::sync::watch::Sender<bool>,
        tokio::task::JoinHandle<()>,
    ) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("the fixture binds an ephemeral port");
        let address = listener
            .local_addr()
            .expect("the fixture listener reports its address");
        let bounded = super::BoundedWebhookListener {
            listener,
            connections: std::sync::Arc::new(Semaphore::new(MAX_WEBHOOK_CONNECTIONS)),
            read_timeout: super::WEBHOOK_CONNECTION_READ_TIMEOUT,
        };
        let (directory, state) = fixture_state();
        let router = axum::Router::new()
            .route("/", axum::routing::post(admit_webhook))
            .layer(axum::extract::DefaultBodyLimit::max(MAX_WEBHOOK_BODY_BYTES))
            .with_state(state);
        let (stop, shutdown) = tokio::sync::watch::channel(false);
        let served = tokio::spawn(super::serve_bounded_webhook_connections(
            bounded, router, shutdown,
        ));
        (address, directory, stop, served)
    }

    /// Appends `count` distinct filler header fields.
    ///
    /// The loop lives here rather than in a test body so each test stays
    /// straight-line, as `docs/agents/testing-style.md` rule 2 requires.
    fn append_filler_headers(headers: &mut HeaderMap, count: usize) {
        for index in 0..count {
            headers.append(
                axum::http::HeaderName::from_bytes(format!("x-fixture-{index}").as_bytes())
                    .expect("the fixture header name is valid"),
                HeaderValue::from_static("1"),
            );
        }
    }

    /// Builds `count` distinct header fields.
    ///
    /// The loop lives here rather than in a test body so each test stays
    /// straight-line, as `docs/agents/testing-style.md` rule 2 requires.
    fn repeated_header_block(count: usize) -> String {
        use std::fmt::Write as _;

        let mut headers = String::new();
        for index in 0..count {
            write!(&mut headers, "x-fixture-{index}: 1\r\n")
                .expect("writing to String cannot fail");
        }
        headers
    }

    async fn request_status_line(address: std::net::SocketAddr, request: &str) -> String {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let mut stream = tokio::net::TcpStream::connect(address)
            .await
            .expect("the fixture connects to its own listener");
        stream
            .write_all(request.as_bytes())
            .await
            .expect("the fixture writes its request head");
        let mut response = Vec::new();
        let _ =
            tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut response)).await;
        String::from_utf8_lossy(&response)
            .lines()
            .next()
            .unwrap_or("")
            .to_owned()
    }

    #[tokio::test]
    async fn a_request_head_beyond_the_field_ceiling_is_refused() {
        let (address, _directory, stop, served) = bounded_listener_fixture().await;
        let request = format!(
            "POST / HTTP/1.1\r\nhost: fixture\r\n{}\r\n",
            repeated_header_block(MAX_WEBHOOK_HEADER_COUNT + 8)
        );

        let status = request_status_line(address, &request).await;

        assert!(
            status.contains("431"),
            "a head beyond the field ceiling must be refused, got {status:?}"
        );
        let _ = stop.send(true);
        let _ = served.await;
    }

    #[test]
    fn a_head_beyond_the_aggregate_byte_ceiling_is_refused() {
        let mut headers = valid_headers();
        let oversized = "x".repeat(MAX_WEBHOOK_HEADER_BYTES);
        headers.insert(
            "x-fixture",
            HeaderValue::from_str(&oversized).expect("the fixture value is an HTTP value"),
        );

        assert_eq!(
            parse_github_headers(&headers),
            Err(WebhookHttpRejection::HeadTooLarge)
        );
    }

    #[test]
    fn a_head_at_the_aggregate_byte_ceiling_is_admitted() {
        let headers = valid_headers();
        let occupied: usize = headers
            .iter()
            .map(|(name, value)| name.as_str().len() + value.len())
            .sum();
        let mut headers = headers;
        let remaining = MAX_WEBHOOK_HEADER_BYTES - occupied - "x-fixture".len();
        headers.insert(
            "x-fixture",
            HeaderValue::from_str(&"x".repeat(remaining))
                .expect("the fixture value is an HTTP value"),
        );

        assert!(parse_github_headers(&headers).is_ok());
    }

    #[test]
    fn a_head_beyond_the_field_count_ceiling_is_refused() {
        let mut headers = valid_headers();
        append_filler_headers(&mut headers, MAX_WEBHOOK_HEADER_COUNT);

        assert_eq!(
            parse_github_headers(&headers),
            Err(WebhookHttpRejection::HeadTooLarge)
        );
    }

    #[tokio::test]
    async fn a_request_head_beyond_the_aggregate_byte_ceiling_is_refused_on_the_wire() {
        let (address, _directory, stop, served) = bounded_listener_fixture().await;
        let oversized = "x".repeat(MAX_WEBHOOK_HEADER_BYTES);
        let request = format!("POST / HTTP/1.1\r\nhost: fixture\r\nx-fixture: {oversized}\r\n\r\n");

        let status = request_status_line(address, &request).await;

        assert!(
            status.contains("431"),
            "a head beyond the aggregate ceiling must be refused, got {status:?}"
        );
        let _ = stop.send(true);
        let _ = served.await;
    }

    #[tokio::test]
    async fn a_head_within_both_ceilings_reaches_header_admission() {
        let (address, _directory, stop, served) = bounded_listener_fixture().await;
        let request = format!(
            "POST / HTTP/1.1\r\nhost: fixture\r\ncontent-length: 0\r\n{}\r\n",
            repeated_header_block(MAX_WEBHOOK_HEADER_COUNT / 4)
        );

        let status = request_status_line(address, &request).await;

        // The head clears both ceilings and is then refused for the GitHub
        // headers it lacks, which is admission rather than an ingress bound.
        assert!(
            status.contains("400"),
            "a head within both ceilings must reach admission, got {status:?}"
        );
        let _ = stop.send(true);
        let _ = served.await;
    }

    #[tokio::test(start_paused = true)]
    async fn a_stalled_connection_read_fails_at_the_deadline() {
        use tokio::io::AsyncReadExt as _;

        let (client, server) = tokio::io::duplex(64);
        let budget = std::sync::Arc::new(Semaphore::new(1));
        let permit = std::sync::Arc::clone(&budget)
            .try_acquire_owned()
            .expect("the fixture budget has one permit");
        let mut connection =
            super::DeadlinedConnection::new(server, WEBHOOK_CONNECTION_READ_TIMEOUT, permit);
        let mut received = [0_u8; 1];

        let error = connection
            .read(&mut received)
            .await
            .expect_err("a peer that never sends must not hold its connection");

        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        drop(client);
    }

    #[tokio::test(start_paused = true)]
    async fn a_stalled_body_read_rejects_at_the_deadline() {
        let stalled = std::future::pending::<Result<axum::body::Bytes, std::convert::Infallible>>();

        let rejection = read_body_within_deadline(stalled, WEBHOOK_BODY_READ_TIMEOUT)
            .await
            .expect_err("a body that never completes must not hold its permit");

        assert_eq!(rejection, WebhookBodyRejection::Deadline);
    }

    #[tokio::test]
    async fn an_oversized_body_is_rejected_as_too_large() {
        let body = axum::body::Body::from(vec![0u8; 64]);

        let rejection =
            read_body_within_deadline(axum::body::to_bytes(body, 8), WEBHOOK_BODY_READ_TIMEOUT)
                .await
                .expect_err("a body over its collection limit must be refused");

        assert_eq!(rejection, WebhookBodyRejection::TooLarge);
    }

    #[tokio::test]
    async fn a_transport_failure_is_unreadable_rather_than_too_large() {
        let failed = std::future::ready(Err::<axum::body::Bytes, std::io::Error>(
            std::io::Error::other("connection reset"),
        ));

        let rejection = read_body_within_deadline(failed, WEBHOOK_BODY_READ_TIMEOUT)
            .await
            .expect_err("a broken transport must not produce body bytes");

        assert_eq!(rejection, WebhookBodyRejection::Unreadable);
    }

    #[test]
    fn body_budget_reserves_both_body_representations() {
        assert_eq!(body_budget_granules(0), 2);
        assert_eq!(body_budget_granules(1), 2);
        assert_eq!(body_budget_granules(64 * 1024), 2);
        assert_eq!(body_budget_granules(64 * 1024 + 1), 4);
    }

    #[test]
    fn undeclared_bodies_cannot_all_reserve_the_whole_budget() {
        let reserved = u64::from(body_budget_granules(MAX_WEBHOOK_BODY_BYTES));
        let concurrent =
            u64::try_from(WEBHOOK_BODY_BUDGET_GRANULES).expect("budget fits in u64") / reserved;

        assert!(concurrent >= 1);
        assert!(concurrent < u64::try_from(MAX_WEBHOOK_IN_FLIGHT).expect("ceiling fits in u64"));
    }
}
