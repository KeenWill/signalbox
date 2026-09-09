//! Program admission and reads with durable identity and receipt correlation.

use super::*;
use arguments::ProgramCommand;
use std::io;

pub(crate) async fn execute(
    client: &mut ProcessClient,
    output: &mut Output<'_>,
    command: ProgramCommand,
) -> Result<(), ClientError> {
    let (run_id, command_id) = match command {
        ProgramCommand::Register {
            registration_id,
            registration,
        } => {
            let bytes = std::fs::read(&registration).map_err(|error| {
                io::Error::new(error.kind(), format!("{}: {error}", registration.display()))
            })?;
            let registration = serde_json::from_slice(&bytes).map_err(|error| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("{}: {error}", registration.display()),
                )
            })?;
            return admit(
                client,
                output,
                ClientRequest::RegisterProgram {
                    registration_id,
                    registration,
                },
            )
            .await;
        }
        ProgramCommand::Start {
            run_id,
            registration_id,
            input,
        } => {
            let bytes = std::fs::read(&input).map_err(|error| {
                io::Error::new(error.kind(), format!("{}: {error}", input.display()))
            })?;
            return admit(
                client,
                output,
                ClientRequest::StartProgramRun {
                    run_id,
                    registration_id,
                    input: bytes,
                },
            )
            .await;
        }
        ProgramCommand::Read { run_id } => {
            let mut connection = client
                .request(ClientRequest::ReadProgramRun { run_id })
                .await?;
            return match connection.message().await? {
                ServerMessage::ProgramRunRead {
                    run_id: observed,
                    run,
                } if observed == run_id => {
                    output.program_run(run_id, run)?;
                    Ok(())
                }
                ServerMessage::Error {
                    code,
                    message,
                    detail,
                } => Err(ClientError::remote(code, message, detail)),
                _ => Err(ClientError::Protocol("program read returned another run")),
            };
        }
        ProgramCommand::Cancel { run_id, command_id } => (run_id, command_id),
    };
    let (command_id, generated) = command_identity(command_id)?;
    if generated {
        output.recovery_value("command_id", &command_id.into_uuid().to_string())?;
    }
    let mut connection = client
        .mutation_request(ClientRequest::CancelProgramRun { command_id, run_id })
        .await?;
    match connection.message().await.map_err(ClientError::mutation)? {
        ServerMessage::ProgramRunCancellationReceipt {
            command_id: recorded_command,
            run_id: recorded_run,
            outcome,
        } if recorded_command == command_id && recorded_run == run_id => {
            output.program_cancellation(run_id, outcome)?;
            Ok(())
        }
        ServerMessage::Error {
            code,
            message,
            detail,
        } => Err(ClientError::remote(code, message, detail).mutation()),
        _ => Err(ClientError::Protocol("program cancellation returned another receipt").mutation()),
    }
}

async fn admit(
    client: &mut ProcessClient,
    output: &mut Output<'_>,
    request: ClientRequest,
) -> Result<(), ClientError> {
    let mut connection = client.mutation_request(request.clone()).await?;
    let message = connection.message().await.map_err(ClientError::mutation)?;
    match (&request, &message) {
        (
            ClientRequest::RegisterProgram {
                registration_id, ..
            },
            ServerMessage::ProgramRegistered {
                registration_id: observed,
            },
        ) if registration_id == observed => {
            output.recovery_value("registration_id", &registration_id.to_string())?;
            return Ok(());
        }
        (
            ClientRequest::StartProgramRun {
                run_id,
                registration_id,
                ..
            },
            ServerMessage::ProgramRunStarted {
                run_id: observed_run,
                registration_id: observed_registration,
            },
        ) if run_id == observed_run && registration_id == observed_registration => {
            output.recovery_value("run_id", &run_id.to_string())?;
            return Ok(());
        }
        _ => {}
    }
    match message {
        ServerMessage::Error {
            code,
            message,
            detail,
        } => Err(ClientError::remote(code, message, detail).mutation()),
        _ => Err(ClientError::Protocol("program admission returned another receipt").mutation()),
    }
}
