use super::*;

pub(crate) async fn send(
    client: &mut ProcessClient,
    output: &mut Output<'_>,
    session_id: CanonicalUuid,
    command_id: Option<CommandId>,
    defaults_version: Option<CanonicalU64>,
    delivery: SendDeliveryArgument,
    content: String,
) -> Result<(), ClientError> {
    let (command_id, generated) = command_identity(command_id)?;
    if generated {
        output.recovery_value(
            "command_id",
            &command_id.into_uuid().hyphenated().to_string(),
        )?;
    }
    let delivery = match delivery {
        SendDeliveryArgument::StartWhenIdle => None,
        SendDeliveryArgument::Queue {
            expected_active_turn_id,
        } => {
            let expected_active_turn = match expected_active_turn_id {
                Some(turn_id) => turn_id,
                None => observe_active_turn(client, session_id).await?,
            };
            output.recovery_value("turn", &expected_active_turn.to_string())?;
            Some(InputDelivery::Queue {
                expected_active_turn_id: expected_active_turn,
            })
        }
    };
    let defaults_version =
        resolve_defaults_version(client, output, session_id, defaults_version).await?;

    let receipt = submit_input(
        client,
        command_id,
        session_id,
        InputContent::new(content),
        Some(defaults_version),
        delivery,
    )
    .await?;
    let SubmitInputReceipt::Turn { turn_id } = receipt else {
        return Err(ClientError::Protocol("send returned a steering receipt").mutation());
    };

    await_and_report_turn(client, output, session_id, turn_id).await
}

pub(crate) async fn steer(
    client: &mut ProcessClient,
    output: &mut Output<'_>,
    session_id: CanonicalUuid,
    command_id: Option<CommandId>,
    turn_id: Option<CanonicalUuid>,
    content: String,
) -> Result<(), ClientError> {
    let (command_id, generated) = command_identity(command_id)?;
    if generated {
        output.recovery_value(
            "command_id",
            &command_id.into_uuid().hyphenated().to_string(),
        )?;
    }
    let expected_active_turn = match turn_id {
        Some(turn_id) => turn_id,
        None => observe_active_turn(client, session_id).await?,
    };
    output.recovery_value("turn", &expected_active_turn.to_string())?;

    let receipt = submit_input(
        client,
        command_id,
        session_id,
        InputContent::new(content),
        None,
        Some(InputDelivery::Steer {
            expected_active_turn_id: expected_active_turn,
        }),
    )
    .await?;
    let SubmitInputReceipt::Steering {
        accepted_input_id,
        acceptance_position,
        source_turn_id,
    } = receipt
    else {
        return Err(ClientError::Protocol("steer returned a turn-origin receipt").mutation());
    };
    if source_turn_id != expected_active_turn {
        return Err(ClientError::Protocol("steer returned another source turn").mutation());
    }
    output.steering_submitted(accepted_input_id, acceptance_position, source_turn_id)?;
    Ok(())
}

/// Supplies the user reconciliation decision a turn parked on an ambiguous
/// model call requires, then continues the session with the given content.
///
/// The parked turn terminalizes as reconciliation-required — its ambiguity is
/// recorded, never resolved into a fabricated outcome — and the content becomes
/// the immediate successor turn this verb then follows to its own terminal.
pub(crate) async fn reconcile(
    client: &mut ProcessClient,
    output: &mut Output<'_>,
    session_id: CanonicalUuid,
    turn_id: CanonicalUuid,
    command_id: Option<CommandId>,
    defaults_version: Option<CanonicalU64>,
    content: String,
) -> Result<(), ClientError> {
    let (command_id, generated) = command_identity(command_id)?;
    if generated {
        output.recovery_value(
            "command_id",
            &command_id.into_uuid().hyphenated().to_string(),
        )?;
    }
    let defaults_version =
        resolve_defaults_version(client, output, session_id, defaults_version).await?;

    let successor_turn_id = reconcile_turn(
        client,
        command_id,
        session_id,
        turn_id,
        InputContent::new(content),
        defaults_version,
    )
    .await?;

    await_and_report_turn(client, output, session_id, successor_turn_id).await
}

