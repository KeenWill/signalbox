use super::{
    Deserialize, IntoResponse, Json, OutboxDispatchError, Path, ProviderModelCallFailureCause,
    Query, QueryRejection, Response, SessionId, SessionTimelineDescriptor,
    SessionTimelineDetailBody, SessionTimelineDetailPage, SessionTimelineEventKind,
    SessionTimelineRepositoryError, SessionTimelineWindow, State, StatusCode, TimelineAddress,
    TimelineBodyContinuation, TimelineBodyField, TimelineContinuation, TimelineDetailContinuation,
    TimelineDetailCursor, TimelineDetailLimits, TimelineModelCallDisposition,
    TimelineModelCallState, TimelineTextExcerpt, TimelineTurnLifecycleKind, TimelineWindowAnchor,
    TimelineWindowLimits, TurnId, WebApiState, WebBlobId, WebProviderModelCallFailureCause,
    WebSessionId, WebSessionTimelineDescriptor, WebSessionTimelineDetail,
    WebSessionTimelineDetailBody, WebSessionTimelineDetailPage, WebSessionTimelineEventKind,
    WebSessionTimelineItem, WebSessionTimelineSizeFacts, WebSessionTimelineWindow,
    WebSessionWorkFacts, WebTimelineAddress, WebTimelineBlobReference, WebTimelineBodyContinuation,
    WebTimelineBodyField, WebTimelineDetailContinuation, WebTimelineEventSequence,
    WebTimelineModelCallDisposition, WebTimelineModelCallState, WebTimelineModelUsage,
    WebTimelineTextExcerpt, WebTimelineTurnLifecycleKind, WebU64, application_error,
};
use signalbox_application::{
    TimelineApprovalActor, TimelineApprovalDecision, TimelineBoundChildAction,
    TimelineDelegationDetail, TimelineDelegationOutcome, TimelineDelegationPolicy,
    TimelineDelegationProvenance, TimelineDelegationReason, TimelineDelegationWaitMode,
    TimelineGoalBlockedReason, TimelineGoalEvent, TimelineModelSettingsDetail,
    TimelineReconciliationOperation, TimelineRunnerSandboxPosture, TimelineRunnerState,
    TimelineToolApprovalPosture, TimelineToolAttempt, TimelineToolBatchState,
    TimelineToolEffectPosture, TimelineToolSandboxPosture, TimelineToolState,
};
use signalbox_application::{
    TimelineOwnershipTransition, TimelineSessionOutcome, TimelineSessionState,
};
use signalbox_domain::ImportedSessionRelationship;
use signalbox_web_contract::{
    WebPositiveU64, WebTimelineApprovalActor, WebTimelineApprovalDecision,
    WebTimelineBoundChildAction, WebTimelineDelegationDetail, WebTimelineDelegationOutcome,
    WebTimelineDelegationPolicy, WebTimelineDelegationProvenance, WebTimelineDelegationReason,
    WebTimelineDelegationWaitMode, WebTimelineEffectiveModelSettings, WebTimelineFastMode,
    WebTimelineFastModeOverlay, WebTimelineGoalBlockedReason, WebTimelineGoalEvent,
    WebTimelineImportedEvidence, WebTimelineImportedRelationship, WebTimelineModelChangeAdjustment,
    WebTimelineModelSelection, WebTimelineModelSettingSource, WebTimelineModelSettingsDetail,
    WebTimelineModelSettingsOverlay, WebTimelineModelSettingsPrecedence,
    WebTimelineModelSettingsSnapshot, WebTimelineReasoningLevel,
    WebTimelineReconciliationOperation, WebTimelineRunnerSandboxPosture, WebTimelineRunnerState,
    WebTimelineServiceTier, WebTimelineSettingOverlay, WebTimelineToolApprovalPosture,
    WebTimelineToolAttempt, WebTimelineToolAttemptEvidence, WebTimelineToolBatchState,
    WebTimelineToolEffectPosture, WebTimelineToolFailureCause, WebTimelineToolSandboxPosture,
    WebTimelineToolState, WebToolName,
};
use signalbox_web_contract::{
    WebTimelineOwnershipTransition, WebTimelineSessionOutcome, WebTimelineSessionState,
};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct TimelineWindowQuery {
    anchor: String,
    address: Option<String>,
    max_items: Option<String>,
    max_bytes: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct TimelineDetailQuery {
    pub(super) max_items: Option<String>,
    pub(super) max_bytes: Option<String>,
    pub(super) cursor_address: Option<String>,
    pub(super) cursor_field: Option<String>,
    pub(super) cursor_member: Option<String>,
    pub(super) cursor_offset: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct TimelineRegionDetailQuery {
    first: Option<String>,
    through: Option<String>,
    max_items: Option<String>,
    max_bytes: Option<String>,
    cursor_address: Option<String>,
    cursor_field: Option<String>,
    cursor_member: Option<String>,
    cursor_offset: Option<String>,
}

#[derive(Clone, Copy, Debug)]
pub(super) enum SessionTimelineRequestError {
    InvalidSessionId,
    InvalidAddress,
    InvalidAnchor,
    MissingBounds,
    InvalidProjectedSessionId,
    InvalidProjectedToolAttempt,
    InvalidProjectedOrdinal,
}

impl SessionTimelineRequestError {
    pub(super) fn into_response(self) -> Response {
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
            Self::InvalidProjectedSessionId
            | Self::InvalidProjectedToolAttempt
            | Self::InvalidProjectedOrdinal => {
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

pub(super) async fn session_descriptor(
    reload: Option<axum::Extension<crate::configuration_reload::ConfigurationReload>>,
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
            Ok(mut descriptor) => {
                let origin = match reload {
                    Some(axum::Extension(reload)) => {
                        match reload.repository_watch_origin(session).await {
                            Ok(origin) => origin,
                            Err(_) => {
                                return application_error(
                                    StatusCode::SERVICE_UNAVAILABLE,
                                    "session_projection_unavailable",
                                    "repository-watch origin could not be read",
                                );
                            }
                        }
                    }
                    None => None,
                };
                descriptor.repository_watch = origin.map(repository_watch_origin_dto);
                Json(descriptor).into_response()
            }
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

pub(super) async fn session_timeline_window(
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

pub(super) async fn session_timeline_item_detail(
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

pub(super) async fn session_timeline_turn_detail(
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

pub(super) async fn session_timeline_region_detail(
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

pub(super) fn parse_detail_query(
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
        "delegation_content" => Ok(TimelineBodyField::DelegationContent),
        "compaction_summary" => Ok(TimelineBodyField::CompactionSummary),
        "goal_text" => Ok(TimelineBodyField::GoalText),
        "approval_rationale" => Ok(TimelineBodyField::ApprovalRationale),
        "tool_failure" => Ok(TimelineBodyField::ToolFailure),
        "tool_result" => Ok(TimelineBodyField::ToolResult),
        "tool_arguments" => Ok(TimelineBodyField::ToolArguments),
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

pub(super) fn parse_session_id(value: &str) -> Result<SessionId, SessionTimelineRequestError> {
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

pub(super) fn address_dto(address: TimelineAddress) -> WebTimelineAddress {
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
        supervision: descriptor.supervision.map(|facts| {
            use signalbox_application::OperatorFailureClass;
            use signalbox_web_contract::{WebSessionSupervision, WebSessionSupervisionClass};

            WebSessionSupervision {
                class: match facts.class {
                    OperatorFailureClass::Infrastructure {
                        commit_ambiguous: false,
                    } => WebSessionSupervisionClass::Infrastructure,
                    OperatorFailureClass::Infrastructure {
                        commit_ambiguous: true,
                    } => WebSessionSupervisionClass::CommitAmbiguous,
                    OperatorFailureClass::FailClosedCorruption => {
                        WebSessionSupervisionClass::Corruption
                    }
                    OperatorFailureClass::IdentityCollision => {
                        WebSessionSupervisionClass::IdentityCollision
                    }
                    OperatorFailureClass::CallerOrHubBug => WebSessionSupervisionClass::Bug,
                },
                cause_code: facts.cause_code,
                pending: facts.pending,
            }
        }),
        workspace_root_kind: descriptor.workspace_root_kind.map(workspace_root_kind_dto),
        repository_watch: None,
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
        SessionTimelineDetailBody::SessionState { state } => {
            WebSessionTimelineDetailBody::SessionState {
                state: session_state_detail_dto(state),
            }
        }
        SessionTimelineDetailBody::SessionTerminal { outcome } => {
            WebSessionTimelineDetailBody::SessionTerminal {
                outcome: session_outcome_detail_dto(outcome),
            }
        }
        SessionTimelineDetailBody::CommandSettlement {
            command_id,
            rejection,
        } => WebSessionTimelineDetailBody::CommandSettlement {
            command_id: web_uuid(command_id.into_uuid()),
            rejection,
        },
        SessionTimelineDetailBody::InjectionSettlement {
            command_id,
            delivered,
            turn_id,
            rejection,
        } => WebSessionTimelineDetailBody::InjectionSettlement {
            command_id: web_uuid(command_id.into_uuid()),
            delivered,
            turn_id: turn_id.map(|turn| web_uuid(turn.into_uuid())),
            rejection,
        },
        SessionTimelineDetailBody::Ownership { transition } => {
            WebSessionTimelineDetailBody::Ownership {
                transition: ownership_detail_dto(transition),
            }
        }

        SessionTimelineDetailBody::EventFact { kind } => WebSessionTimelineDetailBody::EventFact {
            kind: event_kind_dto(kind),
        },
        SessionTimelineDetailBody::SessionCreated {
            workspace_root_kind,
            cause,
            imported_evidence,
        } => WebSessionTimelineDetailBody::SessionCreated {
            workspace_root_kind: workspace_root_kind.map(workspace_root_kind_dto),
            cause: match cause {
                signalbox_domain::SessionCreationCause::Interactive => {
                    signalbox_web_contract::WebTimelineCreationCause::Interactive {}
                }
                signalbox_domain::SessionCreationCause::ModuleDispatched { dispatch } => {
                    match dispatch {
                        signalbox_domain::ModuleDispatch::RepositoryWatch { dispatch } => {
                            signalbox_web_contract::WebTimelineCreationCause::RepositoryWatch {
                                dispatch_id: web_uuid(dispatch.into_uuid()),
                            }
                        }
                        signalbox_domain::ModuleDispatch::Commissioned { dispatch } => {
                            signalbox_web_contract::WebTimelineCreationCause::Commissioned {
                                dispatch_id: web_uuid(dispatch.into_uuid()),
                            }
                        }
                    }
                }
                signalbox_domain::SessionCreationCause::Workflow { run } => {
                    signalbox_web_contract::WebTimelineCreationCause::Workflow {
                        program_run_id: web_uuid(run.run().into_uuid()),
                    }
                }
                signalbox_domain::SessionCreationCause::Delegated { spawning_request } => {
                    signalbox_web_contract::WebTimelineCreationCause::Delegated {
                        spawning_request_id: web_uuid(spawning_request.into_uuid()),
                    }
                }
            },
            imported_evidence: imported_evidence.map(|evidence| WebTimelineImportedEvidence {
                imported_conversation_id: web_uuid(evidence.imported_conversation_id.into_uuid()),
                imported_entry_id: web_uuid(evidence.imported_entry_id.into_uuid()),
                imported_position: WebU64::from_u64(evidence.imported_position),
                relationship: match evidence.relationship {
                    ImportedSessionRelationship::Resume => WebTimelineImportedRelationship::Resume,
                    ImportedSessionRelationship::Fork => WebTimelineImportedRelationship::Fork,
                },
            }),
        },
        SessionTimelineDetailBody::ModelSettings { detail } => {
            WebSessionTimelineDetailBody::ModelSettings {
                detail: model_settings_detail_dto(detail),
            }
        }
        SessionTimelineDetailBody::UserInput {
            turn_id,
            text,
            attachments,
        } => WebSessionTimelineDetailBody::UserInput {
            turn_id: web_uuid(turn_id.into_uuid()),
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
            turn_id: web_uuid(turn_id.into_uuid()),
            model_call_id: web_uuid(model_call_id.into_uuid()),
            state: model_call_state_dto(state),
            model_identity_id: web_uuid(model_identity_id.into_uuid()),
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
        SessionTimelineDetailBody::ToolBatch {
            turn_id,
            producing_model_call_id,
            state,
            projected_member_index,
            tools,
            goal_events,
        } => WebSessionTimelineDetailBody::ToolBatch {
            turn_id: web_uuid(turn_id.into_uuid()),
            producing_model_call_id: web_uuid(producing_model_call_id.into_uuid()),
            state: match state {
                TimelineToolBatchState::Proposed { frontier_id } => {
                    WebTimelineToolBatchState::Proposed {
                        frontier_id: web_uuid(frontier_id.into_uuid()),
                    }
                }
                TimelineToolBatchState::ResultsProjected { frontier_id } => {
                    WebTimelineToolBatchState::ResultsProjected {
                        frontier_id: web_uuid(frontier_id.into_uuid()),
                    }
                }
                TimelineToolBatchState::RecoveryRequired { attempt_id } => {
                    WebTimelineToolBatchState::RecoveryRequired {
                        tool_attempt_id: web_uuid(attempt_id.into_uuid()),
                    }
                }
                TimelineToolBatchState::ChildWaitResumed { attempt_id } => {
                    WebTimelineToolBatchState::ChildWaitResumed {
                        tool_attempt_id: web_uuid(attempt_id.into_uuid()),
                    }
                }
            },
            projected_member_index,
            tools: tools
                .into_iter()
                .map(tool_attempt_dto)
                .collect::<Result<Vec<_>, SessionTimelineRequestError>>()?,
            goal_events: goal_events
                .into_iter()
                .map(goal_event_dto)
                .collect::<Result<Vec<_>, SessionTimelineRequestError>>()?,
        },
        SessionTimelineDetailBody::ToolApprovalDecision {
            turn_id,
            request_id,
            tool_name,
            decision,
            actor,
            rationale,
            approval_judge_escalated,
        } => WebSessionTimelineDetailBody::ToolApprovalDecision {
            turn_id: web_uuid(turn_id.into_uuid()),
            request_id: web_uuid(request_id.into_uuid()),
            tool_name: WebToolName::from_checked(tool_name.into_string()),
            decision: match decision {
                TimelineApprovalDecision::Approve => WebTimelineApprovalDecision::Approve,
                TimelineApprovalDecision::Deny => WebTimelineApprovalDecision::Deny,
            },
            actor: match actor {
                TimelineApprovalActor::Policy => WebTimelineApprovalActor::Policy {},
                TimelineApprovalActor::User { command_id } => WebTimelineApprovalActor::User {
                    command_id: web_uuid(command_id.into_uuid()),
                },
                TimelineApprovalActor::UserOverride {
                    command_id,
                    denied_request_id,
                } => WebTimelineApprovalActor::UserOverride {
                    command_id: web_uuid(command_id.into_uuid()),
                    denied_request_id: web_uuid(denied_request_id.into_uuid()),
                },
                TimelineApprovalActor::Delegate {
                    model_selection_id,
                    model_call_id,
                } => WebTimelineApprovalActor::Delegate {
                    model_selection_id: web_uuid(model_selection_id.into_uuid()),
                    model_call_id: web_uuid(model_call_id.into_uuid()),
                },
            },
            rationale: rationale.map(text_excerpt_dto),
            approval_judge_escalated,
        },
        SessionTimelineDetailBody::GoalEvent { session_id, event } => {
            WebSessionTimelineDetailBody::GoalEvent {
                session_id: web_uuid(session_id.into_uuid()),
                event: goal_event_dto(event)?,
            }
        }
        SessionTimelineDetailBody::ContextCompaction {
            compaction_id,
            model_call_id,
            through_position,
            summary_entry_id,
            result_frontier_id,
            summary,
        } => WebSessionTimelineDetailBody::ContextCompaction {
            compaction_id: web_uuid(compaction_id.into_uuid()),
            model_call_id: web_uuid(model_call_id.into_uuid()),
            through_position: WebU64::from_u64(through_position),
            summary_entry_id: web_uuid(summary_entry_id.into_uuid()),
            result_frontier_id: web_uuid(result_frontier_id.into_uuid()),
            summary: text_excerpt_dto(summary),
        },
        SessionTimelineDetailBody::TurnLifecycle {
            turn_id,
            lifecycle,
            cause_code,
        } => WebSessionTimelineDetailBody::TurnLifecycle {
            turn_id: web_uuid(turn_id.into_uuid()),
            lifecycle: match lifecycle {
                TimelineTurnLifecycleKind::Activated => WebTimelineTurnLifecycleKind::Activated,
                TimelineTurnLifecycleKind::Terminalized => {
                    WebTimelineTurnLifecycleKind::Terminalized
                }
            },
            cause_code,
        },
        SessionTimelineDetailBody::Reconciliation {
            turn_id,
            operation,
            terminal_frontier_id,
        } => {
            let operation = match operation {
                TimelineReconciliationOperation::ModelCall(call) => {
                    WebTimelineReconciliationOperation::ModelCall {
                        model_call_id: web_uuid(call.into_uuid()),
                    }
                }
                TimelineReconciliationOperation::ToolAttempt(attempt) => {
                    WebTimelineReconciliationOperation::ToolAttempt {
                        tool_attempt_id: web_uuid(attempt.into_uuid()),
                    }
                }
            };
            WebSessionTimelineDetailBody::Reconciliation {
                turn_id: web_uuid(turn_id.into_uuid()),
                operation,
                terminal_frontier_id: web_uuid(terminal_frontier_id.into_uuid()),
            }
        }
        SessionTimelineDetailBody::Runner {
            runner_id,
            placement_revision,
            sandbox_posture,
            working_directory,
            state,
        } => WebSessionTimelineDetailBody::Runner {
            runner_id: web_uuid(runner_id.into_uuid()),
            placement_revision: web_positive(placement_revision)?,
            sandbox_posture: match sandbox_posture {
                TimelineRunnerSandboxPosture::Unsandboxed => {
                    WebTimelineRunnerSandboxPosture::Unsandboxed
                }
                TimelineRunnerSandboxPosture::Sandboxed => {
                    WebTimelineRunnerSandboxPosture::Sandboxed
                }
            },
            working_directory,
            state: match state {
                TimelineRunnerState::Pinned => WebTimelineRunnerState::Pinned,
                TimelineRunnerState::Suspect => WebTimelineRunnerState::Suspect,
                TimelineRunnerState::Connected => WebTimelineRunnerState::Connected,
                TimelineRunnerState::RunnerLostBeforePin => {
                    WebTimelineRunnerState::RunnerLostBeforePin
                }
                TimelineRunnerState::RunnerLost => WebTimelineRunnerState::RunnerLost,
                TimelineRunnerState::Replaced => WebTimelineRunnerState::Replaced,
                TimelineRunnerState::WorkingDirectoryChanged => {
                    WebTimelineRunnerState::WorkingDirectoryChanged
                }
                TimelineRunnerState::Abandoned => WebTimelineRunnerState::Abandoned,
            },
        },
        SessionTimelineDetailBody::Delegation(detail) => WebSessionTimelineDetailBody::Delegation {
            detail: delegation_detail_dto(detail)?,
        },
    })
}

fn goal_event_dto(
    event: TimelineGoalEvent,
) -> Result<WebTimelineGoalEvent, SessionTimelineRequestError> {
    let reason_dto = |reason| match reason {
        TimelineGoalBlockedReason::UserInputRequired => {
            WebTimelineGoalBlockedReason::UserInputRequired
        }
        TimelineGoalBlockedReason::ExternalChangeRequired => {
            WebTimelineGoalBlockedReason::ExternalChangeRequired
        }
        TimelineGoalBlockedReason::AuthorizationRequired => {
            WebTimelineGoalBlockedReason::AuthorizationRequired
        }
        TimelineGoalBlockedReason::FinishCheckFailed => {
            WebTimelineGoalBlockedReason::FinishCheckFailed
        }
        TimelineGoalBlockedReason::ExecutionFailure => {
            WebTimelineGoalBlockedReason::ExecutionFailure
        }
    };
    Ok(match event {
        TimelineGoalEvent::Commissioned { generation, text } => {
            WebTimelineGoalEvent::Commissioned {
                generation: web_positive(generation)?,
                text: text_excerpt_dto(text),
            }
        }
        TimelineGoalEvent::Blocked {
            generation,
            reason,
            text,
        } => WebTimelineGoalEvent::Blocked {
            generation: web_positive(generation)?,
            reason: reason_dto(reason),
            text: text_excerpt_dto(text),
        },
        TimelineGoalEvent::Resumed { generation, text } => WebTimelineGoalEvent::Resumed {
            generation: web_positive(generation)?,
            text: text.map(text_excerpt_dto),
        },
        TimelineGoalEvent::Achieved { generation, text } => WebTimelineGoalEvent::Achieved {
            generation: web_positive(generation)?,
            text: text_excerpt_dto(text),
        },
        TimelineGoalEvent::SessionClosed {
            generation,
            outcome,
        } => WebTimelineGoalEvent::SessionClosed {
            generation: web_positive(generation)?,
            outcome: session_outcome_detail_dto(outcome),
        },
        TimelineGoalEvent::UserStopped {
            generation,
            settling_turn,
            abandoned_actions,
        } => WebTimelineGoalEvent::UserStopped {
            generation: web_positive(generation)?,
            settling_turn_id: settling_turn.map(|turn| {
                signalbox_web_contract::WebUuid::from_validated_uuid(turn.into_uuid().to_string())
            }),
            abandoned_actions: abandoned_actions.map(WebU64::from_u64),
        },
        TimelineGoalEvent::Superseded { generation, text } => WebTimelineGoalEvent::Superseded {
            generation: web_positive(generation)?,
            text: text_excerpt_dto(text),
        },
    })
}

fn model_settings_detail_dto(
    detail: TimelineModelSettingsDetail,
) -> WebTimelineModelSettingsDetail {
    match detail {
        TimelineModelSettingsDetail::SessionDefaultsChanged {
            command_id,
            prior_defaults_version,
            installed_defaults_version,
            prior_model,
            installed_model,
            prior_settings,
            installed_settings,
            caller_override,
            adjustments,
        } => WebTimelineModelSettingsDetail::SessionDefaultsChanged {
            command_id: web_uuid(command_id.into_uuid()),
            prior_defaults_version: WebU64::from_u64(prior_defaults_version.as_u64()),
            installed_defaults_version: WebU64::from_u64(installed_defaults_version.as_u64()),
            prior_model: model_selection_request_dto(prior_model),
            installed_model: model_selection_request_dto(installed_model),
            prior_settings: model_settings_snapshot_dto(prior_settings),
            installed_settings: model_settings_snapshot_dto(installed_settings),
            caller_override: model_settings_overlay_dto(caller_override),
            adjustments: adjustments
                .into_iter()
                .map(model_change_adjustment_dto)
                .collect(),
        },
        TimelineModelSettingsDetail::TurnResolved {
            accepted_input_id,
            turn_id,
            defaults_version,
            selection,
            per_call_override,
            settings,
            adjusted_from_selection_id,
            adjustments,
        } => WebTimelineModelSettingsDetail::TurnResolved {
            accepted_input_id: web_uuid(accepted_input_id.into_uuid()),
            turn_id: web_uuid(turn_id.into_uuid()),
            defaults_version: WebU64::from_u64(defaults_version.as_u64()),
            requested_model: frozen_model_selection_dto(selection),
            selected_direct_id: web_uuid(selection.selected_direct().into_uuid()),
            per_call_override: model_settings_overlay_dto(per_call_override),
            settings: model_settings_snapshot_dto(settings),
            adjusted_from_selection_id: adjusted_from_selection_id
                .map(|selection| web_uuid(selection.into_uuid())),
            adjustments: adjustments
                .into_iter()
                .map(model_change_adjustment_dto)
                .collect(),
        },
    }
}

fn model_selection_request_dto(
    selection: signalbox_domain::ModelSelectionRequest,
) -> WebTimelineModelSelection {
    match selection {
        signalbox_domain::ModelSelectionRequest::Direct(selection) => {
            WebTimelineModelSelection::Direct {
                selection_id: web_uuid(selection.into_uuid()),
            }
        }
        signalbox_domain::ModelSelectionRequest::Alias(alias) => WebTimelineModelSelection::Alias {
            alias_id: web_uuid(alias.into_uuid()),
        },
    }
}

fn frozen_model_selection_dto(
    selection: signalbox_domain::FrozenModelSelection,
) -> WebTimelineModelSelection {
    match selection {
        signalbox_domain::FrozenModelSelection::Direct(selection) => {
            WebTimelineModelSelection::Direct {
                selection_id: web_uuid(selection.into_uuid()),
            }
        }
        signalbox_domain::FrozenModelSelection::FrozenAlias { alias, .. } => {
            WebTimelineModelSelection::Alias {
                alias_id: web_uuid(alias.into_uuid()),
            }
        }
    }
}

fn model_settings_snapshot_dto(
    settings: signalbox_domain::ValidatedModelSettings,
) -> WebTimelineModelSettingsSnapshot {
    let precedence = settings.precedence();
    let resolved = settings.resolved();
    let effective = resolved.effective();
    WebTimelineModelSettingsSnapshot {
        precedence: WebTimelineModelSettingsPrecedence {
            per_call: model_settings_overlay_dto(precedence.per_call()),
            session: model_settings_overlay_dto(precedence.session()),
            profile: model_settings_overlay_dto(precedence.profile()),
            global_default: model_settings_overlay_dto(precedence.global_default()),
        },
        effective: WebTimelineEffectiveModelSettings {
            reasoning_level: effective.reasoning_level().map(reasoning_level_dto),
            fast_mode: fast_mode_dto(effective.fast_mode()),
            service_tier: effective.service_tier().map(service_tier_dto),
        },
        reasoning_source: resolved.reasoning_source().map(model_setting_source_dto),
        fast_mode_source: resolved.fast_mode_source().map(model_setting_source_dto),
        service_tier_source: resolved.service_tier_source().map(model_setting_source_dto),
        validated_for_selection_id: settings
            .validated_for()
            .map(|selection| web_uuid(selection.into_uuid())),
    }
}

fn model_settings_overlay_dto(
    overlay: signalbox_domain::ModelSettingsOverlay,
) -> WebTimelineModelSettingsOverlay {
    WebTimelineModelSettingsOverlay {
        reasoning_level: setting_overlay_dto(overlay.reasoning_level(), reasoning_level_dto),
        fast_mode: match overlay.fast_mode() {
            signalbox_domain::FastModeOverlay::Inherit => WebTimelineFastModeOverlay::Inherit,
            signalbox_domain::FastModeOverlay::Value(value) => {
                WebTimelineFastModeOverlay::Value(fast_mode_dto(value))
            }
        },
        service_tier: setting_overlay_dto(overlay.service_tier(), service_tier_dto),
    }
}

fn setting_overlay_dto<DomainT, WebT>(
    value: signalbox_domain::SettingOverlay<DomainT>,
    map: impl FnOnce(DomainT) -> WebT,
) -> WebTimelineSettingOverlay<WebT> {
    match value {
        signalbox_domain::SettingOverlay::Inherit => WebTimelineSettingOverlay::Inherit,
        signalbox_domain::SettingOverlay::ProviderDefault => {
            WebTimelineSettingOverlay::ProviderDefault
        }
        signalbox_domain::SettingOverlay::Value(value) => {
            WebTimelineSettingOverlay::Value(map(value))
        }
    }
}

const fn reasoning_level_dto(value: signalbox_domain::ReasoningLevel) -> WebTimelineReasoningLevel {
    match value {
        signalbox_domain::ReasoningLevel::None => WebTimelineReasoningLevel::None,
        signalbox_domain::ReasoningLevel::Minimal => WebTimelineReasoningLevel::Minimal,
        signalbox_domain::ReasoningLevel::Low => WebTimelineReasoningLevel::Low,
        signalbox_domain::ReasoningLevel::Medium => WebTimelineReasoningLevel::Medium,
        signalbox_domain::ReasoningLevel::High => WebTimelineReasoningLevel::High,
        signalbox_domain::ReasoningLevel::XHigh => WebTimelineReasoningLevel::Xhigh,
        signalbox_domain::ReasoningLevel::Max => WebTimelineReasoningLevel::Max,
        signalbox_domain::ReasoningLevel::Ultra => WebTimelineReasoningLevel::Ultra,
    }
}

const fn fast_mode_dto(value: signalbox_domain::FastMode) -> WebTimelineFastMode {
    match value {
        signalbox_domain::FastMode::Disabled => WebTimelineFastMode::Disabled,
        signalbox_domain::FastMode::Enabled => WebTimelineFastMode::Enabled,
    }
}

const fn model_setting_source_dto(
    value: signalbox_domain::ModelSettingSource,
) -> WebTimelineModelSettingSource {
    match value {
        signalbox_domain::ModelSettingSource::PerCall => WebTimelineModelSettingSource::PerCall,
        signalbox_domain::ModelSettingSource::Session => WebTimelineModelSettingSource::Session,
        signalbox_domain::ModelSettingSource::Profile => WebTimelineModelSettingSource::Profile,
        signalbox_domain::ModelSettingSource::GlobalDefault => {
            WebTimelineModelSettingSource::GlobalDefault
        }
    }
}

const fn service_tier_dto(value: signalbox_domain::ServiceTier) -> WebTimelineServiceTier {
    match value {
        signalbox_domain::ServiceTier::Anthropic(value) => {
            WebTimelineServiceTier::Anthropic(match value {
                signalbox_domain::AnthropicServiceTier::Auto => {
                    signalbox_web_contract::WebTimelineAnthropicServiceTier::Auto
                }
                signalbox_domain::AnthropicServiceTier::StandardOnly => {
                    signalbox_web_contract::WebTimelineAnthropicServiceTier::StandardOnly
                }
            })
        }
        signalbox_domain::ServiceTier::OpenAi(value) => {
            WebTimelineServiceTier::OpenAi(match value {
                signalbox_domain::OpenAiServiceTier::Auto => {
                    signalbox_web_contract::WebTimelineOpenAiServiceTier::Auto
                }
                signalbox_domain::OpenAiServiceTier::Default => {
                    signalbox_web_contract::WebTimelineOpenAiServiceTier::Default
                }
                signalbox_domain::OpenAiServiceTier::Flex => {
                    signalbox_web_contract::WebTimelineOpenAiServiceTier::Flex
                }
                signalbox_domain::OpenAiServiceTier::Scale => {
                    signalbox_web_contract::WebTimelineOpenAiServiceTier::Scale
                }
                signalbox_domain::OpenAiServiceTier::Priority => {
                    signalbox_web_contract::WebTimelineOpenAiServiceTier::Priority
                }
                signalbox_domain::OpenAiServiceTier::Fast => {
                    signalbox_web_contract::WebTimelineOpenAiServiceTier::Fast
                }
            })
        }
        signalbox_domain::ServiceTier::CodexCli(value) => {
            WebTimelineServiceTier::CodexCli(match value {
                signalbox_domain::CodexCliServiceTier::Default => {
                    signalbox_web_contract::WebTimelineCodexCliServiceTier::Default
                }
                signalbox_domain::CodexCliServiceTier::Priority => {
                    signalbox_web_contract::WebTimelineCodexCliServiceTier::Priority
                }
                signalbox_domain::CodexCliServiceTier::Flex => {
                    signalbox_web_contract::WebTimelineCodexCliServiceTier::Flex
                }
            })
        }
    }
}

fn model_change_adjustment_dto(
    adjustment: signalbox_domain::ModelChangeAdjustment,
) -> WebTimelineModelChangeAdjustment {
    match adjustment {
        signalbox_domain::ModelChangeAdjustment::ReasoningLevelClamped { from, to } => {
            WebTimelineModelChangeAdjustment::ReasoningLevelClamped {
                from: reasoning_level_dto(from),
                to: reasoning_level_dto(to),
            }
        }
        signalbox_domain::ModelChangeAdjustment::ReasoningLevelCleared { from } => {
            WebTimelineModelChangeAdjustment::ReasoningLevelCleared {
                from: reasoning_level_dto(from),
            }
        }
        signalbox_domain::ModelChangeAdjustment::FastModeDisabled => {
            WebTimelineModelChangeAdjustment::FastModeDisabled {}
        }
        signalbox_domain::ModelChangeAdjustment::ServiceTierCleared { from } => {
            WebTimelineModelChangeAdjustment::ServiceTierCleared {
                from: service_tier_dto(from),
            }
        }
    }
}

fn delegation_policy_dto(policy: TimelineDelegationPolicy) -> WebTimelineDelegationPolicy {
    match policy {
        TimelineDelegationPolicy::Background => WebTimelineDelegationPolicy::Background,
        TimelineDelegationPolicy::Bound {
            on_parent_stopped,
            on_parent_cancelled,
        } => WebTimelineDelegationPolicy::Bound {
            on_parent_stopped: bound_child_action_dto(on_parent_stopped),
            on_parent_cancelled: bound_child_action_dto(on_parent_cancelled),
        },
    }
}

fn delegation_detail_dto(
    detail: TimelineDelegationDetail,
) -> Result<WebTimelineDelegationDetail, SessionTimelineRequestError> {
    Ok(match detail {
        TimelineDelegationDetail::ChildSpawned {
            relationship_id,
            child,
            policy,
        } => WebTimelineDelegationDetail::ChildSpawned {
            relationship_id: web_uuid(relationship_id.into_uuid()),
            child_session_id: WebSessionId::from_uuid_bytes(*child.into_uuid().as_bytes()),
            policy: delegation_policy_dto(policy),
        },
        TimelineDelegationDetail::ChildWaiting {
            relationship_id,
            child,
            awaiting_request,
            mode,
        } => WebTimelineDelegationDetail::ChildWaiting {
            relationship_id: web_uuid(relationship_id.into_uuid()),
            child_session_id: WebSessionId::from_uuid_bytes(*child.into_uuid().as_bytes()),
            awaiting_request_id: web_uuid(awaiting_request.into_uuid()),
            mode: match mode {
                TimelineDelegationWaitMode::Foreground => WebTimelineDelegationWaitMode::Foreground,
                TimelineDelegationWaitMode::Background => WebTimelineDelegationWaitMode::Background,
            },
        },
        TimelineDelegationDetail::ChildLifecycleDisposition {
            relationship_id,
            child,
            event_ordinal,
            outcome,
            reason,
            provenance,
        } => WebTimelineDelegationDetail::ChildLifecycleDisposition {
            relationship_id: web_uuid(relationship_id.into_uuid()),
            child_session_id: WebSessionId::from_uuid_bytes(*child.into_uuid().as_bytes()),
            event_ordinal: web_positive(event_ordinal)?,
            outcome: delegation_outcome_dto(outcome),
            reason: delegation_reason_dto(reason),
            provenance: delegation_provenance_dto(provenance)?,
        },
        TimelineDelegationDetail::ChildResult {
            relationship_id,
            child,
            outcome,
            reason,
            provenance,
            content,
        } => WebTimelineDelegationDetail::ChildResult {
            relationship_id: web_uuid(relationship_id.into_uuid()),
            child_session_id: WebSessionId::from_uuid_bytes(*child.into_uuid().as_bytes()),
            outcome: delegation_outcome_dto(outcome),
            reason: delegation_reason_dto(reason),
            provenance: delegation_provenance_dto(provenance)?,
            content: content.map(text_excerpt_dto),
        },
        TimelineDelegationDetail::SessionMessage {
            relationship_id,
            message,
            sender,
            recipient,
            message_ordinal,
            delivery_sequence,
            content,
        } => WebTimelineDelegationDetail::SessionMessage {
            relationship_id: web_uuid(relationship_id.into_uuid()),
            message_id: web_uuid(message.into_uuid()),
            sender_session_id: WebSessionId::from_uuid_bytes(*sender.into_uuid().as_bytes()),
            recipient_session_id: WebSessionId::from_uuid_bytes(*recipient.into_uuid().as_bytes()),
            message_ordinal: web_positive(message_ordinal)?,
            delivery_sequence: web_positive(delivery_sequence)?,
            content: text_excerpt_dto(content),
        },
        TimelineDelegationDetail::ResultWake {
            relationship_id,
            awaiting_request,
        } => WebTimelineDelegationDetail::ResultWake {
            relationship_id: web_uuid(relationship_id.into_uuid()),
            awaiting_request_id: awaiting_request.map(|request| web_uuid(request.into_uuid())),
        },
        TimelineDelegationDetail::MessageWake {
            relationship_id,
            message,
        } => WebTimelineDelegationDetail::MessageWake {
            relationship_id: web_uuid(relationship_id.into_uuid()),
            message_id: web_uuid(message.into_uuid()),
        },
    })
}

const fn delegation_outcome_dto(
    outcome: TimelineDelegationOutcome,
) -> WebTimelineDelegationOutcome {
    match outcome {
        TimelineDelegationOutcome::ResultReturned => WebTimelineDelegationOutcome::ResultReturned,
        TimelineDelegationOutcome::ChildFailed => WebTimelineDelegationOutcome::ChildFailed,
        TimelineDelegationOutcome::ChildStopped => WebTimelineDelegationOutcome::ChildStopped,
        TimelineDelegationOutcome::ChildCancelled => WebTimelineDelegationOutcome::ChildCancelled,
        TimelineDelegationOutcome::ContinueRunning => WebTimelineDelegationOutcome::ContinueRunning,
        TimelineDelegationOutcome::AlreadyTerminal => WebTimelineDelegationOutcome::AlreadyTerminal,
    }
}

const fn delegation_reason_dto(reason: TimelineDelegationReason) -> WebTimelineDelegationReason {
    match reason {
        TimelineDelegationReason::ChildCompleted => WebTimelineDelegationReason::ChildCompleted,
        TimelineDelegationReason::ChildExecutionFailed => {
            WebTimelineDelegationReason::ChildExecutionFailed
        }
        TimelineDelegationReason::ChildResultUnavailable => {
            WebTimelineDelegationReason::ChildResultUnavailable
        }
        TimelineDelegationReason::ChildCancelled => WebTimelineDelegationReason::ChildCancelled,
        TimelineDelegationReason::ParentStoppedWithDescendants => {
            WebTimelineDelegationReason::ParentStoppedWithDescendants
        }
        TimelineDelegationReason::ParentCancelledWithDescendants => {
            WebTimelineDelegationReason::ParentCancelledWithDescendants
        }
    }
}

fn delegation_provenance_dto(
    provenance: TimelineDelegationProvenance,
) -> Result<WebTimelineDelegationProvenance, SessionTimelineRequestError> {
    Ok(match provenance {
        TimelineDelegationProvenance::ParentLifecycleCommand { session, command } => {
            WebTimelineDelegationProvenance::ParentLifecycleCommand {
                session_id: web_uuid(session.into_uuid()),
                command_id: web_uuid(command.into_uuid()),
            }
        }
        TimelineDelegationProvenance::ChildTurn { session, turn } => {
            WebTimelineDelegationProvenance::ChildTurn {
                session_id: WebSessionId::from_uuid_bytes(*session.into_uuid().as_bytes()),
                turn_id: web_uuid(turn.into_uuid()),
            }
        }
        TimelineDelegationProvenance::ParentTurnCommand {
            session,
            turn,
            command,
        } => WebTimelineDelegationProvenance::ParentTurnCommand {
            session_id: WebSessionId::from_uuid_bytes(*session.into_uuid().as_bytes()),
            turn_id: web_uuid(turn.into_uuid()),
            command_id: web_uuid(command.into_uuid()),
        },
        TimelineDelegationProvenance::ParentGoalCommand {
            session,
            goal_generation,
            command,
        } => WebTimelineDelegationProvenance::ParentGoalCommand {
            session_id: WebSessionId::from_uuid_bytes(*session.into_uuid().as_bytes()),
            goal_generation: web_positive(goal_generation)?,
            command_id: web_uuid(command.into_uuid()),
        },
    })
}

const fn bound_child_action_dto(action: TimelineBoundChildAction) -> WebTimelineBoundChildAction {
    match action {
        TimelineBoundChildAction::KeepRunning => WebTimelineBoundChildAction::KeepRunning,
        TimelineBoundChildAction::Stop => WebTimelineBoundChildAction::Stop,
        TimelineBoundChildAction::Cancel => WebTimelineBoundChildAction::Cancel,
    }
}

fn tool_attempt_dto(
    attempt: TimelineToolAttempt,
) -> Result<WebTimelineToolAttempt, SessionTimelineRequestError> {
    let evidence = match (
        attempt.attempt_id,
        attempt.effect_posture,
        attempt.state,
        attempt.cause_code.as_deref(),
    ) {
        (None, None, None, None) => WebTimelineToolAttemptEvidence::RequestOnly {},
        (Some(attempt_id), Some(effect_posture), Some(state), cause) => {
            WebTimelineToolAttemptEvidence::PhysicalAttempt {
                attempt_id: web_uuid(attempt_id.into_uuid()),
                result: attempt.result.map(text_excerpt_dto),
                failure: attempt.failure.map(text_excerpt_dto),
                result_present: attempt.has_result,
                failure_present: attempt.has_failure,
                effect_posture: match effect_posture {
                    TimelineToolEffectPosture::EffectFree => {
                        WebTimelineToolEffectPosture::EffectFree
                    }
                    TimelineToolEffectPosture::ExternalEffect => {
                        WebTimelineToolEffectPosture::ExternalEffect
                    }
                },
                sandbox_posture: attempt.sandbox_posture.map(|posture| match posture {
                    TimelineToolSandboxPosture::Unsandboxed => {
                        WebTimelineToolSandboxPosture::Unsandboxed
                    }
                    TimelineToolSandboxPosture::Sandboxed => {
                        WebTimelineToolSandboxPosture::Sandboxed
                    }
                }),
                state: match state {
                    TimelineToolState::Prepared => WebTimelineToolState::Prepared,
                    TimelineToolState::InFlight => WebTimelineToolState::InFlight,
                    TimelineToolState::AwaitingChild => WebTimelineToolState::AwaitingChild,
                    TimelineToolState::Completed => WebTimelineToolState::Completed,
                    TimelineToolState::KnownFailed => WebTimelineToolState::KnownFailed,
                    TimelineToolState::Ambiguous => WebTimelineToolState::Ambiguous,
                },
                cause: match cause {
                    None => None,
                    Some("preauthorization_rejected") => {
                        Some(WebTimelineToolFailureCause::PreauthorizationRejected)
                    }
                    Some("unknown_tool") => Some(WebTimelineToolFailureCause::UnknownTool),
                    Some("invalid_arguments") => {
                        Some(WebTimelineToolFailureCause::InvalidArguments)
                    }
                    Some("execution_failed") => Some(WebTimelineToolFailureCause::ExecutionFailed),
                    Some("result_too_large") => Some(WebTimelineToolFailureCause::ResultTooLarge),
                    Some("result_contains_null") => {
                        Some(WebTimelineToolFailureCause::ResultContainsNull)
                    }
                    Some("crash_lost") => Some(WebTimelineToolFailureCause::CrashLost),
                    Some(_) => {
                        return Err(SessionTimelineRequestError::InvalidProjectedToolAttempt);
                    }
                },
            }
        }
        _ => return Err(SessionTimelineRequestError::InvalidProjectedToolAttempt),
    };
    Ok(WebTimelineToolAttempt {
        request_id: web_uuid(attempt.request_id.into_uuid()),
        tool_name: WebToolName::from_checked(attempt.tool_name.into_string()),
        arguments: attempt.arguments.map(text_excerpt_dto),
        approval_posture: match attempt.approval_posture {
            TimelineToolApprovalPosture::Auto => WebTimelineToolApprovalPosture::Auto,
            TimelineToolApprovalPosture::Delegated => WebTimelineToolApprovalPosture::Delegated,
            TimelineToolApprovalPosture::Human => WebTimelineToolApprovalPosture::Human,
        },
        approval_judge_escalated: attempt.approval_judge_escalated,
        evidence,
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

fn web_uuid(value: uuid::Uuid) -> WebSessionId {
    WebSessionId::from_uuid_bytes(*value.as_bytes())
}

fn web_positive(value: u64) -> Result<WebPositiveU64, SessionTimelineRequestError> {
    std::num::NonZeroU64::new(value)
        .map(WebPositiveU64::from_nonzero)
        .ok_or(SessionTimelineRequestError::InvalidProjectedOrdinal)
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
            TimelineBodyField::DelegationContent => WebTimelineBodyField::DelegationContent,
            TimelineBodyField::CompactionSummary => WebTimelineBodyField::CompactionSummary,
            TimelineBodyField::GoalText => WebTimelineBodyField::GoalText,
            TimelineBodyField::ApprovalRationale => WebTimelineBodyField::ApprovalRationale,
            TimelineBodyField::ToolFailure => WebTimelineBodyField::ToolFailure,
            TimelineBodyField::ToolResult => WebTimelineBodyField::ToolResult,
            TimelineBodyField::ToolArguments => WebTimelineBodyField::ToolArguments,
        },
        member_index: continuation.member_index,
        offset_bytes: WebU64::from_u64(continuation.offset_bytes),
    }
}

pub(super) fn event_kind_dto(kind: SessionTimelineEventKind) -> WebSessionTimelineEventKind {
    match kind {
        SessionTimelineEventKind::AutomaticReconciliationExhausted => {
            WebSessionTimelineEventKind::AutomaticReconciliationExhausted
        }
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

fn session_state_detail_dto(value: TimelineSessionState) -> WebTimelineSessionState {
    match value {
        TimelineSessionState::Created => WebTimelineSessionState::Created,
        TimelineSessionState::Dispatched => WebTimelineSessionState::Dispatched,
        TimelineSessionState::Active => WebTimelineSessionState::Active,
        TimelineSessionState::Waiting => WebTimelineSessionState::Waiting,
        TimelineSessionState::Recovering => WebTimelineSessionState::Recovering,
        TimelineSessionState::Blocked => WebTimelineSessionState::Blocked,
        TimelineSessionState::Parked => WebTimelineSessionState::Parked,
    }
}
fn session_outcome_detail_dto(value: TimelineSessionOutcome) -> WebTimelineSessionOutcome {
    match value {
        TimelineSessionOutcome::AchievedVerified => WebTimelineSessionOutcome::AchievedVerified,
        TimelineSessionOutcome::AchievedDeclared => WebTimelineSessionOutcome::AchievedDeclared,
        TimelineSessionOutcome::FailedRetryable => WebTimelineSessionOutcome::FailedRetryable,
        TimelineSessionOutcome::FailedStructural => WebTimelineSessionOutcome::FailedStructural,
        TimelineSessionOutcome::FailedUnknown => WebTimelineSessionOutcome::FailedUnknown,
        TimelineSessionOutcome::Stopped => WebTimelineSessionOutcome::Stopped,
        TimelineSessionOutcome::Superseded => WebTimelineSessionOutcome::Superseded,
        TimelineSessionOutcome::Abandoned => WebTimelineSessionOutcome::Abandoned,
        TimelineSessionOutcome::Retired => WebTimelineSessionOutcome::Retired,
    }
}
fn ownership_detail_dto(value: TimelineOwnershipTransition) -> WebTimelineOwnershipTransition {
    match value {
        TimelineOwnershipTransition::Adopted => WebTimelineOwnershipTransition::Adopted,
        TimelineOwnershipTransition::Released => WebTimelineOwnershipTransition::Released,
    }
}

#[expect(
    clippy::expect_used,
    reason = "checked domain revisions and pull request numbers are positive"
)]
fn repository_watch_origin_dto(
    origin: signalbox_module_repo_watch_v2::RetainedDispatchAction,
) -> signalbox_web_contract::WebRepositoryWatchProvenance {
    use signalbox_domain::RepoWatchEventKindNameV1;
    use signalbox_web_contract::{
        WebLiveResourceId, WebPositiveU64, WebRepositoryWatchEventKind,
        WebRepositoryWatchProvenance,
    };
    WebRepositoryWatchProvenance {
        dispatch_id: WebLiveResourceId::from_uuid_bytes(*origin.dispatch().into_uuid().as_bytes()),
        action_ordinal: WebPositiveU64::from_nonzero(origin.action_ordinal()),
        repository: origin.repository().as_str().to_owned(),
        rule_id: origin.rule_id().as_str().to_owned(),
        rule_revision: WebPositiveU64::from_nonzero(
            std::num::NonZeroU64::new(origin.rule_revision().get())
                .expect("rule revision is positive"),
        ),
        event_id: WebLiveResourceId::from_uuid_bytes(*origin.event_id().into_uuid().as_bytes()),
        pull_request: origin.pull_request().map(|number| {
            WebPositiveU64::from_nonzero(
                std::num::NonZeroU64::new(number.get()).expect("pull request number is positive"),
            )
        }),
        event_kind: match origin.event_kind() {
            RepoWatchEventKindNameV1::PullRequestOpened => {
                WebRepositoryWatchEventKind::PullRequestOpened
            }
            RepoWatchEventKindNameV1::PullRequestClosed => {
                WebRepositoryWatchEventKind::PullRequestClosed
            }
            RepoWatchEventKindNameV1::PullRequestMerged => {
                WebRepositoryWatchEventKind::PullRequestMerged
            }
            RepoWatchEventKindNameV1::HeadChanged => WebRepositoryWatchEventKind::HeadChanged,
            RepoWatchEventKindNameV1::MergeableStateChanged => {
                WebRepositoryWatchEventKind::MergeableStateChanged
            }
            RepoWatchEventKindNameV1::ChecksCompleted => {
                WebRepositoryWatchEventKind::ChecksCompleted
            }
            RepoWatchEventKindNameV1::CheckRunCompleted => {
                WebRepositoryWatchEventKind::CheckRunCompleted
            }
            RepoWatchEventKindNameV1::BranchWorkflowRunCompleted => {
                WebRepositoryWatchEventKind::BranchWorkflowRunCompleted
            }
            RepoWatchEventKindNameV1::ReviewSubmitted => {
                WebRepositoryWatchEventKind::ReviewSubmitted
            }
            RepoWatchEventKindNameV1::ThreadOpened => WebRepositoryWatchEventKind::ThreadOpened,
            RepoWatchEventKindNameV1::ThreadResolved => WebRepositoryWatchEventKind::ThreadResolved,
            RepoWatchEventKindNameV1::Labeled => WebRepositoryWatchEventKind::Labeled,
            RepoWatchEventKindNameV1::Unlabeled => WebRepositoryWatchEventKind::Unlabeled,
            RepoWatchEventKindNameV1::BaseAdvanced => WebRepositoryWatchEventKind::BaseAdvanced,
            RepoWatchEventKindNameV1::ReactionChanged => {
                WebRepositoryWatchEventKind::ReactionChanged
            }
        },
    }
}

fn workspace_root_kind_dto(
    kind: signalbox_domain::SessionWorkspaceRootKind,
) -> signalbox_web_contract::WebSessionWorkspaceRootKind {
    match kind {
        signalbox_domain::SessionWorkspaceRootKind::Derived => {
            signalbox_web_contract::WebSessionWorkspaceRootKind::Derived
        }
        signalbox_domain::SessionWorkspaceRootKind::Configured => {
            signalbox_web_contract::WebSessionWorkspaceRootKind::Configured
        }
        signalbox_domain::SessionWorkspaceRootKind::Provisioned => {
            signalbox_web_contract::WebSessionWorkspaceRootKind::Provisioned
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use signalbox_domain::{ToolAttemptId, ToolName, ToolRequestId};
    use uuid::Uuid;

    #[test]
    fn detail_preserves_creation_cause_and_originating_identity() {
        use signalbox_domain::{
            CommissionedDispatchId, ModuleDispatch, RepoWatchDispatchId, SessionCreationCause,
        };
        let identity = Uuid::from_u128(0x991);
        for (cause, expected) in [
            (
                SessionCreationCause::Interactive,
                serde_json::json!({ "type": "interactive" }),
            ),
            (
                SessionCreationCause::ModuleDispatched {
                    dispatch: ModuleDispatch::RepositoryWatch {
                        dispatch: RepoWatchDispatchId::from_uuid(identity),
                    },
                },
                serde_json::json!({ "type": "repository_watch", "dispatch_id": identity.to_string() }),
            ),
            (
                SessionCreationCause::ModuleDispatched {
                    dispatch: ModuleDispatch::Commissioned {
                        dispatch: CommissionedDispatchId::from_uuid(identity),
                    },
                },
                serde_json::json!({ "type": "commissioned", "dispatch_id": identity.to_string() }),
            ),
            (
                SessionCreationCause::Delegated {
                    spawning_request: ToolRequestId::from_uuid(identity),
                },
                serde_json::json!({ "type": "delegated", "spawning_request_id": identity.to_string() }),
            ),
        ] {
            let dto = detail_body_dto(SessionTimelineDetailBody::SessionCreated {
                workspace_root_kind: Some(signalbox_domain::SessionWorkspaceRootKind::Configured),
                cause,
                imported_evidence: None,
            })
            .expect("creation detail converts");
            let json = serde_json::to_value(dto).expect("creation detail serializes");
            assert_eq!(json["cause"], expected);
            assert_eq!(json["workspace_root_kind"], "configured");
        }
    }

    #[test]
    fn detail_preserves_the_preauthorization_failure_cause() {
        let dto = tool_attempt_dto(TimelineToolAttempt {
            request_id: ToolRequestId::from_uuid(Uuid::from_u128(0x991)),
            attempt_id: Some(ToolAttemptId::from_uuid(Uuid::from_u128(0x992))),
            tool_name: ToolName::try_new(String::from("read_blob")).expect("valid tool name"),
            arguments: None,
            result: None,
            failure: None,
            has_result: false,
            has_failure: true,
            approval_posture: TimelineToolApprovalPosture::Auto,
            approval_judge_escalated: false,
            effect_posture: Some(TimelineToolEffectPosture::EffectFree),
            sandbox_posture: None,
            state: Some(TimelineToolState::KnownFailed),
            cause_code: Some(String::from("preauthorization_rejected")),
        })
        .expect("the durable failure cause is a web-contract cause");
        assert!(matches!(
            dto.evidence,
            WebTimelineToolAttemptEvidence::PhysicalAttempt {
                cause: Some(WebTimelineToolFailureCause::PreauthorizationRejected),
                ..
            }
        ));
    }
}
