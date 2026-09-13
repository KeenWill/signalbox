use super::{
    AttentionContinuation, AttentionSnapshot, AttentionSort, AttentionSummary, IntoResponse, Json,
    RawQuery, Response, SessionId, State, StatusCode, UNIX_EPOCH, Uuid, WebApiState,
    WebAttentionJudgeFacts, WebSessionCatalogActivity, WebSessionCatalogContinuation,
    WebSessionCatalogSnapshot, WebSessionCatalogSort, WebSessionCatalogSummary, WebSessionId,
    WebU64, WebUuid, application_error, attention_goal_block_dto, attention_projection_error,
    invalid_attention_query, max_attention_filter_tags, max_attention_filter_utf8_bytes,
    max_attention_title_characters, parse_attention_query, web_attention_action,
    web_attention_activity_kind, web_attention_state,
};

#[derive(Debug, Default)]
pub(super) struct SessionCatalogQuery {
    pub(super) search: Option<String>,
    pub(super) required_tag: Vec<String>,
    pub(super) include_archived: Option<String>,
    pub(super) sort: Option<String>,
    pub(super) after_session_id: Option<String>,
    pub(super) after_activity_unix_microseconds: Option<String>,
}

pub(super) async fn session_catalog(
    State(state): State<WebApiState>,
    RawQuery(query): RawQuery,
) -> Response {
    let query = match parse_session_catalog_query(query.as_deref()) {
        Ok(query) => query,
        Err(()) => return invalid_attention_query(),
    };
    let query = match parse_attention_query(query) {
        Ok(query) => query,
        Err(()) => return invalid_attention_query(),
    };
    let Some(repository) = state.attention else {
        return application_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "attention_projection_unavailable",
            "attention projection is not configured",
        );
    };
    let Some(budget) = state.snapshot_reader_budget else {
        return attention_projection_error(None);
    };
    let Ok(_permit) = budget.acquire().await else {
        return attention_projection_error(None);
    };
    match repository.snapshot(query).await {
        Ok(snapshot) => match session_catalog_snapshot_dto(snapshot) {
            Ok(snapshot) => Json(snapshot).into_response(),
            Err(()) => attention_projection_error(None),
        },
        Err(error) => attention_projection_error(Some(error)),
    }
}

pub(super) fn parse_session_catalog_query(raw: Option<&str>) -> Result<SessionCatalogQuery, ()> {
    let mut query = SessionCatalogQuery::default();
    let mut filter_bytes = 0_usize;
    for (key, value) in url::form_urlencoded::parse(raw.unwrap_or_default().as_bytes()) {
        let value = value.into_owned();
        match key.as_ref() {
            "search" => {
                filter_bytes = filter_bytes.checked_add(value.len()).ok_or(())?;
                if filter_bytes > usize::from(max_attention_filter_utf8_bytes()) {
                    return Err(());
                }
                set_once(&mut query.search, value)?;
            }
            "required_tag" => {
                if query.required_tag.len() >= usize::from(max_attention_filter_tags()) {
                    return Err(());
                }
                filter_bytes = filter_bytes.checked_add(value.len()).ok_or(())?;
                if filter_bytes > usize::from(max_attention_filter_utf8_bytes()) {
                    return Err(());
                }
                query.required_tag.push(value);
            }
            "include_archived" => set_once(&mut query.include_archived, value)?,
            "sort" => set_once(&mut query.sort, value)?,
            "after_session_id" => set_once(&mut query.after_session_id, value)?,
            "after_activity_unix_microseconds" => {
                set_once(&mut query.after_activity_unix_microseconds, value)?;
            }
            _ => return Err(()),
        }
    }
    Ok(query)
}

fn set_once(target: &mut Option<String>, value: String) -> Result<(), ()> {
    if target.replace(value).is_some() {
        return Err(());
    }
    Ok(())
}