/// Requests cancellation of the exact active turn through the interrupt
/// treatment, then continues the session with the given content.
///
/// The stopped turn terminalizes through the existing lifecycle — a prepared
/// call cancels directly, an issued call first enters its durable
/// cancellation-requested state — and the content becomes the
/// immediate-successor turn this verb then follows to its own terminal.
#[expect(
    clippy::too_many_arguments,
    reason = "the closed stop request keeps each recovery and scope input explicit"
)]
pub(crate) async fn stop(
    client: &mut ProcessClient,
    output: &mut Output<'_>,
    session_id: CanonicalUuid,
    turn_id: Option<CanonicalUuid>,
    command_id: Option<CommandId>,
    defaults_version: Option<CanonicalU64>,
    descendants: bool,
    content: String,
) -> Result<(), ClientError> {
    let (command_id, generated) = command_identity(command_id)?;
    if generated {
        output.recovery_value(
            "command_id",
            &command_id.into_uuid().hyphenated().to_string(),
        )?;
    }
    let expected_active_turn = match turn_id {
        Some(turn_id) => turn_id,
        None => observe_active_turn(client, session_id).await?,
    };
    output.recovery_value("turn", &expected_active_turn.to_string())?;
    let defaults_version =
        resolve_defaults_version(client, output, session_id, defaults_version).await?;

    let receipt = stop_turn(
        client,
        command_id,
        session_id,
        expected_active_turn,
        InputContent::new(content),
        defaults_version,
        descendant_scope(descendants),
    )
    .await?;

    output.termination_receipt(receipt.termination)?;
    await_and_report_turn(client, output, session_id, receipt.turn_id).await
}

/// Reads the authoritative transcript and returns the single turn holding the
/// session's active slot.
async fn observe_active_turn(
    client: &mut ProcessClient,
    session_id: CanonicalUuid,
) -> Result<CanonicalUuid, ClientError> {
    let mut snapshot = transcript_command(client, session_id).await?;
    snapshot
        .active_turn()?
        .ok_or(ClientError::Input("the session has no active turn"))
}

async fn resolve_defaults_version(
    client: &mut ProcessClient,
    output: &mut Output<'_>,
    session_id: CanonicalUuid,
    defaults_version: Option<CanonicalU64>,
) -> Result<CanonicalU64, ClientError> {
    let defaults_version = match defaults_version {
        Some(version) => version,
        None => {
            let mut selected = None;
            read_session_summaries(client, |summary, _| {
                if summary.session_id == session_id {
                    selected = Some(CanonicalU64::new(summary.defaults_version));
                }
                Ok(())
            })
            .await?;
            selected.ok_or(ClientError::Input("the selected session was not listed"))?
        }
    };
    output.recovery_value("defaults_version", &defaults_version.value().to_string())?;
    Ok(defaults_version)
}

async fn await_and_report_turn(
    client: &mut ProcessClient,
    output: &mut Output<'_>,
    session_id: CanonicalUuid,
    turn_id: CanonicalUuid,
) -> Result<(), ClientError> {
    match await_turn_terminal(client, session_id, turn_id).await? {
        TurnTerminal::Completed => {
            let mut snapshot = transcript_command(client, session_id).await?;
            let state = snapshot.turn_state(turn_id)?;
            if !matches!(state.as_ref(), Some(TurnState::Completed { .. })) {
                return Err(ClientError::Protocol(
                    "terminal reread did not retain completed turn state",
                ));
            }
            write_assistant_texts(&mut snapshot, output, turn_id)?;
            Ok(())
        }
        TurnTerminal::Failed => {
            let mut snapshot = transcript_command(client, session_id).await?;
            match snapshot.turn_state(turn_id)? {
                Some(TurnState::Failed {
                    terminal_model_call,
                    ..
                }) => Err(ClientError::TurnFailed(
                    terminal_model_call.and_then(|call| call.cause()),
                )),
                _ => Err(ClientError::Protocol(
                    "terminal reread did not retain failed turn state",
                )),
            }
        }
        TurnTerminal::Refused => Err(ClientError::TurnRefused),
        TurnTerminal::Cancelled => Err(ClientError::TurnCancelled),
        TurnTerminal::ReconciliationRequired => Err(ClientError::TurnReconciliationRequired),
    }
}

