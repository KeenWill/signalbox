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
    str::FromStr as _,
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
    ETag as TypedEtag, HeaderMapExt as _, IfNoneMatch as TypedIfNoneMatch, IfRange as TypedIfRange,
    Range as TypedRange,
};
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
    WebSessionLiveStreamEvent, WebSessionTimelineDescriptor, WebSessionTimelineDetail,
    WebSessionTimelineDetailBody, WebSessionTimelineDetailPage, WebSessionTimelineEventKind,
    WebSessionTimelineItem, WebSessionTimelineSizeFacts, WebSessionTimelineWindow,
    WebSessionWorkFacts, WebTimelineAddress, WebTimelineBlobReference, WebTimelineBodyContinuation,
    WebTimelineBodyField, WebTimelineDetailContinuation, WebTimelineEventSequence,
    WebTimelineModelCallDisposition, WebTimelineModelCallState, WebTimelineModelUsage,
    WebTimelineTextExcerpt, WebTimelineTurnLifecycleKind, WebTurnId, WebU64,
    WebUsageAggregateGroup, WebUsageAggregateTokenAxes, WebUsageCall, WebUsageCallCount,
    WebUsageCallCursor, WebUsageCallKind, WebUsageCallPage, WebUsageCost, WebUsageCostLabel,
    WebUsageCostUnavailableReason, WebUsageInputSemantics, WebUsageProvenance, WebUsageRateVersion,
    WebUsageSummary, WebUsageTimestampMicros, WebUsageTokenAxes, WebUsageTokenCoverage, WebUuid,
};
use sqlx::{PgPool, types::Uuid};
use tokio::{
    io::AsyncReadExt as _,
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
// numeric-bound: guard - prevents an unbounded caller-supplied filename from reaching a response header
const MAX_DISPLAY_FILENAME_BYTES: usize = 1024;
const BLOB_STREAM_CHUNK_BYTES: usize = 64 * 1024;
// numeric-bound: guard - prevents concurrent blob reads from exhausting process memory and store handles
const MAX_CONCURRENT_WEB_BLOB_READS: usize = 4;
// numeric-bound: guard - prevents a wedged blob store from holding a read permit forever
const BLOB_RESPONSE_TIMEOUT_SECONDS: u64 = 120;

#[derive(Clone, Debug)]
struct WebHttpState {
    blobs: Option<WebBlobRuntime>,
    blob_read_budget: Arc<Semaphore>,
}

#[cfg(test)]
mod live_follow_tests {
    use std::{num::NonZeroU64, sync::Arc, time::Duration};

    use signalbox_domain::{ModelCallId, SessionId, TurnId};
    use signalbox_web_contract::{
        MAX_NDJSON_ITEM_BYTES, WebLiveResourceId, WebPositiveU64, WebSessionLiveStreamEvent,
        WebSessionTimelineEventKind, WebTimelineAddress, WebTimelineEventSequence, WebTurnId,
        WebU64,
    };
    use sqlx::types::Uuid;
    use tokio::sync::watch;

    use super::{LiveFollowState, live_follow_next};
    use crate::{ProcessMonitor, ProcessMonitorUpdate};

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
            observed_through: NonZeroU64::new(observed_through)
                .expect("test snapshot cursors are positive"),
            covered_at_snapshot: queued_at_snapshot,
            queued_at_snapshot,
            pending: std::collections::VecDeque::new(),
            provider_fragment: None,
            shutdown: None,
            ended: false,
        }
    }

    fn provider_text_content_length(event: WebSessionLiveStreamEvent) -> usize {
        let WebSessionLiveStreamEvent::ProviderTextDelta { content, .. } = event else {
            panic!("expected provider text delta");
        };
        content.len()
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
            text: Arc::from("draft"),
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
                turn_id: WebTurnId::from_uuid_bytes(live_turn().into_uuid().into_bytes()),
                model_call_id: WebLiveResourceId::from_uuid_bytes(
                    live_call().into_uuid().into_bytes()
                ),
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
                        NonZeroU64::new(8).expect("the fixture cursor is positive"),
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
            text: Arc::from("stale draft"),
        });
        state.covered_at_snapshot = state.subscription.queued_len();
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
                        NonZeroU64::new(8).expect("the fixture cursor is positive"),
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
                cursor: WebPositiveU64::from_nonzero(
                    NonZeroU64::new(7).expect("the fixture cursor is positive"),
                ),
            }
        );
    }

    #[tokio::test]
    async fn live_follow_consumes_lag_covered_by_snapshot_cutoff() {
        let monitor = ProcessMonitor::test_channel();
        let mut state = live_follow_state(&monitor, 7, 0);
        monitor.fill_for_test(ProcessMonitorUpdate::ProviderTextDelta {
            session: live_session(),
            turn: live_turn(),
            call: live_call(),
            part_index: 0,
            text: Arc::from("stale draft"),
        });
        state.covered_at_snapshot = state.subscription.queued_len();
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
                        NonZeroU64::new(8).expect("the fixture cursor is positive"),
                    ),
                },
                event_kind: WebSessionTimelineEventKind::TurnCompleted,
            }
        );
    }

    #[tokio::test]
    async fn live_follow_discards_a_draft_raced_between_sample_and_snapshot() {
        let monitor = ProcessMonitor::test_channel();
        let mut state = live_follow_state(&monitor, 7, 0);
        state.covered_at_snapshot = state.subscription.queued_len();
        monitor.publish_for_test(ProcessMonitorUpdate::ProviderTextDelta {
            session: live_session(),
            turn: live_turn(),
            call: live_call(),
            part_index: 0,
            text: Arc::from("raced draft"),
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
                        NonZeroU64::new(8).expect("the fixture cursor is positive"),
                    ),
                },
                event_kind: WebSessionTimelineEventKind::TurnCompleted,
            }
        );
    }

    #[tokio::test]
    async fn live_follow_resyncs_a_saturated_monitor_before_draining_fragments() {
        let monitor = ProcessMonitor::test_channel();
        let mut state = live_follow_state(&monitor, 7, 0);
        state.provider_fragment = Some(super::PendingProviderTextDelta {
            turn_id: WebTurnId::from_uuid_bytes(live_turn().into_uuid().into_bytes()),
            model_call_id: WebLiveResourceId::from_uuid_bytes(live_call().into_uuid().into_bytes()),
            part_index: 0,
            text: Arc::from("retained draft"),
            offset: 0,
            emitted_empty: false,
        });
        monitor.fill_for_test(ProcessMonitorUpdate::Durable {
            cursor: 8,
            session: live_session(),
            kind: signalbox_application::SessionTimelineEventKind::TurnActivated,
        });
        let (event, state) = live_follow_next(state)
            .await
            .expect("saturation produces one explicit terminal event");

        assert_eq!(
            event,
            WebSessionLiveStreamEvent::ResyncRequired {
                cursor: WebPositiveU64::from_nonzero(
                    NonZeroU64::new(7).expect("the fixture cursor is positive"),
                ),
            }
        );
        assert!(state.ended);
        assert!(state.provider_fragment.is_none());
    }

    #[tokio::test]
    async fn live_follow_fragments_provider_text_lazily() {
        let monitor = ProcessMonitor::test_channel();
        let state = live_follow_state(&monitor, 7, 0);
        let text: Arc<str> = Arc::from("x".repeat(super::MAX_WEB_PROVIDER_TEXT_FRAGMENT_BYTES + 1));
        monitor.publish_for_test(ProcessMonitorUpdate::ProviderTextDelta {
            session: live_session(),
            turn: live_turn(),
            call: live_call(),
            part_index: 0,
            text: Arc::clone(&text),
        });
        let (first, state) = live_follow_next(state)
            .await
            .expect("the first fragment is delivered");
        let retained = state
            .provider_fragment
            .as_ref()
            .expect("the unencoded suffix remains pending");

        assert!(state.pending.is_empty());
        assert!(Arc::ptr_eq(&text, &retained.text));
        assert_eq!(retained.offset, super::MAX_WEB_PROVIDER_TEXT_FRAGMENT_BYTES);
        assert_eq!(
            provider_text_content_length(first),
            super::MAX_WEB_PROVIDER_TEXT_FRAGMENT_BYTES,
        );
    }

    #[tokio::test]
    async fn live_follow_ends_when_http_shutdown_is_requested() {
        let monitor = ProcessMonitor::test_channel();
        let mut state = live_follow_state(&monitor, 7, 0);
        let (shutdown_sender, shutdown) = watch::channel(false);
        state.shutdown = Some(shutdown);
        shutdown_sender
            .send(true)
            .expect("the follow stream observes shutdown");

        let event = tokio::time::timeout(Duration::from_secs(1), live_follow_next(state))
            .await
            .expect("shutdown ends the follow stream promptly");

        assert!(event.is_none());
    }

    #[tokio::test]
    async fn live_follow_ends_when_the_shutdown_sender_is_dropped() {
        let monitor = ProcessMonitor::test_channel();
        let mut state = live_follow_state(&monitor, 7, 0);
        let (shutdown_sender, shutdown) = watch::channel(false);
        state.shutdown = Some(shutdown);
        drop(shutdown_sender);

        let event = tokio::time::timeout(Duration::from_secs(1), live_follow_next(state))
            .await
            .expect("a closed shutdown channel ends the follow stream promptly");

        assert!(event.is_none());
    }

    /// The retained-fragment fast path returns before the monitor is polled,
    /// so it must observe channel closure itself: provider delta text has no
    /// size bound, and draining it for a slow client would otherwise keep
    /// graceful shutdown waiting arbitrarily long.
    #[tokio::test]
    async fn live_follow_stops_draining_a_retained_fragment_on_closed_shutdown() {
        let monitor = ProcessMonitor::test_channel();
        let mut state = live_follow_state(&monitor, 7, 0);
        state.provider_fragment = Some(super::PendingProviderTextDelta {
            turn_id: WebTurnId::from_uuid_bytes(live_turn().into_uuid().into_bytes()),
            model_call_id: WebLiveResourceId::from_uuid_bytes(live_call().into_uuid().into_bytes()),
            part_index: 0,
            text: Arc::from("retained draft"),
            offset: 0,
            emitted_empty: false,
        });
        let (shutdown_sender, shutdown) = watch::channel(false);
        state.shutdown = Some(shutdown);
        drop(shutdown_sender);

        let event = tokio::time::timeout(Duration::from_secs(1), live_follow_next(state))
            .await
            .expect("a closed shutdown channel preempts retained fragments promptly");

        assert!(event.is_none());
    }

    #[test]
    fn provider_text_fragment_fits_after_worst_case_json_escaping() {
        let source = "\u{0001}".repeat(super::MAX_WEB_PROVIDER_TEXT_FRAGMENT_BYTES);
        let content = super::next_web_text_fragment(&source, &mut 0, &mut false)
            .expect("nonempty provider text has a first fragment");
        let encoded = super::encode_ndjson_item(WebSessionLiveStreamEvent::ProviderTextDelta {
            turn_id: WebTurnId::from_uuid_bytes(live_turn().into_uuid().into_bytes()),
            model_call_id: WebLiveResourceId::from_uuid_bytes(live_call().into_uuid().into_bytes()),
            part_index: u32::MAX,
            content,
        })
        .expect("the worst-case escaped fragment remains below the NDJSON ceiling");

        assert!(encoded.len() <= MAX_NDJSON_ITEM_BYTES + 1);
    }
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
    pub fn into_runtime(self, monitor: ProcessMonitor) -> WebHttpRuntime {
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
        );
        WebHttpRuntime {
            listener: self.listener,
            router,
            follow_shutdown: Some(follow_shutdown),
        }
    }
}

