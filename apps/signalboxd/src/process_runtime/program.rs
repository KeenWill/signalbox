//! User-authorized cancellation of retained program journals.

use super::*;
use signalbox_domain::ProgramRunId;
use signalbox_persistence::program_cancellation::{
    self as store, ProgramCancellationOutcome as Outcome, ProgramCancellationResult as Result,
    ProgramTerminalState as State,
};
use signalbox_process_protocol::{
    ProgramRunCancellationOutcome as WireOutcome, ProgramRunCancelledState, ProgramRunTerminalState,
};

use crate::workflows::WorkflowRuntimeError;
use signalbox_domain::{
    ProgramCapability, ProgramRegistrationId,
    program_registration::{
        NativeProgramRegistrationRequest, ProgramExecutable, ProgramGrants,
        ProgramRegistrationRequest,
    },
};
use signalbox_persistence::program_registration::{
    ProgramRegistrationError, ProgramRegistrationRepository,
};
use signalbox_process_protocol::{
    ProgramExecutableInput, ProgramGrant, ProgramRegistrationInput, ProgramRun, ProgramRunState,
};

pub(super) async fn handle_register_program<Writer: AsyncWrite + Unpin>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    registration_id: CanonicalUuid,
    registration: ProgramRegistrationInput,
    services: &ConnectionServices,
) -> std::result::Result<(), ProcessConnectionError> {
    let result = async {
        let service = services
            .workflows
            .as_ref()
            .ok_or(WorkflowRuntimeError::Stopped)?;
        let id = ProgramRegistrationId::from_uuid(registration_id.into_uuid());
        let grants = ProgramGrants::new(registration.grants.into_iter().map(domain_grant));
        match registration.executable {
            ProgramExecutableInput::JavaScript { source, artifact } => {
                service
                    .register_javascript(
                        id,
                        ProgramRegistrationRequest {
                            name: registration.name,
                            revision: registration.revision,
                            source,
                            artifact,
                            grants,
                        },
                    )
                    .await?;
            }
            ProgramExecutableInput::Native { entry, revision } => {
                let Some(ProgramExecutable::Native { binary_digest, .. }) =
                    service.clock_executable()
                else {
                    return Err(WorkflowRuntimeError::NativeUnavailable);
                };
                service
                    .register_native(
                        id,
                        NativeProgramRegistrationRequest {
                            name: registration.name,
                            revision: registration.revision,
                            entry,
                            native_revision: revision,
                            binary_digest: *binary_digest,
                            grants,
                        },
                    )
                    .await?;
            }
        }
        Ok(())
    }
    .await;
    if let Err(error) = result {
        return write_error(writer, version, request_id, workflow_error(error)).await;
    }
    write_message(
        writer,
        version,
        request_id,
        ServerMessage::ProgramRegistered { registration_id },
    )
    .await
}

pub(super) async fn handle_start_program<Writer: AsyncWrite + Unpin>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    run_id: CanonicalUuid,
    registration_id: CanonicalUuid,
    input: Vec<u8>,
    services: &ConnectionServices,
) -> std::result::Result<(), ProcessConnectionError> {
    let result = async {
        services
            .workflows
            .as_ref()
            .ok_or(WorkflowRuntimeError::Stopped)?
            .start(
                ProgramRunId::from_uuid(run_id.into_uuid()),
                ProgramRegistrationId::from_uuid(registration_id.into_uuid()),
                &input,
            )
            .await
    }
    .await;
    if let Err(error) = result {
        return write_error(writer, version, request_id, workflow_error(error)).await;
    }
    write_message(
        writer,
        version,
        request_id,
        ServerMessage::ProgramRunStarted {
            run_id,
            registration_id,
        },
    )
    .await
}

pub(super) async fn handle_read_program<Writer: AsyncWrite + Unpin>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    run_id: CanonicalUuid,
    services: &ConnectionServices,
) -> std::result::Result<(), ProcessConnectionError> {
    let result = read_program(&services.pool, run_id).await;
    match result {
        Ok(run) => {
            write_message(
                writer,
                version,
                request_id,
                ServerMessage::ProgramRunRead { run_id, run },
            )
            .await
        }
        Err(error) => write_error(writer, version, request_id, error).await,
    }
}

async fn read_program(
    pool: &PgPool,
    run_id: CanonicalUuid,
) -> std::result::Result<ProgramRun, ProtocolError> {
    let run = ProgramRunId::from_uuid(run_id.into_uuid());
    let registrations = ProgramRegistrationRepository::new(pool.clone());
    let registration = registrations
        .for_run(run)
        .await
        .map_err(|error| workflow_error(error.into()))?
        .ok_or_else(|| ProtocolError::without_detail(ErrorCode::NotFound))?;
    let input = registrations
        .input_for_run(run)
        .await
        .map_err(|error| workflow_error(error.into()))?
        .ok_or_else(|| {
            workflow_error(ProgramRegistrationError::Corruption("missing input").into())
        })?;
    let journal =
        signalbox_persistence::program_journal::ProgramJournalRepository::new(pool.clone())
            .load(run)
            .await
            .map_err(|error| {
                use signalbox_persistence::program_journal::ProgramJournalRepositoryError;
                let (code, cause) = match error {
                    ProgramJournalRepositoryError::Database { .. } => {
                        (ErrorCode::Unavailable, "workflow_journal_database")
                    }
                    _ => (ErrorCode::Internal, "workflow_journal_corruption"),
                };
                tracing::error!(cause, "program read failed");
                ProtocolError::without_detail(code)
            })?
            .ok_or_else(|| {
                workflow_error(ProgramRegistrationError::Corruption("missing journal").into())
            })?;
    let outcome = if let Some(result) = journal.result() {
        ProgramRunState::Succeeded {
            result: result.as_bytes().to_vec(),
        }
    } else {
        match journal
            .terminal_delivery()
            .map(signalbox_domain::DeliveryFrame::kind)
        {
            Some(signalbox_domain::DeliveryKind::RunCancel(_)) => ProgramRunState::Cancelled {},
            Some(_) => ProgramRunState::Faulted {},
            None => ProgramRunState::Running {},
        }
    };
    Ok(ProgramRun {
        registration_id: CanonicalUuid::from_uuid(registration.id.into_uuid()),
        input: input.as_bytes().to_vec(),
        outcome,
    })
}

