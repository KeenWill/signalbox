//! Credential administration, operator instructions, and durable receipts (docs/spec/process-protocol.md).

use super::*;
use crate::arguments::CredentialCommand;

pub(crate) async fn credential(
    client: &mut ProcessClient,
    output: &mut Output<'_>,
    command: CredentialCommand,
) -> Result<(), ClientError> {
    let (target, make_request): (_, fn(CommandId, String) -> ClientRequest) = match command {
        CredentialCommand::Exclusions { page_size, after } => {
            return credential_exclusions::list(client, output, page_size, after).await;
        }
        CredentialCommand::Clear { target, command_id } => {
            return credential_exclusions::clear(client, output, target, command_id).await;
        }
        CredentialCommand::Provision(target) => (target, |command_id, profile| {
            ClientRequest::ProvisionOauthCredential {
                command_id,
                profile,
            }
        }),
        CredentialCommand::Reprovision(target) => (target, |command_id, profile| {
            ClientRequest::ReprovisionOauthCredential {
                command_id,
                profile,
            }
        }),
        CredentialCommand::Delete(target) => (target, |command_id, profile| {
            ClientRequest::DeleteOauthCredential {
                command_id,
                profile,
            }
        }),
    };
    let profile = target.profile;
    let (command_id, generated) = command_identity(target.command_id)?;
    if generated {
        output.recovery_value("command_id", &command_id.into_uuid().to_string())?;
    }
    let request = make_request(command_id, profile.clone());
    let mut connection = client.mutation_request(request).await?;
    loop {
        let message = connection.message().await.map_err(ClientError::mutation)?;
        match &message {
            ServerMessage::OauthCredentialAuthorization {
                command_id: actual_id,
                profile: actual_profile,
                ..
            } if *actual_id == command_id && *actual_profile == profile => {
                output
                    .oauth_credential(&message)
                    .map_err(ClientError::from)
                    .map_err(ClientError::mutation)?;
                output
                    .flush()
                    .map_err(ClientError::from)
                    .map_err(ClientError::mutation)?;
            }
            ServerMessage::OauthCredentialReceipt {
                command_id: actual_id,
                profile: actual_profile,
                ..
            } if *actual_id == command_id && *actual_profile == profile => {
                output.oauth_credential(&message)?;
                return Ok(());
            }
            ServerMessage::Error {
                code,
                message,
                detail,
            } => {
                return Err(ClientError::remote(*code, message.clone(), *detail).mutation());
            }
            _ => {
                return Err(ClientError::Protocol("OAuth receipt correlation mismatch").mutation());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arguments::CredentialTarget;
    use signalbox_process_protocol::{
        ErrorCode, ErrorDetail, OauthCredentialOutcome, decode_client_line,
    };
    use tokio::{io::AsyncBufReadExt, net::UnixListener};

    #[tokio::test]
    async fn credential_commit_ambiguous_requires_mutation_recovery()
    -> Result<(), Box<dyn std::error::Error>> {
        let result = credential_with_reply(
            CommandId::try_from_uuid(Uuid::now_v7())?,
            ServerMessage::Error {
                code: ErrorCode::CommitAmbiguous,
                message: "credential command commit outcome is ambiguous".into(),
                detail: ErrorDetail::none(),
            },
        )
        .await?;
        assert!(matches!(result, Err(ClientError::AmbiguousMutation)));
        Ok(())
    }

    #[tokio::test]
    async fn credential_preserves_definitive_remote_errors()
    -> Result<(), Box<dyn std::error::Error>> {
        for code in [ErrorCode::ConflictingReuse, ErrorCode::Unavailable] {
            const DIAGNOSTIC: &str = "credential command rejected";
            let result = credential_with_reply(
                CommandId::try_from_uuid(Uuid::now_v7())?,
                ServerMessage::Error {
                    code,
                    message: DIAGNOSTIC.into(),
                    detail: ErrorDetail::none(),
                },
            )
            .await?;
            assert!(
                matches!(result, Err(ClientError::Remote {
                    code: actual_code, ref message, ref detail,
                }) if actual_code == code && message == DIAGNOSTIC && detail == &ErrorDetail::none()),
                "remote diagnostic must survive OAuth handling for {code:?}: {result:?}"
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn credential_unexpected_post_send_frames_require_mutation_recovery()
    -> Result<(), Box<dyn std::error::Error>> {
        let command_id = CommandId::try_from_uuid(Uuid::now_v7())?;
        for message in [
            ServerMessage::ReviewFindingsEnd {
                finding_count: CanonicalU64::new(0),
            },
            ServerMessage::OauthCredentialReceipt {
                command_id,
                profile: "different".into(),
                outcome: OauthCredentialOutcome::Provisioned {},
            },
        ] {
            let result = credential_with_reply(command_id, message.clone()).await?;
            assert!(
                matches!(result, Err(ClientError::AmbiguousMutation)),
                "unexpected reply must preserve mutation ambiguity: {message:?}: {result:?}"
            );
        }
        Ok(())
    }

    /// Exchanges a provision request for the supplied reply over a local socket.
    async fn credential_with_reply(
        command_id: CommandId,
        message: ServerMessage,
    ) -> Result<Result<(), ClientError>, Box<dyn std::error::Error>> {
        credential_with_reply_and_output(command_id, message, &mut Vec::new()).await
    }

    async fn credential_with_reply_and_output(
        command_id: CommandId,
        message: ServerMessage,
        stdout: &mut dyn std::io::Write,
    ) -> Result<Result<(), ClientError>, Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        let socket = directory.path().join("client.sock");
        let listener = UnixListener::bind(&socket)?;
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await?;
            let (reader, mut writer) = stream.into_split();
            let mut reader = tokio::io::BufReader::new(reader);
            let mut line = Vec::new();
            reader.read_until(b'\n', &mut line).await?;
            let request = decode_client_line(&line).map_err(std::io::Error::other)?;
            let frame = ServerFrame::try_new(request.request_id(), message)
                .map_err(std::io::Error::other)?;
            writer
                .write_all(&encode_server_line(&frame).map_err(std::io::Error::other)?)
                .await
        });
        let mut client = ProcessClient::new(socket);
        let mut stderr = Vec::new();
        let mut output = Output::new(stdout, &mut stderr, false);
        let result = credential(
            &mut client,
            &mut output,
            CredentialCommand::Provision(CredentialTarget {
                profile: "subscription".into(),
                command_id: Some(command_id),
            }),
        )
        .await;
        server.await??;
        Ok(result)
    }

    #[derive(Debug)]
    enum ProgressOutputFailure {
        Write,
        Flush,
    }

    impl std::io::Write for ProgressOutputFailure {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            match self {
                Self::Write => Err(std::io::ErrorKind::BrokenPipe.into()),
                Self::Flush => Ok(bytes.len()),
            }
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Err(std::io::ErrorKind::BrokenPipe.into())
        }
    }

    #[tokio::test]
    async fn credential_progress_output_failures_require_mutation_recovery()
    -> Result<(), Box<dyn std::error::Error>> {
        for mut output in [ProgressOutputFailure::Write, ProgressOutputFailure::Flush] {
            let command_id = CommandId::try_from_uuid(Uuid::now_v7())?;
            let result = credential_with_reply_and_output(
                command_id,
                ServerMessage::OauthCredentialAuthorization {
                    command_id,
                    profile: "subscription".into(),
                    user_code: "OPERATOR-CODE".into(),
                    verification_uri: "https://authorization.example/verify".into(),
                },
                &mut output,
            )
            .await?;
            assert!(
                matches!(result, Err(ClientError::AmbiguousMutation)),
                "post-send progress output failure must preserve mutation ambiguity: {output:?}: {result:?}"
            );
        }
        Ok(())
    }

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
