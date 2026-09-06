#![allow(
    clippy::expect_used,
    reason = "HTTP integration fixtures state expected admission"
)]
mod support;
use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use signalbox_domain::{
    CreateSession, DirectModelSelection, DurableCommandId, ModelSelectionRequest,
    SessionConfigurationDefaults, SessionCreationCause, SessionCreationProvenance, SessionId,
    TranscriptAncestry,
};
use signalbox_persistence::{
    create_session::CreateSessionRepository, disposable_postgres_server_args,
    disposable_postgres_state_tmpfs_from_example, disposable_test_container_labels,
    local_test_connection_options,
};
use sqlx::postgres::PgPoolOptions;
use std::error::Error;
use testcontainers_modules::{
    postgres::Postgres,
    testcontainers::{ImageExt, runners::AsyncRunner},
};
use tower::ServiceExt as _;
use uuid::Uuid;
const MODEL_CONFIGURATION: &str = r#"
version = 1

[[credential_profiles]]
name = "anthropic-primary"
adapter = "anthropic"
billing_kind = "api_metered"
delivery = "file"
file = "/run/secrets/anthropic-primary"

[[credential_pools]]
name = "anthropic-main"
tie_break = "first_listed"
on_pool_exhausted = "park"
members = [{ profile = "anthropic-primary", priority = 1 }]


[[adapter_mappings]]
model_family = "anthropic"
adapter = "anthropic"
credential_pool = "anthropic-main"

[compaction]
prompt = "Summarize the prior conversation faithfully for continuation."

[[models]]
selection_id = "00000000-0000-0000-0000-000000000001"
target_id = "00000000-0000-0000-0000-000000000003"
model_family = "anthropic"
provider_model = "fixture-model"
max_output_tokens = 256
context_window_tokens = 200000
reasoning_levels = ["low"]

"#;

fn submission(session: SessionId, command: Uuid, message: &str) -> Request<Body> {
    Request::post(format!("/api/sessions/{}/input", session.into_uuid()))
        .header("host", "localhost")
        .header("origin", "http://localhost")
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::json!({"command_id": command.to_string(), "message": message}).to_string(),
        ))
        .expect("fixture request is valid")
}
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn web_input_uses_operator_identity_and_retries_one_durable_acceptance()
-> Result<(), Box<dyn Error>> {
    let container = Postgres::default()
        .with_db_name("signalbox_web")
        .with_user("signalbox")
        .with_password("signalbox-test-only")
        .with_cmd(disposable_postgres_server_args())
        .with_mount(disposable_postgres_state_tmpfs_from_example()?)
        .with_tag("18.4-alpine3.23")
        .with_labels(disposable_test_container_labels())
        .start()
        .await?;
    let url = format!(
        "postgres://signalbox:signalbox-test-only@{}:{}/signalbox_web",
        container.get_host().await?,
        container.get_host_port_ipv4(5432).await?
    );
    let pool = PgPoolOptions::new()
        .max_connections(8)
        .connect_with(local_test_connection_options(&url)?)
        .await?;
    signalbox_persistence::migrate(&pool).await?;

    let configuration = support::parse_model_configuration(MODEL_CONFIGURATION)?;
    let session = SessionId::from_uuid(Uuid::from_u128(27));
    let creation = CreateSession::new(
        DurableCommandId::from_uuid(Uuid::from_u128(28)),
        SessionCreationProvenance::new(SessionCreationCause::Interactive, TranscriptAncestry::None),
        SessionConfigurationDefaults::new(ModelSelectionRequest::Direct(
            DirectModelSelection::from_uuid(Uuid::from_u128(1)),
        )),
    )
    .prepare(session)
    .expect("fixture session is admitted");
    CreateSessionRepository::new(pool.clone(), configuration.session_credential_pin())
        .handle(creation)
        .await?;
    let router = signalboxd::web_http::production_router(
        None,
        Some(pool.clone()),
        None,
        Some(configuration),
        None,
        None,
    );
    let command = Uuid::from_u128(30);
    for _ in 0..2 {
        let response = router
            .clone()
            .oneshot(submission(session, command, "Read the synthetic fixture"))
            .await?;
        let status = response.status();
        let body = to_bytes(response.into_body(), 65536).await?;
        assert_eq!(status, StatusCode::NO_CONTENT, "{body:?}");
    }
    let accepted: i64 =
        sqlx::query_scalar("SELECT count(*) FROM accepted_input WHERE session_id = $1")
            .bind(session.into_uuid())
            .fetch_one(&pool)
            .await?;
    assert_eq!(accepted, 1);
    let issuer: String =
        sqlx::query_scalar("SELECT issuer_kind FROM durable_command WHERE command_id = $1")
            .bind(command)
            .fetch_one(&pool)
            .await?;
    assert_eq!(issuer, "operator");
    let response = router
        .clone()
        .oneshot(submission(session, command, "Different input"))
        .await?;
    assert_eq!(response.status(), StatusCode::CONFLICT);
    let body: serde_json::Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 65536).await?)?;
    assert_eq!(body["error"]["code"], "conflicting_command_reuse");
    let missing = router
        .oneshot(submission(
            SessionId::from_uuid(Uuid::from_u128(99)),
            Uuid::from_u128(31),
            "Missing session",
        ))
        .await?;
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
    pool.close().await;
    drop(container);
    Ok(())
}
#[tokio::test]
async fn web_input_requires_same_origin_json_and_valid_text() -> Result<(), Box<dyn Error>> {
    let router = signalboxd::web_http::production_router(None, None, None, None, None, None);
    let session = SessionId::from_uuid(Uuid::from_u128(27));
    let command = Uuid::from_u128(30);
    let mut cross_origin = submission(session, command, "Message");
    cross_origin
        .headers_mut()
        .insert("origin", "https://example.com".parse()?);
    assert_eq!(
        router.clone().oneshot(cross_origin).await?.status(),
        StatusCode::FORBIDDEN
    );
    let mut non_json = submission(session, command, "Message");
    non_json
        .headers_mut()
        .insert("content-type", "text/plain".parse()?);
    assert_eq!(
        router.clone().oneshot(non_json).await?.status(),
        StatusCode::UNSUPPORTED_MEDIA_TYPE
    );
    let mut noncanonical = submission(session, command, "Message");
    *noncanonical.body_mut() = Body::from(
        serde_json::json!({
            "command_id": "00000000-0000-0000-0000-0000000000AF",
            "message": "Message"
        })
        .to_string(),
    );
    assert_eq!(
        router.clone().oneshot(noncanonical).await?.status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        router
            .clone()
            .oneshot(submission(session, command, ""))
            .await?
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        router
            .oneshot(submission(session, command, "Message"))
            .await?
            .status(),
        StatusCode::SERVICE_UNAVAILABLE
    );
    Ok(())
}
