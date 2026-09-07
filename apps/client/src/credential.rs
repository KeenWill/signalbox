//! OAuth operator instructions and durable receipts (docs/spec/process-protocol.md).

use super::*;
use crate::arguments::CredentialCommand;

pub(crate) async fn credential(
    client: &mut ProcessClient,
    output: &mut Output<'_>,
    command: CredentialCommand,
) -> Result<(), ClientError> {
    let target = match &command {
        CredentialCommand::Provision(target)
        | CredentialCommand::Reprovision(target)
        | CredentialCommand::Delete(target) => target,
    };
    let profile = target.profile.clone();
    let (command_id, generated) = command_identity(target.command_id)?;
    if generated {
        output.recovery_value("command_id", &command_id.into_uuid().to_string())?;
    }
    let request = match command {
        CredentialCommand::Provision(_) => ClientRequest::ProvisionOauthCredential {
            command_id,
            profile: profile.clone(),
        },
        CredentialCommand::Reprovision(_) => ClientRequest::ReprovisionOauthCredential {
            command_id,
            profile: profile.clone(),
        },
        CredentialCommand::Delete(_) => ClientRequest::DeleteOauthCredential {
            command_id,
            profile: profile.clone(),
        },
    };
    let mut connection = client.mutation_request(request).await?;
    loop {
        let message = connection.message().await.map_err(ClientError::mutation)?;
        match &message {
            ServerMessage::OauthCredentialAuthorization {
                command_id: actual_id,
                profile: actual_profile,
                ..
            } if *actual_id == command_id && *actual_profile == profile => {
                output.oauth_credential(&message)?;
                output.flush()?;
            }
            ServerMessage::OauthCredentialReceipt {
                command_id: actual_id,
                profile: actual_profile,
                ..
            } if *actual_id == command_id && *actual_profile == profile => {
                output.oauth_credential(&message)?;
                return Ok(());
            }
            _ => return Err(ClientError::Protocol("OAuth receipt correlation mismatch")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arguments::CredentialTarget;
    use signalbox_process_protocol::{OauthCredentialOutcome, decode_client_line};
    use tokio::{io::AsyncBufReadExt, net::UnixListener};

    #[tokio::test]
    async fn credential_client_prints_progress_then_receipt_and_checks_correlation()
    -> Result<(), Box<dyn std::error::Error>> {
        for (returned_profile, expected_success) in [("subscription", true), ("different", false)] {
            let directory = tempfile::tempdir()?;
            let socket = directory.path().join("client.sock");
            let listener = UnixListener::bind(&socket)?;
            let command_id = CommandId::try_from_uuid(Uuid::now_v7())?;
            let server = tokio::spawn(async move {
                let (stream, _) = listener.accept().await?;
                let (reader, mut writer) = stream.into_split();
                let mut reader = tokio::io::BufReader::new(reader);
                let mut line = Vec::new();
                reader.read_until(b'\n', &mut line).await?;
                let request = decode_client_line(&line).map_err(std::io::Error::other)?;
                assert_eq!(
                    request.request(),
                    &ClientRequest::ProvisionOauthCredential {
                        command_id,
                        profile: "subscription".into()
                    }
                );
                for message in [
                    ServerMessage::OauthCredentialAuthorization {
                        command_id,
                        profile: "subscription".into(),
                        user_code: "OPERATOR-CODE".into(),
                        verification_uri: "https://authorization.example/verify".into(),
                    },
                    ServerMessage::OauthCredentialReceipt {
                        command_id,
                        profile: returned_profile.into(),
                        outcome: OauthCredentialOutcome::Provisioned {},
                    },
                ] {
                    let frame = ServerFrame::try_new(request.request_id(), message)
                        .map_err(std::io::Error::other)?;
                    writer
                        .write_all(&encode_server_line(&frame).map_err(std::io::Error::other)?)
                        .await?;
                }
                Ok::<_, std::io::Error>(())
            });
            let mut client = ProcessClient::new(socket);
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();
            let mut output = Output::new(&mut stdout, &mut stderr, false);
            let result = credential(
                &mut client,
                &mut output,
                CredentialCommand::Provision(CredentialTarget {
                    profile: "subscription".into(),
                    command_id: Some(command_id),
                }),
            )
            .await;
            assert_eq!(result.is_ok(), expected_success);
            let text = String::from_utf8(stdout)?;
            assert!(text.contains("user_code=OPERATOR-CODE"));
            assert!(text.contains("verification_uri=https://authorization.example/verify"));
            assert_eq!(text.contains("provisioned"), expected_success);
            server.await??;
        }
        Ok(())
    }
}
