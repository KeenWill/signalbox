#![allow(
    clippy::expect_used,
    reason = "integration fixtures state expected admission"
)]

use std::error::Error;

use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use signalbox_application::{SubmitInputRequest, SubmitInputService, UuidV7SubmitInputIdGenerator};
use signalbox_domain::{
    CreateSession, DeliveryRequest, DirectModelSelection, DurableCommandId, ModelSelectionOverride,
    ModelSelectionRequest, PerInputConfigurationChoices, SessionConfigurationDefaults,
    SessionConfigurationDefaultsVersion, SessionCreationCause, SessionCreationProvenance,
    SessionId, TranscriptAncestry, UserContent,
};
use signalbox_persistence::{
    SessionCredentialPin, SessionModelCredential, create_session::CreateSessionRepository,
    submit_input::SubmitInputRepository,
};
use signalbox_web_contract::WebSessionRates;
use tower::ServiceExt as _;
use uuid::Uuid;

#[derive(Clone)]
struct TestNudge;
impl signalbox_application::EligibilityNudge for TestNudge {
    fn nudge(&self, _session: SessionId) -> signalbox_application::EligibilityNudgeOutcome {
        signalbox_application::EligibilityNudgeOutcome::WorkSourceClosed
    }
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn rates_read_counts_only_the_requested_sessions() -> Result<(), Box<dyn Error>> {
    let (container, pool, _url) =
        signalbox_persistence::test_support::postgres::migrated_postgres(8).await?;
    let session = SessionId::from_uuid(Uuid::from_u128(27));
    let command = CreateSession::new(
        DurableCommandId::from_uuid(Uuid::from_u128(28)),
        SessionCreationProvenance::new(SessionCreationCause::Interactive, TranscriptAncestry::None),
        SessionConfigurationDefaults::new(ModelSelectionRequest::Direct(
            DirectModelSelection::from_uuid(Uuid::from_u128(29)),
        )),
    )
    .prepare(session)
    .expect("interactive creation is admitted");
    CreateSessionRepository::new(
        pool.clone(),
        SessionCredentialPin::try_new(vec![SessionModelCredential::new(
            "test-model-family",
            "test-model-primary",
        )])
        .expect("fixture credential pin is admitted"),
    )
    .handle(command)
    .await?;
    let mut submit = SubmitInputService::new(
        UuidV7SubmitInputIdGenerator,
        SubmitInputRepository::new(pool.clone()),
        TestNudge,
        signalbox_application::InProcessToolDispatchGate::default(),
    );
    submit
        .execute(SubmitInputRequest::try_new(
            DurableCommandId::from_uuid(Uuid::from_u128(30)),
            session,
            UserContent::try_text("Read the synthetic fixture".to_owned())
                .expect("fixture text is admitted"),
            DeliveryRequest::StartWhenNoActiveTurn {
                configuration: PerInputConfigurationChoices::new(
                    SessionConfigurationDefaultsVersion::first(),
                    ModelSelectionOverride::UseSessionDefault,
                ),
            },
        )?)
        .await?;
    let router = signalboxd::web_http::production_router(
        None,
        Some(pool.clone()),
        None,
        None,
        None,
        None,
        None,
    );
    let response = router
        .oneshot(
            Request::get(format!(
                "/api/sessions/rates?session_id={}",
                session.into_uuid()
            ))
            .header("host", "localhost")
            .body(Body::empty())?,
        )
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    let rates: WebSessionRates =
        serde_json::from_slice(&to_bytes(response.into_body(), 65536).await?)?;
    assert_eq!(rates.sessions.len(), 1);
    assert_eq!(
        rates.sessions[0].session_id.as_str(),
        session.into_uuid().to_string()
    );
    assert_eq!(
        rates.sessions[0].turn_count,
        signalbox_web_contract::WebU64::from_u64(1)
    );
    assert_eq!(
        rates.sessions[0].failed_turn_count,
        signalbox_web_contract::WebU64::from_u64(0)
    );
    assert_eq!(rates.sessions[0].last_provider_cause, None);
    pool.close().await;
    drop(container);
    Ok(())
}
