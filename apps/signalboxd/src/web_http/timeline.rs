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
            Self::InvalidProjectedSessionId => {
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
    let origin = match reload {
        Some(axum::Extension(reload)) => match reload.repository_watch_origin(session).await {
            Ok(origin) => origin,
            Err(_) => {
                return application_error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "session_projection_unavailable",
                    "repository-watch origin could not be read",
                );
            }
        },
        None => None,
    };
    match repository.read_descriptor(session).await {
        Ok(Some(descriptor)) => match descriptor_dto(descriptor) {
            Ok(mut descriptor) => {
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
        SessionTimelineDetailBody::UserInput {
            turn_id,
            text,
            attachments,
        } => WebSessionTimelineDetailBody::UserInput {
            turn_id: WebSessionId::from_canonical(turn_id.into_uuid().to_string())
                .ok_or(SessionTimelineRequestError::InvalidProjectedSessionId)?,
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
            turn_id: WebSessionId::from_canonical(turn_id.into_uuid().to_string())
                .ok_or(SessionTimelineRequestError::InvalidProjectedSessionId)?,
            model_call_id: WebSessionId::from_canonical(model_call_id.into_uuid().to_string())
                .ok_or(SessionTimelineRequestError::InvalidProjectedSessionId)?,
            state: model_call_state_dto(state),
            model_identity_id: WebSessionId::from_canonical(
                model_identity_id.into_uuid().to_string(),
            )
            .ok_or(SessionTimelineRequestError::InvalidProjectedSessionId)?,
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
        SessionTimelineDetailBody::TurnLifecycle {
            turn_id,
            lifecycle,
            cause_code,
        } => WebSessionTimelineDetailBody::TurnLifecycle {
            turn_id: WebSessionId::from_canonical(turn_id.into_uuid().to_string())
                .ok_or(SessionTimelineRequestError::InvalidProjectedSessionId)?,
            lifecycle: match lifecycle {
                TimelineTurnLifecycleKind::Activated => WebTimelineTurnLifecycleKind::Activated,
                TimelineTurnLifecycleKind::Terminalized => {
                    WebTimelineTurnLifecycleKind::Terminalized
                }
            },
            cause_code,
        },
        SessionTimelineDetailBody::EventFact { kind } => WebSessionTimelineDetailBody::EventFact {
            kind: event_kind_dto(kind),
        },
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
        },
        member_index: continuation.member_index,
        offset_bytes: WebU64::from_u64(continuation.offset_bytes),
    }
}

pub(super) fn event_kind_dto(kind: SessionTimelineEventKind) -> WebSessionTimelineEventKind {
    match kind {
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
