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
    UsageAggregateTokenAxes, UsageCacheNormalization, UsageCallKind, UsageCredentialProfileLabel,
    UsageInputTokenSemantics, UsageProvenance, UsageTokenAxes, UsageTokenCoverage,
    UsageTokenPresence, max_attention_change_items, max_attention_goal_summary_characters,
    max_attention_snapshot_items, max_attention_title_characters,
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
    attention_snapshot_dto, blob_descriptor_head, content_disposition, deterministic_test_router,
    if_none_match, ndjson_response, parse_byte_range, parse_detail_query,
    production_router as production_router_with_shutdown, reader_body_until, single_range_header,
    try_acquire_web_blob_read_permit, usage_aggregate_cost_dto, usage_cost_dto,
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
        None,
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
        None,
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
async fn rates_reject_duplicate_session_ids_before_reading_storage() {
    let request = Request::get("/api/sessions/rates?session_id=00000000-0000-0000-0000-000000000027&session_id=00000000-0000-0000-0000-000000000027")
        .header(header::HOST, "localhost")
        .body(Body::empty())
        .expect("the request is valid");
    let response = production_router(None, None, None, None, None)
        .oneshot(request)
        .await
        .expect("the router responds");
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
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
    let Some(AttentionContinuation::LastActivity { session, .. }) = parsed.continuation() else {
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
    let response = router_with_closed_snapshot_reader_budget(Some(ProcessMonitor::test_channel()))
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
        super::AutomaticResumeAttemptBounds::new(Some(configured_budget), Some(configured_ceiling),)
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
            recorded_at: UNIX_EPOCH + Duration::from_millis(LARGE_REPRESENTATIVE_UNIX_MILLISECONDS),
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
    let summaries = vec![maximum_attention_summary(); usize::from(max_attention_change_items())]
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
    let request =
        Request::get("/api/sessions/00000000-0000-0000-0000-000000000991/timeline?max_items=nope")
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
    let partial = Request::get("/api/search?strategy=lexical&q=term&max_items=10&after_address=5")
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
    let request =
        Request::get("/api/usage/calls?to_micros=9223372036854775807&order=newest&max_items=100")
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
    let request =
        Request::get("/api/sessions/00000000-0000-0000-0000-000000000991/timeline?max_items=nope")
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
fn detail_cursors_select_a_repeated_tool_member() {
    let query = TimelineDetailQuery {
        max_items: Some(String::from("1")),
        max_bytes: Some(String::from("256")),
        cursor_address: Some(String::from("7")),
        cursor_field: Some(String::from("tool_result")),
        cursor_member: Some(String::from("3")),
        cursor_offset: Some(String::from("31")),
    };
    let (_, cursor) = parse_detail_query(&query).expect("a repeated member cursor is valid");
    let cursor = cursor.expect("a body continuation is retained");
    assert_eq!(cursor.field, Some(TimelineBodyField::ToolResult));
    assert_eq!(cursor.member_index, 3);
    assert_eq!(cursor.offset_bytes, 31);
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
    let body =
        String::from_utf8(response_body(response).await).expect("the deterministic page is UTF-8");

    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("fetch(\"/api/bootstrap\")"));
    assert!(body.contains("fetch(\"/api/test/stream\")"));
}
