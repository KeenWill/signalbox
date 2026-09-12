//! Human title edits through the session metadata replacement service.

use axum::{
    extract::{Path, Request, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use signalbox_application::{
    ReplaceSessionMetadataOutcome, ReplaceSessionMetadataRequest, ReplaceSessionMetadataService,
};
use signalbox_domain::{
    Actor, DurableCommandId, ReplaceSessionMetadataResult, SessionMetadataContent,
};
use signalbox_persistence::session_metadata::{
    SessionMetadataRepository, SessionMetadataRepositoryError,
};
use signalbox_web_contract::WebSessionTitleRequest;
use uuid::Uuid;

use super::{
    WebApiState, application_error, decode_bounded_json, parse_canonical_session_id,
    web_input_conflict,
};

pub(super) async fn replace_title(
    State(state): State<WebApiState>,
    Path(session_id): Path<String>,
    request: Request,
) -> Response {
    let request = match decode_bounded_json::<WebSessionTitleRequest>(request).await {
        Ok(request) => request,
        Err(response) => return response,
    };
    let Ok(session) = parse_canonical_session_id(&session_id) else {
        return invalid_title();
    };
    let Some(command) = request
        .command_id
        .parse::<Uuid>()
        .ok()
        .filter(|id| id.to_string() == request.command_id && !id.is_nil() && !id.is_max())
    else {
        return invalid_title();
    };
    if SessionMetadataContent::try_new(Some(request.title.clone()), Vec::new(), Vec::new(), false)
        .is_err()
    {
        return invalid_title();
    }
    let Some(pool) = state.pool else {
        return title_unconfirmed();
    };
    let repository = SessionMetadataRepository::new(pool);
    let command = DurableCommandId::from_uuid(command);
    match repository.load_command(command).await {
        Ok(Some(recorded)) => {
            if recorded.command().session() != session
                || recorded.command().actor() != Actor::User
                || recorded.command().replacement().title() != Some(request.title.as_str())
            {
                return web_input_conflict();
            }
            return title_result(recorded.result());
        }
        Ok(None) => {}
        Err(SessionMetadataRepositoryError::DifferentCommandKind { .. }) => {
            return web_input_conflict();
        }
        Err(_) => return title_unconfirmed(),
    }
    let current = match repository.load_session_metadata(session).await {
        Ok(Some(current)) => current,
        Ok(None) => {
            return application_error(
                StatusCode::NOT_FOUND,
                "session_not_found",
                "the requested session does not exist",
            );
        }
        Err(_) => return title_unconfirmed(),
    };
    let content = current.content();
    let Ok(replacement) = SessionMetadataContent::try_new(
        Some(request.title),
        content.tags().map(str::to_owned).collect(),
        content
            .attributes()
            .map(|(key, value)| (key.to_owned(), value.to_owned()))
            .collect(),
        content.archived(),
    ) else {
        return invalid_title();
    };
    let Ok(request) = ReplaceSessionMetadataRequest::try_new(command, session, replacement) else {
        return invalid_title();
    };
    match ReplaceSessionMetadataService::new(repository)
        .execute(request)
        .await
    {
        Ok(ReplaceSessionMetadataOutcome::Recorded(result)) => title_result(&result),
        Ok(ReplaceSessionMetadataOutcome::ConflictingReuse { .. }) => title_unconfirmed(),
        Err(_) => title_unconfirmed(),
    }
}

fn title_result(result: &ReplaceSessionMetadataResult) -> Response {
    match result {
        ReplaceSessionMetadataResult::Applied(_) => StatusCode::NO_CONTENT.into_response(),
        ReplaceSessionMetadataResult::Rejected(_) => application_error(
            StatusCode::NOT_FOUND,
            "session_not_found",
            "the requested session does not exist",
        ),
    }
}

fn invalid_title() -> Response {
    application_error(
        StatusCode::BAD_REQUEST,
        "invalid_session_title",
        "session and command identities must be canonical; title must be nonempty and NUL-free",
    )
}

fn title_unconfirmed() -> Response {
    application_error(
        StatusCode::SERVICE_UNAVAILABLE,
        "metadata_outcome_unconfirmed",
        "title update outcome is unconfirmed; retry the same command identity and title",
    )
}

#[cfg(all(test, feature = "test-support"))]
#[allow(
    clippy::expect_used,
    reason = "HTTP fixtures state expected admission and responses"
)]
mod tests {
    use super::*;
    use axum::body::Body;
    use signalbox_domain::{
        CreateSession, DirectModelSelection, ModelSelectionRequest, ReplaceSessionMetadata,
        SessionConfigurationDefaults, SessionCreationCause, SessionCreationProvenance, SessionId,
        TranscriptAncestry,
    };
    use tower::ServiceExt as _;

    fn request(session: SessionId, command: DurableCommandId, title: &str) -> Request {
        Request::patch(format!("/api/sessions/{}/metadata", session.into_uuid()))
            .header("host", "localhost")
            .header("content-type", "application/json")
            .body(Body::from(
                serde_json::json!({"command_id": command.into_uuid().to_string(), "title": title})
                    .to_string(),
            ))
            .expect("title request")
    }

    #[tokio::test]
    #[ignore = "requires ephemeral PostgreSQL"]
    async fn title_edit_preserves_metadata_and_replay_does_not_reinstall_old_values() {
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
        signalbox_persistence::create_session::CreateSessionRepository::new(
            pool.clone(),
            models.session_credential_pin(),
        )
        .handle(creation)
        .await
        .expect("create session");
        let repository = SessionMetadataRepository::new(pool.clone());
        let initial = SessionMetadataContent::try_new(
            Some("Initial".into()),
            vec!["review".into()],
            vec![("source".into(), "fixture".into())],
            true,
        )
        .expect("metadata");
        repository
            .handle(ReplaceSessionMetadata::new(
                DurableCommandId::from_uuid(Uuid::now_v7()),
                session,
                initial,
            ))
            .await
            .expect("initial metadata");
        let router = super::super::production_router(
            None,
            Some(pool.clone()),
            None,
            Some(models),
            None,
            None,
            None,
        );
        let command = DurableCommandId::from_uuid(Uuid::now_v7());
        assert_eq!(
            router
                .clone()
                .oneshot(request(session, command, "Renamed"))
                .await
                .expect("response")
                .status(),
            StatusCode::NO_CONTENT
        );
        let renamed = repository
            .load_session_metadata(session)
            .await
            .expect("read")
            .expect("metadata");
        assert_eq!(renamed.content().title(), Some("Renamed"));
        assert_eq!(renamed.content().tags().collect::<Vec<_>>(), ["review"]);
        assert_eq!(
            renamed.content().attributes().collect::<Vec<_>>(),
            [("source", "fixture")]
        );
        assert!(renamed.content().archived());
        let later = SessionMetadataContent::try_new(Some("Later".into()), vec![], vec![], false)
            .expect("later metadata");
        repository
            .handle(ReplaceSessionMetadata::new(
                DurableCommandId::from_uuid(Uuid::now_v7()),
                session,
                later.clone(),
            ))
            .await
            .expect("later replacement");
        assert_eq!(
            router
                .clone()
                .oneshot(request(session, command, "Renamed"))
                .await
                .expect("replay")
                .status(),
            StatusCode::NO_CONTENT
        );
        assert_eq!(
            repository
                .load_session_metadata(session)
                .await
                .expect("read")
                .expect("metadata")
                .content(),
            &later
        );
        assert_eq!(
            router
                .oneshot(request(session, command, "Conflicting"))
                .await
                .expect("conflict")
                .status(),
            StatusCode::CONFLICT
        );
        pool.close().await;
    }
}
