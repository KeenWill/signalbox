//! Browser session creation through the interactive command services.

use axum::{
    Extension, Json,
    extract::{Request, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use signalbox_application::{
    CreateSessionOutcome, CreateSessionRequest, CreateSessionService, UuidV7SessionIdGenerator,
};
use signalbox_domain::{
    DurableCommandId, SessionId, SessionOwnership, SessionPlacement, SessionTemplateName,
    StartGate, UserContent,
};
use signalbox_persistence::create_session::{
    CreateSessionRepository, CreateSessionRepositoryError,
};
use signalbox_web_contract::{WebCreateSessionRequest, WebCreateSessionResponse, WebSessionId};
use uuid::Uuid;

use super::{
    WebApiState, application_error, decode_bounded_json,
    session_catalog::session_catalog_summary_dto, submit_input, web_input_conflict,
};
use crate::configuration_reload::ConfigurationReload;

pub(super) async fn create_session(
    State(mut state): State<WebApiState>,
    reload: Option<Extension<ConfigurationReload>>,
    request: Request,
) -> Response {
    let request = match decode_bounded_json::<WebCreateSessionRequest>(request).await {
        Ok(request) => request,
        Err(response) => return response,
    };
    let Ok(command_uuid) = request.command_id.as_str().parse::<Uuid>() else {
        return invalid_creation("command identity is invalid");
    };
    if command_uuid.to_string() != request.command_id
        || command_uuid.is_nil()
        || command_uuid.is_max()
    {
        return invalid_creation("command identity must be canonical and non-reserved");
    }
    let Ok(name) = SessionTemplateName::try_new(request.template_name) else {
        return invalid_creation("template name is invalid");
    };
    if let Some(input) = &request.first_input {
        let valid_id = input.command_id.parse::<Uuid>().ok().is_some_and(|id| {
            id.to_string() == input.command_id && !id.is_nil() && !id.is_max() && id != command_uuid
        });
        if !valid_id || UserContent::try_text(input.message.clone()).is_err() {
            return invalid_creation(
                "first input requires a distinct command identity and valid text",
            );
        }
    }
    let Some(Extension(reload)) = reload else {
        return creation_unavailable();
    };
    let catalogs = reload.catalogs();
    state.model_configuration = Some(catalogs.models.clone());
    let Some(pool) = state.pool.clone() else {
        return creation_unavailable();
    };
    if request.first_input.is_some() && state.eligibility_nudge.is_none() {
        return creation_unavailable();
    }
    let command_id = DurableCommandId::from_uuid(command_uuid);
    let repository = CreateSessionRepository::new(pool, catalogs.models.session_credential_pin());
    let session = match repository.load(command_id).await {
        Ok(Some(recorded)) => {
            let command = recorded.command();
            if command
                .template_provenance()
                .map(|provenance| provenance.name())
                != Some(&name)
                || command.placement() != &SessionPlacement::pathless()
                || command.start_gate() != StartGate::Open
                || command.ownership() != SessionOwnership::Unmonitored
                || command.finish_condition().is_some()
            {
                return web_input_conflict();
            }
            recorded.applied_result().session()
        }
        Ok(None) => {
            let Some(template) = catalogs.templates.resolve(&name) else {
                return invalid_creation("template is not configured");
            };
            if catalogs
                .models
                .resolve_session_model(template.defaults().model())
                .is_err()
            {
                return invalid_creation("template model is not configured");
            }
            let Ok(creation) = CreateSessionRequest::try_new_from_template(
                command_id,
                template.provenance().clone(),
                template.defaults().clone(),
            ) else {
                return invalid_creation("command identity is reserved");
            };
            match CreateSessionService::new(UuidV7SessionIdGenerator, repository)
                .execute(creation)
                .await
            {
                Ok(CreateSessionOutcome::Applied(result)) => result.session(),
                Ok(CreateSessionOutcome::ConflictingReuse { .. }) => return web_input_conflict(),
                Err(_) => return creation_unavailable(),
            }
        }
        Err(CreateSessionRepositoryError::DifferentCommandKind { .. }) => {
            return web_input_conflict();
        }
        Err(_) => return creation_unavailable(),
    };
    if let Some(input) = request.first_input {
        let response = submit_input(state.clone(), session.into_uuid().to_string(), input).await;
        if !response.status().is_success() {
            return response;
        }
    }
    creation_response(state, session).await
}

async fn creation_response(state: WebApiState, session: SessionId) -> Response {
    let (Some(repository), Some(budget)) = (state.attention, state.snapshot_reader_budget) else {
        return creation_unavailable();
    };
    let Ok(_permit) = budget.acquire().await else {
        return creation_unavailable();
    };
    let summary = match repository.summary(session).await {
        Ok(Some(summary)) => summary,
        _ => return creation_unavailable(),
    };
    match session_catalog_summary_dto(summary) {
        Ok(summary) => (
            StatusCode::CREATED,
            Json(WebCreateSessionResponse {
                session_id: WebSessionId::from_uuid_bytes(session.into_uuid().into_bytes()),
                summary,
            }),
        )
            .into_response(),
        Err(()) => creation_unavailable(),
    }
}

fn invalid_creation(message: &'static str) -> Response {
    application_error(StatusCode::BAD_REQUEST, "invalid_session_creation", message)
}

fn creation_unavailable() -> Response {
    application_error(
        StatusCode::SERVICE_UNAVAILABLE,
        "creation_outcome_unconfirmed",
        "session creation outcome is unconfirmed; retry the same command identities and payload",
    )
}

#[cfg(all(test, feature = "test-support"))]
#[allow(
    clippy::expect_used,
    reason = "HTTP fixtures state expected admission and responses"
)]
mod tests {
    use super::*;
    use axum::body::{Body, to_bytes};
    use signalbox_application::{
        EligibilitySweep, EligibilitySweepBatch, InProcessEligibilityWorkSource,
    };
    use tower::ServiceExt as _;

