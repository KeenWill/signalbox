//! Program cancellation with durable identity and receipt correlation.

use super::*;
use arguments::ProgramCommand;

pub(crate) async fn execute(
    client: &mut ProcessClient,
    output: &mut Output<'_>,
    command: ProgramCommand,
) -> Result<(), ClientError> {
    let ProgramCommand::Cancel { run_id, command_id } = command;
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
