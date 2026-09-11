#[cfg(test)]
use super::encode_ndjson_item;

use super::{
    Arc, IntoResponse, Json, MAX_WEB_PROVIDER_TEXT_FRAGMENT_BYTES, NonZeroU64, Path,
    ProcessMonitorReceiveError, ProcessMonitorUpdate, Response, SessionId, SessionLiveActiveState,
    SessionLiveReconciliation, SessionLiveRepositoryError, SessionLiveRunnerConnectionHealth,
    SessionLiveRunnerState, SessionLiveSnapshot, State, StatusCode, TimelineAddress, VecDeque,
    WebApiState, WebLiveResourceId, WebPositiveU64, WebSessionId, WebSessionLiveActiveState,
    WebSessionLiveActiveTurn, WebSessionLiveReconciliation, WebSessionLiveRunner,
    WebSessionLiveRunnerConnectionHealth, WebSessionLiveSnapshot, WebSessionLiveStreamEvent,
    WebTurnId, WebU64, address_dto, application_error, empty_ndjson_response, event_kind_dto,
    ndjson_response, parse_session_id, stream, watch,
};

pub(super) async fn session_live_snapshot(
    State(state): State<WebApiState>,
    Path(session_id): Path<String>,
) -> Response {
    let mut shutdown = state.shutdown;
    let session = match parse_session_id(&session_id) {
        Ok(session) => session,
        Err(error) => return error.into_response(),
    };
    let Some(repository) = state.live else {
        return live_projection_unavailable();
    };
    let Some(budget) = state.snapshot_reader_budget else {
        return live_projection_unavailable();
    };
    let snapshot_permit = tokio::select! {
        () = live_follow_shutdown(&mut shutdown) => return live_projection_unavailable(),
        permit = budget.acquire() => permit,
    };
    let Ok(_snapshot_permit) = snapshot_permit else {
        return live_projection_unavailable();
    };
    match tokio::select! {
        () = live_follow_shutdown(&mut shutdown) => return live_projection_unavailable(),
        snapshot = repository.read_live_snapshot(session) => snapshot,
    } {
        Ok(Some(snapshot)) => match live_snapshot_dto(snapshot) {
            Some(snapshot) => Json(snapshot).into_response(),
            None => live_projection_corruption(),
        },
        Ok(None) => application_error(
            StatusCode::NOT_FOUND,
            "session_not_found",
            "the requested session does not exist",
        ),
        Err(error) => live_projection_error(error),
    }
}

pub(super) async fn session_live_follow(
    State(state): State<WebApiState>,
    Path(session_id): Path<String>,
) -> Response {
    let mut shutdown = state.shutdown;
    let session = match parse_session_id(&session_id) {
        Ok(session) => session,
        Err(error) => return error.into_response(),
    };
    let Some(repository) = state.live else {
        return live_projection_unavailable();
    };
    let Some(monitor) = state.monitor else {
        return application_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "session_monitor_unavailable",
            "the live session monitor is not configured",
        );
    };
    let Some(budget) = state.snapshot_reader_budget else {
        return live_projection_unavailable();
    };
    let snapshot_permit = tokio::select! {
        () = live_follow_shutdown(&mut shutdown) => return empty_ndjson_response(),
        permit = Arc::clone(&budget).acquire_owned() => permit,
    };
    let Ok(snapshot_permit) = snapshot_permit else {
        return live_projection_unavailable();
    };
    let subscription = monitor.subscribe();
    let covered_at_snapshot = subscription.queued_len();
    let snapshot = match tokio::select! {
        () = live_follow_shutdown(&mut shutdown) => return empty_ndjson_response(),
        snapshot = repository
            .read_live_snapshot_at_completion(session, || subscription.queued_len()) => snapshot,
    } {
        Ok(Some(snapshot)) => snapshot,
        Ok(None) => {
            return application_error(
                StatusCode::NOT_FOUND,
                "session_not_found",
                "the requested session does not exist",
            );
        }
        Err(error) => return live_projection_error(error),
    };
    drop(snapshot_permit);
    let (snapshot, queued_at_snapshot) = snapshot;
    let Some(observed_through) = NonZeroU64::new(snapshot.observed_through) else {
        return live_projection_corruption();
    };
    let mut pending = VecDeque::new();
    let Some(snapshot) = live_snapshot_dto(snapshot) else {
        return live_projection_corruption();
    };
    pending.push_back(WebSessionLiveStreamEvent::Snapshot {
        snapshot: Box::new(snapshot),
    });
    let source = stream::unfold(
        LiveFollowState {
            subscription,
            session,
            observed_through,
            covered_at_snapshot,
            queued_at_snapshot,
            pending,
            provider_fragment: None,
            shutdown,
            ended: false,
        },
        live_follow_next,
    );
    ndjson_response(source)
}

