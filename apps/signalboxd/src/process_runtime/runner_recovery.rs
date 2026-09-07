//! Process-wire consumers of durable runner recovery commands.

use super::*;
use signalbox_domain::{
    AbandonLostRunner, AbandonLostRunnerResult, PromotePendingRunner, PromotePendingRunnerResult,
    ReplaceLostRunner, ReplaceLostRunnerResult, RunnerEnrollmentRequestId, WorkspaceRevision,
};
use signalbox_persistence::runner_protocol::{
    RunnerProtocolStore, RunnerProtocolStoreError, RunnerRecoveryError, RunnerRecoveryOutcome,
};

pub(super) async fn handle_runner_recovery<Writer: AsyncWrite + Unpin>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    request: ClientRequest,
    services: &ConnectionServices,
    mut shutdown: watch::Receiver<bool>,
) -> Result<(), ProcessConnectionError> {
    let catalog = match crate::runner_protocol_runtime::registration_only_catalog() {
        Ok(catalog) => catalog,
        Err(error) => {
            tracing::error!(failure = ?error, "runner recovery catalog admission failed");
            return write_error(
                writer,
                version,
                request_id,
                ProtocolError::without_detail(ErrorCode::Internal),
            )
            .await;
        }
    };
    let store = RunnerProtocolStore::new(services.pool.clone(), catalog);
    let result = match request {
        ClientRequest::AbandonLostRunner {
            command_id,
            session_id,
        } => store
            .abandon_lost_runner(AbandonLostRunner {
                command_id: DurableCommandId::from_uuid(command_id.into_uuid()),
                session: SessionId::from_uuid(session_id.into_uuid()),
            })
            .await
            .map(|outcome| match outcome {
                RunnerRecoveryOutcome::Recorded(result) => {
                    RunnerRecoveryOutcome::Recorded(ServerMessage::RunnerAbandonmentReceipt {
                        command_id,
                        session_id,
                        outcome: match result {
                            AbandonLostRunnerResult::Abandoned => {
                                signalbox_process_protocol::RunnerAbandonmentOutcome::Abandoned
                            }
                            AbandonLostRunnerResult::Rejected(reason) => {
                                signalbox_process_protocol::RunnerAbandonmentOutcome::Rejected {
                                    reason: rejection(reason),
                                }
                            }
                        },
                    })
                }
                RunnerRecoveryOutcome::ConflictingReuse => RunnerRecoveryOutcome::ConflictingReuse,
                RunnerRecoveryOutcome::Pending => RunnerRecoveryOutcome::Pending,
            }),
        ClientRequest::PromotePendingRunner {
            command_id,
            enrollment_request_id,
        } => store
            .promote_pending_runner(PromotePendingRunner {
                command_id: DurableCommandId::from_uuid(command_id.into_uuid()),
                enrollment_request: RunnerEnrollmentRequestId::from_uuid(
                    enrollment_request_id.into_uuid(),
                ),
            })
            .await
            .map(|outcome| match outcome {
                RunnerRecoveryOutcome::Recorded(result) => {
                    RunnerRecoveryOutcome::Recorded(ServerMessage::RunnerPromotionReceipt {
                        command_id,
                        enrollment_request_id,
                        outcome: match result {
                            PromotePendingRunnerResult::Promoted { runner } => {
                                signalbox_process_protocol::RunnerPromotionOutcome::Promoted {
                                    runner_id: CanonicalUuid::from_uuid(runner.into_uuid()),
                                }
                            }
                            PromotePendingRunnerResult::Rejected(reason) => {
                                signalbox_process_protocol::RunnerPromotionOutcome::Rejected {
                                    reason: rejection(reason),
                                }
                            }
                        },
                    })
                }
                RunnerRecoveryOutcome::ConflictingReuse => RunnerRecoveryOutcome::ConflictingReuse,
                RunnerRecoveryOutcome::Pending => RunnerRecoveryOutcome::Pending,
            }),
        ClientRequest::ReplaceLostRunner {
            command_id,
            session_id,
            revision,
        } => {
            let revision = match revision.map(WorkspaceRevision::try_new).transpose() {
                Ok(revision) => revision,
                Err(error) => {
                    tracing::debug!(session_id = %session_id.into_uuid(), failure = ?error, "runner replacement revision rejected");
                    return write_error(
                        writer,
                        version,
                        request_id,
                        ProtocolError::without_detail(ErrorCode::InvalidRequest),
                    )
                    .await;
                }
            };
            let command = ReplaceLostRunner {
                command_id: DurableCommandId::from_uuid(command_id.into_uuid()),
                session: SessionId::from_uuid(session_id.into_uuid()),
                revision,
            };
            let mut notifications = services.fanouts.runner_recovery.subscribe();
            loop {
                let outcome = match store.replace_lost_runner(command.clone()).await {
                    Ok(RunnerRecoveryOutcome::Pending) => {
                        store.resume_runner_replacement(command.command_id).await
                    }
                    outcome => outcome,
                };
                match outcome {
                    Ok(RunnerRecoveryOutcome::Recorded(result)) => break Ok(
                        RunnerRecoveryOutcome::Recorded(ServerMessage::RunnerReplacementReceipt {
                            command_id,
                            session_id,
                            outcome: match result {
                                ReplaceLostRunnerResult::Replaced {
                                    runner,
                                    placement_revision,
                                } => {
                                    signalbox_process_protocol::RunnerReplacementOutcome::Replaced {
                                        runner_id: CanonicalUuid::from_uuid(runner.into_uuid()),
                                        placement_revision: placement_revision.into(),
                                    }
                                }
                                ReplaceLostRunnerResult::Rejected(reason) => {
                                    signalbox_process_protocol::RunnerReplacementOutcome::Rejected {
                                        reason: rejection(reason),
                                    }
                                }
                            },
                        }),
                    ),
                    Ok(RunnerRecoveryOutcome::ConflictingReuse) => {
                        break Ok(RunnerRecoveryOutcome::ConflictingReuse);
                    }
                    Err(error) => break Err(error),
                    Ok(RunnerRecoveryOutcome::Pending) => {
                        tokio::select! {
                            notification = notifications.changed() => {
                                if notification.is_err() { return Ok(()); }
                            }
                            _ = shutdown.changed() => return Ok(()),
                        }
                    }
                }
            }
        }
        _ => return Err(ProcessConnectionError::EncodeInvariant),
    };
    match result {
        Ok(RunnerRecoveryOutcome::Recorded(message)) => {
            write_message(writer, version, request_id, message).await
        }
        Ok(RunnerRecoveryOutcome::ConflictingReuse) => {
            write_error(
                writer,
                version,
                request_id,
                ProtocolError::without_detail(ErrorCode::ConflictingReuse),
            )
            .await
        }
        Ok(RunnerRecoveryOutcome::Pending) => Err(ProcessConnectionError::EncodeInvariant),
        Err(error) => write_recovery_error(writer, version, request_id, error).await,
    }
}

