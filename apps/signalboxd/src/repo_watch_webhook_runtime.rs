//! Authenticated, bounded GitHub webhook HTTP admission.

use std::{
    collections::HashMap,
    error::Error,
    fmt,
    future::Future,
    net::TcpListener as StdTcpListener,
    num::NonZeroU64,
    sync::{Arc, Mutex, PoisonError},
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
    net::TcpListener,
    sync::{Semaphore, mpsc, watch},
    time::timeout,
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
const SHA256_HEX_BYTES: usize = 64;
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
/// Hard safety ceiling on what one hook may present before proving the shared
/// secret. It is a separate allowance so a forged flood cannot spend what real
/// GitHub deliveries draw on.
pub(crate) const MAX_WEBHOOK_UNVERIFIED_REQUESTS_PER_MINUTE: u32 = 3_000;
/// Hard safety deadline for reading one request body. A peer that opens a
/// request and then stalls its body would otherwise hold a concurrency permit
/// and its share of the memory budget indefinitely.
pub(crate) const WEBHOOK_BODY_READ_TIMEOUT: Duration = Duration::from_secs(30);

const RATE_WINDOW: Duration = Duration::from_secs(60);
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
        mut workers: HashMap<RepositorySlug, mpsc::Sender<()>>,
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
        let router = Router::new()
            .route(&self.path, post(admit_webhook))
            .layer(DefaultBodyLimit::max(MAX_WEBHOOK_BODY_BYTES))
            .with_state(self.state);
        axum::serve(listener, router)
            .with_graceful_shutdown(webhook_shutdown(shutdown))
            .await
            .map_err(RepoWatchWebhookRuntimeError)
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
    nudge: mpsc::Sender<()>,
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
    // Charged before the body is read, against an allowance separate from the
    // authenticated one, so an unauthenticated flood bounds its own cost without
    // spending what signature-valid deliveries for this hook draw on.
    if !state.rate_limiter.admit_unverified(headers.hook_id()) {
        return StatusCode::TOO_MANY_REQUESTS;
    }
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
    // Only a delivery that proved the shared secret spends the authenticated
    // allowance, which is what this ceiling exists to bound.
    if !state.rate_limiter.admit_verified(headers.hook_id()) {
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
    let admission = match RepoWatchWebhookAdmission::try_new(
        RepoWatchWebhookDeliveryKey::new(headers.hook_id(), headers.delivery_id()),
        repository,
        headers.event().to_owned(),
        envelope.action,
        body_digest,
        body.to_vec(),
    ) {
        Ok(admission) => admission,
        Err(_) => return StatusCode::BAD_REQUEST,
    };
    match state.store.admit(&admission).await {
        Ok(RepoWatchWebhookAdmissionOutcome::Admitted(receipt)) => {
            if hook.nudge.try_send(()).is_err() {
                tracing::debug!(
                    repository = %hook.repository.as_str(),
                    hook_id = headers.hook_id().get(),
                    delivery_id = %headers.delivery_id(),
                    receipt_sequence = receipt.sequence().get(),
                    "durable webhook delivery awaits repository worker drain"
                );
            }
            StatusCode::ACCEPTED
        }
        Ok(RepoWatchWebhookAdmissionOutcome::EqualDuplicate(_)) => StatusCode::ACCEPTED,
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
{
    match timeout(deadline, read).await {
        Ok(Ok(body)) => Ok(body),
        Ok(Err(_)) => Err(WebhookBodyRejection::TooLarge),
        Err(_) => Err(WebhookBodyRejection::Deadline),
    }
}

/// How much of the shared body-memory budget one request must reserve.
fn body_budget_granules(bytes: usize) -> u32 {
    u32::try_from(bytes.div_ceil(WEBHOOK_BODY_BUDGET_GRANULE_BYTES).max(1)).unwrap_or(u32::MAX)
}

const fn rejected_http_status(error: WebhookHttpRejection) -> StatusCode {
    match error {
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
    if content_type != JSON_CONTENT_TYPE {
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
    if hex.len() != SHA256_HEX_BYTES {
        return Err(WebhookHttpRejection::InvalidSignature);
    }
    let mut decoded = [0_u8; SHA256_BYTES];
    for (index, pair) in hex.as_bytes().chunks_exact(2).enumerate() {
        decoded[index] = decode_hex_pair(pair)?;
    }
    Ok(decoded)
}

fn decode_hex_pair(pair: &[u8]) -> Result<u8, WebhookHttpRejection> {
    let high = decode_lower_hex(pair[0])?;
    let low = decode_lower_hex(pair[1])?;
    Ok((high << 4) | low)
}

const fn decode_lower_hex(byte: u8) -> Result<u8, WebhookHttpRejection> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        _ => Err(WebhookHttpRejection::InvalidSignature),
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

/// Per-hook admission windows, kept separate for requests that have proved the
/// shared secret and those that have not.
#[derive(Debug)]
pub(crate) struct WebhookRateLimiter {
    unverified: Mutex<HashMap<NonZeroU64, HookRateWindow>>,
    verified: Mutex<HashMap<NonZeroU64, HookRateWindow>>,
}

#[derive(Clone, Copy, Debug)]
struct HookRateWindow {
    started: Instant,
    admitted: u32,
}

impl WebhookRateLimiter {
    pub(crate) fn new() -> Self {
        Self {
            unverified: Mutex::new(HashMap::new()),
            verified: Mutex::new(HashMap::new()),
        }
    }

    pub(crate) fn admit_unverified(&self, hook_id: NonZeroU64) -> bool {
        Self::admit_at(
            &self.unverified,
            hook_id,
            Instant::now(),
            MAX_WEBHOOK_UNVERIFIED_REQUESTS_PER_MINUTE,
        )
    }

    pub(crate) fn admit_verified(&self, hook_id: NonZeroU64) -> bool {
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
            started: now,
            admitted: 0,
        });
        if now.duration_since(window.started) >= RATE_WINDOW {
            *window = HookRateWindow {
                started: now,
                admitted: 0,
            };
        }
        if window.admitted >= ceiling {
            return false;
        }
        window.admitted += 1;
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
                    started,
                    admitted: ceiling,
                },
            );
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, num::NonZeroU64, time::Duration};

    use axum::{
        body::Body,
        extract::State,
        http::{HeaderMap, HeaderValue, Request, StatusCode},
    };
    use hmac::{Hmac, KeyInit, Mac};
    use sha2::Sha256;
    use signalbox_domain::RepositorySlug;
    use signalbox_model_runtime::CredentialReference;
    use signalbox_persistence::repo_watch_webhook::PostgresRepoWatchWebhookStore;
    use sqlx::postgres::PgPoolOptions;
    use tempfile::TempDir;
    use tokio::sync::{Semaphore, mpsc};

    use super::{
        FileCredentialAccess, GitHubWebhookHeadersV1, MAX_WEBHOOK_BODY_BYTES,
        MAX_WEBHOOK_DELIVERIES_PER_MINUTE, MAX_WEBHOOK_IN_FLIGHT, MAX_WEBHOOK_SECRET_BYTES,
        MAX_WEBHOOK_UNVERIFIED_REQUESTS_PER_MINUTE, WEBHOOK_BODY_BUDGET_GRANULES,
        WEBHOOK_BODY_READ_TIMEOUT, WebhookBodyRejection, WebhookHookBinding, WebhookHttpRejection,
        WebhookHttpState, WebhookRateLimiter, admit_webhook, body_budget_granules,
        parse_github_headers, read_body_within_deadline, verify_github_signature,
    };

    const FIXTURE_HOOK_ID: NonZeroU64 = NonZeroU64::new(4_242).expect("fixture is positive");
    const FIXTURE_DELIVERY: &str = "550e8400-e29b-41d4-a716-446655440000";
    const FIXTURE_BODY: &[u8] = br#"{"repository":{"full_name":"keenwill/signalbox"}}"#;
    const FIXTURE_SECRET: &[u8] = b"correct horse battery staple";
    const FIXTURE_REPOSITORY: &str = "keenwill/signalbox";
    const FIXTURE_FOREIGN_BODY: &[u8] = br#"{"repository":{"full_name":"someone/else"}}"#;

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
        let directory = TempDir::new().expect("fixture directory is created");
        let secret_path = directory.path().join("webhook-secret");
        fs::write(&secret_path, FIXTURE_SECRET).expect("fixture secret is written");
        let secret_reference = CredentialReference::new("fixture-webhook");
        let (nudge, _receiver) = mpsc::channel(1);
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
        let pool = PgPoolOptions::new()
            .connect_lazy("postgres://fixture:fixture@127.0.0.1/fixture")
            .expect("fixture PostgreSQL URL is valid");
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
    fn per_hook_rate_window_reopens_at_one_minute() {
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
            started + Duration::from_secs(60),
            MAX_WEBHOOK_DELIVERIES_PER_MINUTE
        ));
    }

    #[test]
    fn an_unverified_flood_leaves_the_authenticated_allowance_intact() {
        let limiter = WebhookRateLimiter::new();
        let started = std::time::Instant::now();
        WebhookRateLimiter::saturate(
            &limiter.unverified,
            FIXTURE_HOOK_ID,
            started,
            MAX_WEBHOOK_UNVERIFIED_REQUESTS_PER_MINUTE,
        );

        assert!(!limiter.admit_unverified(FIXTURE_HOOK_ID));
        assert!(limiter.admit_verified(FIXTURE_HOOK_ID));
    }

    #[tokio::test(start_paused = true)]
    async fn a_stalled_body_read_rejects_at_the_deadline() {
        let stalled = std::future::pending::<Result<axum::body::Bytes, std::convert::Infallible>>();

        let rejection = read_body_within_deadline(stalled, WEBHOOK_BODY_READ_TIMEOUT)
            .await
            .expect_err("a body that never completes must not hold its permit");

        assert_eq!(rejection, WebhookBodyRejection::Deadline);
    }

    #[test]
    fn body_budget_reserves_one_granule_per_declared_chunk() {
        assert_eq!(body_budget_granules(0), 1);
        assert_eq!(body_budget_granules(1), 1);
        assert_eq!(body_budget_granules(64 * 1024), 1);
        assert_eq!(body_budget_granules(64 * 1024 + 1), 2);
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