struct LiveFollowState {
    subscription: crate::ProcessMonitorSubscription,
    session: SessionId,
    observed_through: NonZeroU64,
    covered_at_snapshot: usize,
    queued_at_snapshot: usize,
    pending: VecDeque<WebSessionLiveStreamEvent>,
    provider_fragment: Option<PendingProviderTextDelta>,
    shutdown: Option<watch::Receiver<bool>>,
    ended: bool,
}

struct PendingProviderTextDelta {
    turn_id: WebTurnId,
    model_call_id: WebLiveResourceId,
    part_index: u32,
    text: Arc<str>,
    offset: usize,
    emitted_empty: bool,
}

impl PendingProviderTextDelta {
    fn next_event(&mut self) -> Option<WebSessionLiveStreamEvent> {
        let content =
            next_web_text_fragment(&self.text, &mut self.offset, &mut self.emitted_empty)?;
        Some(WebSessionLiveStreamEvent::ProviderTextDelta {
            turn_id: self.turn_id.clone(),
            model_call_id: self.model_call_id.clone(),
            part_index: self.part_index,
            content,
        })
    }
}

async fn live_follow_next(
    mut state: LiveFollowState,
) -> Option<(WebSessionLiveStreamEvent, LiveFollowState)> {
    if state
        .shutdown
        .as_ref()
        .is_some_and(|shutdown| *shutdown.borrow() || shutdown.has_changed().is_err())
    {
        return None;
    }
    if let Some(event) = state.pending.pop_front() {
        return Some((event, state));
    }
    if let Some(mut fragment) = state.provider_fragment.take() {
        if state.subscription.is_saturated() {
            state.ended = true;
            return Some((
                WebSessionLiveStreamEvent::ResyncRequired {
                    cursor: WebPositiveU64::from_nonzero(state.observed_through),
                },
                state,
            ));
        }
        if let Some(event) = fragment.next_event() {
            state.provider_fragment = Some(fragment);
            return Some((event, state));
        }
    }
    if state.ended {
        return None;
    }
    loop {
        let update = match tokio::select! {
            () = live_follow_shutdown(&mut state.shutdown) => return None,
            update = state.subscription.recv() => update,
        } {
            Ok(update) => update,
            Err(ProcessMonitorReceiveError::Lagged(skipped))
                if skipped <= state.covered_at_snapshot =>
            {
                state.covered_at_snapshot -= skipped;
                state.queued_at_snapshot = state.queued_at_snapshot.saturating_sub(skipped);
                continue;
            }
            Err(ProcessMonitorReceiveError::Lagged(_)) => {
                state.ended = true;
                return Some((
                    WebSessionLiveStreamEvent::ResyncRequired {
                        cursor: WebPositiveU64::from_nonzero(state.observed_through),
                    },
                    state,
                ));
            }
            Err(ProcessMonitorReceiveError::Closed) => return None,
        };
        state.covered_at_snapshot = state.covered_at_snapshot.saturating_sub(1);
        let queued_at_snapshot = if state.queued_at_snapshot == 0 {
            false
        } else {
            state.queued_at_snapshot -= 1;
            true
        };
        match update {
            ProcessMonitorUpdate::Durable {
                cursor,
                session,
                kind,
            } => {
                let Some(sequence) = NonZeroU64::new(cursor)
                    .filter(|sequence| sequence.get() > state.observed_through.get())
                else {
                    continue;
                };
                state.observed_through = sequence;
                if session != state.session {
                    continue;
                }
                return Some((
                    WebSessionLiveStreamEvent::Durable {
                        cursor: WebU64::from_u64(cursor),
                        address: address_dto(TimelineAddress::new(sequence)),
                        event_kind: event_kind_dto(kind),
                    },
                    state,
                ));
            }
            ProcessMonitorUpdate::ProviderTextDelta {
                session,
                turn,
                call,
                part_index,
                text,
            } => {
                if queued_at_snapshot || session != state.session {
                    continue;
                }
                let mut fragment = PendingProviderTextDelta {
                    turn_id: WebTurnId::from_uuid_bytes(turn.into_uuid().into_bytes()),
                    model_call_id: WebLiveResourceId::from_uuid_bytes(
                        call.into_uuid().into_bytes(),
                    ),
                    part_index,
                    text,
                    offset: 0,
                    emitted_empty: false,
                };
                let event = fragment.next_event()?;
                state.provider_fragment = Some(fragment);
                return Some((event, state));
            }
        }
    }
}