pub(super) fn parse_catalog_canonical_u64(value: &str) -> Result<u64, ()> {
    let parsed = value.parse::<u64>().map_err(|_| ())?;
    (parsed.to_string() == value).then_some(parsed).ok_or(())
}

pub(super) fn parse_canonical_session_id(value: &str) -> Result<SessionId, ()> {
    let parsed = value.parse::<Uuid>().map_err(|_| ())?;
    if value != parsed.hyphenated().to_string() {
        return Err(());
    }
    Ok(SessionId::from_uuid(parsed))
}

fn session_catalog_snapshot_dto(
    snapshot: AttentionSnapshot,
) -> Result<WebSessionCatalogSnapshot, ()> {
    let continuation = snapshot
        .continuation
        .map(|continuation| match continuation {
            AttentionContinuation::LastActivity {
                recorded_at,
                session,
            } => Ok(WebSessionCatalogContinuation::LastActivity {
                unix_microseconds: WebU64::from_u64(
                    recorded_at
                        .duration_since(UNIX_EPOCH)
                        .map_err(|_| ())?
                        .as_micros()
                        .try_into()
                        .map_err(|_| ())?,
                ),
                session_id: WebSessionId::from_uuid_bytes(session.into_uuid().into_bytes()),
            }),
            AttentionContinuation::SessionIdentity(session) => {
                Ok(WebSessionCatalogContinuation::SessionIdentity {
                    session_id: WebSessionId::from_uuid_bytes(session.into_uuid().into_bytes()),
                })
            }
        })
        .transpose()?;
    Ok(WebSessionCatalogSnapshot {
        cursor: WebU64::from_u64(snapshot.cursor.value()),
        total: WebU64::from_u64(snapshot.total),
        sort: match snapshot.sort {
            AttentionSort::LastActivityDescending => WebSessionCatalogSort::LastActivityDescending,
            AttentionSort::SessionIdentityAscending => {
                WebSessionCatalogSort::SessionIdentityAscending
            }
        },
        summaries: snapshot
            .summaries
            .into_iter()
            .map(session_catalog_summary_dto)
            .collect::<Result<Vec<_>, _>>()?,
        continuation,
    })
}

pub(super) fn session_catalog_summary_dto(
    summary: AttentionSummary,
) -> Result<WebSessionCatalogSummary, ()> {
    if summary
        .title_summary
        .as_ref()
        .is_some_and(|title| title.chars().count() > usize::from(max_attention_title_characters()))
    {
        return Err(());
    }
    let unix_microseconds = summary
        .last_activity
        .recorded_at
        .duration_since(UNIX_EPOCH)
        .map_err(|_| ())?
        .as_micros()
        .try_into()
        .map_err(|_| ())?;
    let goal_block = attention_goal_block_dto(summary.goal_block)?;
    Ok(WebSessionCatalogSummary {
        session_id: WebSessionId::from_uuid_bytes(summary.session.into_uuid().into_bytes()),
        title_summary: summary.title_summary,
        title_truncated: summary.title_truncated,
        archived: summary.archived,
        current_turn_id: summary
            .current_turn
            .map(|turn| WebUuid::from_validated_uuid(turn.into_uuid().to_string())),
        active_turn_count: WebU64::from_u64(summary.active_turn_count),
        queued_turn_count: WebU64::from_u64(summary.queued_turn_count),
        state: web_attention_state(summary.state),
        action: summary.action.map(web_attention_action),
        goal_block,
        judge: WebAttentionJudgeFacts {
            actionable: summary.judge.actionable.to_string(),
            completed: summary.judge.completed.to_string(),
            escalated: summary.judge.escalated.to_string(),
            failed: summary.judge.failed.to_string(),
        },
        last_activity: WebSessionCatalogActivity {
            unix_microseconds: WebU64::from_u64(unix_microseconds),
            kind: web_attention_activity_kind(summary.last_activity.kind),
        },
    })
}
