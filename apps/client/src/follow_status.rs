use super::*;

/// Maximum time a terminal follower waits before rereading recovery state.
#[cfg(not(test))]
pub(crate) const FOLLOW_RECOVERY_REFETCH_INTERVAL: Duration = Duration::from_secs(30);
/// Short equivalent used by deterministic socket tests.
#[cfg(test)]
pub(crate) const FOLLOW_RECOVERY_REFETCH_INTERVAL: Duration = Duration::from_millis(50);

pub(crate) async fn transcript(
    client: &mut ProcessClient,
    session_id: CanonicalUuid,
) -> Result<TranscriptSnapshot, ClientError> {
    let mut connection = client
        .request(ClientRequest::ReadTranscript { session_id })
        .await?;
    read_snapshot(client, &mut connection, session_id).await
}

pub(crate) async fn follow(
    client: &mut ProcessClient,
    output: &mut Output<'_>,
    session_id: CanonicalUuid,
) -> Result<(), ClientError> {
    let mut displayed_entries = SnapshotIdentitySet::new()?;
    let mut retry = crate::connection::FollowRetry::default();
    loop {
        let result: Result<(), ClientError> = async {
            let mut connection = client
                .request(ClientRequest::FollowSession { session_id })
                .await?;
            let mut snapshot = read_snapshot(client, &mut connection, session_id).await?;
            output.followed_snapshot(&mut snapshot, &mut displayed_entries)?;
            let mut observed_cursor = snapshot.cursor();
            loop {
                match connection.message().await? {
                    ServerMessage::SessionEvent {
                        cursor,
                        session_id: event_session,
                        event,
                    } if event_session == session_id => {
                        if cursor.value() <= observed_cursor {
                            continue;
                        }
                        crate::credential_pool::validate_event(client, session_id, &event).await?;
                        observed_cursor = cursor.value();
                        output.event(observed_cursor, session_id, &event)?;
                        if let Some(selection) = terminal_snapshot_selection(&event, session_id) {
                            let mut refreshed = transcript(client, session_id).await?;
                            output.terminal_material(
                                &mut refreshed,
                                &mut displayed_entries,
                                selection,
                            )?;
                        }
                    }
                    ServerMessage::ProviderTextDelta {
                        session_id: delta_session,
                        turn_id,
                        model_call_id,
                        part_index,
                        content,
                    } if delta_session == session_id => {
                        output.provider_text_delta(
                            session_id,
                            turn_id,
                            model_call_id,
                            part_index.value(),
                            content.as_str(),
                        )?;
                    }
                    ServerMessage::Error {
                        code: ErrorCode::ResyncRequired,
                        ..
                    } => break,
                    ServerMessage::Error {
                        code,
                        message,
                        detail,
                    } => return Err(ClientError::remote(code, message, detail)),
                    _ => {
                        return Err(ClientError::Protocol(
                            "follow returned an unexpected response",
                        ));
                    }
                }
            }
            Ok(())
        }
        .await;
        if let Err(error) = result {
            tokio::time::sleep(retry.next_delay(error)?).await;
        }
    }
}

pub(crate) fn terminal_snapshot_selection(
    event: &SessionEvent,
    session_id: CanonicalUuid,
) -> Option<SnapshotSelection> {
    if child_lifecycle_terminalization(event, session_id) {
        // The cascade names no terminal entry of its own, so the refresh it
        // requires selects whatever material the child produced before it.
        return Some(SnapshotSelection::All);
    }
    match event {
        SessionEvent::TurnCompleted {
            turn_id,
            model_call_id,
            completion_entry_id,
            ..
        } => Some(SnapshotSelection::Completed {
            turn_id: *turn_id,
            model_call_id: *model_call_id,
            terminal_entry_id: *completion_entry_id,
        }),
        SessionEvent::TurnCredentialPoolExhausted {
            turn_id,
            failure_entry_id,
            ..
        }
        | SessionEvent::TurnFailed {
            turn_id,
            failure_entry_id,
            ..
        } => Some(SnapshotSelection::Failed {
            turn_id: *turn_id,
            terminal_entry_id: *failure_entry_id,
        }),
        SessionEvent::TurnCancelled {
            turn_id,
            cancellation_entry_id,
            ..
        } => Some(SnapshotSelection::Cancelled {
            turn_id: *turn_id,
            terminal_entry_id: *cancellation_entry_id,
        }),
        SessionEvent::ToolBatchTransition {
            turn_id,
            model_call_id,
            state: ToolBatchState::Proposed { .. },
        } => Some(SnapshotSelection::ToolBatchProposed {
            turn_id: *turn_id,
            model_call_id: *model_call_id,
        }),
        SessionEvent::ToolBatchTransition {
            turn_id,
            model_call_id,
            state: ToolBatchState::ResultsProjected { .. },
        } => Some(SnapshotSelection::ToolBatchResults {
            turn_id: *turn_id,
            model_call_id: *model_call_id,
        }),
        SessionEvent::ToolBatchTransition {
            state: ToolBatchState::RecoveryRequired { .. } | ToolBatchState::ChildWaitResumed { .. },
            ..
        } => None,
        SessionEvent::TurnToolReconciliationRequired {
            turn_id,
            tool_attempt_id,
            terminal_frontier_id,
        } => Some(SnapshotSelection::ToolReconciliation {
            turn_id: *turn_id,
            tool_attempt_id: *tool_attempt_id,
            terminal_frontier_id: *terminal_frontier_id,
        }),
        SessionEvent::TurnRefused {
            turn_id,
            model_call_id,
            terminal_frontier_id,
        } => Some(SnapshotSelection::Refused {
            turn_id: *turn_id,
            model_call_id: *model_call_id,
            terminal_frontier_id: *terminal_frontier_id,
        }),
        SessionEvent::TurnReconciliationRequired { .. } => None,
        SessionEvent::AutomaticReconciliationExhausted { .. }
        | SessionEvent::SessionCreated {}
        | SessionEvent::SessionModelSettingsChanged { .. }
        | SessionEvent::TurnModelSettingsResolved { .. }
        | SessionEvent::InputAccepted { .. }
        | SessionEvent::GoalTurnRetired { .. }
        | SessionEvent::TurnActivated { .. }
        | SessionEvent::ContextCompacted { .. }
        | SessionEvent::ModelCallTransition { .. }
        | SessionEvent::RunnerStateTransition { .. }
        | SessionEvent::ChildSpawned { .. }
        | SessionEvent::ChildWaiting { .. }
        | SessionEvent::SessionMessage { .. }
        | SessionEvent::ChildResult { .. }
        | SessionEvent::ToolApprovalDecided { .. }
        | SessionEvent::ChildLifecycleDisposition { .. } => None,
    }
}