async fn live_follow_shutdown(shutdown: &mut Option<watch::Receiver<bool>>) {
    let Some(shutdown) = shutdown else {
        std::future::pending::<()>().await;
        return;
    };
    if *shutdown.borrow() {
        return;
    }
    while shutdown.changed().await.is_ok() {
        if *shutdown.borrow() {
            return;
        }
    }
}

fn next_web_text_fragment(
    value: &str,
    offset: &mut usize,
    emitted_empty: &mut bool,
) -> Option<String> {
    if value.is_empty() {
        if *emitted_empty {
            return None;
        }
        *emitted_empty = true;
        return Some(String::new());
    }
    if *offset == value.len() {
        return None;
    }
    let remaining = &value[*offset..];
    let mut length = remaining.len().min(MAX_WEB_PROVIDER_TEXT_FRAGMENT_BYTES);
    while !remaining.is_char_boundary(length) {
        length -= 1;
    }
    let fragment = remaining[..length].to_owned();
    *offset += length;
    Some(fragment)
}

fn live_snapshot_dto(snapshot: SessionLiveSnapshot) -> Option<WebSessionLiveSnapshot> {
    let observed_through = NonZeroU64::new(snapshot.observed_through)?;
    let active = snapshot
        .active
        .map(|active| {
            let state = match active.state {
                SessionLiveActiveState::Running { model_call } => {
                    WebSessionLiveActiveState::Running {
                        model_call_id: model_call.map(|call| {
                            WebLiveResourceId::from_uuid_bytes(call.into_uuid().into_bytes())
                        }),
                    }
                }
                SessionLiveActiveState::AwaitingCredentialAvailability { attempt, cause } => {
                    WebSessionLiveActiveState::AwaitingCredentialAvailability {
                        wait_attempt_id: WebLiveResourceId::from_uuid_bytes(attempt.into_uuid().into_bytes()),
                        cause: match cause {
                            signalbox_domain::CredentialAvailabilityWaitCause::Contended =>
                                signalbox_web_contract::WebCredentialAvailabilityWaitCause::Contended,
                            signalbox_domain::CredentialAvailabilityWaitCause::Exhausted =>
                                signalbox_web_contract::WebCredentialAvailabilityWaitCause::Exhausted,
                            signalbox_domain::CredentialAvailabilityWaitCause::NetworkUnavailable =>
                                signalbox_web_contract::WebCredentialAvailabilityWaitCause::NetworkUnavailable,
                        },
                    }
                }
                SessionLiveActiveState::AwaitingModelCallRecovery { call } => {
                    WebSessionLiveActiveState::AwaitingModelCallRecovery {
                        model_call_id: WebLiveResourceId::from_uuid_bytes(
                            call.into_uuid().into_bytes(),
                        ),
                    }
                }
                SessionLiveActiveState::AwaitingToolApproval { request } => {
                    WebSessionLiveActiveState::AwaitingToolApproval {
                        tool_request_id: WebLiveResourceId::from_uuid_bytes(
                            request.into_uuid().into_bytes(),
                        ),
                    }
                }
                SessionLiveActiveState::AwaitingChild { request, child } => {
                    WebSessionLiveActiveState::AwaitingChild {
                        tool_request_id: WebLiveResourceId::from_uuid_bytes(
                            request.into_uuid().into_bytes(),
                        ),
                        child_session_id: WebSessionId::from_uuid_bytes(
                            child.into_uuid().into_bytes(),
                        ),
                    }
                }
                SessionLiveActiveState::AwaitingToolRecovery { attempt } => {
                    WebSessionLiveActiveState::AwaitingToolRecovery {
                        tool_attempt_id: WebLiveResourceId::from_uuid_bytes(
                            attempt.into_uuid().into_bytes(),
                        ),
                    }
                }
                SessionLiveActiveState::AwaitingRunnerRecovery {
                    runner,
                    placement_revision,
                } => WebSessionLiveActiveState::AwaitingRunnerRecovery {
                    runner_id: WebLiveResourceId::from_uuid_bytes(runner.into_uuid().into_bytes()),
                    placement_revision: WebPositiveU64::from_nonzero(
                        NonZeroU64::new(placement_revision).ok_or(())?,
                    ),
                },
            };
            Ok::<WebSessionLiveActiveTurn, ()>(WebSessionLiveActiveTurn {
                turn_id: WebTurnId::from_uuid_bytes(active.turn.into_uuid().into_bytes()),
                state,
            })
        })
        .transpose()
        .ok()?;
    let runner = snapshot
        .runner
        .map(|runner| {
            let placement_revision =
                WebPositiveU64::from_nonzero(NonZeroU64::new(runner.placement_revision).ok_or(())?);
            Ok::<_, ()>((runner, placement_revision))
        })
        .transpose()
        .ok()?
        .and_then(|(runner, placement_revision)| {
            match (runner.state, runner.runner, runner.connection_health) {
                (SessionLiveRunnerState::Unpinned, None, None) => {
                    Some(WebSessionLiveRunner::Unpinned { placement_revision })
                }
                (SessionLiveRunnerState::Pinned, Some(runner), Some(connection_health)) => {
                    Some(WebSessionLiveRunner::Pinned {
                        runner_id: WebLiveResourceId::from_uuid_bytes(
                            runner.into_uuid().into_bytes(),
                        ),
                        placement_revision,
                        connection_health: match connection_health {
                            SessionLiveRunnerConnectionHealth::Connected => {
                                WebSessionLiveRunnerConnectionHealth::Connected
                            }
                            SessionLiveRunnerConnectionHealth::Suspect => {
                                WebSessionLiveRunnerConnectionHealth::Suspect
                            }
                            SessionLiveRunnerConnectionHealth::Shutdown => {
                                WebSessionLiveRunnerConnectionHealth::Shutdown
                            }
                            SessionLiveRunnerConnectionHealth::Lost => {
                                WebSessionLiveRunnerConnectionHealth::Lost
                            }
                        },
                    })
                }
                (SessionLiveRunnerState::RunnerLostBeforePin, Some(runner), None) => {
                    Some(WebSessionLiveRunner::RunnerLostBeforePin {
                        runner_id: WebLiveResourceId::from_uuid_bytes(
                            runner.into_uuid().into_bytes(),
                        ),
                        placement_revision,
                    })
                }
                (SessionLiveRunnerState::RunnerLost, Some(runner), None) => {
                    Some(WebSessionLiveRunner::RunnerLost {
                        runner_id: WebLiveResourceId::from_uuid_bytes(
                            runner.into_uuid().into_bytes(),
                        ),
                        placement_revision,
                    })
                }
                (SessionLiveRunnerState::RunnerAbandoned, Some(runner), None) => {
                    Some(WebSessionLiveRunner::RunnerAbandoned {
                        runner_id: WebLiveResourceId::from_uuid_bytes(
                            runner.into_uuid().into_bytes(),
                        ),
                        placement_revision,
                    })
                }
                _ => None,
            }
        });
    Some(WebSessionLiveSnapshot {
        session_id: WebSessionId::from_uuid_bytes(snapshot.session.into_uuid().into_bytes()),
        observed_through: WebPositiveU64::from_nonzero(observed_through),
        active,
        queued_turn_count: WebU64::from_u64(snapshot.queued_turn_count),
        queued_turn_ids: snapshot
            .queued_turns
            .into_iter()
            .map(|turn| WebTurnId::from_uuid_bytes(turn.into_uuid().into_bytes()))
            .collect(),
        reconciliation: snapshot
            .reconciliation
            .map(|reconciliation| match reconciliation {
                SessionLiveReconciliation::ModelCall { turn, call } => {
                    WebSessionLiveReconciliation::ModelCall {
                        turn_id: WebTurnId::from_uuid_bytes(turn.into_uuid().into_bytes()),
                        model_call_id: WebLiveResourceId::from_uuid_bytes(
                            call.into_uuid().into_bytes(),
                        ),
                    }
                }
                SessionLiveReconciliation::ToolAttempt { turn, attempt } => {
                    WebSessionLiveReconciliation::ToolAttempt {
                        turn_id: WebTurnId::from_uuid_bytes(turn.into_uuid().into_bytes()),
                        tool_attempt_id: WebLiveResourceId::from_uuid_bytes(
                            attempt.into_uuid().into_bytes(),
                        ),
                    }
                }
            }),
        runner,
    })
}

