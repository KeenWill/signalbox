use super::{
    CONTENT_TYPE, DefaultBodyLimit, HeaderValue, IntoResponse, Json, MAX_JSON_BODY_BYTES, Request,
    Response, Router, State, WebBlobRuntime, WebContractBootstrap, WebContractExample,
    WebHttpState, api_not_found, decode_bounded_json, get, middleware, ndjson_response, post,
    stream, validate_json_mutation,
};

/// Builds an in-memory deterministic server with no persistence dependency.
///
/// It uses the same guards, body decoder, generated DTOs, and NDJSON encoder as
/// production endpoints. The test-only surface is never mounted by
/// [`super::production_router`].
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

pub(super) async fn contract_bootstrap(
    State(state): State<WebHttpState>,
) -> Json<WebContractBootstrap> {
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