pub(crate) fn write_assistant_texts(
    snapshot: &mut TranscriptSnapshot,
    output: &mut Output<'_>,
    selected_turn: CanonicalUuid,
) -> Result<(), ClientError> {
    let mut selected_entry = false;
    for record in snapshot.replay()? {
        match record? {
            SnapshotRecord::Entry(entry) => {
                selected_entry = matches!(
                    entry.kind,
                    transcript::SnapshotEntryKind::Text(
                        signalbox_process_protocol::TranscriptTextEntry::Assistant {
                            turn_id,
                            ..
                        }
                    ) if turn_id == selected_turn
                );
            }
            SnapshotRecord::Content(content) if selected_entry => {
                let ends_with_newline = content.content.as_str().ends_with('\n');
                output.assistant_text_fragment(
                    content.content.as_str(),
                    content.final_fragment,
                    ends_with_newline,
                )?;
                if content.final_fragment {
                    selected_entry = false;
                }
            }
            SnapshotRecord::Turn(_)
            | SnapshotRecord::ModelCallUsage(_)
            | SnapshotRecord::Content(_) => {}
        }
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, PartialOrd, Ord)]
enum OperatorStatusPhase {
    UnavailableComponents,
    LifecycleWeeks,
    LifecycleDeadlineViolations,
    SessionSupervision,
    RepositoryIngestion,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct OperatorStatusCounts {
    unavailable_components: u64,
    repository_ingestion: u64,
    lifecycle_weeks: u64,
    lifecycle_deadline_violations: u64,
    session_supervision: u64,
}

pub(crate) async fn status(
    client: &mut ProcessClient,
    output: &mut Output<'_>,
) -> Result<(), ClientError> {
    let mut connection = client.request(ClientRequest::ReadOperatorStatus {}).await?;
    match connection.message().await? {
        ServerMessage::OperatorStatus(message)
            if matches!(message.as_ref(), OperatorStatusMessage::Start {}) => {}
        ServerMessage::Error {
            code,
            message,
            detail,
        } => return Err(ClientError::remote(code, message, detail)),
        _ => {
            return Err(ClientError::Protocol(
                "operator status did not begin with its start frame",
            ));
        }
    }
    let mut spool = tempfile::tempfile()?;
    let mut phase = OperatorStatusPhase::UnavailableComponents;
    let mut counts = OperatorStatusCounts::default();
    let outbox_quarantines;
    loop {
        let frame = connection.frame().await?;
        let item_phase = match frame.message() {
            ServerMessage::OperatorStatus(message) => match message.as_ref() {
                OperatorStatusMessage::SessionSupervision(_) => {
                    counts.session_supervision = status_increment(counts.session_supervision)?;
                    Some(OperatorStatusPhase::SessionSupervision)
                }
                OperatorStatusMessage::RepositoryIngestion(_) => {
                    counts.repository_ingestion = status_increment(counts.repository_ingestion)?;
                    Some(OperatorStatusPhase::RepositoryIngestion)
                }
                OperatorStatusMessage::UnavailableComponent(_) => {
                    counts.unavailable_components =
                        status_increment(counts.unavailable_components)?;
                    Some(OperatorStatusPhase::UnavailableComponents)
                }
                OperatorStatusMessage::LifecycleWeek(_) => {
                    counts.lifecycle_weeks = status_increment(counts.lifecycle_weeks)?;
                    Some(OperatorStatusPhase::LifecycleWeeks)
                }
                OperatorStatusMessage::LifecycleDeadlineViolation(_) => {
                    counts.lifecycle_deadline_violations =
                        status_increment(counts.lifecycle_deadline_violations)?;
                    Some(OperatorStatusPhase::LifecycleDeadlineViolations)
                }
                OperatorStatusMessage::End(item)
                    if counts
                        == (OperatorStatusCounts {
                            unavailable_components: item.unavailable_component_count.value(),
                            repository_ingestion: item.repository_ingestion_count.value(),
                            session_supervision: item.session_supervision_count.value(),
                            lifecycle_weeks: item.lifecycle_week_count.value(),
                            lifecycle_deadline_violations: item
                                .lifecycle_deadline_violation_count
                                .value(),
                        }) =>
                {
                    outbox_quarantines = item.outbox_quarantine_count.value();
                    break;
                }
                OperatorStatusMessage::Start {} | OperatorStatusMessage::End(_) => {
                    return Err(ClientError::Protocol(
                        "operator status sequence or count was invalid",
                    ));
                }
            },
            ServerMessage::Error {
                code,
                message,
                detail,
            } => return Err(ClientError::remote(*code, message.clone(), *detail)),
            _ => {
                return Err(ClientError::Protocol(
                    "operator status sequence or count was invalid",
                ));
            }
        };
        let Some(item_phase) = item_phase else {
            return Err(ClientError::Protocol(
                "operator status sequence was invalid",
            ));
        };
        if item_phase < phase {
            return Err(ClientError::Protocol(
                "operator status sections were out of order",
            ));
        }
        phase = item_phase;
        spool.write_all(&encode_server_line(&frame)?)?;
    }
    output.operator_status_counts(OperatorStatusPresentationCounts {
        lifecycle_weeks: counts.lifecycle_weeks,
        session_supervision: counts.session_supervision,
        lifecycle_deadline_violations: counts.lifecycle_deadline_violations,
        outbox_quarantines,
    })?;
    spool.seek(SeekFrom::Start(0))?;
    let mut reader = BufReader::new(spool);
    let mut line = Vec::new();
    while reader.read_until(b'\n', &mut line)? != 0 {
        output.operator_status_item(decode_server_line(&line)?.message())?;
        line.clear();
    }
    Ok(output.operator_status_model_usage_omitted()?)
}

fn status_increment(value: u64) -> Result<u64, ClientError> {
    value
        .checked_add(1)
        .ok_or(ClientError::Protocol("operator status count overflowed"))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SessionSummary {
    pub(crate) session_id: CanonicalUuid,
    pub(crate) defaults_version: u64,
}

pub(crate) async fn read_session_summaries(
    client: &mut ProcessClient,
    mut consume: impl FnMut(SessionSummary, &ServerFrame) -> Result<(), ClientError>,
) -> Result<(), ClientError> {
    let mut connection = client.request(ClientRequest::ListSessions {}).await?;
    match connection.message().await? {
        ServerMessage::SessionsStart {} => {}
        ServerMessage::Error {
            code,
            message,
            detail,
        } => return Err(ClientError::remote(code, message, detail)),
        _ => {
            return Err(ClientError::Protocol(
                "session list did not begin with its start frame",
            ));
        }
    }
    let mut prior_session = None;
    let mut summary_count = 0_u64;
    loop {
        let frame = connection.frame().await?;
        match frame.message() {
            ServerMessage::SessionSummary {
                session_id,
                defaults_version,
                ..
            } => {
                if prior_session
                    .is_some_and(|prior: CanonicalUuid| prior.into_uuid() >= session_id.into_uuid())
                {
                    return Err(ClientError::Protocol(
                        "session summaries were not strictly ordered",
                    ));
                }
                let summary = SessionSummary {
                    session_id: *session_id,
                    defaults_version: defaults_version.value(),
                };
                consume(summary, &frame)?;
                prior_session = Some(*session_id);
                summary_count = summary_count
                    .checked_add(1)
                    .ok_or(ClientError::Protocol("session summary count overflowed"))?;
            }
            ServerMessage::SessionsEnd { session_count }
                if session_count.value() == summary_count =>
            {
                return Ok(());
            }
            ServerMessage::Error {
                code,
                message,
                detail,
            } => return Err(ClientError::remote(*code, message.clone(), *detail)),
            _ => {
                return Err(ClientError::Protocol(
                    "session list sequence or count was invalid",
                ));
            }
        }
    }
}

pub(crate) fn placement_display(placement: &SessionPlacement) -> String {
    match placement {
        SessionPlacement::Pathless {} => String::from("placement=pathless"),
        SessionPlacement::Scoped { path } => format!("placement={path}"),
        SessionPlacement::RootGlobalRead { path, .. } => {
            format!("placement={path} root_global_read=acknowledged")
        }
    }
}