fn live_projection_corruption() -> Response {
    application_error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "session_live_projection_corrupt",
        "the durable live session projection contains invalid positive values",
    )
}

fn live_projection_unavailable() -> Response {
    application_error(
        StatusCode::SERVICE_UNAVAILABLE,
        "session_live_projection_unavailable",
        "the live session projection is not configured",
    )
}

fn live_projection_error(error: SessionLiveRepositoryError) -> Response {
    tracing::error!(cause = %error, "session live projection read failed");
    application_error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "session_live_projection_failed",
        "the durable live session projection could not be read",
    )
}

#[cfg(test)]
mod live_follow_tests {
    use std::{num::NonZeroU64, sync::Arc, time::Duration};

    use signalbox_domain::{ModelCallId, SessionId, TurnId};
    use signalbox_web_contract::{
        MAX_NDJSON_ITEM_BYTES, WebLiveResourceId, WebPositiveU64, WebSessionLiveStreamEvent,
        WebSessionTimelineEventKind, WebTimelineAddress, WebTimelineEventSequence, WebTurnId,
        WebU64,
    };
    use sqlx::types::Uuid;
    use tokio::sync::watch;

    use super::{LiveFollowState, live_follow_next};
    use crate::{ProcessMonitor, ProcessMonitorUpdate};

