//! Read-only browser adapter for bounded repository-watch operator projections.

use std::{
    num::NonZeroU64,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use axum::{
    Router,
    extract::{Query, State, rejection::QueryRejection},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
};
use serde::{Deserialize, Serialize};
use signalbox_application::{
    RepoWatchActivityPage, RepoWatchAutomationStatus, RepoWatchChecksStatus, RepoWatchDraftStatus,
    RepoWatchEventCursor, RepoWatchHeldCursor, RepoWatchHeldSlot, RepoWatchHeldSlotBlocker,
    RepoWatchObligationCursor, RepoWatchObligationReadiness, RepoWatchOperatorDispatch,
    RepoWatchOperatorEvent, RepoWatchOperatorSettlement, RepoWatchPagePosition,
    RepoWatchPullRequestLifecycle, RepoWatchPullRequestOperations, RepoWatchPullRequestPage,
    RepoWatchPullRequestSession, RepoWatchPullRequestSessionPage, RepoWatchQueuedObligation,
    RepoWatchRepositoryStatus, RepoWatchRepositoryStatusPage, RepoWatchReviewDecision,
    RepoWatchSessionCursor, RepoWatchSessionPurpose, RepoWatchSingletonKey,
    RepoWatchWebhookActivity, RepoWatchWebhookDisposition, RepoWatchWebhookWindow,
    RepoWatchWorkPage,
};
use signalbox_domain::{
    MergeableState, PullRequestNumber, RepoWatchDispatchId, RepoWatchEventKindNameV1,
    RepositorySlug, SessionId,
};
use signalbox_persistence::repo_watch_operations::{
    PostgresRepoWatchOperations, RepoWatchOperationsError,
};
use signalbox_web_contract::{
    MAX_JSON_BODY_BYTES, WebRepoWatchActivityPage, WebRepoWatchAutomationStatus,
    WebRepoWatchChecksStatus, WebRepoWatchDispatch, WebRepoWatchDraftStatus, WebRepoWatchEvent,
    WebRepoWatchEventCursor, WebRepoWatchEventKind, WebRepoWatchEventKindCount,
    WebRepoWatchHeldCursor, WebRepoWatchHeldSlot, WebRepoWatchHeldSlotBlocker,
    WebRepoWatchLatestWebhook, WebRepoWatchLifecycle, WebRepoWatchMergeable,
    WebRepoWatchObligationCursor, WebRepoWatchObligationReadiness, WebRepoWatchPullRequest,
    WebRepoWatchPullRequestPage, WebRepoWatchPullRequestSession,
    WebRepoWatchPullRequestSessionPage, WebRepoWatchQueuedObligation, WebRepoWatchRepositoryStatus,
    WebRepoWatchRepositoryStatusPage, WebRepoWatchReviewDecision, WebRepoWatchSessionCursor,
    WebRepoWatchSessionPurpose, WebRepoWatchSettlement, WebRepoWatchSingletonScope,
    WebRepoWatchWebhookActivity, WebRepoWatchWebhookDisposition, WebRepoWatchWebhookWindow,
    WebRepoWatchWorkPage,
};
use sqlx::{
    PgPool,
    types::{Uuid, time::OffsetDateTime},
};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::web_http::{application_error, attention_summary_dto, transport_error};

#[derive(Clone, Debug)]
struct RepoWatchApiState {
    operations: Option<PostgresRepoWatchOperations>,
    snapshot_reader_budget: Option<Arc<Semaphore>>,
}

pub(crate) fn router(
    pool: Option<PgPool>,
    snapshot_reader_budget: Option<Arc<Semaphore>>,
) -> Router {
    Router::new()
        .route("/repository-watch/repositories", get(repository_statuses))
        .route("/repository-watch/pull-requests", get(pull_requests))
        .route("/repository-watch/work", get(work))
        .route("/repository-watch/sessions", get(pull_request_sessions))
        .route("/repository-watch/activity", get(activity))
        .with_state(RepoWatchApiState {
            operations: pool.map(PostgresRepoWatchOperations::new),
            snapshot_reader_budget,
        })
}

async fn snapshot_permit(state: &RepoWatchApiState) -> Result<OwnedSemaphorePermit, Response> {
    let Some(budget) = state.snapshot_reader_budget.as_ref() else {
        return Err(projection_error(None));
    };
    Arc::clone(budget)
        .acquire_owned()
        .await
        .map_err(|_| projection_error(None))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RepositoryStatusesQuery {
    after_repository: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PullRequestsQuery {
    repository: String,
    after_pull_request: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkQuery {
    repository: String,
    held_after_unix_milliseconds: Option<String>,
    held_after_dispatch_id: Option<String>,
    obligation_after_unix_milliseconds: Option<String>,
    obligation_after_id: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PullRequestSessionsQuery {
    repository: String,
    pull_request: String,
    before_unix_milliseconds: Option<String>,
    before_session_id: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ActivityQuery {
    repository: String,
    event_before_cursor_generation: Option<String>,
    event_before_ordinal: Option<u32>,
    webhook_before_receipt_sequence: Option<String>,
    include_events: Option<bool>,
    include_webhooks: Option<bool>,
}

fn typed_query<T>(query: Result<Query<T>, QueryRejection>) -> Result<T, Box<Response>> {
    query.map(|Query(query)| query).map_err(|_| {
        Box::new(transport_error(
            StatusCode::BAD_REQUEST,
            "invalid_query",
            "query parameters are invalid",
        ))
    })
}

async fn repository_statuses(
    State(state): State<RepoWatchApiState>,
    query: Result<Query<RepositoryStatusesQuery>, QueryRejection>,
) -> Response {
    let query = match typed_query(query) {
        Ok(query) => query,
        Err(response) => return *response,
    };
    let Some(operations) = state.operations.clone() else {
        return unavailable();
    };
    let after = match query.after_repository.map(repository_slug).transpose() {
        Ok(after) => after,
        Err(()) => return invalid_query("invalid_repository", "repository is not canonical"),
    };
    let _permit = match snapshot_permit(&state).await {
        Ok(permit) => permit,
        Err(response) => return response,
    };
    match operations.repository_statuses(after).await {
        Ok(page) => match repository_status_page_dto(page) {
            Ok(page) => bounded_projection_response(page),
            Err(()) => projection_error(None),
        },
        Err(error) => projection_error(Some(error)),
    }
}

async fn pull_requests(
    State(state): State<RepoWatchApiState>,
    query: Result<Query<PullRequestsQuery>, QueryRejection>,
) -> Response {
    let query = match typed_query(query) {
        Ok(query) => query,
        Err(response) => return *response,
    };
    let Some(operations) = state.operations.clone() else {
        return unavailable();
    };
    let repository = match repository_slug(query.repository) {
        Ok(repository) => repository,
        Err(()) => return invalid_query("invalid_repository", "repository is not canonical"),
    };
    let after = match query
        .after_pull_request
        .map(pull_request_number)
        .transpose()
    {
        Ok(after) => after,
        Err(()) => return invalid_query("invalid_cursor", "pull-request cursor is invalid"),
    };
    let _permit = match snapshot_permit(&state).await {
        Ok(permit) => permit,
        Err(response) => return response,
    };
    match operations.pull_requests(repository, after).await {
        Ok(page) => match pull_request_page_dto(page) {
            Ok(page) => bounded_projection_response(page),
            Err(()) => projection_error(None),
        },
        Err(error) => projection_error(Some(error)),
    }
}

async fn work(
    State(state): State<RepoWatchApiState>,
    query: Result<Query<WorkQuery>, QueryRejection>,
) -> Response {
    let query = match typed_query(query) {
        Ok(query) => query,
        Err(response) => return *response,
    };
    let Some(operations) = state.operations.clone() else {
        return unavailable();
    };
    let repository = match repository_slug(query.repository) {
        Ok(repository) => repository,
        Err(()) => return invalid_query("invalid_repository", "repository is not canonical"),
    };
    let held_after = match held_cursor(
        query.held_after_unix_milliseconds,
        query.held_after_dispatch_id,
    ) {
        Ok(cursor) => cursor,
        Err(()) => return invalid_query("invalid_cursor", "held cursor is incomplete or invalid"),
    };
    let obligation_after = match obligation_cursor(
        query.obligation_after_unix_milliseconds,
        query.obligation_after_id,
    ) {
        Ok(cursor) => cursor,
        Err(()) => {
            return invalid_query(
                "invalid_cursor",
                "obligation cursor is incomplete or invalid",
            );
        }
    };
    let held_after = held_after.map_or(RepoWatchPagePosition::Start, RepoWatchPagePosition::After);
    let obligation_after =
        obligation_after.map_or(RepoWatchPagePosition::Start, RepoWatchPagePosition::After);
    let _permit = match snapshot_permit(&state).await {
        Ok(permit) => permit,
        Err(response) => return response,
    };
    match operations
        .work(repository, held_after, obligation_after)
        .await
    {
        Ok(page) => match work_page_dto(page) {
            Ok(page) => bounded_projection_response(page),
            Err(()) => projection_error(None),
        },
        Err(error) => projection_error(Some(error)),
    }
}

async fn pull_request_sessions(
    State(state): State<RepoWatchApiState>,
    query: Result<Query<PullRequestSessionsQuery>, QueryRejection>,
) -> Response {
    let query = match typed_query(query) {
        Ok(query) => query,
        Err(response) => return *response,
    };
    let Some(operations) = state.operations.clone() else {
        return unavailable();
    };
    let repository = match repository_slug(query.repository) {
        Ok(repository) => repository,
        Err(()) => return invalid_query("invalid_repository", "repository is not canonical"),
    };
    let pull_request = match pull_request_number(query.pull_request) {
        Ok(pull_request) => pull_request,
        Err(()) => return invalid_query("invalid_pull_request", "pull request is not positive"),
    };
    let before = match session_cursor(query.before_unix_milliseconds, query.before_session_id) {
        Ok(cursor) => cursor,
        Err(()) => {
            return invalid_query("invalid_cursor", "session cursor is incomplete or invalid");
        }
    };
    let _permit = match snapshot_permit(&state).await {
        Ok(permit) => permit,
        Err(response) => return response,
    };
    match operations
        .pull_request_sessions(repository, pull_request, before)
        .await
    {
        Ok(page) => match pull_request_session_page_dto(page) {
            Ok(page) => bounded_projection_response(page),
            Err(()) => projection_error(None),
        },
        Err(error) => projection_error(Some(error)),
    }
}

async fn activity(
    State(state): State<RepoWatchApiState>,
    query: Result<Query<ActivityQuery>, QueryRejection>,
) -> Response {
    let query = match typed_query(query) {
        Ok(query) => query,
        Err(response) => return *response,
    };
    let Some(operations) = state.operations.clone() else {
        return unavailable();
    };
    let repository = match repository_slug(query.repository) {
        Ok(repository) => repository,
        Err(()) => return invalid_query("invalid_repository", "repository is not canonical"),
    };
    let events_before = match event_cursor(
        query.event_before_cursor_generation,
        query.event_before_ordinal,
    ) {
        Ok(cursor) => cursor,
        Err(()) => return invalid_query("invalid_cursor", "event cursor is incomplete or invalid"),
    };
    let webhooks_before = match query
        .webhook_before_receipt_sequence
        .map(|value| postgres_bigint(&value))
        .transpose()
    {
        Ok(cursor) => cursor,
        Err(()) => return invalid_query("invalid_cursor", "webhook cursor is invalid"),
    };
    let include_events = query.include_events.unwrap_or(true);
    let include_webhooks = query.include_webhooks.unwrap_or(true);
    if let Err((code, message)) = validate_activity_window(
        include_events,
        include_webhooks,
        events_before.is_some(),
        webhooks_before.is_some(),
    ) {
        return invalid_query(code, message);
    }
    let events_before = if include_events {
        events_before.map_or(RepoWatchPagePosition::Start, RepoWatchPagePosition::After)
    } else {
        RepoWatchPagePosition::Exhausted
    };
    let webhooks_before = if include_webhooks {
        webhooks_before.map_or(RepoWatchPagePosition::Start, RepoWatchPagePosition::After)
    } else {
        RepoWatchPagePosition::Exhausted
    };
    let _permit = match snapshot_permit(&state).await {
        Ok(permit) => permit,
        Err(response) => return response,
    };
    match operations
        .activity(repository, events_before, webhooks_before)
        .await
    {
        Ok(page) => match activity_page_dto(page, include_events, include_webhooks) {
            Ok(page) => bounded_projection_response(page),
            Err(()) => projection_error(None),
        },
        Err(error) => projection_error(Some(error)),
    }
}

fn validate_activity_window(
    include_events: bool,
    include_webhooks: bool,
    has_event_cursor: bool,
    has_webhook_cursor: bool,
) -> Result<(), (&'static str, &'static str)> {
    if !include_events && !include_webhooks {
        return Err((
            "invalid_window",
            "at least one activity feed must be included",
        ));
    }
    if (!include_events && has_event_cursor) || (!include_webhooks && has_webhook_cursor) {
        return Err((
            "invalid_cursor",
            "an excluded activity feed cannot carry a cursor",
        ));
    }
    Ok(())
}

fn repository_slug(value: String) -> Result<RepositorySlug, ()> {
    RepositorySlug::try_new(value).map_err(|_| ())
}

fn pull_request_number(value: String) -> Result<PullRequestNumber, ()> {
    let value = value.parse::<u64>().map_err(|_| ())?;
    let value = NonZeroU64::new(value).ok_or(())?;
    Ok(PullRequestNumber::new(value))
}

fn timestamp(value: String) -> Result<SystemTime, ()> {
    let milliseconds = value.parse::<u64>().map_err(|_| ())?;
    let nanoseconds = i128::from(milliseconds).checked_mul(1_000_000).ok_or(())?;
    let database_time = OffsetDateTime::from_unix_timestamp_nanos(nanoseconds).map_err(|_| ())?;
    Ok(SystemTime::from(database_time))
}

fn uuid(value: String) -> Result<Uuid, ()> {
    value.parse().map_err(|_| ())
}

fn held_cursor(
    milliseconds: Option<String>,
    dispatch: Option<String>,
) -> Result<Option<RepoWatchHeldCursor>, ()> {
    match (milliseconds, dispatch) {
        (None, None) => Ok(None),
        (Some(milliseconds), Some(dispatch)) => Ok(Some(RepoWatchHeldCursor {
            held_since: timestamp(milliseconds)?,
            dispatch: RepoWatchDispatchId::from_uuid(uuid(dispatch)?),
        })),
        _ => Err(()),
    }
}

fn obligation_cursor(
    milliseconds: Option<String>,
    obligation: Option<String>,
) -> Result<Option<RepoWatchObligationCursor>, ()> {
    match (milliseconds, obligation) {
        (None, None) => Ok(None),
        (Some(milliseconds), Some(obligation)) => Ok(Some(RepoWatchObligationCursor {
            owed_since: timestamp(milliseconds)?,
            obligation: signalbox_application::RepoWatchObligationId::from_uuid(uuid(obligation)?),
        })),
        _ => Err(()),
    }
}

fn session_cursor(
    milliseconds: Option<String>,
    session: Option<String>,
) -> Result<Option<RepoWatchSessionCursor>, ()> {
    match (milliseconds, session) {
        (None, None) => Ok(None),
        (Some(milliseconds), Some(session)) => Ok(Some(RepoWatchSessionCursor {
            commissioned_at: timestamp(milliseconds)?,
            session: SessionId::from_uuid(uuid(session)?),
        })),
        _ => Err(()),
    }
}

fn event_cursor(
    generation: Option<String>,
    ordinal: Option<u32>,
) -> Result<Option<RepoWatchEventCursor>, ()> {
    match (generation, ordinal) {
        (None, None) => Ok(None),
        (Some(generation), Some(event_ordinal)) if i32::try_from(event_ordinal).is_ok() => {
            Ok(Some(RepoWatchEventCursor {
                cursor_generation: postgres_bigint(&generation)?,
                event_ordinal,
            }))
        }
        _ => Err(()),
    }
}

fn postgres_bigint(value: &str) -> Result<u64, ()> {
    let value = value.parse::<u64>().map_err(|_| ())?;
    i64::try_from(value).map_err(|_| ())?;
    Ok(value)
}

fn unix_milliseconds(value: SystemTime) -> Result<String, ()> {
    Ok(value
        .duration_since(UNIX_EPOCH)
        .map_err(|_| ())?
        .as_millis()
        .to_string())
}

fn event_kind(kind: RepoWatchEventKindNameV1) -> WebRepoWatchEventKind {
    match kind {
        RepoWatchEventKindNameV1::PullRequestOpened => WebRepoWatchEventKind::PullRequestOpened,
        RepoWatchEventKindNameV1::PullRequestClosed => WebRepoWatchEventKind::PullRequestClosed,
        RepoWatchEventKindNameV1::PullRequestMerged => WebRepoWatchEventKind::PullRequestMerged,
        RepoWatchEventKindNameV1::HeadChanged => WebRepoWatchEventKind::HeadChanged,
        RepoWatchEventKindNameV1::MergeableStateChanged => {
            WebRepoWatchEventKind::MergeableStateChanged
        }
        RepoWatchEventKindNameV1::ChecksCompleted => WebRepoWatchEventKind::ChecksCompleted,
        RepoWatchEventKindNameV1::CheckRunCompleted => WebRepoWatchEventKind::CheckRunCompleted,
        RepoWatchEventKindNameV1::BranchWorkflowRunCompleted => {
            WebRepoWatchEventKind::BranchWorkflowRunCompleted
        }
        RepoWatchEventKindNameV1::ReviewSubmitted => WebRepoWatchEventKind::ReviewSubmitted,
        RepoWatchEventKindNameV1::ThreadOpened => WebRepoWatchEventKind::ThreadOpened,
        RepoWatchEventKindNameV1::ThreadResolved => WebRepoWatchEventKind::ThreadResolved,
        RepoWatchEventKindNameV1::Labeled => WebRepoWatchEventKind::Labeled,
        RepoWatchEventKindNameV1::Unlabeled => WebRepoWatchEventKind::Unlabeled,
        RepoWatchEventKindNameV1::BaseAdvanced => WebRepoWatchEventKind::BaseAdvanced,
        RepoWatchEventKindNameV1::ReactionChanged => WebRepoWatchEventKind::ReactionChanged,
    }
}

fn event_dto(event: RepoWatchOperatorEvent) -> Result<WebRepoWatchEvent, ()> {
    Ok(WebRepoWatchEvent {
        id: event.id.into_uuid().to_string(),
        cursor_generation: event.cursor_generation.to_string(),
        event_ordinal: event.event_ordinal,
        kind: event_kind(event.kind),
        pull_request: event.pull_request.map(|number| number.get().to_string()),
        observed_at_unix_milliseconds: unix_milliseconds(event.observed_at)?,
    })
}

fn dispatch_dto(dispatch: RepoWatchOperatorDispatch) -> Result<WebRepoWatchDispatch, ()> {
    Ok(WebRepoWatchDispatch {
        id: dispatch.id.into_uuid().to_string(),
        event_id: dispatch.event.into_uuid().to_string(),
        rule: dispatch.rule.into_string(),
        attempted_at_unix_milliseconds: unix_milliseconds(dispatch.attempted_at)?,
    })
}

fn settlement_dto(settlement: RepoWatchOperatorSettlement) -> Result<WebRepoWatchSettlement, ()> {
    Ok(WebRepoWatchSettlement {
        dispatch_id: settlement.dispatch.into_uuid().to_string(),
        event_id: settlement.event.into_uuid().to_string(),
        settled_at_unix_milliseconds: unix_milliseconds(settlement.settled_at)?,
    })
}

fn webhook_window_dto(window: RepoWatchWebhookWindow) -> WebRepoWatchWebhookWindow {
    WebRepoWatchWebhookWindow {
        seconds: window.seconds,
        received: window.received.to_string(),
        projected: window.projected.to_string(),
        terminal: window.terminal.to_string(),
        quarantined: window.quarantined.to_string(),
    }
}

fn repository_status_dto(
    status: RepoWatchRepositoryStatus,
) -> Result<WebRepoWatchRepositoryStatus, ()> {
    Ok(WebRepoWatchRepositoryStatus {
        repository: status.repository.into_string(),
        cursor_generation: status.cursor_generation.map(|value| value.to_string()),
        observed_at_unix_milliseconds: status.observed_at.map(unix_milliseconds).transpose()?,
        latest_webhook: status
            .latest_webhook
            .map(|webhook| {
                Ok(WebRepoWatchLatestWebhook {
                    receipt_sequence: webhook.receipt_sequence.to_string(),
                    event_name: webhook.event_name,
                    action_name: webhook.action_name,
                    received_at_unix_milliseconds: unix_milliseconds(webhook.received_at)?,
                })
            })
            .transpose()?,
        previous_five_minutes: webhook_window_dto(status.previous_five_minutes),
        previous_hour: webhook_window_dto(status.previous_hour),
        latest_projection_latency_milliseconds: status
            .latest_projection_latency_milliseconds
            .map(|value| value.to_string()),
        maximum_projection_latency_milliseconds_previous_hour: status
            .maximum_projection_latency_milliseconds_previous_hour
            .map(|value| value.to_string()),
        event_kind_counts_previous_hour: status
            .event_kind_counts_previous_hour
            .into_iter()
            .map(|item| WebRepoWatchEventKindCount {
                kind: event_kind(item.kind),
                count: item.count.to_string(),
            })
            .collect(),
        last_observed_event: status.last_observed_event.map(event_dto).transpose()?,
        last_actionable_event: status.last_actionable_event.map(event_dto).transpose()?,
        last_dispatch_attempt: status.last_dispatch_attempt.map(dispatch_dto).transpose()?,
        last_automation_settlement: status
            .last_automation_settlement
            .map(settlement_dto)
            .transpose()?,
        held_slot_count: status.held_slot_count.to_string(),
        queued_obligation_count: status.queued_obligation_count.to_string(),
    })
}

fn repository_status_page_dto(
    page: RepoWatchRepositoryStatusPage,
) -> Result<WebRepoWatchRepositoryStatusPage, ()> {
    Ok(WebRepoWatchRepositoryStatusPage {
        repositories: page
            .repositories
            .into_iter()
            .map(repository_status_dto)
            .collect::<Result<Vec<_>, _>>()?,
        continuation_after_repository: page.continuation_after.map(RepositorySlug::into_string),
    })
}

fn automation_dto(status: RepoWatchAutomationStatus) -> Result<WebRepoWatchAutomationStatus, ()> {
    Ok(match status {
        RepoWatchAutomationStatus::Unattempted => WebRepoWatchAutomationStatus::Unattempted {},
        RepoWatchAutomationStatus::Held { dispatch } => WebRepoWatchAutomationStatus::Held {
            dispatch_id: dispatch.into_uuid().to_string(),
        },
        RepoWatchAutomationStatus::Queued { latest_event } => {
            WebRepoWatchAutomationStatus::Queued {
                latest_event_id: latest_event.into_uuid().to_string(),
            }
        }
        RepoWatchAutomationStatus::NonConverged { dispatch } => {
            WebRepoWatchAutomationStatus::NonConverged {
                dispatch_id: dispatch.into_uuid().to_string(),
            }
        }
        RepoWatchAutomationStatus::StaleSeal {
            dispatch,
            sealed_event,
        } => WebRepoWatchAutomationStatus::StaleSeal {
            dispatch_id: dispatch.into_uuid().to_string(),
            sealed_event_id: sealed_event.into_uuid().to_string(),
        },
        RepoWatchAutomationStatus::CurrentHeadSealed {
            dispatch,
            sealed_event,
            settled_at,
        } => WebRepoWatchAutomationStatus::CurrentHeadSealed {
            dispatch_id: dispatch.into_uuid().to_string(),
            sealed_event_id: sealed_event.into_uuid().to_string(),
            settled_at_unix_milliseconds: unix_milliseconds(settled_at)?,
        },
    })
}

fn pull_request_dto(
    pull_request: RepoWatchPullRequestOperations,
) -> Result<WebRepoWatchPullRequest, ()> {
    Ok(WebRepoWatchPullRequest {
        number: pull_request.number.get().to_string(),
        title: pull_request.title.into_string(),
        head: pull_request.head.into_string(),
        head_repository: pull_request.head_repository.into_string(),
        head_branch: pull_request.head_branch.into_string(),
        base_branch: pull_request.base_branch.into_string(),
        lifecycle: match pull_request.lifecycle {
            RepoWatchPullRequestLifecycle::Open => WebRepoWatchLifecycle::Open,
            RepoWatchPullRequestLifecycle::Closed => WebRepoWatchLifecycle::Closed,
            RepoWatchPullRequestLifecycle::Merged => WebRepoWatchLifecycle::Merged,
        },
        mergeable: match pull_request.mergeable {
            MergeableState::Mergeable => WebRepoWatchMergeable::Mergeable,
            MergeableState::Conflicting => WebRepoWatchMergeable::Conflicting,
            MergeableState::Unknown => WebRepoWatchMergeable::Unknown,
        },
        draft: match pull_request.draft {
            RepoWatchDraftStatus::Draft => WebRepoWatchDraftStatus::Draft,
            RepoWatchDraftStatus::ReadyForReview => WebRepoWatchDraftStatus::ReadyForReview,
        },
        checks: match pull_request.checks {
            RepoWatchChecksStatus::NoCompletedSuites => WebRepoWatchChecksStatus::NoCompletedSuites,
            RepoWatchChecksStatus::Passing => WebRepoWatchChecksStatus::Passing,
            RepoWatchChecksStatus::Failing => WebRepoWatchChecksStatus::Failing,
        },
        review_decision: match pull_request.review_decision {
            RepoWatchReviewDecision::None => WebRepoWatchReviewDecision::None,
            RepoWatchReviewDecision::Commented => WebRepoWatchReviewDecision::Commented,
            RepoWatchReviewDecision::Approved => WebRepoWatchReviewDecision::Approved,
            RepoWatchReviewDecision::ChangesRequested => {
                WebRepoWatchReviewDecision::ChangesRequested
            }
        },
        stale_review_count: pull_request.stale_review_count.to_string(),
        unresolved_thread_count: pull_request.unresolved_thread_count.to_string(),
        open_parent: pull_request
            .open_parent
            .map(|number| number.get().to_string()),
        open_child_count: pull_request.open_child_count.to_string(),
        automation: automation_dto(pull_request.automation)?,
        last_observed_event: pull_request
            .last_observed_event
            .map(event_dto)
            .transpose()?,
        last_actionable_event: pull_request
            .last_actionable_event
            .map(event_dto)
            .transpose()?,
        last_dispatch_attempt: pull_request
            .last_dispatch_attempt
            .map(dispatch_dto)
            .transpose()?,
        last_automation_settlement: pull_request
            .last_automation_settlement
            .map(settlement_dto)
            .transpose()?,
        held_slot_count: pull_request.held_slot_count.to_string(),
        queued_obligation_count: pull_request.queued_obligation_count.to_string(),
        commissioned_session_count: pull_request.commissioned_session_count.to_string(),
    })
}

fn pull_request_page_dto(
    page: RepoWatchPullRequestPage,
) -> Result<WebRepoWatchPullRequestPage, ()> {
    let repository = page.repository.into_string();
    let continuation_after_pull_request = page
        .continuation_after
        .map(|number| number.get().to_string());
    let mut pull_requests = page
        .pull_requests
        .into_iter()
        .map(pull_request_dto)
        .collect::<Result<Vec<_>, _>>()?;
    let complete_page_len = pull_requests.len();

    loop {
        let continuation = if pull_requests.len() < complete_page_len {
            pull_requests
                .last()
                .map(|pull_request| pull_request.number.clone())
        } else {
            continuation_after_pull_request.clone()
        };
        let candidate = WebRepoWatchPullRequestPage {
            repository: repository.clone(),
            pull_requests: pull_requests.clone(),
            continuation_after_pull_request: continuation,
        };
        if serde_json::to_vec(&candidate)
            .map(|encoded| encoded.len() <= MAX_JSON_BODY_BYTES)
            .unwrap_or(false)
        {
            return Ok(candidate);
        }
        if pull_requests.len() <= 1 {
            return Err(());
        }
        pull_requests.pop();
    }
}

fn singleton_scope(singleton: RepoWatchSingletonKey) -> WebRepoWatchSingletonScope {
    match singleton {
        RepoWatchSingletonKey::PullRequest { repository, number } => {
            WebRepoWatchSingletonScope::PullRequest {
                repository: repository.into_string(),
                number: number.get().to_string(),
            }
        }
        RepoWatchSingletonKey::Stack {
            repository,
            root_pull_request,
        } => WebRepoWatchSingletonScope::Stack {
            repository: repository.into_string(),
            root_pull_request: root_pull_request.get().to_string(),
        },
        RepoWatchSingletonKey::Rule => WebRepoWatchSingletonScope::Rule {},
        RepoWatchSingletonKey::Repository { repository } => {
            WebRepoWatchSingletonScope::Repository {
                repository: repository.into_string(),
            }
        }
    }
}

fn held_slot_dto(slot: RepoWatchHeldSlot) -> Result<WebRepoWatchHeldSlot, ()> {
    Ok(WebRepoWatchHeldSlot {
        dispatch_id: slot.dispatch.into_uuid().to_string(),
        scope: singleton_scope(slot.singleton),
        rule: slot.rule.into_string(),
        held_since_unix_milliseconds: unix_milliseconds(slot.held_since)?,
        session_ids: slot
            .sessions
            .into_iter()
            .map(|session| session.into_uuid().to_string())
            .collect(),
        blockers: slot
            .blockers
            .into_iter()
            .map(|blocker| match blocker {
                RepoWatchHeldSlotBlocker::UndeliveredAction => {
                    WebRepoWatchHeldSlotBlocker::UndeliveredAction
                }
                RepoWatchHeldSlotBlocker::DeliveryTurnRuntimeRelevant => {
                    WebRepoWatchHeldSlotBlocker::DeliveryTurnRuntimeRelevant
                }
                RepoWatchHeldSlotBlocker::LiveRuntimeTurn => {
                    WebRepoWatchHeldSlotBlocker::LiveRuntimeTurn
                }
                RepoWatchHeldSlotBlocker::PursuingGoal => WebRepoWatchHeldSlotBlocker::PursuingGoal,
            })
            .collect(),
    })
}

fn readiness_dto(
    readiness: RepoWatchObligationReadiness,
) -> Result<WebRepoWatchObligationReadiness, ()> {
    Ok(match readiness {
        RepoWatchObligationReadiness::Ready => WebRepoWatchObligationReadiness::Ready {},
        RepoWatchObligationReadiness::Occupied { dispatch, sessions } => {
            WebRepoWatchObligationReadiness::Occupied {
                dispatch_id: dispatch.into_uuid().to_string(),
                session_ids: sessions
                    .into_iter()
                    .map(|session| session.into_uuid().to_string())
                    .collect(),
            }
        }
        RepoWatchObligationReadiness::Cooldown { eligible_at } => {
            WebRepoWatchObligationReadiness::Cooldown {
                eligible_at_unix_milliseconds: eligible_at.map(unix_milliseconds).transpose()?,
            }
        }
        RepoWatchObligationReadiness::Parked { parked_at } => {
            WebRepoWatchObligationReadiness::Parked {
                parked_at_unix_milliseconds: unix_milliseconds(parked_at)?,
            }
        }
    })
}

fn obligation_dto(
    obligation: RepoWatchQueuedObligation,
) -> Result<WebRepoWatchQueuedObligation, ()> {
    Ok(WebRepoWatchQueuedObligation {
        id: obligation.id.into_uuid().to_string(),
        scope: singleton_scope(obligation.singleton),
        rule: obligation.rule.into_string(),
        first_event_id: obligation.first_event.into_uuid().to_string(),
        latest_event_id: obligation.latest_event.into_uuid().to_string(),
        matched_event_count: obligation.matched_event_count.to_string(),
        owed_since_unix_milliseconds: unix_milliseconds(obligation.owed_since)?,
        latest_match_at_unix_milliseconds: unix_milliseconds(obligation.latest_match_at)?,
        failed_attempts: obligation.failed_attempts.to_string(),
        readiness: readiness_dto(obligation.readiness)?,
    })
}

fn work_page_dto(page: RepoWatchWorkPage) -> Result<WebRepoWatchWorkPage, ()> {
    Ok(WebRepoWatchWorkPage {
        held_slots: page
            .held_slots
            .into_iter()
            .map(held_slot_dto)
            .collect::<Result<Vec<_>, _>>()?,
        held_continuation_after: match page.held_continuation_after {
            RepoWatchPagePosition::After(cursor) => Some(WebRepoWatchHeldCursor {
                held_since_unix_milliseconds: unix_milliseconds(cursor.held_since)?,
                dispatch_id: cursor.dispatch.into_uuid().to_string(),
            }),
            RepoWatchPagePosition::Start | RepoWatchPagePosition::Exhausted => None,
        },
        queued_obligations: page
            .queued_obligations
            .into_iter()
            .map(obligation_dto)
            .collect::<Result<Vec<_>, _>>()?,
        obligation_continuation_after: match page.obligation_continuation_after {
            RepoWatchPagePosition::After(cursor) => Some(WebRepoWatchObligationCursor {
                owed_since_unix_milliseconds: unix_milliseconds(cursor.owed_since)?,
                obligation_id: cursor.obligation.into_uuid().to_string(),
            }),
            RepoWatchPagePosition::Start | RepoWatchPagePosition::Exhausted => None,
        },
    })
}

fn pull_request_session_dto(
    session: RepoWatchPullRequestSession,
) -> Result<WebRepoWatchPullRequestSession, ()> {
    Ok(WebRepoWatchPullRequestSession {
        commissioned_at_unix_milliseconds: unix_milliseconds(session.commissioned_at)?,
        purpose: match session.purpose {
            RepoWatchSessionPurpose::RuleDispatch {
                dispatch,
                event,
                rule,
                template,
            } => WebRepoWatchSessionPurpose::RuleDispatch {
                dispatch_id: dispatch.into_uuid().to_string(),
                event_id: event.into_uuid().to_string(),
                rule: rule.into_string(),
                template,
            },
            RepoWatchSessionPurpose::OperatorCommission { dispatch, template } => {
                WebRepoWatchSessionPurpose::OperatorCommission {
                    dispatch_id: dispatch.into_uuid().to_string(),
                    template,
                }
            }
        },
        attention: attention_summary_dto(session.attention)?,
    })
}

fn pull_request_session_page_dto(
    page: RepoWatchPullRequestSessionPage,
) -> Result<WebRepoWatchPullRequestSessionPage, ()> {
    Ok(WebRepoWatchPullRequestSessionPage {
        sessions: page
            .sessions
            .into_iter()
            .map(pull_request_session_dto)
            .collect::<Result<Vec<_>, _>>()?,
        continuation_before: page
            .continuation_before
            .map(|cursor| {
                Ok(WebRepoWatchSessionCursor {
                    commissioned_at_unix_milliseconds: unix_milliseconds(cursor.commissioned_at)?,
                    session_id: cursor.session.into_uuid().to_string(),
                })
            })
            .transpose()?,
    })
}

fn webhook_activity_dto(
    webhook: RepoWatchWebhookActivity,
) -> Result<WebRepoWatchWebhookActivity, ()> {
    Ok(WebRepoWatchWebhookActivity {
        receipt_sequence: webhook.receipt_sequence.to_string(),
        event_name: webhook.event_name,
        action_name: webhook.action_name,
        received_at_unix_milliseconds: unix_milliseconds(webhook.received_at)?,
        projection_count: webhook.projection_count.to_string(),
        latest_projected_at_unix_milliseconds: webhook
            .latest_projected_at
            .map(unix_milliseconds)
            .transpose()?,
        disposition: webhook.disposition.map(|disposition| match disposition {
            RepoWatchWebhookDisposition::Projected => WebRepoWatchWebhookDisposition::Projected,
            RepoWatchWebhookDisposition::DuplicateState => {
                WebRepoWatchWebhookDisposition::DuplicateState
            }
            RepoWatchWebhookDisposition::Superseded => WebRepoWatchWebhookDisposition::Superseded,
            RepoWatchWebhookDisposition::Ignored => WebRepoWatchWebhookDisposition::Ignored,
            RepoWatchWebhookDisposition::Quarantined => WebRepoWatchWebhookDisposition::Quarantined,
        }),
    })
}

fn activity_page_dto(
    page: RepoWatchActivityPage,
    include_events: bool,
    include_webhooks: bool,
) -> Result<WebRepoWatchActivityPage, ()> {
    Ok(WebRepoWatchActivityPage {
        events: if include_events {
            page.events
                .into_iter()
                .map(event_dto)
                .collect::<Result<Vec<_>, _>>()?
        } else {
            Vec::new()
        },
        event_continuation_before: match page.event_continuation_before {
            RepoWatchPagePosition::After(cursor) if include_events => {
                Some(WebRepoWatchEventCursor {
                    cursor_generation: cursor.cursor_generation.to_string(),
                    event_ordinal: cursor.event_ordinal,
                })
            }
            RepoWatchPagePosition::Start
            | RepoWatchPagePosition::After(_)
            | RepoWatchPagePosition::Exhausted => None,
        },
        webhooks: if include_webhooks {
            page.webhooks
                .into_iter()
                .map(webhook_activity_dto)
                .collect::<Result<Vec<_>, _>>()?
        } else {
            Vec::new()
        },
        webhook_continuation_before_receipt_sequence: match page.webhook_continuation_before {
            RepoWatchPagePosition::After(sequence) if include_webhooks => {
                Some(sequence.to_string())
            }
            RepoWatchPagePosition::Start
            | RepoWatchPagePosition::After(_)
            | RepoWatchPagePosition::Exhausted => None,
        },
    })
}

fn bounded_projection_response<T>(value: T) -> Response
where
    T: Serialize,
{
    match serde_json::to_vec(&value) {
        Ok(encoded) if encoded.len() <= MAX_JSON_BODY_BYTES => (
            [(axum::http::header::CONTENT_TYPE, "application/json")],
            encoded,
        )
            .into_response(),
        Ok(_) | Err(_) => projection_error(None),
    }
}

fn unavailable() -> Response {
    application_error(
        StatusCode::SERVICE_UNAVAILABLE,
        "repository_watch_projection_unavailable",
        "repository-watch projection is not configured",
    )
}

fn invalid_query(code: &'static str, message: &'static str) -> Response {
    application_error(StatusCode::BAD_REQUEST, code, message)
}

fn projection_error(error: Option<RepoWatchOperationsError>) -> Response {
    if let Some(error) = error.as_ref() {
        let failure_class = match error {
            RepoWatchOperationsError::Database(_) => "infrastructure",
            RepoWatchOperationsError::Attention(_)
            | RepoWatchOperationsError::RepoWatch(_)
            | RepoWatchOperationsError::Corruption(_) => "fail_closed_corruption",
        };
        tracing::error!(failure_class, cause = %error, "repository-watch projection read failed");
    }
    application_error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "repository_watch_projection_failed",
        "the repository-watch projection could not be read",
    )
}

#[cfg(test)]
mod tests {
    use axum::{
        body::{Body, to_bytes},
        http::{Request, StatusCode},
    };
    use signalbox_web_contract::{MAX_JSON_BODY_BYTES, WebApiErrorResponse};
    use tower::ServiceExt;

    use super::{
        bounded_projection_response, event_cursor, held_cursor, postgres_bigint, session_cursor,
        timestamp, validate_activity_window,
    };

    #[tokio::test]
    async fn oversized_repository_projection_fails_closed_with_a_bounded_error() {
        let response = bounded_projection_response(vec![b'x'; MAX_JSON_BODY_BYTES]);

        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let body = to_bytes(response.into_body(), MAX_JSON_BODY_BYTES)
            .await
            .expect("the projection error remains within the contract ceiling");
        let error: WebApiErrorResponse =
            serde_json::from_slice(&body).expect("the failure follows the web API contract");
        assert_eq!(error.error.code, "repository_watch_projection_failed");
        assert!(body.len() <= MAX_JSON_BODY_BYTES);
    }

    #[test]
    fn partial_composite_cursors_fail_closed() {
        assert!(held_cursor(Some("1".to_owned()), None).is_err());
        assert!(session_cursor(None, Some(uuid::Uuid::nil().to_string())).is_err());
        assert!(event_cursor(Some("1".to_owned()), None).is_err());
    }

    #[test]
    fn complete_event_cursor_preserves_both_keyset_fields() {
        let cursor = event_cursor(Some("9".to_owned()), Some(7))
            .expect("the complete cursor is valid")
            .expect("the complete cursor is present");

        assert_eq!(cursor.cursor_generation, 9);
        assert_eq!(cursor.event_ordinal, 7);
    }

    #[test]
    fn activity_cursors_reject_values_outside_postgres_integer_ranges() {
        let above_bigint = (i64::MAX as u64 + 1).to_string();

        assert!(postgres_bigint(&above_bigint).is_err());
        assert!(event_cursor(Some(above_bigint), Some(1)).is_err());
        assert!(event_cursor(Some("1".to_owned()), Some(i32::MAX as u32 + 1)).is_err());
    }

    #[test]
    fn timestamp_rejects_values_outside_the_database_time_range() {
        assert!(timestamp(u64::MAX.to_string()).is_err());
    }

    #[test]
    fn activity_window_rejects_excluding_both_feeds() {
        assert_eq!(
            validate_activity_window(false, false, false, false),
            Err((
                "invalid_window",
                "at least one activity feed must be included"
            ))
        );
    }

    #[test]
    fn activity_window_rejects_a_cursor_for_an_excluded_feed() {
        assert_eq!(
            validate_activity_window(true, false, false, true),
            Err((
                "invalid_cursor",
                "an excluded activity feed cannot carry a cursor"
            ))
        );
    }

    #[tokio::test]
    async fn malformed_typed_query_returns_the_json_api_error_contract() {
        let response = super::router(None, None)
            .oneshot(
                Request::builder()
                    .uri(
                        "/repository-watch/activity?repository=example%2Frepository&include_events=x",
                    )
                    .body(Body::empty())
                    .expect("the request is valid"),
            )
            .await
            .expect("the router responds");

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = to_bytes(response.into_body(), 65_536)
            .await
            .expect("the error body is bounded");
        let error: WebApiErrorResponse =
            serde_json::from_slice(&body).expect("the error follows the web API contract");
        assert_eq!(
            error.error.kind,
            signalbox_web_contract::WebApiErrorKind::Transport
        );
        assert_eq!(error.error.code, "invalid_query");
    }
}
