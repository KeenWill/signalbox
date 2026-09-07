use super::{
    Arc, AttentionAction, AttentionActivityKind, AttentionBlockedReason, AttentionChanges,
    AttentionContinuation, AttentionGoalBlock, AttentionLifecycleState, AttentionPage,
    AttentionQuery, AttentionRepositoryError, AttentionSort, AttentionState, AttentionSummary,
    BTreeSet, Deserialize, Duration, IntoResponse, Json, Query, QueryRejection, Response,
    SessionCatalogQuery, SessionId, State, StatusCode, UNIX_EPOCH, WebApiState, WebAttentionAction,
    WebAttentionActivity, WebAttentionActivityKind, WebAttentionBlockedReason,
    WebAttentionGoalBlock, WebAttentionJudgeFacts, WebAttentionLifecycleState,
    WebAttentionSnapshot, WebAttentionState, WebAttentionStreamEvent, WebAttentionSummary,
    application_error, max_attention_goal_summary_characters, ndjson_response,
    parse_canonical_session_id, parse_catalog_canonical_u64, stream, transport_error,
    wait_for_web_shutdown,
};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct AttentionPageQuery {
    after_session_id: Option<String>,
}

pub(super) async fn attention_snapshot(
    State(state): State<WebApiState>,
    query: Result<Query<AttentionPageQuery>, QueryRejection>,
) -> Response {
    let Query(query) = match query {
        Ok(query) => query,
        Err(_) => {
            return transport_error(
                StatusCode::BAD_REQUEST,
                "invalid_query_parameters",
                "attention query parameters are invalid",
            );
        }
    };
    let Some(repository) = state.attention else {
        return application_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "attention_projection_unavailable",
            "attention projection is not configured",
        );
    };
    let continuation = match query.after_session_id {
        Some(value) => match parse_canonical_session_id(&value) {
            Ok(session) => Some(session),
            Err(()) => {
                return application_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_session_id",
                    "attention continuation is not a canonical UUID",
                );
            }
        },
        None => None,
    };
    let query = attention_page_query(continuation);
    let Some(budget) = state.snapshot_reader_budget else {
        return attention_projection_error(None);
    };
    let Ok(_permit) = budget.acquire().await else {
        return attention_projection_error(None);
    };
    match repository.page(query).await {
        Ok(snapshot) => match attention_snapshot_dto(snapshot) {
            Ok(snapshot) => Json(snapshot).into_response(),
            Err(()) => attention_projection_error(None),
        },
        Err(error) => attention_projection_error(Some(error)),
    }
}

fn attention_page_query(after: Option<SessionId>) -> AttentionQuery {
    AttentionQuery::identity_page(after)
}

pub(super) fn parse_attention_query(query: SessionCatalogQuery) -> Result<AttentionQuery, ()> {
    let sort = match query.sort.as_deref() {
        None | Some("last_activity_descending") => AttentionSort::LastActivityDescending,
        Some("session_identity_ascending") => AttentionSort::SessionIdentityAscending,
        Some(_) => return Err(()),
    };
    let include_archived = match query.include_archived.as_deref() {
        None | Some("false") => false,
        Some("true") => true,
        Some(_) => return Err(()),
    };
    let after_session = query
        .after_session_id
        .map(|value| parse_canonical_session_id(&value))
        .transpose()?;
    let after_activity_micros = query
        .after_activity_unix_microseconds
        .map(|value| parse_catalog_canonical_u64(&value))
        .transpose()?;
    if after_activity_micros.is_some_and(|value| {
        sqlx::types::time::OffsetDateTime::from_unix_timestamp_nanos(i128::from(value) * 1_000)
            .is_err()
    }) {
        return Err(());
    }
    let after_activity = after_activity_micros
        .map(|value| {
            UNIX_EPOCH
                .checked_add(Duration::from_micros(value))
                .ok_or(())
        })
        .transpose()?;
    let continuation = match (sort, after_session, after_activity) {
        (AttentionSort::LastActivityDescending, None, None)
        | (AttentionSort::SessionIdentityAscending, None, None) => None,
        (AttentionSort::LastActivityDescending, Some(session), Some(recorded_at)) => {
            Some(AttentionContinuation::LastActivity {
                recorded_at,
                session,
            })
        }
        (AttentionSort::SessionIdentityAscending, Some(session), None) => {
            Some(AttentionContinuation::SessionIdentity(session))
        }
        _ => return Err(()),
    };
    AttentionQuery::try_new(
        query.search,
        query.required_tag,
        include_archived,
        sort,
        continuation,
    )
    .map_err(|_| ())
}