    fn live_session() -> SessionId {
        SessionId::from_uuid(Uuid::from_u128(0x991))
    }

    fn live_turn() -> TurnId {
        TurnId::from_uuid(Uuid::from_u128(0x992))
    }

    fn live_call() -> ModelCallId {
        ModelCallId::from_uuid(Uuid::from_u128(0x993))
    }

    fn live_follow_state(
        monitor: &ProcessMonitor,
        observed_through: u64,
        queued_at_snapshot: usize,
    ) -> LiveFollowState {
        LiveFollowState {
            subscription: monitor.subscribe(),
            session: live_session(),
            observed_through: NonZeroU64::new(observed_through)
                .expect("test snapshot cursors are positive"),
            covered_at_snapshot: queued_at_snapshot,
            queued_at_snapshot,
            pending: std::collections::VecDeque::new(),
            provider_fragment: None,
            shutdown: None,
            ended: false,
        }
    }

    fn provider_text_content_length(event: WebSessionLiveStreamEvent) -> usize {
        let WebSessionLiveStreamEvent::ProviderTextDelta { content, .. } = event else {
            panic!("expected provider text delta");
        };
        content.len()
    }

    #[tokio::test]
    async fn live_follow_orders_provider_draft_before_later_durable_header() {
        let monitor = ProcessMonitor::test_channel();
        let state = live_follow_state(&monitor, 7, 0);
        monitor.publish_for_test(ProcessMonitorUpdate::ProviderTextDelta {
            session: live_session(),
            turn: live_turn(),
            call: live_call(),
            part_index: 2,
            text: Arc::from("draft"),
        });
        let (draft, state) = live_follow_next(state)
            .await
            .expect("the provider draft is delivered");
        monitor.publish_for_test(ProcessMonitorUpdate::Durable {
            cursor: 8,
            session: live_session(),
            kind: signalbox_application::SessionTimelineEventKind::ModelCallTransition,
        });
        let (durable, _) = live_follow_next(state)
            .await
            .expect("the durable header follows the draft");

        assert_eq!(
            draft,
            WebSessionLiveStreamEvent::ProviderTextDelta {
                turn_id: WebTurnId::from_uuid_bytes(live_turn().into_uuid().into_bytes()),
                model_call_id: WebLiveResourceId::from_uuid_bytes(
                    live_call().into_uuid().into_bytes()
                ),
                part_index: 2,
                content: "draft".to_owned(),
            }
        );
        assert_eq!(
            durable,
            WebSessionLiveStreamEvent::Durable {
                cursor: WebU64::from_u64(8),
                address: WebTimelineAddress {
                    event_sequence: WebTimelineEventSequence::from_nonzero(
                        NonZeroU64::new(8).expect("the fixture cursor is positive"),
                    ),
                },
                event_kind: WebSessionTimelineEventKind::ModelCallTransition,
            }
        );
    }