    struct EmptySweep;
    impl EligibilitySweep for EmptySweep {
        type Error = std::convert::Infallible;
        async fn find_sessions(&mut self) -> Result<EligibilitySweepBatch, Self::Error> {
            Ok(EligibilitySweepBatch::new(Vec::new(), false))
        }
    }

    fn router(pool: sqlx::PgPool, templates: &str) -> axum::Router {
        let models =
            crate::HubModelConfiguration::parse(crate::configuration::tests::CONFIGURATION)
                .expect("model fixture");
        let templates = crate::SessionTemplateConfiguration::parse_snapshot(templates, &models)
            .expect("template fixture");
        let reload = ConfigurationReload::new(
            pool.clone(),
            models.clone(),
            templates,
            "/unused/models.toml".into(),
            "/unused/templates.toml".into(),
            None,
        )
        .expect("reload fixture");
        let (nudge, _work) = InProcessEligibilityWorkSource::with_options(EmptySweep, None, None);
        super::super::production_router(
            None,
            Some(pool),
            None,
            Some(models),
            None,
            None,
            Some(nudge),
        )
        .layer(Extension(reload))
    }

    fn request(command: Uuid, first_input: Uuid) -> Request {
        Request::post("/api/sessions")
            .header("host", "localhost")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::json!({
                "command_id": command.to_string(), "template_name": "chat",
                "first_input": {"command_id": first_input.to_string(), "message": "Inspect the fixture"}
            }).to_string())).expect("creation request")
    }

    #[tokio::test]
    #[ignore = "requires ephemeral PostgreSQL"]
    async fn creation_replay_keeps_one_session_and_first_input_after_template_removal() {
        let (_container, pool, _) =
            signalbox_persistence::test_support::postgres::migrated_postgres(64)
                .await
                .expect("PostgreSQL fixture");
        let templates = "version = 1\n[[templates]]\nname = \"chat\"\nversion = 1\nalias = \"30000000-0000-4000-8000-000000000001\"\ndangerous_tool_auto_approval = false\nsystem_prompt = \"Follow the fixture instructions\"\n";
        let command = Uuid::now_v7();
        let input = Uuid::now_v7();
        let response = router(pool.clone(), templates)
            .oneshot(request(command, input))
            .await
            .expect("HTTP response");
        let status = response.status();
        let bytes = to_bytes(
            response.into_body(),
            signalbox_web_contract::MAX_JSON_BODY_BYTES,
        )
        .await
        .expect("body");
        assert_eq!(status, StatusCode::CREATED, "{bytes:?}");
        let first: WebCreateSessionResponse =
            serde_json::from_slice(&bytes).expect("creation receipt");
        assert_eq!(first.session_id, first.summary.session_id);
        assert_eq!(
            first.summary.queued_turn_count,
            signalbox_web_contract::WebU64::from_u64(1)
        );

        let response = router(pool.clone(), "version = 1\n")
            .oneshot(request(command, input))
            .await
            .expect("replay response");
        assert_eq!(response.status(), StatusCode::CREATED);
        let replay: WebCreateSessionResponse = serde_json::from_slice(
            &to_bytes(
                response.into_body(),
                signalbox_web_contract::MAX_JSON_BODY_BYTES,
            )
            .await
            .expect("body"),
        )
        .expect("replay receipt");
        assert_eq!(replay.session_id, first.session_id);
        let accepted: i64 = sqlx::query_scalar("SELECT count(*) FROM accepted_input")
            .fetch_one(&pool)
            .await
            .expect("input count");
        assert_eq!(accepted, 1);
        pool.close().await;
    }

    #[tokio::test]
    #[ignore = "requires ephemeral PostgreSQL"]
    async fn point_summary_includes_archived_sessions() {
        use signalbox_domain::{
            CreateSession, DirectModelSelection, ModelSelectionRequest, ReplaceSessionMetadata,
            SessionConfigurationDefaults, SessionCreationCause, SessionCreationProvenance,
            SessionMetadataContent, TranscriptAncestry,
        };
        let (_container, pool, _) =
            signalbox_persistence::test_support::postgres::migrated_postgres(8)
                .await
                .expect("PostgreSQL fixture");
        let models =
            crate::HubModelConfiguration::parse(crate::configuration::tests::CONFIGURATION)
                .expect("models");
        let session = SessionId::from_uuid(Uuid::now_v7());
        let creation = CreateSession::new(
            DurableCommandId::from_uuid(Uuid::now_v7()),
            SessionCreationProvenance::new(
                SessionCreationCause::Interactive,
                TranscriptAncestry::None,
            ),
            SessionConfigurationDefaults::new(ModelSelectionRequest::Direct(
                DirectModelSelection::from_uuid(uuid::uuid!(
                    "10000000-0000-4000-8000-000000000001"
                )),
            )),
        )
        .prepare(session)
        .expect("creation");
        CreateSessionRepository::new(pool.clone(), models.session_credential_pin())
            .handle(creation)
            .await
            .expect("created session");
        signalbox_persistence::session_metadata::SessionMetadataRepository::new(pool.clone())
            .handle(ReplaceSessionMetadata::new(
                DurableCommandId::from_uuid(Uuid::now_v7()),
                session,
                SessionMetadataContent::try_new(
                    Some("Archived review".into()),
                    Vec::new(),
                    Vec::new(),
                    true,
                )
                .expect("metadata"),
            ))
            .await
            .expect("archive session");
        let repository = signalbox_persistence::attention::AttentionRepository::new(
            pool.clone(),
            signalbox_persistence::attention::AutomaticResumeAttemptBounds::unbounded(),
        );
        let summary = repository
            .summary(session)
            .await
            .expect("summary read")
            .expect("archived summary");
        assert_eq!(summary.session, session);
        assert!(summary.archived);
        assert_eq!(summary.title_summary.as_deref(), Some("Archived review"));
        assert!(
            repository
                .summary(SessionId::from_uuid(Uuid::now_v7()))
                .await
                .expect("absent read")
                .is_none()
        );
        pool.close().await;
    }
}