async fn write_recovery_error<Writer: AsyncWrite + Unpin>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request: RequestId,
    error: RunnerRecoveryError,
) -> Result<(), ProcessConnectionError> {
    let code = match error {
        RunnerRecoveryError::InvalidCommandId => ErrorCode::InvalidRequest,
        RunnerRecoveryError::Store(RunnerProtocolStoreError::CommitAmbiguous(_)) => {
            ErrorCode::CommitAmbiguous
        }
        RunnerRecoveryError::Store(RunnerProtocolStoreError::Database(_)) => ErrorCode::Unavailable,
        RunnerRecoveryError::Store(_) | RunnerRecoveryError::Registry(_) => ErrorCode::Internal,
    };
    tracing::error!(failure = ?code, "runner recovery command failed");
    write_error(
        writer,
        version,
        request,
        ProtocolError::without_detail(code),
    )
    .await
}

fn rejection(
    reason: signalbox_domain::RunnerRecoveryRejection,
) -> signalbox_process_protocol::RunnerRecoveryRejection {
    use signalbox_domain::RunnerRecoveryRejection as Domain;
    use signalbox_process_protocol::RunnerRecoveryRejection as Wire;
    match reason {
        Domain::SessionNotFound => Wire::SessionNotFound,
        Domain::PlacementNotLost => Wire::PlacementNotLost,
        Domain::ExistingControlRequired => Wire::ExistingControlRequired,
        Domain::TurnTerminalized => Wire::TurnTerminalized,
        Domain::PendingRunnerNotFound => Wire::PendingRunnerNotFound,
        Domain::RunnerUnavailable => Wire::RunnerUnavailable,
        Domain::ReplacementPending => Wire::ReplacementPending,
        Domain::PlacementUnavailable => Wire::PlacementUnavailable,
        Domain::RevisionWithoutRepository => Wire::RevisionWithoutRepository,
        Domain::ProvisioningFailed => Wire::ProvisioningFailed,
    }
}
