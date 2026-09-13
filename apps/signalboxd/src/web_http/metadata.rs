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

pub(super) async fn suggest_title(
    State(state): State<WebApiState>,
    reload: Option<axum::Extension<crate::configuration_reload::ConfigurationReload>>,
    tasks: Option<axum::Extension<tokio::sync::mpsc::Sender<super::SessionTitleTask>>>,
    Path(session_id): Path<String>,
    request: Request,
) -> Response {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct SuggestRequest {}
    if let Err(response) = decode_bounded_json::<SuggestRequest>(request).await {
        return response;
    }
    let Ok(session) = parse_canonical_session_id(&session_id) else {
        return invalid_title();
    };
    let service = state
        .pool
        .and_then(|pool| reload.and_then(|reload| reload.0.session_titles(pool)));
    let (Some(service), Some(axum::Extension(tasks))) = (service, tasks) else {
        return application_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "session_titles_unavailable",
            "Session name suggestions are not configured.",
        );
    };
    match await_title_generation(tasks, async move { service.generate(session, None).await }).await
    {
        Ok(Some(title)) => {
            axum::Json(signalbox_web_contract::WebSessionTitleSuggestion { title }).into_response()
        }
        Ok(None) | Err(crate::session_titles::TitleError::NotFound) => application_error(
            StatusCode::NOT_FOUND,
            "session_not_found",
            "The session does not exist.",
        ),
        Err(error) => {
            tracing::warn!(session_id = %session.into_uuid(), ?error, "session title suggestion failed");
            application_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "session_title_generation_failed",
                "A name could not be suggested. Try again.",
            )
        }
    }
}