/// Supplies one user decision for a pending tool request and validates the
/// exact recorded receipt.
pub(crate) async fn decide(
    client: &mut ProcessClient,
    output: &mut Output<'_>,
    session_id: CanonicalUuid,
    tool_request_id: CanonicalUuid,
    command_id: Option<CommandId>,
    decision: ToolDecision,
) -> Result<(), ClientError> {
    let (command_id, generated) = command_identity(command_id)?;
    if generated {
        output.recovery_value(
            "command_id",
            &command_id.into_uuid().hyphenated().to_string(),
        )?;
    }
    let mut connection = client
        .mutation_request(ClientRequest::DecideToolRequest {
            command_id,
            session_id,
            tool_request_id,
            decision: decision.clone(),
        })
        .await?;
    match connection.message().await.map_err(ClientError::mutation)? {
        ServerMessage::ToolRequestDecided {
            tool_request_id: decided_request,
            decision: recorded_decision,
        } if decided_request == tool_request_id && recorded_decision == decision => {
            output.tool_request_decided(tool_request_id, &decision)?;
            Ok(())
        }
        ServerMessage::Error {
            code,
            message,
            detail,
        } => Err(ClientError::remote(code, message, detail).mutation()),
        _ => Err(ClientError::Protocol("decision returned an unexpected receipt").mutation()),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SubmitInputReceipt {
    Turn {
        turn_id: CanonicalUuid,
    },
    Steering {
        accepted_input_id: CanonicalUuid,
        acceptance_position: u64,
        source_turn_id: CanonicalUuid,
    },
}

pub(crate) async fn submit_input(
    client: &mut ProcessClient,
    command_id: CommandId,
    session_id: CanonicalUuid,
    content: InputContent,
    expected_defaults_version: Option<CanonicalU64>,
    delivery: Option<InputDelivery>,
) -> Result<SubmitInputReceipt, ClientError> {
    let mut connection = client
        .mutation_request(ClientRequest::SubmitInput {
            command_id,
            session_id,
            content: signalbox_process_protocol::UserInputContent::text(content.into_string()),
            expected_defaults_version,
            model_settings: ModelSettingsOverlay::inherit_all(),
            delivery,
        })
        .await?;
    match connection.message().await.map_err(ClientError::mutation)? {
        ServerMessage::InputSubmitted {
            session_id: submitted_session,
            turn_id,
            termination: None,
            ..
        } if submitted_session == session_id => Ok(SubmitInputReceipt::Turn { turn_id }),
        ServerMessage::SteeringSubmitted {
            session_id: submitted_session,
            accepted_input_id,
            acceptance_position,
            source_turn_id,
        } if submitted_session == session_id => Ok(SubmitInputReceipt::Steering {
            accepted_input_id,
            acceptance_position: acceptance_position.value(),
            source_turn_id,
        }),
        ServerMessage::Error {
            code,
            message,
            detail,
        } => Err(ClientError::remote(code, message, detail).mutation()),
        _ => Err(ClientError::Protocol("submit returned an unexpected response").mutation()),
    }
}

pub(crate) async fn reconcile_turn(
    client: &mut ProcessClient,
    command_id: CommandId,
    session_id: CanonicalUuid,
    expected_active_turn_id: CanonicalUuid,
    content: InputContent,
    defaults_version: CanonicalU64,
) -> Result<CanonicalUuid, ClientError> {
    let mut connection = client
        .mutation_request(ClientRequest::ReconcileTurn {
            command_id,
            session_id,
            expected_active_turn_id,
            content: signalbox_process_protocol::UserInputContent::text(content.into_string()),
            expected_defaults_version: defaults_version,
            model_settings: ModelSettingsOverlay::inherit_all(),
        })
        .await?;
    match connection.message().await.map_err(ClientError::mutation)? {
        ServerMessage::InputSubmitted {
            session_id: submitted_session,
            turn_id,
            termination: None,
            ..
        } if submitted_session == session_id => Ok(turn_id),
        ServerMessage::Error {
            code,
            message,
            detail,
        } => Err(ClientError::remote(code, message, detail).mutation()),
        _ => Err(ClientError::Protocol("reconcile returned an unexpected response").mutation()),
    }
}

pub(crate) const fn descendant_scope(descendants: bool) -> DescendantTerminationScope {
    if descendants {
        DescendantTerminationScope::ParentAndDescendants
    } else {
        DescendantTerminationScope::ParentAlone
    }
}

pub(crate) struct StopTurnReceipt {
    pub(crate) turn_id: CanonicalUuid,
    pub(crate) termination: signalbox_process_protocol::TerminationReceipt,
}

pub(crate) async fn stop_turn(
    client: &mut ProcessClient,
    command_id: CommandId,
    session_id: CanonicalUuid,
    expected_active_turn_id: CanonicalUuid,
    content: InputContent,
    defaults_version: CanonicalU64,
    descendant_scope: DescendantTerminationScope,
) -> Result<StopTurnReceipt, ClientError> {
    let mut connection = client
        .mutation_request(ClientRequest::StopTurn {
            command_id,
            session_id,
            expected_active_turn_id,
            content: signalbox_process_protocol::UserInputContent::text(content.into_string()),
            expected_defaults_version: defaults_version,
            descendant_scope,
            model_settings: ModelSettingsOverlay::inherit_all(),
        })
        .await?;
    match connection.message().await.map_err(ClientError::mutation)? {
        ServerMessage::InputSubmitted {
            session_id: submitted_session,
            turn_id,
            termination: Some(termination),
            ..
        } if submitted_session == session_id
            && termination.descendant_scope == descendant_scope =>
        {
            Ok(StopTurnReceipt {
                turn_id,
                termination,
            })
        }
        ServerMessage::Error {
            code,
            message,
            detail,
        } => Err(ClientError::remote(code, message, detail).mutation()),
        _ => Err(ClientError::Protocol("stop returned an unexpected response").mutation()),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TurnTerminal {
    Completed,
    Failed,
    Refused,
    Cancelled,
    ReconciliationRequired,
}

pub(crate) async fn await_turn_terminal(
    client: &mut ProcessClient,
    session_id: CanonicalUuid,
    turn_id: CanonicalUuid,
) -> Result<TurnTerminal, ClientError> {
    let mut retry = crate::connection::FollowRetry::default();
    loop {
        let result: Result<Option<TurnTerminal>, ClientError> = async {
            let mut connection = client
                .request(ClientRequest::FollowSession { session_id })
                .await?;
            let mut snapshot = read_snapshot(client, &mut connection, session_id).await?;
            let state = snapshot.turn_state(turn_id)?;
            if let Some(terminal) = terminal_snapshot_state(state.as_ref())? {
                return Ok(Some(terminal));
            }
            queued_turn_recovery(&mut snapshot, turn_id)?;
            let mut poll_automatic_recovery = automatic_recovery_pending(&mut snapshot, turn_id)?;
            let mut observed_cursor = snapshot.cursor();
            loop {
                let message = if poll_automatic_recovery {
                    match tokio::time::timeout(
                        FOLLOW_RECOVERY_REFETCH_INTERVAL,
                        connection.message(),
                    )
                    .await
                    {
                        Ok(message) => message?,
                        Err(_) => {
                            let mut refreshed = transcript_command(client, session_id).await?;
                            let refreshed_state = refreshed.turn_state(turn_id)?;
                            if let Some(terminal) =
                                terminal_snapshot_state(refreshed_state.as_ref())?
                            {
                                return Ok(Some(terminal));
                            }
                            queued_turn_recovery(&mut refreshed, turn_id)?;
                            poll_automatic_recovery =
                                automatic_recovery_pending(&mut refreshed, turn_id)?;
                            continue;
                        }
                    }
                } else {
                    connection.message().await?
                };
                match message {
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
                        if let Some(terminal) = terminal_event_state(&event, turn_id) {
                            if matches!(event, SessionEvent::TurnFailed { .. }) {
                                let mut refreshed = transcript_command(client, session_id).await?;
                                let state = refreshed.turn_state(turn_id)?;
                                return terminal_snapshot_state(state.as_ref())?
                                    .ok_or(ClientError::Protocol(
                                        "a failure event lacks a terminal snapshot",
                                    ))
                                    .map(Some);
                            }
                            return Ok(Some(terminal));
                        }
                        if selected_turn_recovery_transition(&event, turn_id) {
                            let mut refreshed = transcript_command(client, session_id).await?;
                            let refreshed_state = refreshed.turn_state(turn_id)?;
                            if let Some(terminal) =
                                terminal_snapshot_state(refreshed_state.as_ref())?
                            {
                                return Ok(Some(terminal));
                            }
                            queued_turn_recovery(&mut refreshed, turn_id)?;
                            poll_automatic_recovery =
                                automatic_recovery_pending(&mut refreshed, turn_id)?;
                            if poll_automatic_recovery {
                                continue;
                            }
                            if !runner_recovery_transition(&event) {
                                return Err(ClientError::Protocol(
                                    "a recovery event did not produce recovery or terminal state",
                                ));
                            }
                        }
                        if child_lifecycle_terminalization(&event, session_id) {
                            let mut refreshed = transcript_command(client, session_id).await?;
                            let refreshed_state = refreshed.turn_state(turn_id)?;
                            if let Some(terminal) =
                                terminal_snapshot_state(refreshed_state.as_ref())?
                            {
                                return Ok(Some(terminal));
                            }
                            poll_automatic_recovery =
                                automatic_recovery_pending(&mut refreshed, turn_id)?;
                        }
                        if session_recovery_transition(&event) {
                            let mut refreshed = transcript_command(client, session_id).await?;
                            let refreshed_state = refreshed.turn_state(turn_id)?;
                            if let Some(terminal) =
                                terminal_snapshot_state(refreshed_state.as_ref())?
                            {
                                return Ok(Some(terminal));
                            }
                            queued_turn_recovery(&mut refreshed, turn_id)?;
                            poll_automatic_recovery =
                                automatic_recovery_pending(&mut refreshed, turn_id)?;
                        }
                    }
                    ServerMessage::ProviderTextDelta {
                        session_id: delta_session,
                        ..
                    } if delta_session == session_id => {}
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
            Ok(None)
        }
        .await;
        match result {
            Ok(Some(terminal)) => return Ok(terminal),
            Ok(None) => {}
            Err(error) => tokio::time::sleep(retry.next_delay(error)?).await,
        }
    }
}

/// Reports whether bounded daemon reconciliation still owns the wait the
/// follow is blocked on, so the follower keeps polling instead of exiting.
///
/// Both recovery waits carry the same durable budget and the same
/// `operator_action_required` projection, so a tool wait is followed on
/// exactly the terms a model-call wait is.
fn automatic_recovery_pending(
    snapshot: &mut TranscriptSnapshot,
    selected_turn: CanonicalUuid,
) -> Result<bool, ClientError> {
    let selected_state = snapshot
        .turn_state(selected_turn)?
        .ok_or(ClientError::Protocol(
            "follow snapshot omitted the submitted turn",
        ))?;
    if matches!(
        selected_state,
        TurnState::ActiveAwaitingModelCallRecovery {
            operator_action_required: false,
            ..
        } | TurnState::ActiveAwaitingToolRecovery {
            operator_action_required: false,
            ..
        }
    ) {
        return Ok(true);
    }
    if !matches!(selected_state, TurnState::Queued { .. }) {
        return Ok(false);
    }
    let Some(active_turn) = snapshot.active_turn()? else {
        return Ok(false);
    };
    Ok(matches!(
        snapshot.turn_state(active_turn)?,
        Some(
            TurnState::ActiveAwaitingModelCallRecovery {
                operator_action_required: false,
                ..
            } | TurnState::ActiveAwaitingToolRecovery {
                operator_action_required: false,
                ..
            }
        )
    ))
}

pub(crate) fn queued_turn_recovery(
    snapshot: &mut TranscriptSnapshot,
    selected_turn: CanonicalUuid,
) -> Result<(), ClientError> {
    let selected_state = snapshot
        .turn_state(selected_turn)?
        .ok_or(ClientError::Protocol(
            "follow snapshot omitted the submitted queued turn",
        ))?;
    if !matches!(selected_state, TurnState::Queued { .. }) {
        return Ok(());
    }

    if let Some(active_turn) = snapshot.active_turn()? {
        let active_state = snapshot
            .turn_state(active_turn)?
            .ok_or(ClientError::Protocol(
                "follow snapshot omitted the session active turn",
            ))?;
        blocker_recovery_snapshot_state(&active_state)?;
    }
    queued_turn_runner_recovery(snapshot.runner())
}

pub(crate) fn blocker_recovery_snapshot_state(state: &TurnState) -> Result<(), ClientError> {
    match state {
        TurnState::ActiveAwaitingModelCallRecovery {
            operator_action_required: true,
            ..
        }
        | TurnState::ActiveAwaitingToolRecovery {
            operator_action_required: true,
            ..
        } => Err(ClientError::TurnRecoveryRequired),
        TurnState::ActiveAwaitingModelCallRecovery {
            operator_action_required: false,
            ..
        }
        | TurnState::ActiveAwaitingToolRecovery {
            operator_action_required: false,
            ..
        } => Ok(()),
        TurnState::ActiveAwaitingRunnerRecovery { .. } => Err(ClientError::RunnerRecoveryRequired),
        TurnState::Queued { .. }
        | TurnState::QueuedDelegated { .. }
        | TurnState::QueuedDelegationWake { .. }
        | TurnState::DelegationTerminated { .. }
        | TurnState::ActiveRunning { .. }
        | TurnState::ActiveAwaitingToolApproval { .. }
        | TurnState::ActiveAwaitingCredentialAvailability { .. }
        | TurnState::ActiveAwaitingChild { .. }
        | TurnState::Completed { .. }
        | TurnState::FailedCredentialPoolExhausted { .. }
        | TurnState::Failed { .. }
        | TurnState::Refused { .. }
        | TurnState::Cancelled { .. }
        | TurnState::ReconciliationRequired { .. }
        | TurnState::ToolReconciliationRequired { .. } => Ok(()),
    }
}

/// Reports whether this event terminalizes the followed session's own turn as
/// the child of a parent cascade.
///
/// A descendant cascade addresses its lifecycle disposition to the terminalized
/// child as well as to the commanding parent, and the child-addressed row names
/// the relationship rather than the child turn. A follower therefore cannot
/// project the disposition onto its tracked turn directly; it re-reads
/// authoritative turn state, where the retained delegated turn projects as
/// `TurnState::DelegationTerminated`.
pub(crate) fn child_lifecycle_terminalization(
    event: &SessionEvent,
    session_id: CanonicalUuid,
) -> bool {
    matches!(
        event,
        SessionEvent::ChildLifecycleDisposition {
            child_session_id,
            ..
        } if *child_session_id == session_id
    )
}

pub(crate) fn session_recovery_transition(event: &SessionEvent) -> bool {
    matches!(
        event,
        SessionEvent::ModelCallTransition {
            state: ModelCallState::Terminal {
                disposition: ModelCallDisposition::Ambiguous,
            },
            ..
        } | SessionEvent::ToolBatchTransition {
            state: ToolBatchState::RecoveryRequired { .. },
            ..
        } | SessionEvent::TurnReconciliationRequired { .. }
            | SessionEvent::TurnToolReconciliationRequired { .. }
    )
}

pub(crate) fn tool_recovery_transition(event: &SessionEvent, selected_turn: CanonicalUuid) -> bool {
    matches!(
        event,
        SessionEvent::ToolBatchTransition {
            turn_id,
            state: ToolBatchState::RecoveryRequired { .. },
            ..
        } if *turn_id == selected_turn
    )
}

pub(crate) fn model_call_recovery_transition(
    event: &SessionEvent,
    selected_turn: CanonicalUuid,
) -> bool {
    matches!(
        event,
        SessionEvent::ModelCallTransition {
            turn_id,
            state: ModelCallState::Terminal {
                disposition: ModelCallDisposition::Ambiguous,
            },
            ..
        } if *turn_id == selected_turn
    )
}

fn runner_recovery_transition(event: &SessionEvent) -> bool {
    matches!(
        event,
        SessionEvent::RunnerStateTransition {
            state: RunnerStateTransitionState::RunnerLostBeforePin
                | RunnerStateTransitionState::RunnerLost,
            ..
        }
    )
}

/// Rejects when the authoritative snapshot retains a current runner loss that
/// prevents the selected queued turn from activating.
pub(crate) fn queued_turn_runner_recovery(
    runner: Option<&RunnerProjection>,
) -> Result<(), ClientError> {
    if runner.is_some_and(|runner| {
        matches!(
            runner.state(),
            RunnerProjectionState::RunnerLostBeforePin | RunnerProjectionState::RunnerLost
        ) || matches!(
            runner.connection_health(),
            Some(RunnerConnectionHealth::Shutdown | RunnerConnectionHealth::Lost)
        )
    }) {
        return Err(ClientError::RunnerRecoveryRequired);
    }
    Ok(())
}

pub(crate) fn selected_turn_recovery_transition(
    event: &SessionEvent,
    selected_turn: CanonicalUuid,
) -> bool {
    model_call_recovery_transition(event, selected_turn)
        || tool_recovery_transition(event, selected_turn)
        || runner_recovery_transition(event)
}

pub(crate) fn terminal_snapshot_state(
    state: Option<&TurnState>,
) -> Result<Option<TurnTerminal>, ClientError> {
    match state {
        Some(TurnState::Completed { .. }) => Ok(Some(TurnTerminal::Completed)),
        Some(TurnState::FailedCredentialPoolExhausted { .. }) | Some(TurnState::Failed { .. }) => {
            Ok(Some(TurnTerminal::Failed))
        }
        Some(TurnState::Refused { .. }) => Ok(Some(TurnTerminal::Refused)),
        Some(TurnState::Cancelled { .. }) => Ok(Some(TurnTerminal::Cancelled)),
        Some(TurnState::DelegationTerminated { .. }) => Ok(Some(TurnTerminal::Cancelled)),
        Some(
            TurnState::ReconciliationRequired { .. } | TurnState::ToolReconciliationRequired { .. },
        ) => Ok(Some(TurnTerminal::ReconciliationRequired)),
        Some(
            TurnState::Queued { .. }
            | TurnState::QueuedDelegated { .. }
            | TurnState::QueuedDelegationWake { .. }
            | TurnState::ActiveRunning { .. }
            | TurnState::ActiveAwaitingToolApproval { .. }
            | TurnState::ActiveAwaitingCredentialAvailability { .. }
            | TurnState::ActiveAwaitingChild { .. },
        ) => Ok(None),
        Some(
            TurnState::ActiveAwaitingModelCallRecovery {
                operator_action_required: true,
                ..
            }
            | TurnState::ActiveAwaitingToolRecovery {
                operator_action_required: true,
                ..
            },
        ) => Err(ClientError::TurnRecoveryRequired),
        Some(
            TurnState::ActiveAwaitingModelCallRecovery {
                operator_action_required: false,
                ..
            }
            | TurnState::ActiveAwaitingToolRecovery {
                operator_action_required: false,
                ..
            },
        ) => Ok(None),
        Some(TurnState::ActiveAwaitingRunnerRecovery { .. }) => {
            Err(ClientError::RunnerRecoveryRequired)
        }
        None => Err(ClientError::Protocol(
            "follow snapshot omitted the submitted turn",
        )),
    }
}

pub(crate) fn terminal_event_state(
    event: &SessionEvent,
    selected_turn: CanonicalUuid,
) -> Option<TurnTerminal> {
    match event {
        SessionEvent::TurnCompleted { turn_id, .. } if *turn_id == selected_turn => {
            Some(TurnTerminal::Completed)
        }
        SessionEvent::TurnCredentialPoolExhausted { turn_id, .. }
        | SessionEvent::TurnFailed { turn_id, .. }
            if *turn_id == selected_turn =>
        {
            Some(TurnTerminal::Failed)
        }
        SessionEvent::TurnRefused { turn_id, .. } if *turn_id == selected_turn => {
            Some(TurnTerminal::Refused)
        }
        SessionEvent::TurnCancelled { turn_id, .. } if *turn_id == selected_turn => {
            Some(TurnTerminal::Cancelled)
        }
        SessionEvent::TurnReconciliationRequired { turn_id, .. } if *turn_id == selected_turn => {
            Some(TurnTerminal::ReconciliationRequired)
        }
        SessionEvent::TurnToolReconciliationRequired { turn_id, .. }
            if *turn_id == selected_turn =>
        {
            Some(TurnTerminal::ReconciliationRequired)
        }
        SessionEvent::SessionCreated {}
        | SessionEvent::SessionModelSettingsChanged { .. }
        | SessionEvent::TurnModelSettingsResolved { .. }
        | SessionEvent::InputAccepted { .. }
        | SessionEvent::GoalTurnRetired { .. }
        | SessionEvent::TurnActivated { .. }
        | SessionEvent::ContextCompacted { .. }
        | SessionEvent::ToolApprovalDecided { .. }
        | SessionEvent::ModelCallTransition { .. }
        | SessionEvent::ToolBatchTransition { .. }
        | SessionEvent::RunnerStateTransition { .. }
        | SessionEvent::TurnCompleted { .. }
        | SessionEvent::TurnCredentialPoolExhausted { .. }
        | SessionEvent::TurnFailed { .. }
        | SessionEvent::TurnRefused { .. }
        | SessionEvent::TurnCancelled { .. }
        | SessionEvent::TurnReconciliationRequired { .. }
        | SessionEvent::TurnToolReconciliationRequired { .. }
        | SessionEvent::ChildSpawned { .. }
        | SessionEvent::ChildWaiting { .. }
        | SessionEvent::SessionMessage { .. }
        | SessionEvent::ChildResult { .. }
        | SessionEvent::ChildLifecycleDisposition { .. } => None,
    }
}
