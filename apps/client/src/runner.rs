//! Terminal consumers for the three durable runner recovery commands.

use super::*;
use arguments::RunnerCommand;

pub(crate) async fn run(
    client: &mut ProcessClient,
    output: &mut Output<'_>,
    command: RunnerCommand,
) -> Result<(), ClientError> {
    let supplied = match &command {
        RunnerCommand::Replace { command_id, .. }
        | RunnerCommand::Abandon { command_id, .. }
        | RunnerCommand::Promote { command_id, .. } => *command_id,
    };
    let (command_id, generated) = command_identity(supplied)?;
    if generated {
        output.recovery_value("command_id", &command_id.into_uuid().to_string())?;
    }
    let request = match command {
        RunnerCommand::Replace {
            session, revision, ..
        } => ClientRequest::ReplaceLostRunner {
            command_id,
            session_id: session,
            revision,
        },
        RunnerCommand::Abandon { session, .. } => ClientRequest::AbandonLostRunner {
            command_id,
            session_id: session,
        },
        RunnerCommand::Promote {
            enrollment_request, ..
        } => ClientRequest::PromotePendingRunner {
            command_id,
            enrollment_request_id: enrollment_request,
        },
    };
    let mut connection = client.mutation_request(request.clone()).await?;
    let message = connection.message().await.map_err(ClientError::mutation)?;
    let matches_request = match (&request, &message) {
        (
            ClientRequest::ReplaceLostRunner { session_id, .. },
            ServerMessage::RunnerReplacementReceipt {
                command_id: observed_command,
                session_id: observed_session,
                ..
            },
        )
        | (
            ClientRequest::AbandonLostRunner { session_id, .. },
            ServerMessage::RunnerAbandonmentReceipt {
                command_id: observed_command,
                session_id: observed_session,
                ..
            },
        ) => *observed_command == command_id && observed_session == session_id,
        (
            ClientRequest::PromotePendingRunner {
                enrollment_request_id,
                ..
            },
            ServerMessage::RunnerPromotionReceipt {
                command_id: observed_command,
                enrollment_request_id: observed_request,
                ..
            },
        ) => *observed_command == command_id && observed_request == enrollment_request_id,
        _ => false,
    };
    if matches_request {
        output.runner_recovery_receipt(&message)?;
        let rejected = matches!(
            message,
            ServerMessage::RunnerReplacementReceipt {
                outcome: signalbox_process_protocol::RunnerReplacementOutcome::Rejected { .. },
                ..
            } | ServerMessage::RunnerAbandonmentReceipt {
                outcome: signalbox_process_protocol::RunnerAbandonmentOutcome::Rejected { .. },
                ..
            } | ServerMessage::RunnerPromotionReceipt {
                outcome: signalbox_process_protocol::RunnerPromotionOutcome::Rejected { .. },
                ..
            }
        );
        if rejected {
            return Err(ClientError::Input(
                "runner recovery was rejected; see the recorded receipt",
            ));
        }
        return Ok(());
    }
    match message {
        ServerMessage::Error {
            code,
            message,
            detail,
        } => Err(ClientError::remote(code, message, detail).mutation()),
        _ => Err(
            ClientError::Protocol("runner recovery returned an uncorrelated receipt").mutation(),
        ),
    }
}