async fn await_title_generation(
    tasks: tokio::sync::mpsc::Sender<super::SessionTitleTask>,
    generation: impl std::future::Future<
        Output = Result<Option<String>, crate::session_titles::TitleError>,
    > + Send
    + 'static,
) -> Result<Option<String>, crate::session_titles::TitleError> {
    let (completed, result) = tokio::sync::oneshot::channel();
    tasks
        .send(Box::pin(async move {
            let _ = completed.send(generation.await);
        }))
        .await
        .map_err(|_| crate::session_titles::TitleError::Generation)?;
    result
        .await
        .unwrap_or(Err(crate::session_titles::TitleError::Generation))
}

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
    async fn runtime_abort_drains_a_suggestion_after_its_request_is_cancelled() {
        let (tasks, mut pending) = tokio::sync::mpsc::channel(1);
        let (started, ready) = tokio::sync::oneshot::channel();
        let (held, mut ended) = tokio::sync::oneshot::channel::<()>();
        let request = tokio::spawn(await_title_generation(tasks, async move {
            started.send(()).expect("fixture observes execution");
            std::future::pending::<()>().await;
            drop(held);
            Ok(None)
        }));
        let mut runtime_tasks = tokio::task::JoinSet::new();
        runtime_tasks.spawn(pending.recv().await.expect("runtime receives title task"));
        ready.await.expect("generation started");
        request.abort();
        assert!(request.await.expect_err("request cancelled").is_cancelled());
        assert!(matches!(
            ended.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));
        runtime_tasks.abort_all();
        while runtime_tasks.join_next().await.is_some() {}
        assert!(
            ended.await.is_err(),
            "runtime drain drops the generation future"
        );
    }

    #[tokio::test]
    #[ignore = "requires ephemeral PostgreSQL"]
    async fn cancelled_suggestion_request_still_settles_its_call_and_releases_capacity()
    -> Result<(), Box<dyn std::error::Error>> {
        use signalbox_application::UsageTokenAxes;
        use signalbox_domain::{ModelCallId, ProviderModelIdentity, ResolvedProviderTarget};
        use signalbox_persistence::{
            credential_invocations,
            session_titles::{
                PrepareSessionTitleOutcome, SessionTitleCall, SessionTitleRepository,
            },
        };
        let (_database, pool, _) =
            signalbox_persistence::test_support::postgres::migrated_postgres(4).await?;
        let models =
            crate::HubModelConfiguration::parse(crate::configuration::tests::CONFIGURATION)?;
        let session = SessionId::from_uuid(Uuid::now_v7());
        let selection =
            DirectModelSelection::from_uuid(uuid::uuid!("10000000-0000-4000-8000-000000000001"));
        let creation = CreateSession::new(
            DurableCommandId::from_uuid(Uuid::now_v7()),
            SessionCreationProvenance::new(
                SessionCreationCause::Interactive,
                TranscriptAncestry::None,
            ),
            SessionConfigurationDefaults::new(ModelSelectionRequest::Direct(selection)),
        )
        .prepare(session)
        .map_err(|_| "session creation rejected")?;
        signalbox_persistence::create_session::CreateSessionRepository::new(
            pool.clone(),
            models.session_credential_pin(),
        )
        .handle(creation)
        .await?;
        let profile = "codex-cancelled-title-fixture";
        credential_invocations::replace_registrations(
            &pool,
            &[(profile.to_owned(), std::num::NonZeroU32::new(1))],
        )
        .await?;
        let mut call = SessionTitleCall {
            call: ModelCallId::from_uuid(Uuid::now_v7()),
            session,
            selection,
            target: ResolvedProviderTarget::naming(
                ProviderModelIdentity::from_uuid(Uuid::now_v7()),
            ),
            credential_reference: profile.to_owned(),
            input_includes_cache_tokens: false,
            initial_for_turn: None,
        };
        let repository = SessionTitleRepository::new(pool.clone());
        let (prepared_tx, prepared_rx) = tokio::sync::oneshot::channel();
        let (resume_tx, resume_rx) = tokio::sync::oneshot::channel();
        let (settled_tx, settled_rx) = tokio::sync::oneshot::channel();
        let mut generation_call = call.clone();
        let generation_repository = repository.clone();
        let (tasks, mut pending) = tokio::sync::mpsc::channel(1);
        let mut runtime_tasks = tokio::task::JoinSet::new();
        let request = tokio::spawn(await_title_generation(tasks, async move {
            assert!(
                generation_repository
                    .prepare(&mut generation_call, &Default::default())
                    .await
                    .expect("prepare")
                    == PrepareSessionTitleOutcome::Prepared
            );
            generation_repository
                .authorize(generation_call.call)
                .await
                .expect("authorize");
            prepared_tx.send(()).expect("request observes preparation");
            resume_rx
                .await
                .expect("fixture releases the provider response");
            let title = generation_repository
                .finish_generated(
                    DurableCommandId::from_uuid(Uuid::now_v7()),
                    generation_call.call,
                    Some("Database indexing work".to_owned()),
                    UsageTokenAxes {
                        input: Some(17),
                        output: Some(5),
                        cache_creation_input: None,
                        cache_read_input: None,
                    },
                )
                .await
                .expect("settlement");
            settled_tx.send(()).expect("fixture observes settlement");
            Ok(title)
        }));
        runtime_tasks.spawn(pending.recv().await.expect("runtime receives title task"));
        prepared_rx.await?;
        request.abort();
        assert!(
            request
                .await
                .expect_err("request was cancelled")
                .is_cancelled()
        );
        resume_tx.send(()).expect("generation outlives its request");
        settled_rx.await?;
        runtime_tasks
            .join_next()
            .await
            .expect("title task")
            .expect("title completes");
        let completed: (String, Option<String>, bool) = sqlx::query_as(
            "SELECT call.state_kind, call.title, reservation.released_at IS NOT NULL
             FROM session_title_model_call call JOIN credential_invocation_reservation reservation USING (model_call_id)
             WHERE model_call_id = $1").bind(call.call.into_uuid()).fetch_one(&pool).await?;
        assert_eq!(
            completed,
            (
                "terminal".to_owned(),
                Some("Database indexing work".to_owned()),
                true
            )
        );
        call.call = ModelCallId::from_uuid(Uuid::now_v7());
        assert!(
            repository.prepare(&mut call, &Default::default()).await?
                == PrepareSessionTitleOutcome::Prepared,
            "the next invocation can use capacity"
        );
        repository.abandon(call.call).await?;
        pool.close().await;
        Ok(())
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
