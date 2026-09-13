//! Session actions through the same application commands as the local client.

use axum::{
    extract::{Path, Request, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use signalbox_application::{DecideToolRequestService, UuidV7ToolLoopIdGenerator};
use signalbox_domain::{
    DecideToolRequest, DecideToolRequestRejectedResult, DecideToolRequestResult, DurableCommandId,
    ToolApprovalDecision, ToolDenialReason, ToolRequestId,
};
use signalbox_persistence::{
    process_read::ProcessReadRepository,
    tool_loop::{PostgresToolLoopRepository, ToolLoopRepositoryError},
};
use signalbox_web_contract::{WebApprovalDecision, WebApprovalRequest};
use uuid::Uuid;

use super::{
    WebApiState, application_error, decode_bounded_json, parse_canonical_session_id,
    web_input_conflict,
};

fn command_id(value: &str) -> Option<DurableCommandId> {
    value
        .parse::<Uuid>()
        .ok()
        .filter(|id| id.to_string() == value && !id.is_nil() && !id.is_max())
        .map(DurableCommandId::from_uuid)
}

fn invalid_action() -> Response {
    application_error(
        StatusCode::BAD_REQUEST,
        "invalid_session_action",
        "the session action requires canonical identities and valid content",
    )
}

fn unconfirmed() -> Response {
    application_error(
        StatusCode::SERVICE_UNAVAILABLE,
        "action_outcome_unconfirmed",
        "action outcome is unconfirmed; retry the same command identity and payload",
    )
}

pub(super) async fn decide_approval(
    State(state): State<WebApiState>,
    Path((session_id, request_id)): Path<(String, String)>,
    request: Request,
) -> Response {
    let request = match decode_bounded_json::<WebApprovalRequest>(request).await {
        Ok(request) => request,
        Err(response) => return response,
    };
    let Ok(session) = parse_canonical_session_id(&session_id) else {
        return invalid_action();
    };
    let Some(command_id) = command_id(&request.command_id) else {
        return invalid_action();
    };
    let Some(tool_request) = command_id_from_request(&request_id) else {
        return invalid_action();
    };
    let decision = match (request.decision, request.note) {
        (WebApprovalDecision::Approve, None) => ToolApprovalDecision::Approve,
        (WebApprovalDecision::Approve, Some(_)) => {
            return application_error(
                StatusCode::BAD_REQUEST,
                "approval_note_unsupported",
                "approval does not carry a note; notes explain denials",
            );
        }
        (WebApprovalDecision::Deny, note) => {
            let Ok(reason) = note.map(ToolDenialReason::try_new).transpose() else {
                return invalid_action();
            };
            ToolApprovalDecision::Deny { reason }
        }
    };
    let Ok(command) = DecideToolRequest::try_new(command_id, tool_request, decision) else {
        return invalid_action();
    };
    let (Some(pool), Some(nudge)) = (state.pool, state.eligibility_nudge) else {
        return unconfirmed();
    };
    let repository = PostgresToolLoopRepository::new(pool.clone());
    let claimed = match repository.load_recorded_decision(command_id).await {
        Ok(Some(_)) | Err(ToolLoopRepositoryError::DifferentCommandKind) => true,
        Ok(None) => false,
        Err(_) => return unconfirmed(),
    };
    if !claimed {
        match ProcessReadRepository::new(pool)
            .tool_request_session(tool_request)
            .await
        {
            Ok(Some(owner)) if owner != session => {
                match repository.load_recorded_decision(command_id).await {
                    Ok(Some(_)) | Err(ToolLoopRepositoryError::DifferentCommandKind) => {}
                    Ok(None) => {
                        return application_error(
                            StatusCode::CONFLICT,
                            "tool_request_not_in_session",
                            "the pending request belongs to another session",
                        );
                    }
                    Err(_) => return unconfirmed(),
                }
            }
            Ok(_) => {}
            Err(_) => return unconfirmed(),
        }
    }
    match DecideToolRequestService::new(UuidV7ToolLoopIdGenerator, repository)
        .execute(command)
        .await
    {
        Ok(prepared) => match prepared.result() {
            DecideToolRequestResult::Applied(_) => {
                use signalbox_application::EligibilityNudge as _;
                let _ = nudge.nudge(session);
                StatusCode::NO_CONTENT.into_response()
            }
            DecideToolRequestResult::Rejected(reason) => {
                let (code, message) = match reason {
                    DecideToolRequestRejectedResult::AwaitingApprovalJudge { .. } => (
                        "awaiting_approval_judge",
                        "the approval judge must finish before a human decision",
                    ),
                    DecideToolRequestRejectedResult::RequestNotFound { .. } => (
                        "tool_request_not_found",
                        "the requested approval does not exist",
                    ),
                    DecideToolRequestRejectedResult::AlreadyResolved { .. } => (
                        "tool_request_already_resolved",
                        "the request already has a decision",
                    ),
                    DecideToolRequestRejectedResult::NotEarliestUndecided { .. } => (
                        "tool_request_not_earliest_undecided",
                        "decide the earlier pending request first",
                    ),
                };
                application_error(StatusCode::CONFLICT, code, message)
            }
        },
        Err(
            ToolLoopRepositoryError::DifferentCommandKind
            | ToolLoopRepositoryError::ConflictingCommandReuse,
        ) => web_input_conflict(),
        Err(_) => unconfirmed(),
    }
}

fn command_id_from_request(value: &str) -> Option<ToolRequestId> {
    command_id(value).map(|id| ToolRequestId::from_uuid(id.into_uuid()))
}

pub(super) async fn cancel_turn(
    State(mut state): State<WebApiState>,
    reload: Option<axum::Extension<crate::configuration_reload::ConfigurationReload>>,
    gate: Option<axum::Extension<signalbox_application::InProcessToolDispatchGate>>,
    Path(session_id): Path<String>,
    request: Request,
) -> Response {
    use signalbox_application::{
        EligibilityNudge as _, SubmitInputOutcome, SubmitInputRequest, SubmitInputService,
        UuidV7SubmitInputIdGenerator,
    };
    use signalbox_domain::{
        DeliveryRequest, DescendantTerminationScope, ModelSelectionOverride,
        PerInputConfigurationChoices, TurnId, UserContent,
    };
    use signalbox_persistence::{
        session::SessionRepository,
        submit_input::{SubmitInputRepository, SubmitInputRepositoryError},
    };
    let request =
        match decode_bounded_json::<signalbox_web_contract::WebCancelTurnRequest>(request).await {
            Ok(request) => request,
            Err(response) => return response,
        };
    let Ok(session) = parse_canonical_session_id(&session_id) else {
        return invalid_action();
    };
    let Some(identity) = command_id(&request.command_id) else {
        return invalid_action();
    };
    let Some(expected_turn) = command_id(&request.expected_active_turn_id) else {
        return invalid_action();
    };
    let expected_active_turn = TurnId::from_uuid(expected_turn.into_uuid());
    let Ok(content) = UserContent::try_text(request.message) else {
        return invalid_action();
    };
    if let Some(axum::Extension(reload)) = reload {
        state.model_configuration = Some(reload.catalogs().models);
    }
    let (Some(pool), Some(configuration), Some(nudge), Some(axum::Extension(gate))) = (
        state.pool,
        state.model_configuration,
        state.eligibility_nudge,
        gate,
    ) else {
        return unconfirmed();
    };
    let repository = SubmitInputRepository::with_model_capabilities(
        pool.clone(),
        configuration.model_capability_catalog(),
    )
    .with_stop_receipt();
    // Resolve defaults only for a new command; replay retains the original choices.
    match repository.load(identity).await {
        Ok(Some(recorded)) => {
            if recorded.command().session() != session
                || recorded.command().content() != &content
                || !matches!(recorded.command().delivery(), DeliveryRequest::Interrupt {
                    expected_active_turn: recorded_turn,
                    descendant_scope: DescendantTerminationScope::ParentAlone,
                    configuration,
                } if recorded_turn == expected_active_turn && configuration == PerInputConfigurationChoices::new(
                    configuration.expected_session_defaults_version(), ModelSelectionOverride::UseSessionDefault,
                ))
            {
                return web_input_conflict();
            }
            if matches!(
                recorded.result(),
                signalbox_domain::SubmitInputResult::Applied(_)
            ) {
                let _ = nudge.nudge(session);
            }
            return super::web_input_result(recorded.result());
        }
        Ok(None) => {}
        Err(SubmitInputRepositoryError::DifferentCommandKind { .. }) => {
            return web_input_conflict();
        }
        Err(_) => return unconfirmed(),
    }
    let current = match SessionRepository::new(pool).load_session(session).await {
        Ok(Some(current)) => current,
        Ok(None) => {
            return application_error(
                StatusCode::NOT_FOUND,
                "session_not_found",
                "the requested session does not exist",
            );
        }
        Err(_) => return unconfirmed(),
    };
    let delivery = DeliveryRequest::Interrupt {
        expected_active_turn,
        descendant_scope: DescendantTerminationScope::ParentAlone,
        configuration: PerInputConfigurationChoices::new(
            current.current_configuration_defaults().version(),
            ModelSelectionOverride::UseSessionDefault,
        ),
    };
    let maximum = configuration
        .numeric_bounds()
        .integer("max_message_utf8_bytes")
        .flatten()
        .and_then(|value| usize::try_from(value).ok());
    let Ok(request) = SubmitInputRequest::try_new_with_content_limit(
        identity, session, content, delivery, maximum,
    ) else {
        return invalid_action();
    };
    let mut service = SubmitInputService::new(
        UuidV7SubmitInputIdGenerator,
        crate::process_runtime::ConfiguredSubmitInputTransaction {
            repository,
            model_configuration: &configuration,
            principal: signalbox_domain::CommandPrincipal::Operator,
            cascade_root_kind: signalbox_domain::ParentTerminationKind::Cancelled,
        },
        nudge,
        gate,
    );
    match service.execute(request).await {
        Ok(SubmitInputOutcome::Recorded(result)) => super::web_input_result(&result),
        // A concurrent invocation may have resolved different defaults; replay reads the winner.
        Ok(SubmitInputOutcome::ConflictingReuse { .. }) => unconfirmed(),
        Err(SubmitInputRepositoryError::UnsupportedModelSetting(_)) => application_error(
            StatusCode::CONFLICT,
            "unsupported_model_setting",
            "the selected model does not support an explicitly requested setting",
        ),
        Err(_) => unconfirmed(),
    }
}

pub(super) async fn set_goal(
    State(state): State<WebApiState>,
    reload: Option<axum::Extension<crate::configuration_reload::ConfigurationReload>>,
    Path(session_id): Path<String>,
    request: Request,
) -> Response {
    let request = match decode_bounded_json::<signalbox_web_contract::WebGoalRequest>(request).await
    {
        Ok(request) => request,
        Err(response) => return response,
    };
    let Ok(statement) = signalbox_domain::GoalStatement::try_new(request.statement) else {
        return invalid_action();
    };
    let configuration = reload
        .map(|axum::Extension(reload)| reload.catalogs().models.clone())
        .or_else(|| state.model_configuration.clone());
    goal_action(
        state,
        configuration,
        session_id,
        request.command_id,
        signalbox_domain::GoalUserAction::Attach(statement),
        None,
    )
    .await
}

pub(super) async fn clear_goal(
    State(state): State<WebApiState>,
    reload: Option<axum::Extension<crate::configuration_reload::ConfigurationReload>>,
    gate: Option<axum::Extension<signalbox_application::InProcessToolDispatchGate>>,
    Path(session_id): Path<String>,
    request: Request,
) -> Response {
    let request =
        match decode_bounded_json::<signalbox_web_contract::WebSessionActionRequest>(request).await
        {
            Ok(request) => request,
            Err(response) => return response,
        };
    let Some(axum::Extension(gate)) = gate else {
        return unconfirmed();
    };
    let configuration = reload
        .map(|axum::Extension(reload)| reload.catalogs().models.clone())
        .or_else(|| state.model_configuration.clone());
    goal_action(
        state,
        configuration,
        session_id,
        request.command_id,
        signalbox_domain::GoalUserAction::Stop {
            descendant_scope: signalbox_domain::DescendantTerminationScope::ParentAlone,
        },
        Some(gate),
    )
    .await
}

async fn goal_action(
    state: WebApiState,
    configuration: Option<std::sync::Arc<crate::HubModelConfiguration>>,
    session_id: String,
    identity: String,
    action: signalbox_domain::GoalUserAction,
    gate: Option<signalbox_application::InProcessToolDispatchGate>,
) -> Response {
    use signalbox_domain::{
        AcceptedInputId, GoalCommandRejection as Rejected, GoalCommandResult, GoalUserCommand,
        TurnId,
    };
    use signalbox_persistence::{
        goal::{GoalCommandHandlingOutcome as Outcome, GoalRepository, GoalRepositoryError},
        goal_turn::GoalTurnCandidates,
    };
    let Ok(session) = parse_canonical_session_id(&session_id) else {
        return invalid_action();
    };
    let Some(identity) = command_id(&identity) else {
        return invalid_action();
    };
    let (Some(pool), Some(configuration), Some(nudge)) =
        (state.pool, configuration, state.eligibility_nudge)
    else {
        return unconfirmed();
    };
    let candidates = action.starts_pursuit().then(|| {
        GoalTurnCandidates::new(
            AcceptedInputId::from_uuid(Uuid::now_v7()),
            TurnId::from_uuid(Uuid::now_v7()),
        )
    });
    let repository = GoalRepository::new(pool);
    let repository = match gate {
        Some(gate) => repository.with_tool_dispatch_gate(gate),
        None => repository,
    };
    match repository
        .handle_user_command(
            GoalUserCommand::new(identity, session, action),
            candidates,
            |alias| configuration.resolve_alias(alias),
        )
        .await
    {
        Ok(Outcome::Recorded(GoalCommandResult::Applied(_))) => {
            use signalbox_application::EligibilityNudge as _;
            let _ = nudge.nudge(session);
            StatusCode::NO_CONTENT.into_response()
        }
        Ok(Outcome::Recorded(GoalCommandResult::Rejected(reason))) => {
            let (code, message) = match reason {
                Rejected::SessionNotFound => {
                    ("session_not_found", "the requested session does not exist")
                }
                Rejected::SessionClosing => ("session_closing", "the session is closing"),
                Rejected::GoalAlreadyAttached => (
                    "goal_already_attached",
                    "clear the current goal before setting another",
                ),
                Rejected::GoalNotAttached => ("goal_not_attached", "the session has no goal"),
                Rejected::UnknownModelAlias => {
                    ("unknown_model_alias", "the session model is unavailable")
                }
                Rejected::AcceptancePositionExhausted => (
                    "acceptance_position_exhausted",
                    "the session cannot accept more input",
                ),
                Rejected::RequiresBlocked => ("goal_requires_blocked", "the goal must be blocked"),
                Rejected::RequiresPursuingOrBlocked => (
                    "goal_requires_pursuing_or_blocked",
                    "the goal is already finished",
                ),
                Rejected::GenerationExhausted => (
                    "goal_generation_exhausted",
                    "the session cannot accept another goal",
                ),
                Rejected::EventOrdinalExhausted => (
                    "goal_event_ordinal_exhausted",
                    "the goal cannot accept another event",
                ),
            };
            application_error(StatusCode::CONFLICT, code, message)
        }
        Ok(Outcome::StopAwaitingApproval { .. }) => application_error(
            StatusCode::CONFLICT,
            "goal_stop_awaiting_approval",
            "deny the pending approval request before clearing the goal",
        ),
        Ok(Outcome::TargetBusy { .. }) => application_error(
            StatusCode::CONFLICT,
            "commission_target_busy",
            "another session is working on this pull request",
        ),
        Ok(Outcome::ConflictingReuse { .. })
        | Err(GoalRepositoryError::DifferentCommandKind { .. }) => web_input_conflict(),
        Ok(Outcome::LineageMoved) | Err(_) => unconfirmed(),
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "HTTP fixtures state their expected responses"
)]
mod tests {
    use super::*;
    use axum::body::{Body, to_bytes};
    use tower::ServiceExt as _;

    // Arbitrary canonical identities; these admission tests do not access stored sessions.
    const SESSION: &str = "10000000-0000-4000-8000-000000000001";
    const REQUEST: &str = "20000000-0000-4000-8000-000000000001";
    const COMMAND: &str = "30000000-0000-4000-8000-000000000001";

    fn router() -> axum::Router {
        super::super::production_router(None, None, None, None, None, None, None)
    }

    async fn error_code(response: Response) -> String {
        let bytes = to_bytes(
            response.into_body(),
            signalbox_web_contract::MAX_JSON_BODY_BYTES,
        )
        .await
        .expect("response body");
        let body: signalbox_web_contract::WebApiErrorResponse =
            serde_json::from_slice(&bytes).expect("typed error");
        body.error.code
    }

    #[tokio::test]
    async fn session_actions_reject_cross_origin_before_reading_commands() {
        for (method, path) in [
            (
                "POST",
                format!("/api/sessions/{SESSION}/approvals/{REQUEST}"),
            ),
            ("POST", format!("/api/sessions/{SESSION}/cancel")),
            ("PUT", format!("/api/sessions/{SESSION}/goal")),
            ("DELETE", format!("/api/sessions/{SESSION}/goal")),
        ] {
            let response = router()
                .oneshot(
                    Request::builder()
                        .method(method)
                        .uri(path)
                        .header("host", "localhost")
                        .header("origin", "https://outside.example")
                        .header("content-type", "application/json")
                        .body(Body::from("{}"))
                        .expect("request"),
                )
                .await
                .expect("response");
            assert_eq!(response.status(), StatusCode::FORBIDDEN, "{method}");
            assert_eq!(
                error_code(response).await,
                "cross_origin_mutation_rejected",
                "{method}"
            );
        }
    }

    #[tokio::test]
    async fn session_actions_require_json_content_type() {
        for (method, path) in [
            (
                "POST",
                format!("/api/sessions/{SESSION}/approvals/{REQUEST}"),
            ),
            ("POST", format!("/api/sessions/{SESSION}/cancel")),
            ("PUT", format!("/api/sessions/{SESSION}/goal")),
            ("DELETE", format!("/api/sessions/{SESSION}/goal")),
        ] {
            let response = router()
                .oneshot(
                    Request::builder()
                        .method(method)
                        .uri(path)
                        .header("host", "localhost")
                        .body(Body::from("{}"))
                        .expect("request"),
                )
                .await
                .expect("response");
            assert_eq!(
                response.status(),
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "{method}"
            );
            assert_eq!(
                error_code(response).await,
                "json_content_type_required",
                "{method}"
            );
        }
    }

    #[tokio::test]
    async fn approval_refuses_invalid_denial_notes_before_mutation() {
        let response = router().oneshot(Request::post(format!("/api/sessions/{SESSION}/approvals/{REQUEST}"))
            .header("host", "localhost").header("content-type", "application/json")
            .body(Body::from(serde_json::json!({"command_id": COMMAND, "decision": "deny", "note": " padded "}).to_string()))
            .expect("request")).await.expect("response");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(error_code(response).await, "invalid_session_action");
    }

    #[tokio::test]
    async fn approval_refuses_unknown_fields() {
        let response = router().oneshot(Request::post(format!("/api/sessions/{SESSION}/approvals/{REQUEST}"))
            .header("host", "localhost").header("content-type", "application/json")
            .body(Body::from(serde_json::json!({"command_id": COMMAND, "decision": "approve", "bypass_judge": true}).to_string()))
            .expect("request")).await.expect("response");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(error_code(response).await, "invalid_json");
    }

    #[cfg(feature = "test-support")]
    struct EmptySweep;

    #[cfg(feature = "test-support")]
    impl signalbox_application::EligibilitySweep for EmptySweep {
        type Error = std::convert::Infallible;
        async fn find_sessions(
            &mut self,
        ) -> Result<signalbox_application::EligibilitySweepBatch, Self::Error> {
            Ok(signalbox_application::EligibilitySweepBatch::new(
                Vec::new(),
                false,
            ))
        }
    }

    #[cfg(feature = "test-support")]
    fn database_router(pool: sqlx::PgPool) -> axum::Router {
        let models =
            crate::HubModelConfiguration::parse(crate::configuration::tests::CONFIGURATION)
                .expect("models");
        let (nudge, _work) = signalbox_application::InProcessEligibilityWorkSource::with_options(
            EmptySweep, None, None,
        );
        super::super::production_router(
            None,
            Some(pool),
            None,
            Some(models),
            None,
            None,
            Some(nudge),
        )
        .layer(axum::Extension(
            signalbox_application::InProcessToolDispatchGate::default(),
        ))
    }

    #[cfg(feature = "test-support")]
    fn json_request(method: &str, path: &str, payload: serde_json::Value) -> Request {
        Request::builder()
            .method(method)
            .uri(path)
            .header("host", "localhost")
            .header("content-type", "application/json")
            .body(Body::from(payload.to_string()))
            .expect("request")
    }

    #[cfg(feature = "test-support")]
    #[tokio::test]
    #[ignore = "requires ephemeral PostgreSQL"]
    async fn missing_approval_replays_its_recorded_refusal() {
        let (_container, pool, _) =
            signalbox_persistence::test_support::postgres::migrated_postgres(8)
                .await
                .expect("PostgreSQL");
        let router = database_router(pool.clone());
        let path = format!("/api/sessions/{SESSION}/approvals/{REQUEST}");
        let payload =
            serde_json::json!({"command_id": COMMAND, "decision": "deny", "note": "Not requested"});
        let first = router
            .clone()
            .oneshot(json_request("POST", &path, payload.clone()))
            .await
            .expect("first response");
        assert_eq!(first.status(), StatusCode::CONFLICT);
        assert_eq!(error_code(first).await, "tool_request_not_found");
        let replay = router
            .clone()
            .oneshot(json_request("POST", &path, payload))
            .await
            .expect("replay response");
        assert_eq!(replay.status(), StatusCode::CONFLICT);
        assert_eq!(error_code(replay).await, "tool_request_not_found");
        let conflict = router
            .oneshot(json_request(
                "POST",
                &path,
                serde_json::json!({"command_id": COMMAND, "decision": "approve"}),
            ))
            .await
            .expect("conflict response");
        assert_eq!(conflict.status(), StatusCode::CONFLICT);
        assert_eq!(error_code(conflict).await, "conflicting_command_reuse");
        pool.close().await;
    }

    #[cfg(feature = "test-support")]
    #[tokio::test]
    #[ignore = "requires ephemeral PostgreSQL"]
    async fn goal_routes_retain_the_canonical_command_refusal() {
        let (_container, pool, _) =
            signalbox_persistence::test_support::postgres::migrated_postgres(8)
                .await
                .expect("PostgreSQL");
        let router = database_router(pool.clone());
        let path = format!("/api/sessions/{SESSION}/goal");
        let payload = serde_json::json!({"command_id": COMMAND, "statement": "Inspect the change"});
        let first = router
            .clone()
            .oneshot(json_request("PUT", &path, payload.clone()))
            .await
            .expect("first response");
        assert_eq!(first.status(), StatusCode::CONFLICT);
        assert_eq!(error_code(first).await, "session_not_found");
        let replay = router
            .clone()
            .oneshot(json_request("PUT", &path, payload))
            .await
            .expect("replay response");
        assert_eq!(replay.status(), StatusCode::CONFLICT);
        assert_eq!(error_code(replay).await, "session_not_found");
        let conflict = router
            .oneshot(json_request(
                "DELETE",
                &path,
                serde_json::json!({"command_id": COMMAND}),
            ))
            .await
            .expect("conflict response");
        assert_eq!(conflict.status(), StatusCode::CONFLICT);
        assert_eq!(error_code(conflict).await, "conflicting_command_reuse");
        pool.close().await;
    }

    #[cfg(feature = "test-support")]
    async fn create_session(pool: &sqlx::PgPool) -> signalbox_domain::SessionId {
        use signalbox_domain::{
            CreateSession, DirectModelSelection, ModelSelectionRequest,
            SessionConfigurationDefaults, SessionCreationCause, SessionCreationProvenance,
            SessionId, TranscriptAncestry,
        };
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
        .expect("session created");
        session
    }

    #[cfg(feature = "test-support")]
    #[tokio::test]
    #[ignore = "requires ephemeral PostgreSQL"]
    async fn goal_replay_does_not_restart_a_stopped_goal() {
        let (_container, pool, _) =
            signalbox_persistence::test_support::postgres::migrated_postgres(8)
                .await
                .expect("PostgreSQL");
        let session = create_session(&pool).await;
        let router = database_router(pool.clone());
        let path = format!("/api/sessions/{}/goal", session.into_uuid());
        let payload = serde_json::json!({"command_id": COMMAND, "statement": "Inspect the change"});
        let attach = router
            .clone()
            .oneshot(json_request("PUT", &path, payload.clone()))
            .await
            .expect("attach response");
        assert_eq!(attach.status(), StatusCode::NO_CONTENT);
        let stop = router
            .clone()
            .oneshot(json_request(
                "DELETE",
                &path,
                serde_json::json!({"command_id": Uuid::now_v7().to_string()}),
            ))
            .await
            .expect("stop response");
        assert_eq!(stop.status(), StatusCode::NO_CONTENT);
        let replay = router
            .oneshot(json_request("PUT", &path, payload))
            .await
            .expect("replay response");
        assert_eq!(replay.status(), StatusCode::NO_CONTENT);
        let goal = signalbox_persistence::goal::GoalRepository::new(pool.clone())
            .load_goal(session)
            .await
            .expect("goal read")
            .expect("goal lineage");
        assert_eq!(
            goal.current().state(),
            &signalbox_domain::GoalState::UserStopped
        );
        assert_eq!(goal.events().len(), 2);
        pool.close().await;
    }
    #[cfg(feature = "test-support")]
    #[tokio::test]
    #[ignore = "requires ephemeral PostgreSQL"]
    async fn cancel_accepts_one_successor_and_replay_does_not_stop_it() {
        use signalbox_application::{
            StartEligibleTurnOutcome, StartEligibleTurnService, UuidV7StartEligibleTurnIdGenerator,
        };
        use signalbox_persistence::start_eligible_turn::StartEligibleTurnRepository;
        let (_container, pool, _) =
            signalbox_persistence::test_support::postgres::migrated_postgres(8)
                .await
                .expect("PostgreSQL");
        let session = create_session(&pool).await;
        let router = database_router(pool.clone());
        let path = format!("/api/sessions/{}/cancel", session.into_uuid());
        let initial = router.clone().oneshot(json_request("POST", &format!("/api/sessions/{}/input", session.into_uuid()),
            serde_json::json!({"command_id": Uuid::now_v7().to_string(), "message": "Review the patch"}))).await.expect("input");
        assert_eq!(initial.status(), StatusCode::NO_CONTENT);
        let mut activation = StartEligibleTurnService::new(
            UuidV7StartEligibleTurnIdGenerator,
            StartEligibleTurnRepository::new(pool.clone()),
        );
        let StartEligibleTurnOutcome::Activated(first) =
            activation.execute(session).await.expect("activation")
        else {
            panic!("initial turn must activate")
        };
        crate::WorkspaceInstructionRuntime::new(pool.clone(), None, Vec::new())
            .prepare(session, first.turn())
            .await
            .expect("instructions");
        let payload = serde_json::json!({"command_id": COMMAND, "expected_active_turn_id": first.turn().into_uuid().to_string(), "message": "Focus on the parser"});
        let response = router
            .clone()
            .oneshot(json_request("POST", &path, payload.clone()))
            .await
            .expect("stop");
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        let StartEligibleTurnOutcome::Activated(successor) = activation
            .execute(session)
            .await
            .expect("successor activation")
        else {
            panic!("successor must activate")
        };
        assert_ne!(successor.turn(), first.turn());
        let replay = router
            .clone()
            .oneshot(json_request("POST", &path, payload.clone()))
            .await
            .expect("replay");
        assert_eq!(replay.status(), StatusCode::NO_CONTENT);
        let mut changed = payload.clone();
        changed["message"] = serde_json::json!("Different message");
        let conflict = router
            .clone()
            .oneshot(json_request("POST", &path, changed))
            .await
            .expect("conflict");
        assert_eq!(error_code(conflict).await, "conflicting_command_reuse");
        let mut stale = payload;
        stale["command_id"] = serde_json::json!(Uuid::now_v7().to_string());
        let refused = router
            .oneshot(json_request("POST", &path, stale))
            .await
            .expect("stale stop");
        assert_eq!(error_code(refused).await, "active_turn_mismatch");
        let repository =
            signalbox_persistence::submit_input::SubmitInputRepository::new(pool.clone());
        assert!(
            repository
                .is_recorded_stop(command_id(COMMAND).expect("command"))
                .await
                .expect("stop receipt")
        );
        let turns: Vec<(Uuid, String)> = sqlx::query_as(
            "SELECT turn_id, state_kind FROM turn_lifecycle WHERE session_id = $1 ORDER BY turn_id",
        )
        .bind(session.into_uuid())
        .fetch_all(&pool)
        .await
        .expect("turns");
        assert_eq!(turns.len(), 2);
        assert!(
            turns
                .iter()
                .any(|(id, state)| *id == successor.turn().into_uuid() && state == "active")
        );
        pool.close().await;
    }
}
