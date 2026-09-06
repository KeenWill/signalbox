use super::*;

pub(super) async fn reject_uncomposed_spawn<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    write_error(
        writer,
        version,
        request_id,
        ProtocolError::without_detail(ErrorCode::InvalidRequest),
    )
    .await
}

#[expect(
    clippy::too_many_arguments,
    reason = "the closed await request keeps every durable correlation explicit"
)]
pub(super) async fn handle_await_session<Reader, Writer>(
    reader: &mut Reader,
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    session_id: CanonicalUuid,
    turn_id: CanonicalUuid,
    tool_request_id: CanonicalUuid,
    child_session_id: CanonicalUuid,
    mode: WireDelegationWaitMode,
    services: &ConnectionServices,
    mut shutdown: watch::Receiver<bool>,
) -> Result<(), ProcessConnectionError>
where
    Reader: AsyncBufRead + Unpin,
    Writer: AsyncWrite + Unpin,
{
    let session = SessionId::from_uuid(session_id.into_uuid());
    let turn = TurnId::from_uuid(turn_id.into_uuid());
    let request = ToolRequestId::from_uuid(tool_request_id.into_uuid());
    let child = SessionId::from_uuid(child_session_id.into_uuid());
    let mode = match mode {
        WireDelegationWaitMode::Foreground => DomainDelegationWaitMode::Foreground,
        WireDelegationWaitMode::Background => DomainDelegationWaitMode::Background,
    };
    let mut subscription = services.fanouts.durable.subscribe();
    let port = PostgresSessionDelegationPort::new(services.pool.clone());
    let Some(outcome) = run_until_shutdown(
        &mut shutdown,
        port.await_process_session(session, turn, request, child, mode),
    )
    .await
    else {
        return Ok(());
    };
    match outcome {
        Ok(ProcessDelegationOutcome::Applied(AwaitSessionPortOutcome::BackgroundRegistered(
            receipt,
        ))) => {
            nudge_delegation_issuer(&services.eligibility_nudge, session);
            write_message(
                writer,
                version,
                request_id,
                ServerMessage::SessionAwaitRegistered {
                    tool_request_id: wire_uuid(receipt.tool_request().into_uuid()),
                    child_session_id: wire_uuid(receipt.child().into_uuid()),
                    mode: WireDelegationWaitMode::Background,
                },
            )
            .await
        }
        Ok(ProcessDelegationOutcome::Applied(AwaitSessionPortOutcome::Delivered(result))) => {
            write_message(writer, version, request_id, wire_child_result(&result)?).await
        }
        Ok(ProcessDelegationOutcome::Applied(AwaitSessionPortOutcome::ForegroundPending(wait))) => {
            wait_for_foreground_child_result(
                reader,
                writer,
                version,
                request_id,
                &port,
                wait,
                turn,
                &mut subscription,
                shutdown,
            )
            .await
        }
        Ok(ProcessDelegationOutcome::InvalidRequest) => {
            write_error(
                writer,
                version,
                request_id,
                ProtocolError::without_detail(ErrorCode::InvalidRequest),
            )
            .await
        }
        Ok(ProcessDelegationOutcome::Rejected(rejection)) => {
            nudge_after_process_await_rejection(&services.eligibility_nudge, session, rejection);
            write_error(
                writer,
                version,
                request_id,
                process_delegation_rejection_for_recipient(
                    rejection,
                    session_id,
                    turn_id,
                    tool_request_id,
                    child_session_id,
                    session_id,
                ),
            )
            .await
        }
        Ok(ProcessDelegationOutcome::Applied(
            AwaitSessionPortOutcome::Rejected | AwaitSessionPortOutcome::DurablyRejected,
        )) => {
            write_error(
                writer,
                version,
                request_id,
                internal_protocol_error(
                    Some(session_id.into_uuid()),
                    InternalDiagnostic::SessionDelegationContract,
                ),
            )
            .await
        }
        Err(error) => {
            write_delegation_port_error(writer, version, request_id, session_id, error).await
        }
    }
}