pub(super) fn invalid_attention_query() -> Response {
    application_error(
        StatusCode::BAD_REQUEST,
        "invalid_session_catalog_query",
        "session catalog query parameters are malformed or outside the contract bounds",
    )
}

pub(super) async fn attention_follow(State(state): State<WebApiState>) -> Response {
    let mut shutdown = state.shutdown;
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
    let snapshot_permit = tokio::select! {
        () = wait_for_web_shutdown(&mut shutdown) => return empty_ndjson_response(),
        permit = Arc::clone(&budget).acquire_owned() => permit,
    };
    let Ok(snapshot_permit) = snapshot_permit else {
        return attention_projection_error(None);
    };
    let snapshot = match tokio::select! {
        () = wait_for_web_shutdown(&mut shutdown) => return empty_ndjson_response(),
        snapshot = repository.page(attention_page_query(None)) => snapshot,
    } {
        Ok(snapshot) => snapshot,
        Err(error) => return attention_projection_error(Some(error)),
    };
    drop(snapshot_permit);
    let cursor = snapshot.cursor;
    let live_page_has_capacity = snapshot.continuation.is_none();
    let visible_sessions = snapshot
        .summaries
        .iter()
        .map(|summary| summary.session)
        .collect::<BTreeSet<_>>();
    let snapshot = match attention_snapshot_dto(snapshot) {
        Ok(snapshot) => snapshot,
        Err(()) => return attention_projection_error(None),
    };
    let source = stream::unfold(
        (
            repository,
            Some(WebAttentionStreamEvent::Snapshot { snapshot }),
            cursor,
            visible_sessions,
            live_page_has_capacity,
            budget,
            shutdown,
            AttentionFollowDisposition::Continue,
        ),
        |(
            repository,
            pending,
            cursor,
            visible_sessions,
            live_page_has_capacity,
            budget,
            mut shutdown,
            disposition,
        )| async move {
            if shutdown.as_ref().is_some_and(|shutdown| *shutdown.borrow()) {
                return None;
            }
            if let Some(event) = pending {
                return Some((
                    event,
                    (
                        repository,
                        None,
                        cursor,
                        visible_sessions,
                        live_page_has_capacity,
                        budget,
                        shutdown,
                        AttentionFollowDisposition::Continue,
                    ),
                ));
            }
            if disposition == AttentionFollowDisposition::End {
                return None;
            }
            let mut cursor = cursor;
            let mut delay = Duration::from_millis(250);
            loop {
                tokio::select! {
                    () = wait_for_web_shutdown(&mut shutdown) => return None,
                    () = tokio::time::sleep(delay) => {}
                }
                let permit = tokio::select! {
                    () = wait_for_web_shutdown(&mut shutdown) => return None,
                    permit = Arc::clone(&budget).acquire_owned() => permit,
                };
                let Ok(_permit) = permit else {
                    return None;
                };
                let changes = tokio::select! {
                    () = wait_for_web_shutdown(&mut shutdown) => return None,
                    changes = repository.changes_after(cursor) => changes,
                };
                match changes {
                    Ok(AttentionChanges::Updated {
                        cursor: next,
                        summaries,
                    }) if summaries.is_empty() => {
                        cursor = next;
                        delay = delay.saturating_mul(2).min(Duration::from_secs(4));
                    }
                    Ok(AttentionChanges::Updated {
                        cursor: next,
                        summaries,
                    }) => {
                        if attention_changes_require_resync(
                            &summaries,
                            &visible_sessions,
                            live_page_has_capacity,
                        ) {
                            return Some((
                                WebAttentionStreamEvent::ResyncRequired {
                                    cursor: next.value().to_string(),
                                },
                                (
                                    repository,
                                    None,
                                    next,
                                    visible_sessions,
                                    live_page_has_capacity,
                                    budget,
                                    shutdown,
                                    AttentionFollowDisposition::End,
                                ),
                            ));
                        }
                        let summaries =
                            page_scoped_attention_summaries(summaries, &visible_sessions)
                                .into_iter()
                                .map(attention_summary_dto)
                                .collect::<Result<Vec<_>, _>>()
                                .ok()?;
                        if summaries.is_empty() {
                            cursor = next;
                            continue;
                        }
                        return Some((
                            WebAttentionStreamEvent::Update {
                                cursor: next.value().to_string(),
                                summaries,
                            },
                            (
                                repository,
                                None,
                                next,
                                visible_sessions,
                                live_page_has_capacity,
                                budget,
                                shutdown,
                                AttentionFollowDisposition::Continue,
                            ),
                        ));
                    }
                    Ok(AttentionChanges::ResyncRequired { cursor: next }) => {
                        return Some((
                            WebAttentionStreamEvent::ResyncRequired {
                                cursor: next.value().to_string(),
                            },
                            (
                                repository,
                                None,
                                next,
                                visible_sessions,
                                live_page_has_capacity,
                                budget,
                                shutdown,
                                AttentionFollowDisposition::End,
                            ),
                        ));
                    }
                    Err(error) => {
                        log_attention_projection_error(&error);
                        return None;
                    }
                }
            }
        },
    );
    ndjson_response(source)
}