    #[tokio::test]
    async fn live_follow_discards_provider_draft_queued_before_snapshot() {
        let monitor = ProcessMonitor::test_channel();
        let mut state = live_follow_state(&monitor, 7, 0);
        monitor.publish_for_test(ProcessMonitorUpdate::ProviderTextDelta {
            session: live_session(),
            turn: live_turn(),
            call: live_call(),
            part_index: 0,
            text: Arc::from("stale draft"),
        });
        state.covered_at_snapshot = state.subscription.queued_len();
        state.queued_at_snapshot = state.subscription.queued_len();
        monitor.publish_for_test(ProcessMonitorUpdate::Durable {
            cursor: 8,
            session: live_session(),
            kind: signalbox_application::SessionTimelineEventKind::TurnCompleted,
        });
        let (event, _) = live_follow_next(state)
            .await
            .expect("the post-snapshot durable event is delivered");

        assert_eq!(
            event,
            WebSessionLiveStreamEvent::Durable {
                cursor: WebU64::from_u64(8),
                address: WebTimelineAddress {
                    event_sequence: WebTimelineEventSequence::from_nonzero(
                        NonZeroU64::new(8).expect("the fixture cursor is positive"),
                    ),
                },
                event_kind: WebSessionTimelineEventKind::TurnCompleted,
            }
        );
    }

    #[tokio::test]
    async fn live_follow_lag_requires_transient_presentation_resync() {
        let monitor = ProcessMonitor::test_channel();
        let state = live_follow_state(&monitor, 7, 0);
        monitor.fill_for_test(ProcessMonitorUpdate::Durable {
            cursor: 8,
            session: live_session(),
            kind: signalbox_application::SessionTimelineEventKind::TurnActivated,
        });
        let (event, _) = live_follow_next(state)
            .await
            .expect("lag produces one explicit terminal event");

        assert_eq!(
            event,
            WebSessionLiveStreamEvent::ResyncRequired {
                cursor: WebPositiveU64::from_nonzero(
                    NonZeroU64::new(7).expect("the fixture cursor is positive"),
                ),
            }
        );
    }