pub(super) fn nudge_after_process_await_rejection(
    eligibility_nudge: &impl EligibilityNudge,
    issuer: SessionId,
    rejection: ProcessDelegationRequestRejection,
) {
    let attempt_ended = match rejection {
        ProcessDelegationRequestRejection::Operation(
            DelegationOperationRejection::RelationshipNotFound
            | DelegationOperationRejection::DeliverySequenceExhausted
            | DelegationOperationRejection::Transition { .. },
        ) => true,
        ProcessDelegationRequestRejection::SessionNotFound
        | ProcessDelegationRequestRejection::ToolRequestNotFound
        | ProcessDelegationRequestRejection::ToolRequestNotInSession
        | ProcessDelegationRequestRejection::RequestNotInTurn
        | ProcessDelegationRequestRejection::AwaitConflict
        | ProcessDelegationRequestRejection::MessageConflict
        | ProcessDelegationRequestRejection::MessageIdentityCollision { .. }
        | ProcessDelegationRequestRejection::Operation(
            DelegationOperationRejection::StaleDispatch { .. }
            | DelegationOperationRejection::MessageIdentityCollision,
        ) => false,
    };
    if attempt_ended {
        nudge_delegation_issuer(eligibility_nudge, issuer);
    }
}

pub(super) fn nudge_after_process_message_rejection(
    eligibility_nudge: &impl EligibilityNudge,
    issuer: SessionId,
    rejection: ProcessDelegationRequestRejection,
) {
    let attempt_ended = match rejection {
        ProcessDelegationRequestRejection::MessageIdentityCollision { .. }
        | ProcessDelegationRequestRejection::Operation(
            DelegationOperationRejection::RelationshipNotFound
            | DelegationOperationRejection::MessageIdentityCollision
            | DelegationOperationRejection::DeliverySequenceExhausted
            | DelegationOperationRejection::Transition { .. },
        ) => true,
        ProcessDelegationRequestRejection::SessionNotFound
        | ProcessDelegationRequestRejection::ToolRequestNotFound
        | ProcessDelegationRequestRejection::ToolRequestNotInSession
        | ProcessDelegationRequestRejection::RequestNotInTurn
        | ProcessDelegationRequestRejection::AwaitConflict
        | ProcessDelegationRequestRejection::MessageConflict
        | ProcessDelegationRequestRejection::Operation(
            DelegationOperationRejection::StaleDispatch { .. },
        ) => false,
    };
    if attempt_ended {
        nudge_delegation_issuer(eligibility_nudge, issuer);
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "foreground delivery keeps socket cancellation and durable correlation explicit"
)]
pub(super) async fn wait_for_foreground_child_result<Reader, Writer>(
    reader: &mut Reader,
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    port: &PostgresSessionDelegationPort,
    wait: DelegationWait,
    turn: TurnId,
    subscription: &mut broadcast::Receiver<ProcessUpdate>,
    mut shutdown: watch::Receiver<bool>,
) -> Result<(), ProcessConnectionError>
where
    Reader: AsyncBufRead + Unpin,
    Writer: AsyncWrite + Unpin,
{
    loop {
        let Some(delivery) =
            run_until_shutdown(&mut shutdown, port.load_foreground_delivery(wait)).await
        else {
            return Ok(());
        };
        match preserve_committed_foreground_wait(delivery) {
            CommittedForegroundDelivery::Delivered(result) => {
                return write_message(writer, version, request_id, wire_child_result(&result)?)
                    .await;
            }
            CommittedForegroundDelivery::Pending => {}
            CommittedForegroundDelivery::Retry(error) => {
                tracing::error!(
                    diagnostic = "delegation_foreground_delivery_reread_failed",
                    cause_code = error.operator_failure_cause_code(),
                    session_id = %wait.parent().as_uuid(),
                    turn_id = %turn.as_uuid(),
                    "foreground process delivery reread failed after wait commit"
                );
                tokio::select! {
                    () = wait_for_shutdown(&mut shutdown) => return Ok(()),
                    peer = foreground_peer_activity(reader) => return peer,
                    () = sleep(DELEGATION_DELIVERY_RETRY_INTERVAL) => continue,
                }
            }
        }
        loop {
            let update = tokio::select! {
                () = wait_for_shutdown(&mut shutdown) => return Ok(()),
                peer = foreground_peer_activity(reader) => return peer,
                update = subscription.recv() => update,
            };
            match update {
                Ok(update) if update_signals_child_result(&update, wait) => break,
                Ok(_) => {}
                Err(broadcast::error::RecvError::Lagged(_)) => break,
                Err(broadcast::error::RecvError::Closed) => return Ok(()),
            }
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
pub(super) enum CommittedForegroundDelivery<T, E> {
    Delivered(T),
    Pending,
    Retry(E),
}

pub(super) fn preserve_committed_foreground_wait<T, E>(
    delivery: Result<Option<T>, E>,
) -> CommittedForegroundDelivery<T, E> {
    match delivery {
        Ok(Some(delivered)) => CommittedForegroundDelivery::Delivered(delivered),
        Ok(None) => CommittedForegroundDelivery::Pending,
        Err(error) => CommittedForegroundDelivery::Retry(error),
    }
}

pub(super) async fn foreground_peer_activity<Reader>(
    reader: &mut Reader,
) -> Result<(), ProcessConnectionError>
where
    Reader: AsyncBufRead + Unpin,
{
    reader
        .fill_buf()
        .await
        .map_err(ProcessConnectionError::PeerIo)?;
    Err(ProcessConnectionError::PeerIo(io::Error::new(
        io::ErrorKind::ConnectionAborted,
        "foreground delegation peer ended or sent another request",
    )))
}

pub(super) fn update_signals_child_result(update: &ProcessUpdate, wait: DelegationWait) -> bool {
    match update {
        ProcessUpdate::Durable { session, event, .. } => match event {
            ProcessUpdateEvent::DelegationUpdate(delegation) => match delegation {
                DispatchedDelegationUpdate::ChildResult {
                    spawning_request,
                    child,
                    ..
                } => {
                    *session == wait.parent()
                        && *spawning_request == wait.spawning_request()
                        && *child == wait.child()
                }
                DispatchedDelegationUpdate::ChildSpawned { .. }
                | DispatchedDelegationUpdate::ChildWaiting { .. }
                | DispatchedDelegationUpdate::ChildLifecycleDisposition { .. }
                | DispatchedDelegationUpdate::SessionMessage { .. } => false,
            },
            ProcessUpdateEvent::SessionCreated
            | ProcessUpdateEvent::SessionModelSettingsChanged(_)
            | ProcessUpdateEvent::TurnModelSettingsResolved(_)
            | ProcessUpdateEvent::InputAccepted { .. }
            | ProcessUpdateEvent::GoalTurnRetired { .. }
            | ProcessUpdateEvent::TurnActivated { .. }
            | ProcessUpdateEvent::ModelCallTransition { .. }
            | ProcessUpdateEvent::ToolBatchTransition { .. }
            | ProcessUpdateEvent::ToolApprovalDecided { .. }
            | ProcessUpdateEvent::RunnerStateTransition { .. }
            | ProcessUpdateEvent::ContextCompacted { .. }
            | ProcessUpdateEvent::TurnCompleted { .. }
            | ProcessUpdateEvent::TurnFailed { .. }
            | ProcessUpdateEvent::TurnRefused { .. }
            | ProcessUpdateEvent::TurnCancelled { .. }
            | ProcessUpdateEvent::TurnReconciliationRequired { .. } => false,
        },
        ProcessUpdate::ProviderTextDelta(_) => false,
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "the closed message request keeps every durable correlation explicit"
)]
pub(super) async fn handle_send_session_message<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    session_id: CanonicalUuid,
    turn_id: CanonicalUuid,
    tool_request_id: CanonicalUuid,
    peer_session_id: CanonicalUuid,
    content: String,
    pool: &PgPool,
    eligibility_nudge: &InProcessEligibilityNudge,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let port = PostgresSessionDelegationPort::new(pool.clone());
    let result = port
        .send_process_message(
            SessionId::from_uuid(session_id.into_uuid()),
            TurnId::from_uuid(turn_id.into_uuid()),
            ToolRequestId::from_uuid(tool_request_id.into_uuid()),
            SessionId::from_uuid(peer_session_id.into_uuid()),
            content,
        )
        .await;
    match result {
        Ok(ProcessDelegationOutcome::Applied(receipt)) => {
            nudge_delegation_issuer(
                eligibility_nudge,
                SessionId::from_uuid(session_id.into_uuid()),
            );
            let direction = match receipt.direction() {
                DomainDelegationMessageDirection::ParentToChild => {
                    signalbox_process_protocol::DelegationMessageDirection::ParentToChild
                }
                DomainDelegationMessageDirection::ChildToParent => {
                    signalbox_process_protocol::DelegationMessageDirection::ChildToParent
                }
            };
            write_message(
                writer,
                version,
                request_id,
                ServerMessage::SessionMessageSent {
                    tool_request_id: wire_uuid(receipt.tool_request().into_uuid()),
                    message_id: wire_uuid(receipt.message().into_uuid()),
                    direction,
                    ordinal: CanonicalU64::new(receipt.ordinal().get()),
                    delivery_sequence: CanonicalU64::new(receipt.delivery_sequence().get()),
                },
            )
            .await
        }
        Ok(ProcessDelegationOutcome::InvalidRequest) => {
            write_error(
                writer,
                version,
                request_id,
                ProtocolError::without_detail(ErrorCode::InvalidRequest),
            )
            .await
        }
        Ok(ProcessDelegationOutcome::Rejected(rejection)) => {
            nudge_after_process_message_rejection(
                eligibility_nudge,
                SessionId::from_uuid(session_id.into_uuid()),
                rejection,
            );
            write_error(
                writer,
                version,
                request_id,
                process_delegation_rejection(
                    rejection,
                    session_id,
                    turn_id,
                    tool_request_id,
                    peer_session_id,
                ),
            )
            .await
        }
        Err(error) => {
            write_delegation_port_error(writer, version, request_id, session_id, error).await
        }
    }
}

pub(super) fn process_delegation_rejection(
    rejection: ProcessDelegationRequestRejection,
    session_id: CanonicalUuid,
    turn_id: CanonicalUuid,
    tool_request_id: CanonicalUuid,
    peer_session_id: CanonicalUuid,
) -> ProtocolError {
    process_delegation_rejection_for_recipient(
        rejection,
        session_id,
        turn_id,
        tool_request_id,
        peer_session_id,
        peer_session_id,
    )
}

pub(super) fn process_delegation_rejection_for_recipient(
    rejection: ProcessDelegationRequestRejection,
    session_id: CanonicalUuid,
    turn_id: CanonicalUuid,
    tool_request_id: CanonicalUuid,
    peer_session_id: CanonicalUuid,
    delivery_recipient_id: CanonicalUuid,
) -> ProtocolError {
    let detail = match rejection {
        ProcessDelegationRequestRejection::SessionNotFound => {
            RejectionDetail::SessionNotFound { session_id }
        }
        ProcessDelegationRequestRejection::ToolRequestNotFound => {
            RejectionDetail::ToolRequestNotFound { tool_request_id }
        }
        ProcessDelegationRequestRejection::ToolRequestNotInSession => {
            RejectionDetail::ToolRequestNotInSession {
                session_id,
                tool_request_id,
            }
        }
        ProcessDelegationRequestRejection::RequestNotInTurn => {
            RejectionDetail::DelegationRequestNotInTurn {
                session_id,
                turn_id,
                tool_request_id,
            }
        }
        ProcessDelegationRequestRejection::AwaitConflict => {
            RejectionDetail::DelegationAwaitConflict { tool_request_id }
        }
        ProcessDelegationRequestRejection::MessageConflict
        | ProcessDelegationRequestRejection::Operation(
            DelegationOperationRejection::Transition {
                failure: signalbox_domain::DelegationTransitionFailure::ConflictingMessageReplay,
                ..
            },
        ) => RejectionDetail::DelegationMessageConflict { tool_request_id },
        ProcessDelegationRequestRejection::MessageIdentityCollision { message } => {
            RejectionDetail::DelegationMessageIdentityCollision {
                message_id: wire_uuid(message.into_uuid()),
            }
        }
        ProcessDelegationRequestRejection::Operation(
            DelegationOperationRejection::RelationshipNotFound,
        ) => RejectionDetail::DelegationRelationNotFound {
            session_id,
            peer_session_id,
        },
        ProcessDelegationRequestRejection::Operation(
            DelegationOperationRejection::StaleDispatch { state },
        ) => RejectionDetail::DelegationToolRequestNotExecutable {
            tool_request_id,
            state: match state {
                DelegationRequestExecutionState::AwaitingApproval => {
                    WireDelegationToolRequestState::AwaitingApproval
                }
                DelegationRequestExecutionState::Denied => WireDelegationToolRequestState::Denied,
                DelegationRequestExecutionState::Approved => {
                    WireDelegationToolRequestState::Approved
                }
                DelegationRequestExecutionState::Prepared => {
                    WireDelegationToolRequestState::Prepared
                }
                DelegationRequestExecutionState::Closed => WireDelegationToolRequestState::Closed,
                DelegationRequestExecutionState::AttemptEnded => {
                    WireDelegationToolRequestState::AttemptEnded
                }
            },
        },
        ProcessDelegationRequestRejection::Operation(
            DelegationOperationRejection::Transition {
                spawning_request,
                failure: signalbox_domain::DelegationTransitionFailure::EventOrdinalExhausted,
            },
        ) => RejectionDetail::DelegationEventOrdinalExhausted {
            spawning_request_id: wire_uuid(spawning_request.into_uuid()),
            last: CanonicalU64::new(u64::MAX),
        },
        ProcessDelegationRequestRejection::Operation(
            DelegationOperationRejection::DeliverySequenceExhausted,
        ) => RejectionDetail::DelegationDeliverySequenceExhausted {
            recipient_session_id: delivery_recipient_id,
            last: CanonicalU64::new(u64::MAX),
        },
        ProcessDelegationRequestRejection::Operation(
            DelegationOperationRejection::MessageIdentityCollision
            | DelegationOperationRejection::Transition { .. },
        ) => {
            return internal_protocol_error(
                Some(session_id.into_uuid()),
                InternalDiagnostic::SessionDelegationContract,
            );
        }
    };
    ProtocolError::rejected(detail)
}

pub(super) fn wire_child_result(
    result: &DeliveredChildResult,
) -> Result<ServerMessage, ProcessConnectionError> {
    let wait = result.wait();
    let outcome = match result.kind() {
        DomainDelegationOutcomeKind::ResultReturned => WireDelegationOutcome::Returned,
        DomainDelegationOutcomeKind::ChildFailed => WireDelegationOutcome::Failed,
        DomainDelegationOutcomeKind::ChildStopped => WireDelegationOutcome::Stopped,
        DomainDelegationOutcomeKind::ChildCancelled => WireDelegationOutcome::Cancelled,
        DomainDelegationOutcomeKind::AlreadyTerminal
        | DomainDelegationOutcomeKind::ContinueRunning => {
            return Err(ProcessConnectionError::EncodeInvariant);
        }
    };
    let reason = match result.reason() {
        DomainDelegationOutcomeReason::ChildCompleted => WireDelegationReason::ChildCompleted,
        DomainDelegationOutcomeReason::ChildExecutionFailed => {
            WireDelegationReason::ChildExecutionFailed
        }
        DomainDelegationOutcomeReason::ChildResultUnavailable => {
            WireDelegationReason::ChildResultUnavailable
        }
        DomainDelegationOutcomeReason::ChildCancelled => WireDelegationReason::ChildCancelled,
        DomainDelegationOutcomeReason::ParentStopped { .. } => WireDelegationReason::ParentStopped,
        DomainDelegationOutcomeReason::ParentCancelled { .. } => {
            WireDelegationReason::ParentCancelled
        }
    };
    Ok(ServerMessage::ChildResult {
        await_request_id: wire_uuid(wait.awaiting_request().into_uuid()),
        spawning_request_id: wire_uuid(wait.spawning_request().into_uuid()),
        child_session_id: wire_uuid(wait.child().into_uuid()),
        outcome,
        content: result.content().map(|content| content.as_str().to_owned()),
        reason,
        provenance: wire_domain_delegation_provenance(result.provenance())?,
    })
}

pub(super) fn wire_domain_delegation_provenance(
    provenance: DomainDelegationProvenance,
) -> Result<WireDelegationProvenance, ProcessConnectionError> {
    let authority = match provenance.projection() {
        signalbox_domain::DelegationProvenanceProjection::ChildTurn { terminal } => {
            return Ok(WireDelegationProvenance::ChildTurn {
                child_session_id: wire_uuid(terminal.session().into_uuid()),
                child_turn_id: wire_uuid(terminal.turn().into_uuid()),
            });
        }
        signalbox_domain::DelegationProvenanceProjection::ParentCommand { authority } => authority,
        signalbox_domain::DelegationProvenanceProjection::ToolRequest { .. } => {
            return Err(ProcessConnectionError::EncodeInvariant);
        }
    };
    let descendant_scope = match authority.scope() {
        DescendantTerminationScope::ParentAlone => WireDescendantTerminationScope::ParentAlone,
        DescendantTerminationScope::ParentAndDescendants => {
            WireDescendantTerminationScope::ParentAndDescendants
        }
    };
    match authority.source() {
        ParentTerminationCommandSource::Turn { turn } => {
            Ok(WireDelegationProvenance::ParentTurnCommand {
                parent_session_id: wire_uuid(authority.parent().into_uuid()),
                parent_turn_id: wire_uuid(turn.into_uuid()),
                command_id: wire_uuid(authority.command().into_uuid()),
                descendant_scope,
            })
        }
        ParentTerminationCommandSource::Goal { generation } => {
            Ok(WireDelegationProvenance::ParentGoalCommand {
                parent_session_id: wire_uuid(authority.parent().into_uuid()),
                goal_generation: CanonicalU64::new(generation.get()),
                command_id: wire_uuid(authority.command().into_uuid()),
                descendant_scope,
            })
        }
        ParentTerminationCommandSource::Lifecycle => {
            Ok(WireDelegationProvenance::ParentLifecycleCommand {
                parent_session_id: wire_uuid(authority.parent().into_uuid()),
                command_id: wire_uuid(authority.command().into_uuid()),
                descendant_scope,
            })
        }
    }
}

pub(super) async fn write_delegation_port_error<Writer>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    session_id: CanonicalUuid,
    error: PostgresSessionDelegationPortError,
) -> Result<(), ProcessConnectionError>
where
    Writer: AsyncWrite + Unpin,
{
    let protocol_error = match error {
        PostgresSessionDelegationPortError::Repository(
            signalbox_persistence::session_delegation::SessionDelegationRepositoryError::Database(
                _,
            ),
        ) => unavailable_protocol_error(InternalDiagnostic::SessionDelegationDatabase),
        PostgresSessionDelegationPortError::Repository(
            signalbox_persistence::session_delegation::SessionDelegationRepositoryError::CommitAmbiguous(
                _,
            ),
        ) => ProtocolError::mutation_commit_ambiguous(),
        PostgresSessionDelegationPortError::Repository(
            signalbox_persistence::session_delegation::SessionDelegationRepositoryError::ToolLoop(
                error,
            ),
        ) => {
            return write_tool_loop_error(writer, version, request_id, session_id, error).await;
        }
        PostgresSessionDelegationPortError::Repository(
            signalbox_persistence::session_delegation::SessionDelegationRepositoryError::Corruption(
                _,
            ),
        ) => internal_protocol_error(
            Some(session_id.into_uuid()),
            InternalDiagnostic::SessionDelegationCorruption,
        ),
        PostgresSessionDelegationPortError::Repository(
            signalbox_persistence::session_delegation::SessionDelegationRepositoryError::InvalidTransition(
                _,
            ),
        )
        | PostgresSessionDelegationPortError::Contract => internal_protocol_error(
            Some(session_id.into_uuid()),
            InternalDiagnostic::SessionDelegationContract,
        ),
    };
    write_error(writer, version, request_id, protocol_error).await
}