impl WebHttpRuntime {
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
        let snapshot_reader_budget = super::process_runtime::shared_snapshot_reader_budget(
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
) -> Router {
    let snapshot_reader_budget = pool.as_ref().and_then(|pool| {
        super::process_runtime::shared_snapshot_reader_budget(
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
    )
}

fn production_router_with_budget(
    asset_root: Option<PathBuf>,
    pool: Option<PgPool>,
    blobs: Option<WebBlobRuntime>,
    model_configuration: Option<HubModelConfiguration>,
    blob_store_registry: Option<Arc<BlobStoreRegistry>>,
    read_runtime: ProductionReadRuntime,
) -> Router {
    let http_state = WebHttpState {
        blobs,
        blob_read_budget: Arc::new(Semaphore::new(MAX_CONCURRENT_WEB_BLOB_READS)),
    };
    let automatic_resume_attempts =
        configured_automatic_resume_attempts(model_configuration.as_ref());
    let state = WebApiState {
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
    };
    // Every route that reads session data sits behind the loopback authority
    // gate. The attention projection returns session identities, goal-need
    // summaries, and operator state, so it belongs here for the same reason the
    // descriptor and timeline reads do: the listener is unauthenticated, and a
    // rebound origin must not reach session data with an attacker's authority.
    // `same_origin_router` additionally gates the whole listener, `/bootstrap`
    // and the static assets included, so this route layer is the inner of two.
    let session_reads = Router::new()
        .route("/sessions/{session_id}", get(session_descriptor))
        .route(
            "/sessions/{session_id}/timeline",
            get(session_timeline_window),
        )
        .route("/sessions/{session_id}/live", get(session_live_snapshot))
        .route("/sessions/{session_id}/follow", get(session_live_follow))
        .route("/sessions", get(session_catalog))
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
        .merge(blob_reads);
    // Imported-conversation reads need both a pool and hub model settings; the
    // bootstrap and session surfaces stay routable without either.
    let api = match (pool, model_configuration) {
        (Some(pool), Some(model_configuration)) => api.nest(
            "/imports",
            web_imports::router(pool, model_configuration, blob_store_registry),
        ),
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
    router.layer(middleware::from_fn(validate_loopback_host))
}

#[derive(Clone, Debug)]
struct WebApiState {
    attention: Option<AttentionRepository>,
    timeline: Option<SessionTimelineRepository>,
    live: Option<SessionLiveRepository>,
    search: Option<SearchRepository>,
    usage: Option<UsageRepository>,
    model_configuration: Option<Arc<HubModelConfiguration>>,
    snapshot_reader_budget: Option<Arc<Semaphore>>,
    shutdown: Option<watch::Receiver<bool>>,
    monitor: Option<ProcessMonitor>,
}

#[derive(Debug, Default)]
struct SessionCatalogQuery {
    search: Option<String>,
    required_tag: Vec<String>,
    include_archived: Option<String>,
    sort: Option<String>,
    after_session_id: Option<String>,
    after_activity_unix_microseconds: Option<String>,
}

async fn session_catalog(State(state): State<WebApiState>, RawQuery(query): RawQuery) -> Response {
    let query = match parse_session_catalog_query(query.as_deref()) {
        Ok(query) => query,
        Err(()) => return invalid_attention_query(),
    };
    let query = match parse_attention_query(query) {
        Ok(query) => query,
        Err(()) => return invalid_attention_query(),
    };
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
    let Ok(_permit) = budget.acquire().await else {
        return attention_projection_error(None);
    };
    match repository.snapshot(query).await {
        Ok(snapshot) => match session_catalog_snapshot_dto(snapshot) {
            Ok(snapshot) => Json(snapshot).into_response(),
            Err(()) => attention_projection_error(None),
        },
        Err(error) => attention_projection_error(Some(error)),
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AttentionPageQuery {
    after_session_id: Option<String>,
}

async fn attention_snapshot(
    State(state): State<WebApiState>,
    query: Result<Query<AttentionPageQuery>, QueryRejection>,
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
    let continuation = match query.after_session_id {
        Some(value) => match parse_canonical_session_id(&value) {
            Ok(session) => Some(session),
            Err(()) => {
                return application_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_session_id",
                    "attention continuation is not a canonical UUID",
                );
            }
        },
        None => None,
    };
    let query = attention_page_query(continuation);
    let Some(budget) = state.snapshot_reader_budget else {
        return attention_projection_error(None);
    };
    let Ok(_permit) = budget.acquire().await else {
        return attention_projection_error(None);
    };
    match repository.page(query).await {
        Ok(snapshot) => match attention_snapshot_dto(snapshot) {
            Ok(snapshot) => Json(snapshot).into_response(),
            Err(()) => attention_projection_error(None),
        },
        Err(error) => attention_projection_error(Some(error)),
    }
}

fn parse_session_catalog_query(raw: Option<&str>) -> Result<SessionCatalogQuery, ()> {
    let mut query = SessionCatalogQuery::default();
    let mut filter_bytes = 0_usize;
    for (key, value) in url::form_urlencoded::parse(raw.unwrap_or_default().as_bytes()) {
        let value = value.into_owned();
        match key.as_ref() {
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

fn parse_catalog_canonical_u64(value: &str) -> Result<u64, ()> {
    let parsed = value.parse::<u64>().map_err(|_| ())?;
    (parsed.to_string() == value).then_some(parsed).ok_or(())
}

fn parse_canonical_session_id(value: &str) -> Result<SessionId, ()> {
    let parsed = value.parse::<Uuid>().map_err(|_| ())?;
    if value != parsed.hyphenated().to_string() {
        return Err(());
    }
    Ok(SessionId::from_uuid(parsed))
}

fn attention_page_query(after: Option<SessionId>) -> AttentionQuery {
    AttentionQuery::identity_page(after)
}

fn parse_attention_query(query: SessionCatalogQuery) -> Result<AttentionQuery, ()> {
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
        .map(|value| parse_catalog_canonical_u64(&value))
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
        snapshot = repository.page(attention_page_query(None)) => snapshot,
    } {
        Ok(snapshot) => snapshot,
        Err(error) => return attention_projection_error(Some(error)),
    };
    drop(snapshot_permit);
    let cursor = snapshot.cursor;
    let live_page_has_capacity = snapshot.continuation.is_none();
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
    let page_boundary = visible_sessions.last();
    summaries.iter().any(|summary| {
        !visible_sessions.contains(&summary.session)
            && (live_page_has_capacity
                || page_boundary.is_some_and(|boundary| summary.session < *boundary))
    })
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

fn attention_snapshot_dto(snapshot: AttentionPage) -> Result<WebAttentionSnapshot, ()> {
    if snapshot.sort != AttentionSort::SessionIdentityAscending {
        return Err(());
    }
    let continuation_after_session_id = match snapshot.continuation {
        Some(AttentionContinuation::SessionIdentity(session)) => {
            Some(session.into_uuid().to_string())
        }
        None => None,
        Some(AttentionContinuation::LastActivity { .. }) => return Err(()),
    };
    Ok(WebAttentionSnapshot {
        cursor: snapshot.cursor.value().to_string(),
        summaries: snapshot
            .summaries
            .into_iter()
            .map(attention_summary_dto)
            .collect::<Result<Vec<_>, _>>()?,
        continuation_after_session_id,
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
    let goal_block = attention_goal_block_dto(summary.goal_block)?;
    Ok(WebAttentionSummary {
        session_id: summary.session.into_uuid().to_string(),
        current_turn_id: summary
            .current_turn
            .map(|turn| turn.into_uuid().to_string()),
        state: web_attention_state(summary.state),
        lifecycle_state: web_attention_lifecycle_state(summary.lifecycle_state),
        action: summary.action.map(web_attention_action),
        goal_block,
        judge: WebAttentionJudgeFacts {
            actionable: summary.judge.actionable.to_string(),
            completed: summary.judge.completed.to_string(),
            escalated: summary.judge.escalated.to_string(),
            failed: summary.judge.failed.to_string(),
        },
        last_activity: WebAttentionActivity {
            unix_milliseconds,
            kind: web_attention_activity_kind(summary.last_activity.kind),
        },
    })
}

fn session_catalog_snapshot_dto(
    snapshot: AttentionSnapshot,
) -> Result<WebSessionCatalogSnapshot, ()> {
    let continuation = snapshot
        .continuation
        .map(|continuation| match continuation {
            AttentionContinuation::LastActivity {
                recorded_at,
                session,
            } => Ok(WebSessionCatalogContinuation::LastActivity {
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
                Ok(WebSessionCatalogContinuation::SessionIdentity {
                    session_id: WebSessionId::from_uuid_bytes(session.into_uuid().into_bytes()),
                })
            }
        })
        .transpose()?;
    Ok(WebSessionCatalogSnapshot {
        cursor: WebU64::from_u64(snapshot.cursor.value()),
        total: WebU64::from_u64(snapshot.total),
        sort: match snapshot.sort {
            AttentionSort::LastActivityDescending => WebSessionCatalogSort::LastActivityDescending,
            AttentionSort::SessionIdentityAscending => {
                WebSessionCatalogSort::SessionIdentityAscending
            }
        },
        summaries: snapshot
            .summaries
            .into_iter()
            .map(session_catalog_summary_dto)
            .collect::<Result<Vec<_>, _>>()?,
        continuation,
    })
}

fn session_catalog_summary_dto(summary: AttentionSummary) -> Result<WebSessionCatalogSummary, ()> {
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
    let goal_block = attention_goal_block_dto(summary.goal_block)?;
    Ok(WebSessionCatalogSummary {
        session_id: WebSessionId::from_uuid_bytes(summary.session.into_uuid().into_bytes()),
        title_summary: summary.title_summary,
        title_truncated: summary.title_truncated,
        archived: summary.archived,
        current_turn_id: summary
            .current_turn
            .map(|turn| WebUuid::from_validated_uuid(turn.into_uuid().to_string())),
        active_turn_count: WebU64::from_u64(summary.active_turn_count),
        queued_turn_count: WebU64::from_u64(summary.queued_turn_count),
        state: web_attention_state(summary.state),
        action: summary.action.map(web_attention_action),
        goal_block,
        judge: WebAttentionJudgeFacts {
            actionable: summary.judge.actionable.to_string(),
            completed: summary.judge.completed.to_string(),
            escalated: summary.judge.escalated.to_string(),
            failed: summary.judge.failed.to_string(),
        },
        last_activity: WebSessionCatalogActivity {
            unix_microseconds: WebU64::from_u64(unix_microseconds),
            kind: web_attention_activity_kind(summary.last_activity.kind),
        },
    })
}

fn attention_goal_block_dto(
    goal: Option<AttentionGoalBlock>,
) -> Result<Option<WebAttentionGoalBlock>, ()> {
    goal.map(|goal| {
        if goal.need_summary.chars().count() > usize::from(max_attention_goal_summary_characters())
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
                AttentionBlockedReason::FinishCheckFailed => {
                    WebAttentionBlockedReason::FinishCheckFailed
                }
            },
            need_summary: goal.need_summary,
        })
    })
    .transpose()
}

const fn web_attention_lifecycle_state(
    state: AttentionLifecycleState,
) -> WebAttentionLifecycleState {
    match state {
        AttentionLifecycleState::Created => WebAttentionLifecycleState::Created,
        AttentionLifecycleState::Dispatched => WebAttentionLifecycleState::Dispatched,
        AttentionLifecycleState::Active => WebAttentionLifecycleState::Active,
        AttentionLifecycleState::Waiting => WebAttentionLifecycleState::Waiting,
        AttentionLifecycleState::Recovering => WebAttentionLifecycleState::Recovering,
        AttentionLifecycleState::Blocked => WebAttentionLifecycleState::Blocked,
        AttentionLifecycleState::Parked => WebAttentionLifecycleState::Parked,
        AttentionLifecycleState::Terminal => WebAttentionLifecycleState::Terminal,
    }
}

const fn web_attention_state(state: AttentionState) -> WebAttentionState {
    match state {
        AttentionState::Active => WebAttentionState::Active,
        AttentionState::Queued => WebAttentionState::Queued,
        AttentionState::Blocked => WebAttentionState::Blocked,
        AttentionState::AwaitingApproval => WebAttentionState::AwaitingApproval,
        AttentionState::Ambiguous => WebAttentionState::Ambiguous,
        AttentionState::AwaitingToolRecovery => WebAttentionState::AwaitingToolRecovery,
        AttentionState::AwaitingReconciliation => WebAttentionState::AwaitingReconciliation,
        AttentionState::RunnerLost => WebAttentionState::RunnerLost,
        AttentionState::Parked => WebAttentionState::Parked,
        AttentionState::Idle => WebAttentionState::Idle,
    }
}

const fn web_attention_action(action: AttentionAction) -> WebAttentionAction {
    match action {
        AttentionAction::ProvideGoalNeed => WebAttentionAction::ProvideGoalNeed,
        AttentionAction::DecideApproval => WebAttentionAction::DecideApproval,
        AttentionAction::ReconcileTurn => WebAttentionAction::ReconcileTurn,
    }
}

const fn web_attention_activity_kind(kind: AttentionActivityKind) -> WebAttentionActivityKind {
    match kind {
        AttentionActivityKind::Session => WebAttentionActivityKind::Session,
        AttentionActivityKind::Turn => WebAttentionActivityKind::Turn,
        AttentionActivityKind::Goal => WebAttentionActivityKind::Goal,
        AttentionActivityKind::ApprovalJudge => WebAttentionActivityKind::ApprovalJudge,
        AttentionActivityKind::Runner => WebAttentionActivityKind::Runner,
    }
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
    // The lexical read holds one pooled connection across `SET TRANSACTION`,
    // the term probe, and the page query, so it is a snapshot reader on the
    // same footing as the attention snapshot: it draws its permit from the
    // daemon-wide budget that reserves pool connections for mutations and
    // outbox work. Admitting it after the query and repository checks keeps a
    // malformed or unconfigured request from spending a permit.
    let Some(budget) = state.snapshot_reader_budget else {
        return search_projection_failed();
    };
    let Ok(_permit) = budget.acquire().await else {
        return search_projection_failed();
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
    value
        .parse::<u64>()
        .ok()
        .and_then(std::num::NonZeroU64::new)
}

fn parse_positive_i64(value: &str) -> Option<std::num::NonZeroU64> {
    value
        .parse::<i64>()
        .ok()
        .and_then(|value| u64::try_from(value).ok())
        .and_then(std::num::NonZeroU64::new)
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
    search_projection_failed()
}

fn search_projection_failed() -> Response {
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
    // The aggregate read holds one pooled connection for its grouped scan, so
    // it is a snapshot reader on the same footing as the attention snapshot
    // and the lexical page: it draws its permit from the daemon-wide budget
    // that reserves pool connections for mutations and outbox work. Admitting
    // it after the query and repository checks keeps a malformed or
    // unconfigured request from spending a permit.
    let Some(budget) = state.snapshot_reader_budget else {
        return usage_projection_failed();
    };
    let Ok(_permit) = budget.acquire().await else {
        return usage_projection_failed();
    };
    match repository.aggregate(query).await {
        Ok(report) => Json(WebUsageSummary {
            groups: report
                .groups()
                .iter()
                .map(|group| usage_aggregate_dto(group, &configuration))
                .collect(),
            truncated: report.completeness() == UsageAggregateCompleteness::Truncated,
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
    // Same footing as the aggregate read above: one pooled connection for the
    // keyset page, drawn from the shared snapshot-reader budget after the
    // request and repository checks.
    let Some(budget) = state.snapshot_reader_budget else {
        return usage_projection_failed();
    };
    let Ok(_permit) = budget.acquire().await else {
        return usage_projection_failed();
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
                .calls()
                .iter()
                .map(|call| usage_call_dto(call, &configuration))
                .collect(),
            continuation: page.next().map(|cursor| WebUsageCallCursor {
                recorded_at_micros: WebUsageTimestampMicros::from_application(
                    cursor.recorded_at.get(),
                ),
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
    let time = UsageTimeRange::new(
        from_inclusive.map(UsageTimeFromInclusive),
        to_exclusive.map(UsageTimeToExclusive),
    )
    .ok()?;
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
    UsageTimestampMicros::new(value.parse().ok()?).ok()
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
        "context_compaction" => Some(UsageCallKind::ContextCompaction),
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

fn usage_projection_failed() -> Response {
    application_error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "usage_projection_failed",
        "the durable usage projection could not be read",
    )
}

fn usage_repository_error(error: UsageRepositoryError) -> Response {
    let failure_class = match &error {
        UsageRepositoryError::Database(_) => "infrastructure",
        UsageRepositoryError::Corruption(_) => "fail_closed_corruption",
    };
    tracing::error!(failure_class, cause = %error, "usage projection read failed");
    usage_projection_failed()
}

fn usage_aggregate_dto(
    group: &UsageAggregateGroup,
    configuration: &HubModelConfiguration,
) -> WebUsageAggregateGroup {
    WebUsageAggregateGroup {
        call_kind: usage_call_kind_dto(group.key().call_kind),
        model_id: web_uuid(group.key().model.identity().into_uuid()),
        profile_id: signalbox_web_contract::WebUsageProfileId::from_bounded(
            group.key().credential_profile.as_str().to_owned(),
        ),
        provenance: usage_provenance_dto(group.key().provenance),
        input_semantics: usage_input_semantics_dto(group.key().input_semantics),
        coverage: WebUsageTokenCoverage {
            input: group.key().coverage.input == UsageTokenPresence::Present,
            output: group.key().coverage.output == UsageTokenPresence::Present,
            cache_creation_input: group.key().coverage.cache_creation_input
                == UsageTokenPresence::Present,
            cache_read_input: group.key().coverage.cache_read_input == UsageTokenPresence::Present,
        },
        call_count: WebUsageCallCount::from_positive(group.call_count()),
        tokens: usage_aggregate_tokens_dto(group.tokens()),
        cost: usage_aggregate_cost_dto(configuration, group),
    }
}

fn usage_call_dto(call: &UsageCallEvidence, configuration: &HubModelConfiguration) -> WebUsageCall {
    WebUsageCall {
        call_kind: usage_call_kind_dto(call.scope.call_kind()),
        call_id: web_uuid(call.call.into_uuid()),
        session_id: WebSessionId::from_uuid_bytes(*call.session.into_uuid().as_bytes()),
        turn_id: call.scope.turn().map(|turn| web_uuid(turn.into_uuid())),
        model_id: web_uuid(call.model.identity().into_uuid()),
        profile_id: signalbox_web_contract::WebUsageProfileId::from_bounded(
            call.credential_profile.as_str().to_owned(),
        ),
        provenance: usage_provenance_dto(call.provenance),
        input_semantics: usage_input_semantics_dto(call.input_semantics),
        tokens: usage_tokens_dto(call.tokens),
        recorded_at_micros: WebUsageTimestampMicros::from_application(call.recorded_at.get()),
        cost: usage_cost_dto(
            configuration,
            call.model,
            call.credential_reference.as_deref(),
            call.input_semantics,
            call.tokens,
            true,
        ),
    }
}

fn usage_cost_dto(
    configuration: &HubModelConfiguration,
    model: ResolvedProviderTarget,
    credential_profile: Option<&str>,
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
            if tokens.input.is_some_and(|input| {
                tokens
                    .cache_creation_input
                    .zip(tokens.cache_read_input)
                    .is_some_and(|(creation, read)| {
                        creation.checked_add(read).is_none_or(|cache| input < cache)
                    })
            }) {
                return unavailable(WebUsageCostUnavailableReason::InvalidCacheBreakdown);
            }
            ProcessModelCallInputTokenSemantics::CacheInclusive
        }
    };
    if !cost_derivation_safe {
        return unavailable(WebUsageCostUnavailableReason::InvalidCacheBreakdown);
    }
    let Some(credential_profile) = credential_profile else {
        return unavailable(WebUsageCostUnavailableReason::ConfigurationUnavailable);
    };
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
    let tokens = group.tokens();
    if tokens.input.is_none()
        && tokens.output.is_none()
        && tokens.cache_creation_input.is_none()
        && tokens.cache_read_input.is_none()
    {
        return unavailable(WebUsageCostUnavailableReason::NoTokenEvidence);
    }
    let semantics = match group.key().input_semantics {
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
            if tokens
                .cache_creation_input
                .zip(tokens.cache_read_input)
                .is_some_and(|(creation, read)| {
                    creation
                        .checked_add(read)
                        .is_none_or(|cache| tokens.input.is_some_and(|input| input < cache))
                })
            {
                return unavailable(WebUsageCostUnavailableReason::InvalidCacheBreakdown);
            }
            ProcessModelCallInputTokenSemantics::CacheInclusive
        }
    };
    // `Unsafe` conflates two distinct states: a constituent call whose cache
    // breakdown contradicts its input total, and a group that never reported
    // the cache axes normalization would need. Only the first contradicts the
    // evidence. When the group's coverage lacks an axis, normalization is
    // merely incomplete, and the independently reported axes stay priceable
    // exactly as they do on the individual-call path.
    if group.key().input_semantics == UsageInputTokenSemantics::CacheInclusive
        && group.cache_normalization() == UsageCacheNormalization::Unsafe
        && tokens.input.is_some()
        && tokens.cache_creation_input.is_some()
        && tokens.cache_read_input.is_some()
    {
        return unavailable(WebUsageCostUnavailableReason::InvalidCacheBreakdown);
    }
    let Some(credential_reference) = group.key().credential_reference.as_deref() else {
        return unavailable(WebUsageCostUnavailableReason::ConfigurationUnavailable);
    };
    let Some(cost) = configuration.derive_usage_aggregate_cost(
        group.key().model,
        credential_reference,
        semantics,
        [
            tokens.input,
            tokens.output,
            tokens.cache_creation_input,
            tokens.cache_read_input,
        ],
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
    InvalidProjectedSessionId,
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
    let (Some(first), Some(through)) = (query.first.as_deref(), query.through.as_deref()) else {
        return invalid_timeline_detail_query();
    };
    let first = match parse_timeline_address(first) {
        Ok(address) => address,
        Err(error) => return error.into_response(),
    };
    let through = match parse_timeline_address(through) {
        Ok(address) => address,
        Err(error) => return error.into_response(),
    };
    let detail_query = TimelineDetailQuery {
        max_items: query.max_items,
        max_bytes: query.max_bytes,
        cursor_address: query.cursor_address,
        cursor_field: query.cursor_field,
        cursor_member: query.cursor_member,
        cursor_offset: query.cursor_offset,
    };
    let Some((limits, cursor)) = parse_detail_query(&detail_query) else {
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
        SessionTimelineRepositoryError::InvalidStoredUtf8
        | SessionTimelineRepositoryError::Corruption(_) => "fail_closed_corruption",
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
        SessionTimelineDetailBody::UserInput {
            turn_id,
            text,
            attachments,
        } => WebSessionTimelineDetailBody::UserInput {
            turn_id: WebSessionId::from_canonical(turn_id.into_uuid().to_string())
                .ok_or(SessionTimelineRequestError::InvalidProjectedSessionId)?,
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
            turn_id: WebSessionId::from_canonical(turn_id.into_uuid().to_string())
                .ok_or(SessionTimelineRequestError::InvalidProjectedSessionId)?,
            model_call_id: WebSessionId::from_canonical(model_call_id.into_uuid().to_string())
                .ok_or(SessionTimelineRequestError::InvalidProjectedSessionId)?,
            state: model_call_state_dto(state),
            model_identity_id: WebSessionId::from_canonical(
                model_identity_id.into_uuid().to_string(),
            )
            .ok_or(SessionTimelineRequestError::InvalidProjectedSessionId)?,
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
        SessionTimelineDetailBody::TurnLifecycle {
            turn_id,
            lifecycle,
            cause_code,
        } => WebSessionTimelineDetailBody::TurnLifecycle {
            turn_id: WebSessionId::from_canonical(turn_id.into_uuid().to_string())
                .ok_or(SessionTimelineRequestError::InvalidProjectedSessionId)?,
            lifecycle: match lifecycle {
                TimelineTurnLifecycleKind::Activated => WebTimelineTurnLifecycleKind::Activated,
                TimelineTurnLifecycleKind::Terminalized => {
                    WebTimelineTurnLifecycleKind::Terminalized
                }
            },
            cause_code,
        },
        SessionTimelineDetailBody::EventFact { kind } => WebSessionTimelineDetailBody::EventFact {
            kind: event_kind_dto(kind),
        },
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
        },
        member_index: continuation.member_index,
        offset_bytes: WebU64::from_u64(continuation.offset_bytes),
    }
}

fn event_kind_dto(kind: SessionTimelineEventKind) -> WebSessionTimelineEventKind {
    match kind {
        SessionTimelineEventKind::SessionCreated => WebSessionTimelineEventKind::SessionCreated,
        SessionTimelineEventKind::SessionStateChanged => {
            WebSessionTimelineEventKind::SessionStateChanged
        }
        SessionTimelineEventKind::SessionTerminal => WebSessionTimelineEventKind::SessionTerminal,
        SessionTimelineEventKind::GoalChanged => WebSessionTimelineEventKind::GoalChanged,
        SessionTimelineEventKind::CommandSettled => WebSessionTimelineEventKind::CommandSettled,
        SessionTimelineEventKind::InjectionSettled => WebSessionTimelineEventKind::InjectionSettled,
        SessionTimelineEventKind::SessionOwnershipChanged => {
            WebSessionTimelineEventKind::SessionOwnershipChanged
        }
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
    let mut shutdown = state.shutdown;
    let session = match parse_session_id(&session_id) {
        Ok(session) => session,
        Err(error) => return error.into_response(),
    };
    let Some(repository) = state.live else {
        return live_projection_unavailable();
    };
    let Some(budget) = state.snapshot_reader_budget else {
        return live_projection_unavailable();
    };
    let snapshot_permit = tokio::select! {
        () = live_follow_shutdown(&mut shutdown) => return live_projection_unavailable(),
        permit = budget.acquire() => permit,
    };
    let Ok(_snapshot_permit) = snapshot_permit else {
        return live_projection_unavailable();
    };
    match tokio::select! {
        () = live_follow_shutdown(&mut shutdown) => return live_projection_unavailable(),
        snapshot = repository.read_live_snapshot(session) => snapshot,
    } {
        Ok(Some(snapshot)) => match live_snapshot_dto(snapshot) {
            Some(snapshot) => Json(snapshot).into_response(),
            None => live_projection_corruption(),
        },
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
    let mut shutdown = state.shutdown;
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
    let Some(budget) = state.snapshot_reader_budget else {
        return live_projection_unavailable();
    };
    let snapshot_permit = tokio::select! {
        () = live_follow_shutdown(&mut shutdown) => return empty_ndjson_response(),
        permit = Arc::clone(&budget).acquire_owned() => permit,
    };
    let Ok(snapshot_permit) = snapshot_permit else {
        return live_projection_unavailable();
    };
    let subscription = monitor.subscribe();
    let covered_at_snapshot = subscription.queued_len();
    let snapshot = match tokio::select! {
        () = live_follow_shutdown(&mut shutdown) => return empty_ndjson_response(),
        snapshot = repository
            .read_live_snapshot_at_completion(session, || subscription.queued_len()) => snapshot,
    } {
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
    drop(snapshot_permit);
    let (snapshot, queued_at_snapshot) = snapshot;
    let Some(observed_through) = NonZeroU64::new(snapshot.observed_through) else {
        return live_projection_corruption();
    };
    let mut pending = VecDeque::new();
    let Some(snapshot) = live_snapshot_dto(snapshot) else {
        return live_projection_corruption();
    };
    pending.push_back(WebSessionLiveStreamEvent::Snapshot {
        snapshot: Box::new(snapshot),
    });
    let source = stream::unfold(
        LiveFollowState {
            subscription,
            session,
            observed_through,
            covered_at_snapshot,
            queued_at_snapshot,
            pending,
            provider_fragment: None,
            shutdown,
            ended: false,
        },
        live_follow_next,
    );
    ndjson_response(source)
}

struct LiveFollowState {
    subscription: crate::ProcessMonitorSubscription,
    session: SessionId,
    observed_through: NonZeroU64,
    covered_at_snapshot: usize,
    queued_at_snapshot: usize,
    pending: VecDeque<WebSessionLiveStreamEvent>,
    provider_fragment: Option<PendingProviderTextDelta>,
    shutdown: Option<watch::Receiver<bool>>,
    ended: bool,
}

struct PendingProviderTextDelta {
    turn_id: WebTurnId,
    model_call_id: WebLiveResourceId,
    part_index: u32,
    text: Arc<str>,
    offset: usize,
    emitted_empty: bool,
}

impl PendingProviderTextDelta {
    fn next_event(&mut self) -> Option<WebSessionLiveStreamEvent> {
        let content =
            next_web_text_fragment(&self.text, &mut self.offset, &mut self.emitted_empty)?;
        Some(WebSessionLiveStreamEvent::ProviderTextDelta {
            turn_id: self.turn_id.clone(),
            model_call_id: self.model_call_id.clone(),
            part_index: self.part_index,
            content,
        })
    }
}

async fn live_follow_next(
    mut state: LiveFollowState,
) -> Option<(WebSessionLiveStreamEvent, LiveFollowState)> {
    if state
        .shutdown
        .as_ref()
        .is_some_and(|shutdown| *shutdown.borrow() || shutdown.has_changed().is_err())
    {
        return None;
    }
    if let Some(event) = state.pending.pop_front() {
        return Some((event, state));
    }
    if let Some(mut fragment) = state.provider_fragment.take() {
        if state.subscription.is_saturated() {
            state.ended = true;
            return Some((
                WebSessionLiveStreamEvent::ResyncRequired {
                    cursor: WebPositiveU64::from_nonzero(state.observed_through),
                },
                state,
            ));
        }
        if let Some(event) = fragment.next_event() {
            state.provider_fragment = Some(fragment);
            return Some((event, state));
        }
    }
    if state.ended {
        return None;
    }
    loop {
        let update = match tokio::select! {
            () = live_follow_shutdown(&mut state.shutdown) => return None,
            update = state.subscription.recv() => update,
        } {
            Ok(update) => update,
            Err(ProcessMonitorReceiveError::Lagged(skipped))
                if skipped <= state.covered_at_snapshot =>
            {
                state.covered_at_snapshot -= skipped;
                state.queued_at_snapshot = state.queued_at_snapshot.saturating_sub(skipped);
                continue;
            }
            Err(ProcessMonitorReceiveError::Lagged(_)) => {
                state.ended = true;
                return Some((
                    WebSessionLiveStreamEvent::ResyncRequired {
                        cursor: WebPositiveU64::from_nonzero(state.observed_through),
                    },
                    state,
                ));
            }
            Err(ProcessMonitorReceiveError::Closed) => return None,
        };
        state.covered_at_snapshot = state.covered_at_snapshot.saturating_sub(1);
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
                let Some(sequence) = NonZeroU64::new(cursor)
                    .filter(|sequence| sequence.get() > state.observed_through.get())
                else {
                    continue;
                };
                state.observed_through = sequence;
                if session != state.session {
                    continue;
                }
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
                let mut fragment = PendingProviderTextDelta {
                    turn_id: WebTurnId::from_uuid_bytes(turn.into_uuid().into_bytes()),
                    model_call_id: WebLiveResourceId::from_uuid_bytes(
                        call.into_uuid().into_bytes(),
                    ),
                    part_index,
                    text,
                    offset: 0,
                    emitted_empty: false,
                };
                let event = fragment.next_event()?;
                state.provider_fragment = Some(fragment);
                return Some((event, state));
            }
        }
    }
}

async fn live_follow_shutdown(shutdown: &mut Option<watch::Receiver<bool>>) {
    let Some(shutdown) = shutdown else {
        std::future::pending::<()>().await;
        return;
    };
    if *shutdown.borrow() {
        return;
    }
    while shutdown.changed().await.is_ok() {
        if *shutdown.borrow() {
            return;
        }
    }
}

fn next_web_text_fragment(
    value: &str,
    offset: &mut usize,
    emitted_empty: &mut bool,
) -> Option<String> {
    if value.is_empty() {
        if *emitted_empty {
            return None;
        }
        *emitted_empty = true;
        return Some(String::new());
    }
    if *offset == value.len() {
        return None;
    }
    let remaining = &value[*offset..];
    let mut length = remaining.len().min(MAX_WEB_PROVIDER_TEXT_FRAGMENT_BYTES);
    while !remaining.is_char_boundary(length) {
        length -= 1;
    }
    let fragment = remaining[..length].to_owned();
    *offset += length;
    Some(fragment)
}

fn live_snapshot_dto(snapshot: SessionLiveSnapshot) -> Option<WebSessionLiveSnapshot> {
    let observed_through = NonZeroU64::new(snapshot.observed_through)?;
    let active = snapshot
        .active
        .map(|active| {
            let state = match active.state {
                SessionLiveActiveState::Running { model_call } => {
                    WebSessionLiveActiveState::Running {
                        model_call_id: model_call.map(|call| {
                            WebLiveResourceId::from_uuid_bytes(call.into_uuid().into_bytes())
                        }),
                    }
                }
                SessionLiveActiveState::AwaitingModelCallRecovery { call } => {
                    WebSessionLiveActiveState::AwaitingModelCallRecovery {
                        model_call_id: WebLiveResourceId::from_uuid_bytes(
                            call.into_uuid().into_bytes(),
                        ),
                    }
                }
                SessionLiveActiveState::AwaitingToolApproval { request } => {
                    WebSessionLiveActiveState::AwaitingToolApproval {
                        tool_request_id: WebLiveResourceId::from_uuid_bytes(
                            request.into_uuid().into_bytes(),
                        ),
                    }
                }
                SessionLiveActiveState::AwaitingChild { request, child } => {
                    WebSessionLiveActiveState::AwaitingChild {
                        tool_request_id: WebLiveResourceId::from_uuid_bytes(
                            request.into_uuid().into_bytes(),
                        ),
                        child_session_id: WebSessionId::from_uuid_bytes(
                            child.into_uuid().into_bytes(),
                        ),
                    }
                }
                SessionLiveActiveState::AwaitingToolRecovery { attempt } => {
                    WebSessionLiveActiveState::AwaitingToolRecovery {
                        tool_attempt_id: WebLiveResourceId::from_uuid_bytes(
                            attempt.into_uuid().into_bytes(),
                        ),
                    }
                }
                SessionLiveActiveState::AwaitingRunnerRecovery {
                    runner,
                    placement_revision,
                } => WebSessionLiveActiveState::AwaitingRunnerRecovery {
                    runner_id: WebLiveResourceId::from_uuid_bytes(runner.into_uuid().into_bytes()),
                    placement_revision: WebPositiveU64::from_nonzero(
                        NonZeroU64::new(placement_revision).ok_or(())?,
                    ),
                },
            };
            Ok::<WebSessionLiveActiveTurn, ()>(WebSessionLiveActiveTurn {
                turn_id: WebTurnId::from_uuid_bytes(active.turn.into_uuid().into_bytes()),
                state,
            })
        })
        .transpose()
        .ok()?;
    let runner = snapshot
        .runner
        .map(|runner| {
            let placement_revision =
                WebPositiveU64::from_nonzero(NonZeroU64::new(runner.placement_revision).ok_or(())?);
            Ok::<_, ()>((runner, placement_revision))
        })
        .transpose()
        .ok()?
        .and_then(|(runner, placement_revision)| {
            match (runner.state, runner.runner, runner.connection_health) {
                (SessionLiveRunnerState::Unpinned, None, None) => {
                    Some(WebSessionLiveRunner::Unpinned { placement_revision })
                }
                (SessionLiveRunnerState::Pinned, Some(runner), Some(connection_health)) => {
                    Some(WebSessionLiveRunner::Pinned {
                        runner_id: WebLiveResourceId::from_uuid_bytes(
                            runner.into_uuid().into_bytes(),
                        ),
                        placement_revision,
                        connection_health: match connection_health {
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
                        },
                    })
                }
                (SessionLiveRunnerState::RunnerLostBeforePin, Some(runner), None) => {
                    Some(WebSessionLiveRunner::RunnerLostBeforePin {
                        runner_id: WebLiveResourceId::from_uuid_bytes(
                            runner.into_uuid().into_bytes(),
                        ),
                        placement_revision,
                    })
                }
                (SessionLiveRunnerState::RunnerLost, Some(runner), None) => {
                    Some(WebSessionLiveRunner::RunnerLost {
                        runner_id: WebLiveResourceId::from_uuid_bytes(
                            runner.into_uuid().into_bytes(),
                        ),
                        placement_revision,
                    })
                }
                (SessionLiveRunnerState::RunnerAbandoned, Some(runner), None) => {
                    Some(WebSessionLiveRunner::RunnerAbandoned {
                        runner_id: WebLiveResourceId::from_uuid_bytes(
                            runner.into_uuid().into_bytes(),
                        ),
                        placement_revision,
                    })
                }
                _ => None,
            }
        });
    Some(WebSessionLiveSnapshot {
        session_id: WebSessionId::from_uuid_bytes(snapshot.session.into_uuid().into_bytes()),
        observed_through: WebPositiveU64::from_nonzero(observed_through),
        active,
        queued_turn_count: WebU64::from_u64(snapshot.queued_turn_count),
        queued_turn_ids: snapshot
            .queued_turns
            .into_iter()
            .map(|turn| WebTurnId::from_uuid_bytes(turn.into_uuid().into_bytes()))
            .collect(),
        reconciliation: snapshot
            .reconciliation
            .map(|reconciliation| match reconciliation {
                SessionLiveReconciliation::ModelCall { turn, call } => {
                    WebSessionLiveReconciliation::ModelCall {
                        turn_id: WebTurnId::from_uuid_bytes(turn.into_uuid().into_bytes()),
                        model_call_id: WebLiveResourceId::from_uuid_bytes(
                            call.into_uuid().into_bytes(),
                        ),
                    }
                }
                SessionLiveReconciliation::ToolAttempt { turn, attempt } => {
                    WebSessionLiveReconciliation::ToolAttempt {
                        turn_id: WebTurnId::from_uuid_bytes(turn.into_uuid().into_bytes()),
                        tool_attempt_id: WebLiveResourceId::from_uuid_bytes(
                            attempt.into_uuid().into_bytes(),
                        ),
                    }
                }
            }),
        runner,
    })
}

fn live_projection_corruption() -> Response {
    application_error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "session_live_projection_corrupt",
        "the durable live session projection contains invalid positive values",
    )
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
    let mut bootstrap = WebContractBootstrap::current();
    bootstrap.capabilities.bounded_session_timeline = false;
    bootstrap.capabilities.bounded_session_live = false;
    Json(bootstrap)
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
                Arc::clone(&state.blob_read_budget),
                digest,
                WebImageDerivativeKind::Thumbnail,
                WebBlobViewKind::Thumbnail,
                &mut available_views,
            )
            .await;
            append_image_derivative_view(
                &runtime,
                Arc::clone(&state.blob_read_budget),
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

async fn blob_descriptor_head() -> Response {
    let mut response = transport_error(
        StatusCode::METHOD_NOT_ALLOWED,
        "descriptor_method_not_allowed",
        "blob descriptors are available through GET",
    );
    insert_header(response.headers_mut(), ALLOW, String::from("GET"));
    response
}

async fn append_image_derivative_view(
    runtime: &WebBlobRuntime,
    read_budget: Arc<Semaphore>,
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
    let Some(_permit) = try_acquire_web_blob_read_permit(read_budget) else {
        return;
    };
    if open_recorded_blob_verified(runtime.registry(), &entry)
        .await
        .is_err()
    {
        return;
    }
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
    let requested_range = match applicable_range_header(request.headers(), &etag) {
        Ok(range) => range,
        Err(()) => return range_not_satisfiable(total, &etag),
    };
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
    // A head response owes the same status as the equivalent `GET`, so read
    // admission covers both methods. The head response then releases its permit
    // at once, because it never opens a replica or streams blob bytes.
    let Some(streamed_length) = NonZeroU64::new(length) else {
        return range_not_satisfiable(total, &etag);
    };
    let Some(permit) = try_acquire_web_blob_read_permit(Arc::clone(&state.blob_read_budget)) else {
        return application_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "blob_read_busy",
            "blob read capacity is busy",
        );
    };
    let body = if method == Method::HEAD {
        drop(permit);
        Body::empty()
    } else {
        let deadline = Instant::now() + Duration::from_secs(BLOB_RESPONSE_TIMEOUT_SECONDS);
        let opened = timeout_at(deadline, async {
            if streamed_length.get() <= MAX_BLOB_RANGE_BYTES {
                open_recorded_blob_range(runtime.registry(), &entry, offset, streamed_length).await
            } else {
                let mut reader = open_recorded_blob_verified(runtime.registry(), &entry).await?;
                let skipped =
                    tokio::io::copy(&mut (&mut reader).take(offset), &mut tokio::io::sink())
                        .await
                        .map_err(|_| crate::blob_read_runtime::BlobReadError::Unavailable)?;
                if skipped != offset {
                    return Err(crate::blob_read_runtime::BlobReadError::Integrity);
                }
                Ok(reader)
            }
        })
        .await;
        let reader = match opened {
            Ok(Ok(reader)) => reader,
            Ok(Err(error)) => return blob_read_error_response(error),
            Err(_) => {
                return blob_read_error_response(
                    crate::blob_read_runtime::BlobReadError::Unavailable,
                );
            }
        };
        reader_body_until(reader, streamed_length.get(), permit, deadline)
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

fn try_acquire_web_blob_read_permit(budget: Arc<Semaphore>) -> Option<OwnedSemaphorePermit> {
    budget.try_acquire_owned().ok()
}

fn reader_body_until(
    mut reader: signalbox_blob_store::BlobReader,
    length: u64,
    permit: OwnedSemaphorePermit,
    deadline: Instant,
) -> Body {
    let (sender, receiver) = mpsc::channel(1);
    tokio::spawn(async move {
        let _permit = permit;
        let produce = async move {
            let mut remaining = length;
            while remaining > 0 {
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
                remaining -=
                    u64::try_from(read).map_err(|_| io::Error::other("blob read is invalid"))?;
                sender
                    .send(Ok::<Bytes, io::Error>(Bytes::from(buffer)))
                    .await
                    .map_err(|_| {
                        io::Error::new(io::ErrorKind::BrokenPipe, "blob response closed")
                    })?;
            }
            Ok::<(), io::Error>(())
        };
        let _ = timeout_at(deadline, produce).await;
    });
    Body::from_stream(stream::unfold(receiver, |mut receiver| async move {
        receiver.recv().await.map(|item| (item, receiver))
    }))
}

fn parse_byte_range(value: &HeaderValue, total: u64) -> Result<(u64, u64, bool), ()> {
    if value.as_bytes().contains(&b',') {
        return Err(());
    }
    let mut headers = HeaderMap::new();
    headers.insert(RANGE, value.clone());
    let range = headers
        .typed_try_get::<TypedRange>()
        .map_err(|_| ())?
        .ok_or(())?;
    let mut ranges = range.satisfiable_ranges(total);
    let (start, end) = ranges.next().ok_or(())?;
    if ranges.next().is_some() {
        return Err(());
    }
    let std::ops::Bound::Included(start) = start else {
        return Err(());
    };
    let end = match end {
        std::ops::Bound::Included(end) => end.min(total.saturating_sub(1)),
        std::ops::Bound::Unbounded => total.checked_sub(1).ok_or(())?,
        std::ops::Bound::Excluded(_) => return Err(()),
    };
    if start > end || start >= total {
        return Err(());
    }
    Ok((start, end - start + 1, true))
}

/// Reports the `Range` field a blob response applies, once `If-Range` has decided.
///
/// A failed `If-Range` condition makes the whole `Range` field inapplicable, so
/// the condition is evaluated before the field is validated. A field this
/// endpoint would otherwise reject — repeated occurrences included — is then
/// ignored and the full representation is served, rather than answered with
/// `416`; `Err` is reserved for a rejectable field the condition admitted.
fn applicable_range_header<'headers>(
    headers: &'headers HeaderMap,
    etag: &str,
) -> Result<Option<&'headers HeaderValue>, ()> {
    if !if_range_matches(headers, etag) {
        return Ok(None);
    }
    single_range_header(headers)
}

fn single_range_header(headers: &HeaderMap) -> Result<Option<&HeaderValue>, ()> {
    let mut values = headers.get_all(RANGE).iter();
    let first = values.next();
    if values.next().is_some() {
        return Err(());
    }
    Ok(first)
}

fn if_none_match(headers: &HeaderMap, etag: &str) -> bool {
    let Ok(etag) = etag.parse::<TypedEtag>() else {
        return false;
    };
    headers
        .typed_try_get::<TypedIfNoneMatch>()
        .ok()
        .flatten()
        .is_some_and(|condition| !condition.precondition_passes(&etag))
}

fn if_range_matches(headers: &HeaderMap, etag: &str) -> bool {
    let mut values = headers.get_all(IF_RANGE).iter();
    if values.next().is_none() {
        return true;
    }
    if values.next().is_some() {
        return false;
    }
    let Ok(etag) = etag.parse::<TypedEtag>() else {
        return false;
    };
    headers
        .typed_try_get::<TypedIfRange>()
        .ok()
        .flatten()
        .is_some_and(|condition| !condition.is_modified(Some(&etag), None))
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

/// Decodes one UTF-8 request body after enforcing a caller-owned byte ceiling.
pub(crate) async fn decode_bounded_utf8(
    request: Request,
    maximum_bytes: usize,
) -> Result<String, Response> {
    let bytes = to_bytes(request.into_body(), maximum_bytes)
        .await
        .map_err(|error| {
            if error_chain_contains_length_limit(&error) {
                transport_error(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "text_body_too_large",
                    "text request body exceeds the configured import limit",
                )
            } else {
                transport_error(
                    StatusCode::BAD_REQUEST,
                    "text_body_read_failed",
                    "text request body could not be read",
                )
            }
        })?;
    String::from_utf8(bytes.to_vec()).map_err(|_| {
        transport_error(
            StatusCode::BAD_REQUEST,
            "invalid_utf8",
            "request body is not valid UTF-8",
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

pub(crate) async fn validate_json_mutation(request: Request, next: Next) -> Response {
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

pub(crate) async fn validate_text_mutation(request: Request, next: Next) -> Response {
    if request.method() != Method::POST {
        return transport_error(
            StatusCode::METHOD_NOT_ALLOWED,
            "mutation_method_not_allowed",
            "browser mutations use POST",
        );
    }
    if !has_content_type(request.headers(), TEXT_CONTENT_TYPE) {
        return transport_error(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "text_content_type_required",
            "exact import searches require text/plain",
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

async fn validate_loopback_host(request: Request, next: Next) -> Response {
    if !has_loopback_host(request.headers(), request.uri()) {
        return transport_error(
            StatusCode::FORBIDDEN,
            "non_loopback_host_rejected",
            "browser requests require a loopback request authority",
        );
    }
    next.run(request).await
}

fn has_loopback_host(headers: &HeaderMap, uri: &axum::http::Uri) -> bool {
    headers
        .get(HOST)
        .and_then(|host| host.to_str().ok())
        .and_then(|host| host.parse::<axum::http::uri::Authority>().ok())
        .or_else(|| uri.authority().cloned())
        .is_some_and(|authority| is_loopback_authority(&authority))
}

fn is_loopback_authority(authority: &axum::http::uri::Authority) -> bool {
    let host = normalized_authority_host(authority);
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<IpAddr>()
            .is_ok_and(|address| address.is_loopback())
}

fn normalized_authority_host(authority: &axum::http::uri::Authority) -> &str {
    normalized_host(authority.host())
}

fn normalized_host(host: &str) -> &str {
    host.trim_start_matches('[').trim_end_matches(']')
}

fn has_json_content_type(headers: &HeaderMap) -> bool {
    has_content_type(headers, JSON_CONTENT_TYPE)
}

fn has_content_type(headers: &HeaderMap, expected: &str) -> bool {
    headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<mime::Mime>().ok())
        .is_some_and(|value| value.essence_str() == expected)
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
        origin.host_str().is_some_and(|host| {
            normalized_host(host).eq_ignore_ascii_case(normalized_authority_host(&authority))
        }) && origin.port_or_known_default() == Some(authority_port)
    });
    if matching {
        Ok(())
    } else {
        Err(OriginValidationError::Mismatch)
    }
}

pub(crate) fn transport_error(
    status: StatusCode,
    code: &'static str,
    message: &'static str,
) -> Response {
    api_error(status, WebApiErrorKind::Transport, code, message)
}

pub(crate) fn application_error(
    status: StatusCode,
    code: &'static str,
    message: &'static str,
) -> Response {
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
        collections::BTreeSet,
        ffi::OsString,
        io::{self, Write as _},
        net::SocketAddr,
        path::PathBuf,
        sync::Arc,
        time::{Duration, UNIX_EPOCH},
    };

    use axum::{
        Router,
        body::{Body, Bytes},
        http::{Request, StatusCode, header},
    };
    use http_body_util::BodyExt as _;
    use signalbox_application::{
        AttentionAction, AttentionActivity, AttentionActivityKind, AttentionBlockedReason,
        AttentionContinuation, AttentionCursor, AttentionGoalBlock, AttentionJudgeFacts,
        AttentionLifecycleState, AttentionSort, AttentionState, AttentionSummary, TimelineAddress,
        TimelineBodyField, TimelineDetailCursor, UsageAggregateGroup, UsageAggregateKey,
        UsageAggregateTokenAxes, UsageCacheNormalization, UsageCallKind,
        UsageCredentialProfileLabel, UsageInputTokenSemantics, UsageProvenance, UsageTokenAxes,
        UsageTokenCoverage, UsageTokenPresence, max_attention_change_items,
        max_attention_goal_summary_characters, max_attention_snapshot_items,
        max_attention_title_characters,
    };
    use signalbox_domain::{ProviderModelIdentity, ResolvedProviderTarget, SessionId, TurnId};
    use signalbox_persistence::attention::AttentionPage;
    use signalbox_web_contract::{
        MAX_JSON_BODY_BYTES, MAX_NDJSON_ITEM_BYTES, WebAttentionStreamEvent, WebContractBootstrap,
        WebContractExample, WebUsageCost, WebUsageCostUnavailableReason,
    };
    use sqlx::{PgPool, types::Uuid};
    use tokio::sync::{Semaphore, mpsc, watch};
    use tower::ServiceExt as _;
    use url::Url;

    use super::{
        DEFAULT_WEB_BIND_ADDRESS, MAX_CONCURRENT_WEB_BLOB_READS, TimelineDetailQuery,
        WebHttpConfiguration, WebHttpConfigurationError, WebHttpRuntime, WebHttpRuntimeError,
        attention_snapshot_dto, blob_descriptor_head, content_disposition,
        deterministic_test_router, if_none_match, ndjson_response, parse_byte_range,
        parse_detail_query, production_router as production_router_with_shutdown,
        reader_body_until, single_range_header, try_acquire_web_blob_read_permit,
        usage_aggregate_cost_dto, usage_cost_dto,
    };
    use crate::{BlobStoreRegistry, HubModelConfiguration, ProcessMonitor, WebBlobRuntime};

    /// A descriptor method rejection must name the method clients can use.
    #[tokio::test]
    async fn descriptor_method_rejection_advertises_get() {
        let response = blob_descriptor_head().await;
        let status = response.status();
        let allow = response
            .headers()
            .get(header::ALLOW)
            .expect("the rejection advertises an allowed method")
            .to_str()
            .expect("the allowed method is ASCII")
            .to_owned();

        assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
        assert_eq!(allow, "GET");
    }

    fn loopback_ephemeral() -> SocketAddr {
        "127.0.0.1:0"
            .parse()
            .expect("the test listener address is valid")
    }

    fn production_router(
        asset_root: Option<PathBuf>,
        pool: Option<PgPool>,
        blobs: Option<WebBlobRuntime>,
        model_configuration: Option<HubModelConfiguration>,
        blob_store_registry: Option<Arc<BlobStoreRegistry>>,
    ) -> Router {
        production_router_with_shutdown(
            asset_root,
            pool,
            blobs,
            model_configuration,
            blob_store_registry,
            None,
        )
    }

    fn router_with_closed_snapshot_reader_budget(monitor: Option<ProcessMonitor>) -> Router {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://signalbox:signalbox@localhost/signalbox")
            .expect("the unused fixture pool URL is valid");
        let budget = Arc::new(Semaphore::new(1));
        budget.close();
        super::production_router_with_budget(
            None,
            Some(pool),
            None,
            None,
            None,
            super::ProductionReadRuntime {
                snapshot_reader_budget: Some(budget),
                shutdown: None,
                monitor,
            },
        )
    }

    fn router_with_snapshot_reader_shutdown(
        budget: Arc<Semaphore>,
        shutdown: watch::Receiver<bool>,
    ) -> Router {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://signalbox:signalbox@localhost/signalbox")
            .expect("the unused fixture pool URL is valid");
        super::production_router_with_budget(
            None,
            Some(pool),
            None,
            None,
            None,
            super::ProductionReadRuntime {
                snapshot_reader_budget: Some(budget),
                shutdown: Some(shutdown),
                monitor: None,
            },
        )
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
    fn http_byte_ranges_use_rfc_digit_grammar_and_reject_multiple_or_unsatisfied_forms() {
        let multiple = parse_byte_range(&header::HeaderValue::from_static("bytes=0-1,4-5"), 10);
        let partly_unsatisfied =
            parse_byte_range(&header::HeaderValue::from_static("bytes=0-1,20-21"), 10);
        let noncanonical = parse_byte_range(&header::HeaderValue::from_static("bytes=01-2"), 10);
        let unsatisfied = parse_byte_range(&header::HeaderValue::from_static("bytes=10-"), 10);

        assert_eq!(multiple, Err(()));
        assert_eq!(partly_unsatisfied, Err(()));
        assert_eq!(noncanonical, Ok((1, 2, true)));
        assert_eq!(unsatisfied, Err(()));
    }

    #[test]
    fn repeated_http_range_fields_are_rejected() {
        let mut headers = header::HeaderMap::new();
        headers.append(header::RANGE, header::HeaderValue::from_static("bytes=0-1"));
        headers.append(header::RANGE, header::HeaderValue::from_static("bytes=4-5"));

        assert_eq!(single_range_header(&headers), Err(()));
    }

    #[test]
    fn open_ended_ranges_can_exceed_one_storage_chunk() {
        let total = signalbox_blob_store::MAX_BLOB_RANGE_BYTES + 2;
        let range = parse_byte_range(&header::HeaderValue::from_static("bytes=1-"), total);

        assert_eq!(range, Ok((1, total - 1, true)));
    }

    #[test]
    fn repeated_if_none_match_fields_are_all_evaluated() {
        let mut headers = header::HeaderMap::new();
        headers.append(
            header::IF_NONE_MATCH,
            header::HeaderValue::from_static("\"other\""),
        );
        headers.append(
            header::IF_NONE_MATCH,
            header::HeaderValue::from_static("W/\"matching\""),
        );

        assert!(if_none_match(&headers, "\"matching\""));
    }

    #[test]
    fn typed_if_none_match_finds_a_matching_member_after_opaque_material() {
        let mut headers = header::HeaderMap::new();
        headers.insert(
            header::IF_NONE_MATCH,
            header::HeaderValue::from_static("garbage, \"matching\""),
        );

        assert!(if_none_match(&headers, "\"matching\""));
    }

    #[test]
    fn typed_if_none_match_finds_a_member_after_a_wildcard_token() {
        let mut headers = header::HeaderMap::new();
        headers.insert(
            header::IF_NONE_MATCH,
            header::HeaderValue::from_static("*, \"matching\""),
        );

        assert!(if_none_match(&headers, "\"matching\""));
    }

    #[test]
    fn malformed_if_range_is_not_treated_as_absent() {
        let mut headers = header::HeaderMap::new();
        headers.insert(
            header::IF_RANGE,
            header::HeaderValue::from_bytes(&[0xff])
                .expect("the fixture is an opaque HTTP field value"),
        );

        assert!(!super::if_range_matches(&headers, "\"matching\""));
    }

    #[test]
    fn repeated_if_range_fields_fail_the_condition() {
        let mut headers = header::HeaderMap::new();
        headers.append(
            header::IF_RANGE,
            header::HeaderValue::from_static("\"matching\""),
        );
        headers.append(
            header::IF_RANGE,
            header::HeaderValue::from_static("\"other\""),
        );

        assert!(!super::if_range_matches(&headers, "\"matching\""));
    }

    #[test]
    fn a_failed_if_range_condition_ignores_repeated_range_fields() {
        // Repeated `Range` fields are rejectable on their own, but a failed
        // `If-Range` makes the field inapplicable before that rejection can
        // apply, so the response owes the full representation rather than
        // `416`.
        let mut headers = header::HeaderMap::new();
        headers.insert(
            header::IF_RANGE,
            header::HeaderValue::from_static("\"other\""),
        );
        headers.append(header::RANGE, header::HeaderValue::from_static("bytes=0-1"));
        headers.append(header::RANGE, header::HeaderValue::from_static("bytes=2-3"));

        assert_eq!(
            super::applicable_range_header(&headers, "\"matching\""),
            Ok(None)
        );
    }

    #[test]
    fn a_matching_if_range_condition_still_rejects_repeated_range_fields() {
        let mut headers = header::HeaderMap::new();
        headers.insert(
            header::IF_RANGE,
            header::HeaderValue::from_static("\"matching\""),
        );
        headers.append(header::RANGE, header::HeaderValue::from_static("bytes=0-1"));
        headers.append(header::RANGE, header::HeaderValue::from_static("bytes=2-3"));

        assert_eq!(
            super::applicable_range_header(&headers, "\"matching\""),
            Err(())
        );
    }

    #[test]
    fn an_absent_if_range_condition_still_rejects_repeated_range_fields() {
        let mut headers = header::HeaderMap::new();
        headers.append(header::RANGE, header::HeaderValue::from_static("bytes=0-1"));
        headers.append(header::RANGE, header::HeaderValue::from_static("bytes=2-3"));

        assert_eq!(
            super::applicable_range_header(&headers, "\"matching\""),
            Err(())
        );
    }

    #[test]
    fn a_matching_if_range_condition_applies_its_single_range_field() {
        let mut headers = header::HeaderMap::new();
        headers.insert(
            header::IF_RANGE,
            header::HeaderValue::from_static("\"matching\""),
        );
        headers.insert(header::RANGE, header::HeaderValue::from_static("bytes=0-1"));

        assert_eq!(
            super::applicable_range_header(&headers, "\"matching\""),
            Ok(Some(&header::HeaderValue::from_static("bytes=0-1")))
        );
    }

    #[test]
    fn web_blob_read_budget_rejects_without_waiting_and_recovers_on_drop() {
        let budget = Arc::new(Semaphore::new(MAX_CONCURRENT_WEB_BLOB_READS));
        let held = Arc::clone(&budget)
            .try_acquire_many_owned(
                u32::try_from(MAX_CONCURRENT_WEB_BLOB_READS)
                    .expect("the fixed web blob read capacity fits u32"),
            )
            .expect("the fixture acquires the complete read budget");

        assert!(try_acquire_web_blob_read_permit(Arc::clone(&budget)).is_none());
        drop(held);
        assert!(try_acquire_web_blob_read_permit(budget).is_some());
    }

    #[tokio::test]
    async fn stalled_blob_response_releases_its_read_permit_at_the_deadline() {
        let budget = Arc::new(Semaphore::new(1));
        let permit = Arc::clone(&budget)
            .try_acquire_owned()
            .expect("the fixture acquires the read permit");
        let reader: signalbox_blob_store::BlobReader = Box::new(tokio::io::repeat(1));
        let _body = reader_body_until(
            reader,
            u64::try_from(super::BLOB_STREAM_CHUNK_BYTES * 3).expect("the fixture length fits u64"),
            permit,
            tokio::time::Instant::now() + Duration::from_millis(10),
        );

        tokio::time::timeout(Duration::from_secs(1), async {
            while budget.available_permits() == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the stalled response releases its permit within the test bound");

        assert_eq!(budget.available_permits(), 1);
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
    fn non_loopback_bind_fails_closed() {
        let error = WebHttpConfiguration::from_values(Some(OsString::from("0.0.0.0:8080")), None)
            .expect_err("the unauthenticated browser surface remains loopback-only");

        assert_eq!(error, WebHttpConfigurationError::NonLoopbackBindAddress);
        assert_eq!(
            error.to_string(),
            "setting SIGNALBOX_WEB_BIND must use a loopback address"
        );
    }

    #[test]
    fn explicit_constructor_rejects_non_loopback_bind() {
        let bind_address: SocketAddr = "0.0.0.0:8080"
            .parse()
            .expect("the fixture address is valid");
        let error = WebHttpConfiguration::new(bind_address, None)
            .expect_err("every production configuration path remains loopback-only");

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
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://signalbox:signalbox@localhost/signalbox")
            .expect("the unused fixture pool URL is valid");
        let models = crate::configuration::checked_in_example_configuration()
            .expect("the checked-in example model configuration parses");
        let runtime = WebHttpRuntime::bind(
            WebHttpConfiguration::new(loopback_ephemeral(), Some(assets.path().to_path_buf()))
                .expect("the loopback fixture configuration is valid"),
            pool,
            None,
            models,
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
    async fn bind_rejects_a_pool_too_small_to_fund_the_reader_budget() {
        // Two connections is exactly `RESERVED_POOL_CONNECTIONS_OUTSIDE_SNAPSHOTS`
        // (`process_runtime::snapshot_reader_capacity`), leaving zero for the
        // shared snapshot reader budget. The daemon entry point in `main.rs`
        // refuses to start in this configuration; the standalone production
        // binder must refuse construction the same way instead of returning a
        // runtime whose session-read routes can never obtain a reader permit.
        let assets = tempfile::tempdir().expect("the static asset directory exists");
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect_lazy("postgres://signalbox:signalbox@localhost/signalbox")
            .expect("the unused fixture pool URL is valid");

        let models = crate::configuration::checked_in_example_configuration()
            .expect("the checked-in example model configuration parses");

        let outcome = WebHttpRuntime::bind(
            WebHttpConfiguration::new(loopback_ephemeral(), Some(assets.path().to_path_buf()))
                .expect("the loopback fixture configuration is valid"),
            pool,
            None,
            models,
            None,
        )
        .await;
        let error = outcome
            .err()
            .expect("a pool that cannot fund any reader permit must fail construction");

        assert_eq!(error, WebHttpRuntimeError::Bind);
    }

    #[tokio::test]
    async fn malformed_blob_query_is_a_structured_transport_error() {
        let request = Request::get("/api/blobs/not-a-digest/descriptor")
            .header(header::HOST, "localhost")
            .body(Body::empty())
            .expect("the request is valid");
        let response = production_router(None, None, None, None, None)
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
    async fn descriptor_head_is_rejected_without_blob_runtime_work() {
        let request = Request::head(
            "/api/blobs/sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/descriptor",
        )
        .header(header::HOST, "localhost")
        .body(Body::empty())
        .expect("the request is valid");
        let response = production_router(None, None, None, None, None)
            .oneshot(request)
            .await
            .expect("the production router responds");

        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
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
    async fn mutation_with_matching_ipv6_origin_round_trips_bounded_json() {
        let request = Request::post("/api/test/mutate")
            .header(header::HOST, "[::1]:37231")
            .header(header::ORIGIN, "http://[::1]:37231")
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
            .header(header::HOST, "127.0.0.1")
            .body(Body::empty())
            .expect("the request is valid");
        let response = production_router(Some(assets.path().to_path_buf()), None, None, None, None)
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
    async fn production_router_rejects_non_loopback_hostnames() {
        let request = Request::get("/api/bootstrap")
            .header(header::HOST, "attacker.example")
            .body(Body::empty())
            .expect("the request is valid");
        let response = production_router(None, None, None, None, None)
            .oneshot(request)
            .await
            .expect("the production router responds");
        let status = response.status();
        let body: serde_json::Value =
            serde_json::from_slice(&response_body(response).await).expect("the rejection is JSON");

        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body["error"]["code"], "non_loopback_host_rejected");
    }

    #[test]
    fn loopback_host_accepts_localhost_and_uri_authority() {
        let localhost = Request::get("/api/bootstrap")
            .header(header::HOST, "localhost:37231")
            .body(Body::empty())
            .expect("the localhost request is valid");
        let authority = Request::get("http://127.0.0.1:37231/api/bootstrap")
            .body(Body::empty())
            .expect("the authority request is valid");

        assert!(super::has_loopback_host(
            localhost.headers(),
            localhost.uri()
        ));
        assert!(super::has_loopback_host(
            authority.headers(),
            authority.uri()
        ));
    }

    #[tokio::test]
    async fn attention_snapshot_requires_projection_configuration() {
        let request = Request::get("/api/attention")
            .header(header::HOST, "localhost")
            .body(Body::empty())
            .expect("the request is valid");
        let response = production_router(None, None, None, None, None)
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
            .header(header::HOST, "localhost")
            .body(Body::empty())
            .expect("the request is valid");
        let response = production_router(None, None, None, None, None)
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
    async fn session_catalog_query_rejection_uses_typed_application_error() {
        let request = Request::get("/api/sessions?unexpected=true")
            .header(header::HOST, "localhost")
            .body(Body::empty())
            .expect("the request is valid");
        let response = production_router(None, None, None, None, None)
            .oneshot(request)
            .await
            .expect("the production router responds");
        let status = response.status();
        let body: serde_json::Value = serde_json::from_slice(&response_body(response).await)
            .expect("the typed application failure is JSON");

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"]["kind"], "application");
        assert_eq!(body["error"]["code"], "invalid_session_catalog_query");
    }

    #[tokio::test]
    async fn session_catalog_semantic_rejection_precedes_projection_availability() {
        let request = Request::get("/api/sessions?sort=unknown")
            .header(header::HOST, "localhost")
            .body(Body::empty())
            .expect("the request is valid");
        let response = production_router(None, None, None, None, None)
            .oneshot(request)
            .await
            .expect("the production router responds");
        let status = response.status();
        let body: serde_json::Value = serde_json::from_slice(&response_body(response).await)
            .expect("the typed application failure is JSON");

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"]["kind"], "application");
        assert_eq!(body["error"]["code"], "invalid_session_catalog_query");
    }

    #[test]
    fn session_catalog_query_decodes_bounded_filters_and_activity_keyset() {
        let raw = concat!(
            "search=needle+title&required_tag=focus&required_tag=urgent",
            "&include_archived=true&sort=last_activity_descending",
            "&after_session_id=00000000-0000-0000-0000-000000000991",
            "&after_activity_unix_microseconds=1724200000000000"
        );
        let parsed = super::parse_session_catalog_query(Some(raw))
            .and_then(super::parse_attention_query)
            .expect("the bounded catalog query is valid");
        let tags = parsed.required_tags().collect::<Vec<_>>();
        let Some(AttentionContinuation::LastActivity { session, .. }) = parsed.continuation()
        else {
            panic!("the activity query carries its typed continuation");
        };

        assert_eq!(parsed.search(), Some("needle title"));
        assert_eq!(tags, vec!["focus", "urgent"]);
        assert!(parsed.include_archived());
        assert_eq!(parsed.sort(), AttentionSort::LastActivityDescending);
        assert_eq!(*session, SessionId::from_uuid(Uuid::from_u128(0x991)));
    }

    #[test]
    fn session_catalog_query_rejects_sort_cursor_and_filter_bound_violations() {
        let mismatched = super::parse_session_catalog_query(Some(
            "sort=last_activity_descending&after_session_id=00000000-0000-0000-0000-000000000991",
        ))
        .and_then(super::parse_attention_query);
        let duplicate = super::parse_session_catalog_query(Some("search=one&search=two"));
        let too_many_tags = super::parse_session_catalog_query(Some(
            "required_tag=1&required_tag=2&required_tag=3&required_tag=4&required_tag=5&required_tag=6&required_tag=7&required_tag=8&required_tag=9",
        ));

        assert!(mismatched.is_err());
        assert!(duplicate.is_err());
        assert!(too_many_tags.is_err());
    }

    #[tokio::test]
    async fn attention_follow_requires_projection_configuration() {
        let request = Request::get("/api/attention/follow")
            .header(header::HOST, "localhost")
            .body(Body::empty())
            .expect("the request is valid");
        let response = production_router(None, None, None, None, None)
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
    async fn live_snapshot_requires_an_available_snapshot_reader_permit() {
        let request = Request::get("/api/sessions/00000000-0000-0000-0000-000000000991/live")
            .header(header::HOST, "localhost")
            .body(Body::empty())
            .expect("the request is valid");
        let response = router_with_closed_snapshot_reader_budget(None)
            .oneshot(request)
            .await
            .expect("the production router responds");
        let status = response.status();
        let body: serde_json::Value = serde_json::from_slice(&response_body(response).await)
            .expect("the typed application failure is JSON");

        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["error"]["code"], "session_live_projection_unavailable");
    }

    #[tokio::test]
    async fn live_snapshot_reader_wait_stops_when_web_shutdown_begins() {
        let budget = Arc::new(Semaphore::new(1));
        let _held = Arc::clone(&budget)
            .acquire_owned()
            .await
            .expect("the fixture holds the snapshot reader permit");
        let (shutdown, shutdown_receiver) = watch::channel(false);
        let request = Request::get("/api/sessions/00000000-0000-0000-0000-000000000991/live")
            .header(header::HOST, "localhost")
            .body(Body::empty())
            .expect("the request is valid");
        let mut waiting = tokio::spawn(
            router_with_snapshot_reader_shutdown(budget, shutdown_receiver).oneshot(request),
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(50), &mut waiting)
                .await
                .is_err(),
            "the held reader permit keeps the snapshot request pending before shutdown"
        );

        shutdown
            .send(true)
            .expect("the snapshot request still observes web shutdown");
        let response = tokio::time::timeout(Duration::from_secs(1), waiting)
            .await
            .expect("the snapshot request exits promptly on shutdown")
            .expect("the snapshot request task completes cleanly")
            .expect("the production router responds");
        let status = response.status();
        let body: serde_json::Value = serde_json::from_slice(&response_body(response).await)
            .expect("the typed application failure is JSON");

        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["error"]["code"], "session_live_projection_unavailable");
    }

    #[tokio::test]
    async fn live_follow_requires_an_available_snapshot_reader_permit() {
        let request = Request::get("/api/sessions/00000000-0000-0000-0000-000000000991/follow")
            .header(header::HOST, "localhost")
            .body(Body::empty())
            .expect("the request is valid");
        let response =
            router_with_closed_snapshot_reader_budget(Some(ProcessMonitor::test_channel()))
                .oneshot(request)
                .await
                .expect("the production router responds");
        let status = response.status();
        let body: serde_json::Value = serde_json::from_slice(&response_body(response).await)
            .expect("the typed application failure is JSON");

        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["error"]["code"], "session_live_projection_unavailable");
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
            title_summary: None,
            title_truncated: false,
            archived: false,
            current_turn: None,
            active_turn_count: 0,
            queued_turn_count: 0,
            state: AttentionState::Idle,
            lifecycle_state: AttentionLifecycleState::Created,
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
            title_summary: None,
            title_truncated: false,
            archived: false,
            current_turn: None,
            active_turn_count: 0,
            queued_turn_count: 0,
            state: AttentionState::Idle,
            lifecycle_state: AttentionLifecycleState::Created,
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
    fn attention_follow_resyncs_when_a_new_identity_enters_a_full_live_page() {
        let first = SessionId::from_uuid(Uuid::from_u128(2));
        let boundary = SessionId::from_uuid(Uuid::from_u128(3));
        let entering = SessionId::from_uuid(Uuid::from_u128(1));
        let off_page = SessionId::from_uuid(Uuid::from_u128(4));
        let summary = |session| AttentionSummary {
            session,
            title_summary: None,
            title_truncated: false,
            archived: false,
            current_turn: None,
            active_turn_count: 0,
            queued_turn_count: 0,
            state: AttentionState::Idle,
            lifecycle_state: AttentionLifecycleState::Created,
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
        let visible_sessions = BTreeSet::from([first, boundary]);

        assert!(super::attention_changes_require_resync(
            &[summary(entering)],
            &visible_sessions,
            false,
        ));
        assert!(!super::attention_changes_require_resync(
            &[summary(off_page)],
            &visible_sessions,
            false,
        ));
    }

    /// The projection reports a blocked goal as still owed automatic
    /// resumption until one of the deployment's two attempt limits ends its
    /// run, so it must read both numbers the daemon's resume planner applies
    /// (`goal_mode::GoalModeNumericBounds`) rather than compiled-in ones.
    #[test]
    fn the_attention_projection_reads_both_configured_automatic_resume_limits() {
        let configuration = crate::configuration::checked_in_example_configuration()
            .expect("checked-in example parses");
        let configured_budget = configuration
            .numeric_bounds()
            .integer("automatic_resume_attempt_budget")
            .flatten()
            .and_then(|budget| u32::try_from(budget).ok())
            .expect("the example configures an automatic-resume attempt budget");
        let configured_ceiling = configuration
            .numeric_bounds()
            .integer("automatic_resume_attempt_ceiling")
            .flatten()
            .and_then(|ceiling| u32::try_from(ceiling).ok())
            .expect("the example configures an automatic-resume attempt ceiling");

        assert_eq!(
            super::configured_automatic_resume_attempts(Some(&configuration)),
            super::AutomaticResumeAttemptBounds::new(
                Some(configured_budget),
                Some(configured_ceiling),
            )
        );
        assert_eq!(
            super::configured_automatic_resume_attempts(None),
            super::AutomaticResumeAttemptBounds::unbounded()
        );
    }

    /// The largest summary the projection can carry: every scalar at its
    /// maximum, a blocked goal whose need summary sits exactly on the
    /// character ceiling, and activity at a representative far-future instant.
    fn maximum_attention_summary() -> AttentionSummary {
        AttentionSummary {
            session: SessionId::from_uuid(Uuid::from_u128(u128::MAX)),
            title_summary: Some(
                String::from('\u{1}').repeat(usize::from(max_attention_title_characters())),
            ),
            title_truncated: true,
            archived: true,
            current_turn: Some(TurnId::from_uuid(Uuid::from_u128(u128::MAX))),
            active_turn_count: u64::MAX,
            queued_turn_count: u64::MAX,
            state: AttentionState::Blocked,
            lifecycle_state: AttentionLifecycleState::Blocked,
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
        }
    }

    #[test]
    fn a_goal_summary_one_character_past_the_ceiling_is_rejected() {
        let mut oversized_summary = maximum_attention_summary();
        oversized_summary
            .goal_block
            .as_mut()
            .expect("the maximum summary carries a goal block")
            .need_summary
            .push('x');

        assert!(super::attention_summary_dto(maximum_attention_summary()).is_ok());
        assert!(super::attention_summary_dto(oversized_summary).is_err());
    }

    #[test]
    fn maximum_attention_snapshot_fits_one_ndjson_item() {
        let summary = maximum_attention_summary();
        let continuation = AttentionContinuation::SessionIdentity(summary.session);
        let snapshot = attention_snapshot_dto(AttentionPage {
            cursor: AttentionCursor::new(u64::MAX),
            sort: AttentionSort::SessionIdentityAscending,
            summaries: vec![summary; usize::from(max_attention_snapshot_items())],
            continuation: Some(continuation),
        })
        .expect("the maximum snapshot timestamp is representable");
        let mut writer = super::NdjsonItemWriter::new();

        serde_json::to_writer(&mut writer, &WebAttentionStreamEvent::Snapshot { snapshot })
            .expect("the maximum snapshot serializes within one item");
        writer
            .write_all(b"\n")
            .expect("the NDJSON terminator fits the item");

        assert!(writer.encoded.len() <= MAX_NDJSON_ITEM_BYTES);
    }

    #[test]
    fn maximum_attention_update_fits_one_ndjson_item() {
        let summaries =
            vec![maximum_attention_summary(); usize::from(max_attention_change_items())]
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
    async fn malformed_timeline_query_uses_the_structured_error_envelope() {
        let request = Request::get(
            "/api/sessions/00000000-0000-0000-0000-000000000991/timeline?max_items=nope",
        )
        .header(header::HOST, "localhost")
        .body(Body::empty())
        .expect("the request is valid");
        let response = production_router(None, None, None, None, None)
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
        let response = production_router(None, None, None, None, None)
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
        let response = production_router(None, None, None, None, None)
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
    async fn search_rejects_a_non_product_strategy() {
        let unsupported = Request::get("/api/search?strategy=postgres&q=term&max_items=10")
            .header(header::HOST, "localhost")
            .body(Body::empty())
            .expect("the request is valid");
        let unsupported = production_router(None, None, None, None, None)
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
        let partial = production_router(None, None, None, None, None)
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
        let oversized = production_router(None, None, None, None, None)
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
        let response = production_router(None, None, None, None, None)
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
            "/api/usage/calls?from_micros=0&to_micros=1777777777123456&provenance=estimated&call_kind=context_compaction&order=newest&max_items=100",
        )
        .header(header::HOST, "localhost")
        .body(Body::empty())
        .expect("the request is valid");
        let response = production_router(None, None, None, None, None)
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
            "/api/usage/calls?to_micros=9223372036854775807&order=newest&max_items=100",
        )
        .header(header::HOST, "localhost")
        .body(Body::empty())
        .expect("the request is valid");
        let response = production_router(None, None, None, None, None)
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
        let response = production_router(None, None, None, None, None)
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
            Some("anthropic-primary"),
            UsageInputTokenSemantics::CacheExclusive,
            tokens,
            true,
        );
        let metered_equivalent = usage_cost_dto(
            &configuration,
            rated_example_target(),
            Some("codex-subscription-primary"),
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
            Some("anthropic-primary"),
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
            Some("anthropic-primary"),
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

    fn cache_inclusive_aggregate_group(
        tokens: UsageAggregateTokenAxes,
        coverage: UsageTokenCoverage,
    ) -> UsageAggregateGroup {
        UsageAggregateGroup::new(
            UsageAggregateKey {
                call_kind: UsageCallKind::ModelCall,
                model: rated_example_target(),
                credential_profile: UsageCredentialProfileLabel::new(String::from(
                    "exact:anthropic-primary",
                ))
                .expect("the label is discriminated and bounded"),
                credential_reference: Some(String::from("anthropic-primary")),
                provenance: UsageProvenance::Reported,
                input_semantics: UsageInputTokenSemantics::CacheInclusive,
                coverage,
            },
            2,
            tokens,
            UsageCacheNormalization::Unsafe,
        )
        .expect("the group agrees with its declared coverage and normalization")
    }

    #[test]
    fn aggregate_usage_cost_prices_independent_axes_when_cache_axes_are_absent() {
        let configuration = example_model_configuration();
        let group = cache_inclusive_aggregate_group(
            UsageAggregateTokenAxes {
                input: Some(10),
                output: Some(2),
                cache_creation_input: None,
                cache_read_input: None,
            },
            UsageTokenCoverage {
                input: UsageTokenPresence::Present,
                output: UsageTokenPresence::Present,
                cache_creation_input: UsageTokenPresence::Absent,
                cache_read_input: UsageTokenPresence::Absent,
            },
        );

        let cost = usage_aggregate_cost_dto(&configuration, &group);

        assert!(
            matches!(cost, WebUsageCost::Derived { .. }),
            "incomplete normalization must keep the independently reported \
             output axis priceable, as the individual-call path does: {cost:?}"
        );
    }

    #[test]
    fn aggregate_usage_cost_rejects_a_constituent_cache_breakdown_contradiction() {
        let configuration = example_model_configuration();
        let group = cache_inclusive_aggregate_group(
            UsageAggregateTokenAxes {
                input: Some(10),
                output: Some(2),
                cache_creation_input: Some(3),
                cache_read_input: Some(1),
            },
            UsageTokenCoverage {
                input: UsageTokenPresence::Present,
                output: UsageTokenPresence::Present,
                cache_creation_input: UsageTokenPresence::Present,
                cache_read_input: UsageTokenPresence::Present,
            },
        );

        let cost = usage_aggregate_cost_dto(&configuration, &group);

        assert_eq!(
            cost,
            WebUsageCost::Unavailable {
                reason: WebUsageCostUnavailableReason::InvalidCacheBreakdown,
            }
        );
    }

    #[test]
    fn configured_usage_cost_prices_overflowing_cache_axes_without_total_input() {
        let configuration = example_model_configuration();
        let cost = usage_cost_dto(
            &configuration,
            rated_example_target(),
            Some("anthropic-primary"),
            UsageInputTokenSemantics::CacheInclusive,
            UsageTokenAxes {
                input: None,
                output: None,
                cache_creation_input: Some(u64::MAX),
                cache_read_input: Some(1),
            },
            true,
        );

        assert!(matches!(cost, WebUsageCost::Derived { .. }));
    }

    #[test]
    fn configured_usage_cost_reports_unpriceable_incomplete_cache_evidence() {
        let configuration = example_model_configuration();
        let cost = usage_cost_dto(
            &configuration,
            rated_example_target(),
            Some("anthropic-primary"),
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
        let response = production_router(None, None, None, None, None)
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

    #[tokio::test]
    async fn attention_snapshot_reads_reject_non_loopback_host_authorities() {
        // The attention projection returns session identities, goal-need text,
        // and operator state across the whole fleet, so a rebound origin must
        // not reach it any more than it may reach the per-session reads beside
        // it. This route is asserted because it was registered outside the
        // guarded router and had to be moved into it.
        let request = Request::get("/api/attention")
            .header(header::HOST, "attacker.example")
            .body(Body::empty())
            .expect("the request is valid");
        let response = production_router(None, None, None, None, None)
            .oneshot(request)
            .await
            .expect("the production router responds");
        let status = response.status();
        let body: serde_json::Value = serde_json::from_slice(&response_body(response).await)
            .expect("the rejection is structured JSON");

        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "`/api/attention` must reject a non-loopback authority",
        );
        assert_eq!(body["error"]["kind"], "transport");
        assert_eq!(body["error"]["code"], "non_loopback_host_rejected");
    }

    #[tokio::test]
    async fn attention_follow_reads_reject_non_loopback_host_authorities() {
        // Mirrors `attention_snapshot_reads_reject_non_loopback_host_authorities`
        // for the follow route: it was registered outside the guarded router
        // beside the snapshot route and had to be moved into it too.
        let request = Request::get("/api/attention/follow")
            .header(header::HOST, "attacker.example")
            .body(Body::empty())
            .expect("the request is valid");
        let response = production_router(None, None, None, None, None)
            .oneshot(request)
            .await
            .expect("the production router responds");
        let status = response.status();
        let body: serde_json::Value = serde_json::from_slice(&response_body(response).await)
            .expect("the rejection is structured JSON");

        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "`/api/attention/follow` must reject a non-loopback authority",
        );
        assert_eq!(body["error"]["kind"], "transport");
        assert_eq!(body["error"]["code"], "non_loopback_host_rejected");
    }

    /// Drives one route with a rebound origin and returns what it answered.
    ///
    /// Request plumbing only: the status and body it hands back are what the
    /// calling test asserts on.
    async fn rebound_origin_response(path: &str) -> (StatusCode, serde_json::Value) {
        let request = Request::get(path)
            .header(header::HOST, "attacker.example")
            .body(Body::empty())
            .expect("the request is valid");
        let response = production_router(None, None, None, None, None)
            .oneshot(request)
            .await
            .expect("the production router responds");
        let status = response.status();
        let body: serde_json::Value = serde_json::from_slice(&response_body(response).await)
            .expect("the rejection is structured JSON");
        (status, body)
    }

    /// Asserts one unauthenticated read turned a rebound origin away at the
    /// loopback gate, before any session-attached content was read.
    ///
    /// Two layers enforce this, and the assertion is deliberately about the
    /// guarantee rather than either one: `same_origin_router` gates the whole
    /// listener, and `session_reads` gates these routes again. Removing the
    /// inner `route_layer` alone therefore does not make a caller fail — the
    /// same is true of every sibling assertion here — so what this holds is the
    /// promise a reader depends on, not a particular layer's presence.
    ///
    /// `#[track_caller]` puts a failure at the calling test, so each route
    /// names itself.
    #[track_caller]
    fn assert_rebound_origin_rejected(status: StatusCode, body: &serde_json::Value) {
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body["error"]["kind"], "transport");
        assert_eq!(body["error"]["code"], "non_loopback_host_rejected");
    }

    /// The bounded usage summary carries per-session spend and resolved model
    /// identity across the whole installation, so a rebound origin must not
    /// reach it any more than it may reach the session reads beside it.
    #[tokio::test]
    async fn usage_summary_reads_reject_non_loopback_host_authorities() {
        let (status, body) = rebound_origin_response("/api/usage/summary").await;

        assert_rebound_origin_rejected(status, &body);
    }

    /// Usage-call detail carries per-call spend, resolved model identity, and
    /// call provenance, so it is protected exactly as the summary above it is.
    #[tokio::test]
    async fn usage_call_reads_reject_non_loopback_host_authorities() {
        let (status, body) = rebound_origin_response("/api/usage/calls").await;

        assert_rebound_origin_rejected(status, &body);
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
        production_router(None, None, None, None, None)
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

    const BLOB_READ_PATHS: [&str; 3] = [
        "/api/blobs/sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/descriptor?media_type=image/png",
        "/api/blobs/sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/content/image-png",
        "/api/blobs/sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/download?media_type=image/png",
    ];

    /// Drives a blob read at the loopback gate and reports only the status.
    ///
    /// Each path is otherwise valid — a well-formed digest, and a
    /// `media_type` query where the route requires one — so a `FORBIDDEN`
    /// can only come from the gate, never from the handler behind it.
    async fn blob_read_status_for_host(path: &str, host: &str) -> StatusCode {
        let request = Request::get(path)
            .header(header::HOST, host)
            .body(Body::empty())
            .expect("the request is valid");
        production_router(None, None, None, None, None)
            .oneshot(request)
            .await
            .expect("the production router responds")
            .status()
    }

    #[tokio::test]
    async fn blob_reads_reject_non_loopback_host_authorities() {
        // Mirrors `session_reads_reject_non_loopback_host_authorities`: the
        // descriptor, content, and download routes were registered outside
        // the guarded router and had to be moved into it too, since a
        // rebound origin that knows a digest could otherwise read blob
        // bytes, or start image derivation work, with an attacker's
        // authority.
        for path in BLOB_READ_PATHS {
            assert_eq!(
                blob_read_status_for_host(path, "attacker.example").await,
                StatusCode::FORBIDDEN,
                "`{path}` must reject a non-loopback authority",
            );
        }
    }

    #[tokio::test]
    async fn blob_reads_admit_loopback_host_authorities() {
        // A regression that moved the guard without preserving admission
        // would 403 legitimate same-origin blob reads; each path here must
        // reach its handler and fail only because no blob runtime is
        // configured in this fixture.
        for path in BLOB_READ_PATHS {
            assert_eq!(
                blob_read_status_for_host(path, "localhost").await,
                StatusCode::SERVICE_UNAVAILABLE,
                "`{path}` is a loopback authority and must reach the handler",
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
