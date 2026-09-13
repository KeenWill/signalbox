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
    let Ok(replacement) =
        SessionMetadataContent::try_new(Some(request.title.clone()), Vec::new(), Vec::new(), false)
    else {
        return invalid_title();
    };
    let Some(pool) = state.pool else {
        return title_unconfirmed();
    };
    let repository = SessionMetadataRepository::for_title_update(pool);
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
    let Ok(request) = ReplaceSessionMetadataRequest::try_new(command, session, replacement) else {
        return invalid_title();
    };
    match ReplaceSessionMetadataService::new(repository)
        .execute(request)
        .await
    {
        Ok(ReplaceSessionMetadataOutcome::Recorded(result)) => title_result(&result),
        Ok(ReplaceSessionMetadataOutcome::ConflictingReuse { .. }) => web_input_conflict(),
        Err(SessionMetadataRepositoryError::InvalidTitleMerge(_)) => invalid_title(),
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
        "session and command identities must be canonical; title must be nonempty, NUL-free, and fit the metadata size limit",
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
        let initial_command = DurableCommandId::from_uuid(Uuid::now_v7());
        repository
            .handle(ReplaceSessionMetadata::new(
                initial_command,
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
        assert_eq!(
            router
                .clone()
                .oneshot(request(session, initial_command, "Initial"))
                .await
                .expect("full replacement collision")
                .status(),
            StatusCode::CONFLICT
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
        assert!(matches!(
            repository.load_command(command).await,
            Err(SessionMetadataRepositoryError::DifferentCommandKind { .. })
        ));
        assert!(matches!(
            repository.handle(ReplaceSessionMetadata::new(command, session, renamed.content().clone())).await.expect("full replacement collision"),
            signalbox_persistence::session_metadata::ReplaceSessionMetadataHandlingOutcome::ConflictingReuse { .. }
        ));
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
        let title_repository = SessionMetadataRepository::for_title_update(pool.clone());
        let resampled =
            SessionMetadataContent::try_new(Some("Renamed".into()), vec![], vec![], false)
                .expect("resampled preserved fields");
        assert!(matches!(
            title_repository.handle(ReplaceSessionMetadata::new(command, session, resampled)).await.expect("transaction replay"),
            signalbox_persistence::session_metadata::ReplaceSessionMetadataHandlingOutcome::Recorded(_)
        ));
        assert_eq!(
            repository
                .load_session_metadata(session)
                .await
                .expect("read")
                .expect("metadata")
                .content(),
            &later
        );
        pool.close().await;
    }

    #[tokio::test]
    #[ignore = "requires ephemeral PostgreSQL"]
    async fn oversized_title_merge_returns_bad_request_without_retaining_changes_or_command() {
        const INITIAL_TITLE: &str = "a";
        const OVERSIZED_TITLE: &str = "bb";
        const FITTING_TITLE: &str = "b";
        const ATTRIBUTE_KEY: &str = "payload";
        const PRESERVED_TAG: &str = "retained";
        const CONFIGURED_MODEL: Uuid = uuid::uuid!("10000000-0000-4000-8000-000000000001");
        const POOL_CONNECTIONS: u32 = 8;
        let (_container, pool, _) =
            signalbox_persistence::test_support::postgres::migrated_postgres(POOL_CONNECTIONS)
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
                DirectModelSelection::from_uuid(CONFIGURED_MODEL),
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
        let payload = "x".repeat(
            SessionMetadataContent::MAX_TOTAL_UTF8_BYTES
                - INITIAL_TITLE.len()
                - ATTRIBUTE_KEY.len()
                - PRESERVED_TAG.len(),
        );
        let archived = true;
        let initial = SessionMetadataContent::try_new(
            Some(INITIAL_TITLE.into()),
            vec![PRESERVED_TAG.into()],
            vec![(ATTRIBUTE_KEY.into(), payload.clone())],
            archived,
        )
        .expect("metadata exactly at the total size limit");
        let repository = SessionMetadataRepository::new(pool.clone());
        repository
            .handle(ReplaceSessionMetadata::new(
                DurableCommandId::from_uuid(Uuid::now_v7()),
                session,
                initial.clone(),
            ))
            .await
            .expect("initial metadata");
        let title_repository = SessionMetadataRepository::for_title_update(pool.clone());
        let command = DurableCommandId::from_uuid(Uuid::now_v7());
        let title_intent =
            SessionMetadataContent::try_new(Some(OVERSIZED_TITLE.into()), vec![], vec![], false)
                .expect("standalone title fits");
        assert!(matches!(
            title_repository
                .handle(ReplaceSessionMetadata::new(command, session, title_intent))
                .await,
            Err(SessionMetadataRepositoryError::InvalidTitleMerge(
                signalbox_domain::SessionMetadataContentError::TotalUtf8BytesExceeded
            ))
        ));
        let router = super::super::production_router(
            None,
            Some(pool.clone()),
            None,
            Some(models),
            None,
            None,
            None,
        );
        let response = router
            .clone()
            .oneshot(request(session, command, OVERSIZED_TITLE))
            .await
            .expect("oversized merge response");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("error body");
        let error: signalbox_web_contract::WebApiErrorResponse =
            serde_json::from_slice(&body).expect("JSON error");
        assert_eq!(error.error.code, "invalid_session_title");
        assert_eq!(
            repository
                .load_session_metadata(session)
                .await
                .expect("read unchanged metadata")
                .expect("metadata")
                .content(),
            &initial
        );
        assert!(
            title_repository
                .load_command(command)
                .await
                .expect("read rolled-back command")
                .is_none()
        );
        assert_eq!(
            router
                .oneshot(request(session, command, FITTING_TITLE))
                .await
                .expect("fitting title response")
                .status(),
            StatusCode::NO_CONTENT
        );
        let expected = SessionMetadataContent::try_new(
            Some(FITTING_TITLE.into()),
            vec![PRESERVED_TAG.into()],
            vec![(ATTRIBUTE_KEY.into(), payload)],
            archived,
        )
        .expect("fitting merged snapshot");
        assert_eq!(
            repository
                .load_session_metadata(session)
                .await
                .expect("read renamed metadata")
                .expect("metadata")
                .content(),
            &expected
        );
        pool.close().await;
    }

    #[tokio::test]
    #[ignore = "requires ephemeral PostgreSQL"]
    async fn title_requests_reject_full_metadata_command_identity() {
        let (_container, pool, _) =
            signalbox_persistence::test_support::postgres::migrated_postgres(8)
                .await
                .expect("PostgreSQL fixture");
        let session = SessionId::from_uuid(Uuid::now_v7());
        let command = DurableCommandId::from_uuid(Uuid::now_v7());
        let full = SessionMetadataContent::try_new(
            Some("Same title".into()),
            vec!["full".into()],
            vec![("source".into(), "protocol".into())],
            true,
        )
        .expect("full replacement");
        let repository = SessionMetadataRepository::new(pool.clone());
        let title_repository = SessionMetadataRepository::for_title_update(pool.clone());
        assert!(
            title_repository
                .load_command(command)
                .await
                .expect("unseen title request")
                .is_none()
        );
        repository
            .handle(ReplaceSessionMetadata::new(command, session, full))
            .await
            .expect("record full replacement");
        let router =
            super::super::production_router(None, Some(pool.clone()), None, None, None, None, None);
        assert_eq!(
            router
                .oneshot(request(session, command, "Same title"))
                .await
                .expect("response")
                .status(),
            StatusCode::CONFLICT
        );
        // Exercise the transaction's collision path after an earlier unseen-command read.
        let title =
            SessionMetadataContent::try_new(Some("Same title".into()), vec![], vec![], false)
                .expect("title intent");
        let request =
            ReplaceSessionMetadataRequest::try_new(command, session, title).expect("request");
        assert!(matches!(
            ReplaceSessionMetadataService::new(title_repository)
                .execute(request)
                .await
                .expect("raced full replacement"),
            ReplaceSessionMetadataOutcome::ConflictingReuse { .. }
        ));
        pool.close().await;
    }
    #[tokio::test]
    #[ignore = "requires ephemeral PostgreSQL"]
    async fn missing_session_title_rejection_retains_command_identity() {
        let (_container, pool, _) =
            signalbox_persistence::test_support::postgres::migrated_postgres(8)
                .await
                .expect("PostgreSQL fixture");
        let session = SessionId::from_uuid(Uuid::now_v7());
        let command = DurableCommandId::from_uuid(Uuid::now_v7());
        let router =
            super::super::production_router(None, Some(pool.clone()), None, None, None, None, None);
        assert_eq!(
            router
                .clone()
                .oneshot(request(session, command, "Missing"))
                .await
                .expect("first rejection")
                .status(),
            StatusCode::NOT_FOUND
        );
        let recorded = SessionMetadataRepository::for_title_update(pool.clone())
            .load_command(command)
            .await
            .expect("load rejection")
            .expect("durable rejection");
        assert!(matches!(
            recorded.result(),
            ReplaceSessionMetadataResult::Rejected(_)
        ));
        assert_eq!(
            router
                .clone()
                .oneshot(request(session, command, "Missing"))
                .await
                .expect("replayed rejection")
                .status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            router
                .clone()
                .oneshot(request(session, command, "Different"))
                .await
                .expect("conflicting title")
                .status(),
            StatusCode::CONFLICT
        );
        let other_session = SessionId::from_uuid(Uuid::now_v7());
        assert_eq!(
            router
                .oneshot(request(other_session, command, "Missing"))
                .await
                .expect("conflicting target")
                .status(),
            StatusCode::CONFLICT
        );
        pool.close().await;
    }
}