    #[tokio::test]
    async fn live_follow_consumes_lag_covered_by_snapshot_cutoff() {
        let monitor = ProcessMonitor::test_channel();
        let mut state = live_follow_state(&monitor, 7, 0);
        monitor.fill_for_test(ProcessMonitorUpdate::ProviderTextDelta {
            session: live_session(),
            turn: live_turn(),
            call: live_call(),
            part_index: 0,
            text: Arc::from("stale draft"),
        });
        state.covered_at_snapshot = state.subscription.queued_len();
        state.queued_at_snapshot = state.subscription.queued_len();
        monitor.publish_for_test(ProcessMonitorUpdate::Durable {
            cursor: 8,
            session: live_session(),
            kind: signalbox_application::SessionTimelineEventKind::TurnCompleted,
        });
        let (event, _) = live_follow_next(state)
            .await
            .expect("the post-snapshot durable event is delivered");

        assert_eq!(
            event,
            WebSessionLiveStreamEvent::Durable {
                cursor: WebU64::from_u64(8),
                address: WebTimelineAddress {
                    event_sequence: WebTimelineEventSequence::from_nonzero(
                        NonZeroU64::new(8).expect("the fixture cursor is positive"),
                    ),
                },
                event_kind: WebSessionTimelineEventKind::TurnCompleted,
            }
        );
    }

    #[tokio::test]
    async fn live_follow_discards_a_draft_raced_between_sample_and_snapshot() {
        let monitor = ProcessMonitor::test_channel();
        let mut state = live_follow_state(&monitor, 7, 0);
        state.covered_at_snapshot = state.subscription.queued_len();
        monitor.publish_for_test(ProcessMonitorUpdate::ProviderTextDelta {
            session: live_session(),
            turn: live_turn(),
            call: live_call(),
            part_index: 0,
            text: Arc::from("raced draft"),
        });
        state.queued_at_snapshot = state.subscription.queued_len();
        monitor.publish_for_test(ProcessMonitorUpdate::Durable {
            cursor: 8,
            session: live_session(),
            kind: signalbox_application::SessionTimelineEventKind::TurnCompleted,
        });
        let (event, _) = live_follow_next(state)
            .await
            .expect("the post-snapshot durable event is delivered");

        assert_eq!(
            event,
            WebSessionLiveStreamEvent::Durable {
                cursor: WebU64::from_u64(8),
                address: WebTimelineAddress {
                    event_sequence: WebTimelineEventSequence::from_nonzero(
                        NonZeroU64::new(8).expect("the fixture cursor is positive"),
                    ),
                },
                event_kind: WebSessionTimelineEventKind::TurnCompleted,
            }
        );
    }

    #[tokio::test]
    async fn live_follow_resyncs_a_saturated_monitor_before_draining_fragments() {
        let monitor = ProcessMonitor::test_channel();
        let mut state = live_follow_state(&monitor, 7, 0);
        state.provider_fragment = Some(super::PendingProviderTextDelta {
            turn_id: WebTurnId::from_uuid_bytes(live_turn().into_uuid().into_bytes()),
            model_call_id: WebLiveResourceId::from_uuid_bytes(live_call().into_uuid().into_bytes()),
            part_index: 0,
            text: Arc::from("retained draft"),
            offset: 0,
            emitted_empty: false,
        });
        monitor.fill_for_test(ProcessMonitorUpdate::Durable {
            cursor: 8,
            session: live_session(),
            kind: signalbox_application::SessionTimelineEventKind::TurnActivated,
        });
        let (event, state) = live_follow_next(state)
            .await
            .expect("saturation produces one explicit terminal event");

        assert_eq!(
            event,
            WebSessionLiveStreamEvent::ResyncRequired {
                cursor: WebPositiveU64::from_nonzero(
                    NonZeroU64::new(7).expect("the fixture cursor is positive"),
                ),
            }
        );
        assert!(state.ended);
        assert!(state.provider_fragment.is_none());
    }