fn workflow_error(error: WorkflowRuntimeError) -> ProtocolError {
    tracing::warn!(cause = error.cause_code(), "program command failed");
    let code = match error {
        WorkflowRuntimeError::Registration(
            ProgramRegistrationError::RegistrationConflict { .. }
            | ProgramRegistrationError::RunConflict { .. },
        ) => ErrorCode::ConflictingReuse,
        WorkflowRuntimeError::Registration(ProgramRegistrationError::Database {
            commit_ambiguous: true,
            ..
        }) => return ProtocolError::mutation_commit_ambiguous(),
        WorkflowRuntimeError::Registration(ProgramRegistrationError::Database {
            source, ..
        }) => {
            if source
                .as_database_error()
                .is_some_and(|error| error.is_foreign_key_violation())
            {
                ErrorCode::NotFound
            } else {
                ErrorCode::Unavailable
            }
        }
        WorkflowRuntimeError::Registration(ProgramRegistrationError::GrantsDenied) => {
            ErrorCode::Rejected
        }
        WorkflowRuntimeError::Registration(ProgramRegistrationError::RunMissing) => {
            ErrorCode::NotFound
        }
        WorkflowRuntimeError::NativeUnavailable => ErrorCode::InvalidRequest,
        WorkflowRuntimeError::Stopped => ErrorCode::Unavailable,
        _ => ErrorCode::Internal,
    };
    ProtocolError::without_detail(code)
}

fn domain_grant(grant: ProgramGrant) -> ProgramCapability {
    match grant {
        ProgramGrant::Time => ProgramCapability::Time,
        ProgramGrant::Random => ProgramCapability::Random,
        ProgramGrant::Sleep => ProgramCapability::Sleep,
        ProgramGrant::Subscribe => ProgramCapability::Subscribe,
        ProgramGrant::Session => ProgramCapability::Session,
        ProgramGrant::Judge => ProgramCapability::Judge,
        ProgramGrant::ExecStage => ProgramCapability::ExecStage,
        ProgramGrant::Corpus => ProgramCapability::Corpus,
        ProgramGrant::EvalRecord => ProgramCapability::EvalRecord,
        ProgramGrant::Blob => ProgramCapability::Blob,
        ProgramGrant::Register => ProgramCapability::Register,
    }
}

pub(super) async fn handle_cancel_program_run<Writer: AsyncWrite + Unpin>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    command_id: signalbox_process_protocol::CommandId,
    run_id: CanonicalUuid,
    services: &ConnectionServices,
) -> std::result::Result<(), ProcessConnectionError> {
    let command = store::CancelProgramRun {
        command_id: DurableCommandId::from_uuid(command_id.into_uuid()),
        run_id: ProgramRunId::from_uuid(run_id.into_uuid()),
    };
    let outcome = match store::cancel(&services.pool, command).await {
        Ok(Result::Recorded(Outcome::Applied)) => WireOutcome::Applied {
            terminal_state: ProgramRunCancelledState::Cancelled,
            result: (),
        },
        Ok(Result::Recorded(Outcome::NotFound)) => WireOutcome::NotFound {},
        Ok(Result::Recorded(Outcome::AlreadyTerminal(state))) => {
            WireOutcome::AlreadyTerminal(match state {
                State::Cancelled => ProgramRunTerminalState::Cancelled { result: () },
                State::Faulted => ProgramRunTerminalState::Faulted { result: () },
                State::Succeeded(result) => ProgramRunTerminalState::Succeeded {
                    result: result.as_bytes().to_vec(),
                },
            })
        }
        Ok(Result::ConflictingReuse) => {
            return write_error(
                writer,
                version,
                request_id,
                ProtocolError::without_detail(ErrorCode::ConflictingReuse),
            )
            .await;
        }
        Err(error) => {
            let protocol = match error {
                store::ProgramCancellationError::CommitAmbiguous(_) => ProtocolError::mutation_commit_ambiguous(),
                store::ProgramCancellationError::Database(_) => unavailable_protocol_error(InternalDiagnostic::ProgramCancellationDatabase),
                store::ProgramCancellationError::Journal(signalbox_persistence::program_journal::ProgramJournalRepositoryError::Database { .. }) => unavailable_protocol_error(InternalDiagnostic::ProgramCancellationDatabase),
                store::ProgramCancellationError::Journal(_) | store::ProgramCancellationError::Corruption => internal_protocol_error(None, InternalDiagnostic::ProgramCancellationCorruption),
            };
            return write_error(writer, version, request_id, protocol).await;
        }
    };
    write_message(
        writer,
        version,
        request_id,
        ServerMessage::ProgramRunCancellationReceipt {
            command_id,
            run_id,
            outcome,
        },
    )
    .await
}