pub(super) fn page_scoped_attention_summaries(
    summaries: Vec<AttentionSummary>,
    visible_sessions: &BTreeSet<SessionId>,
) -> Vec<AttentionSummary> {
    summaries
        .into_iter()
        .filter(|summary| visible_sessions.contains(&summary.session))
        .collect()
}

pub(super) fn attention_changes_require_resync(
    summaries: &[AttentionSummary],
    visible_sessions: &BTreeSet<SessionId>,
    live_page_has_capacity: bool,
) -> bool {
    let page_boundary = visible_sessions.last();
    summaries.iter().any(|summary| {
        !visible_sessions.contains(&summary.session)
            && (live_page_has_capacity
                || page_boundary.is_some_and(|boundary| summary.session < *boundary))
    })
}

pub(super) fn empty_ndjson_response() -> Response {
    ndjson_response(stream::empty::<WebAttentionStreamEvent>())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AttentionFollowDisposition {
    Continue,
    End,
}

pub(super) fn attention_snapshot_dto(snapshot: AttentionPage) -> Result<WebAttentionSnapshot, ()> {
    if snapshot.sort != AttentionSort::SessionIdentityAscending {
        return Err(());
    }
    let continuation_after_session_id = match snapshot.continuation {
        Some(AttentionContinuation::SessionIdentity(session)) => {
            Some(session.into_uuid().to_string())
        }
        None => None,
        Some(AttentionContinuation::LastActivity { .. }) => return Err(()),
    };
    Ok(WebAttentionSnapshot {
        cursor: snapshot.cursor.value().to_string(),
        summaries: snapshot
            .summaries
            .into_iter()
            .map(attention_summary_dto)
            .collect::<Result<Vec<_>, _>>()?,
        continuation_after_session_id,
    })
}

pub(crate) fn attention_summary_dto(summary: AttentionSummary) -> Result<WebAttentionSummary, ()> {
    let unix_milliseconds = summary
        .last_activity
        .recorded_at
        .duration_since(UNIX_EPOCH)
        .map_err(|_| ())?
        .as_millis()
        .to_string();
    let goal_block = attention_goal_block_dto(summary.goal_block)?;
    Ok(WebAttentionSummary {
        session_id: summary.session.into_uuid().to_string(),
        current_turn_id: summary
            .current_turn
            .map(|turn| turn.into_uuid().to_string()),
        state: web_attention_state(summary.state),
        lifecycle_state: web_attention_lifecycle_state(summary.lifecycle_state),
        action: summary.action.map(web_attention_action),
        goal_block,
        judge: WebAttentionJudgeFacts {
            actionable: summary.judge.actionable.to_string(),
            completed: summary.judge.completed.to_string(),
            escalated: summary.judge.escalated.to_string(),
            failed: summary.judge.failed.to_string(),
        },
        last_activity: WebAttentionActivity {
            unix_milliseconds,
            kind: web_attention_activity_kind(summary.last_activity.kind),
        },
    })
}

pub(super) fn attention_goal_block_dto(
    goal: Option<AttentionGoalBlock>,
) -> Result<Option<WebAttentionGoalBlock>, ()> {
    goal.map(|goal| {
        if goal.need_summary.chars().count() > usize::from(max_attention_goal_summary_characters())
        {
            return Err(());
        }
        Ok(WebAttentionGoalBlock {
            generation: goal.generation.to_string(),
            reason: match goal.reason {
                AttentionBlockedReason::UserInputRequired => {
                    WebAttentionBlockedReason::UserInputRequired
                }
                AttentionBlockedReason::ExternalChangeRequired => {
                    WebAttentionBlockedReason::ExternalChangeRequired
                }
                AttentionBlockedReason::AuthorizationRequired => {
                    WebAttentionBlockedReason::AuthorizationRequired
                }
                AttentionBlockedReason::ExecutionFailure => {
                    WebAttentionBlockedReason::ExecutionFailure
                }
                AttentionBlockedReason::FinishCheckFailed => {
                    WebAttentionBlockedReason::FinishCheckFailed
                }
            },
            need_summary: goal.need_summary,
        })
    })
    .transpose()
}

const fn web_attention_lifecycle_state(
    state: AttentionLifecycleState,
) -> WebAttentionLifecycleState {
    match state {
        AttentionLifecycleState::Created => WebAttentionLifecycleState::Created,
        AttentionLifecycleState::Dispatched => WebAttentionLifecycleState::Dispatched,
        AttentionLifecycleState::Active => WebAttentionLifecycleState::Active,
        AttentionLifecycleState::Waiting => WebAttentionLifecycleState::Waiting,
        AttentionLifecycleState::Recovering => WebAttentionLifecycleState::Recovering,
        AttentionLifecycleState::Blocked => WebAttentionLifecycleState::Blocked,
        AttentionLifecycleState::Parked => WebAttentionLifecycleState::Parked,
        AttentionLifecycleState::Terminal => WebAttentionLifecycleState::Terminal,
    }
}

pub(super) const fn web_attention_state(state: AttentionState) -> WebAttentionState {
    match state {
        AttentionState::Active => WebAttentionState::Active,
        AttentionState::Queued => WebAttentionState::Queued,
        AttentionState::Blocked => WebAttentionState::Blocked,
        AttentionState::AwaitingApproval => WebAttentionState::AwaitingApproval,
        AttentionState::Ambiguous => WebAttentionState::Ambiguous,
        AttentionState::AwaitingToolRecovery => WebAttentionState::AwaitingToolRecovery,
        AttentionState::AwaitingReconciliation => WebAttentionState::AwaitingReconciliation,
        AttentionState::RunnerLost => WebAttentionState::RunnerLost,
        AttentionState::Parked => WebAttentionState::Parked,
        AttentionState::Idle => WebAttentionState::Idle,
    }
}

pub(super) const fn web_attention_action(action: AttentionAction) -> WebAttentionAction {
    match action {
        AttentionAction::ProvideGoalNeed => WebAttentionAction::ProvideGoalNeed,
        AttentionAction::DecideApproval => WebAttentionAction::DecideApproval,
        AttentionAction::ReconcileTurn => WebAttentionAction::ReconcileTurn,
    }
}

pub(super) const fn web_attention_activity_kind(
    kind: AttentionActivityKind,
) -> WebAttentionActivityKind {
    match kind {
        AttentionActivityKind::Session => WebAttentionActivityKind::Session,
        AttentionActivityKind::Turn => WebAttentionActivityKind::Turn,
        AttentionActivityKind::Goal => WebAttentionActivityKind::Goal,
        AttentionActivityKind::ApprovalJudge => WebAttentionActivityKind::ApprovalJudge,
        AttentionActivityKind::Runner => WebAttentionActivityKind::Runner,
    }
}

pub(super) fn attention_projection_error(error: Option<AttentionRepositoryError>) -> Response {
    if let Some(error) = error.as_ref() {
        log_attention_projection_error(error);
    }
    application_error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "attention_projection_failed",
        "the attention projection could not be read",
    )
}

fn log_attention_projection_error(error: &AttentionRepositoryError) {
    let failure_class = match error {
        AttentionRepositoryError::Database(_) => "infrastructure",
        AttentionRepositoryError::Corruption(_) => "fail_closed_corruption",
    };
    tracing::error!(failure_class, cause = %error, "attention projection read failed");
}
