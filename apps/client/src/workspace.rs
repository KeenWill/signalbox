//! Operator consumers for workspace registration and Git remote facts.

use super::*;
use arguments::WorkspaceCommand;

pub(crate) async fn run(
    client: &mut ProcessClient,
    output: &mut Output<'_>,
    command: WorkspaceCommand,
) -> Result<(), ClientError> {
    let supplied = match &command {
        WorkspaceCommand::Register { command_id, .. }
        | WorkspaceCommand::MintRemote { command_id, .. }
        | WorkspaceCommand::WithdrawRemote { command_id, .. } => *command_id,
    };
    let (command_id, generated) = command_identity(supplied)?;
    if generated {
        output.recovery_value("command_id", &command_id.into_uuid().to_string())?;
    }
    let request = match command {
        WorkspaceCommand::Register { root, .. } => {
            ClientRequest::RegisterWorkspace { command_id, root }
        }
        WorkspaceCommand::MintRemote {
            workspace,
            name,
            url,
            ..
        } => ClientRequest::MintGitRemote {
            command_id,
            workspace_id: workspace,
            name,
            url,
        },
        WorkspaceCommand::WithdrawRemote { mint, .. } => ClientRequest::WithdrawGitRemote {
            command_id,
            mint_id: mint,
        },
    };
    let mut connection = client.mutation_request(request.clone()).await?;
    let message = connection.message().await.map_err(ClientError::mutation)?;
    let receipt = match (&request, &message) {
        (
            ClientRequest::RegisterWorkspace { .. },
            ServerMessage::WorkspaceRegistered {
                command_id: observed,
                workspace_id,
            },
        ) if *observed == command_id => Some(("workspace_id", *workspace_id)),
        (
            ClientRequest::MintGitRemote { .. },
            ServerMessage::GitRemoteMinted {
                command_id: observed,
                mint_id,
            },
        ) if *observed == command_id => Some(("mint_id", *mint_id)),
        (
            ClientRequest::WithdrawGitRemote { .. },
            ServerMessage::GitRemoteWithdrawn {
                command_id: observed,
                withdrawal_id,
            },
        ) if *observed == command_id => Some(("withdrawal_id", *withdrawal_id)),
        _ => None,
    };
    if let Some((label, identity)) = receipt {
        output.recovery_value(label, &identity.into_uuid().to_string())?;
        return Ok(());
    }
    match message {
        ServerMessage::Error {
            code,
            message,
            detail,
        } => Err(ClientError::remote(code, message, detail).mutation()),
        _ => Err(
            ClientError::Protocol("workspace command returned an uncorrelated receipt").mutation(),
        ),
    }
}