    #[tokio::test]
    async fn live_follow_fragments_provider_text_lazily() {
        let monitor = ProcessMonitor::test_channel();
        let state = live_follow_state(&monitor, 7, 0);
        let text: Arc<str> = Arc::from("x".repeat(super::MAX_WEB_PROVIDER_TEXT_FRAGMENT_BYTES + 1));
        monitor.publish_for_test(ProcessMonitorUpdate::ProviderTextDelta {
            session: live_session(),
            turn: live_turn(),
            call: live_call(),
            part_index: 0,
            text: Arc::clone(&text),
        });
        let (first, state) = live_follow_next(state)
            .await
            .expect("the first fragment is delivered");
        let retained = state
            .provider_fragment
            .as_ref()
            .expect("the unencoded suffix remains pending");

        assert!(state.pending.is_empty());
        assert!(Arc::ptr_eq(&text, &retained.text));
        assert_eq!(retained.offset, super::MAX_WEB_PROVIDER_TEXT_FRAGMENT_BYTES);
        assert_eq!(
            provider_text_content_length(first),
            super::MAX_WEB_PROVIDER_TEXT_FRAGMENT_BYTES,
        );
    }

    #[tokio::test]
    async fn live_follow_ends_when_http_shutdown_is_requested() {
        let monitor = ProcessMonitor::test_channel();
        let mut state = live_follow_state(&monitor, 7, 0);
        let (shutdown_sender, shutdown) = watch::channel(false);
        state.shutdown = Some(shutdown);
        shutdown_sender
            .send(true)
            .expect("the follow stream observes shutdown");

        let event = tokio::time::timeout(Duration::from_secs(1), live_follow_next(state))
            .await
            .expect("shutdown ends the follow stream promptly");

        assert!(event.is_none());
    }

    #[tokio::test]
    async fn live_follow_ends_when_the_shutdown_sender_is_dropped() {
        let monitor = ProcessMonitor::test_channel();
        let mut state = live_follow_state(&monitor, 7, 0);
        let (shutdown_sender, shutdown) = watch::channel(false);
        state.shutdown = Some(shutdown);
        drop(shutdown_sender);

        let event = tokio::time::timeout(Duration::from_secs(1), live_follow_next(state))
            .await
            .expect("a closed shutdown channel ends the follow stream promptly");

        assert!(event.is_none());
    }

    /// The retained-fragment fast path returns before the monitor is polled,
    /// so it must observe channel closure itself: provider delta text has no
    /// size bound, and draining it for a slow client would otherwise keep
    /// graceful shutdown waiting arbitrarily long.
    #[tokio::test]
    async fn live_follow_stops_draining_a_retained_fragment_on_closed_shutdown() {
        let monitor = ProcessMonitor::test_channel();
        let mut state = live_follow_state(&monitor, 7, 0);
        state.provider_fragment = Some(super::PendingProviderTextDelta {
            turn_id: WebTurnId::from_uuid_bytes(live_turn().into_uuid().into_bytes()),
            model_call_id: WebLiveResourceId::from_uuid_bytes(live_call().into_uuid().into_bytes()),
            part_index: 0,
            text: Arc::from("retained draft"),
            offset: 0,
            emitted_empty: false,
        });
        let (shutdown_sender, shutdown) = watch::channel(false);
        state.shutdown = Some(shutdown);
        drop(shutdown_sender);

        let event = tokio::time::timeout(Duration::from_secs(1), live_follow_next(state))
            .await
            .expect("a closed shutdown channel preempts retained fragments promptly");

        assert!(event.is_none());
    }

    #[test]
    fn provider_text_fragment_fits_after_worst_case_json_escaping() {
        let source = "\u{0001}".repeat(super::MAX_WEB_PROVIDER_TEXT_FRAGMENT_BYTES);
        let content = super::next_web_text_fragment(&source, &mut 0, &mut false)
            .expect("nonempty provider text has a first fragment");
        let encoded = super::encode_ndjson_item(WebSessionLiveStreamEvent::ProviderTextDelta {
            turn_id: WebTurnId::from_uuid_bytes(live_turn().into_uuid().into_bytes()),
            model_call_id: WebLiveResourceId::from_uuid_bytes(live_call().into_uuid().into_bytes()),
            part_index: u32::MAX,
            content,
        })
        .expect("the worst-case escaped fragment remains below the NDJSON ceiling");

        assert!(encoded.len() <= MAX_NDJSON_ITEM_BYTES + 1);
    }
}
