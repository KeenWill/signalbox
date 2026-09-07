//! OAuth administration admission and durable rejection replay.

use super::*;
use signalbox_process_protocol::{OauthCredentialFailure, OauthCredentialOutcome};

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn oauth_admin_rejects_profiles_and_replays_after_registration_changes()
-> Result<(), Box<dyn Error>> {
    let mut runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let mut retained = Vec::new();
    for (profile, expected_reason) in [
        ("undeclared", OauthCredentialFailure::UnknownProfile),
        ("anthropic-primary", OauthCredentialFailure::NonOauthProfile),
    ] {
        let command_id = CommandId::try_from_uuid(Uuid::now_v7())?;
        let requests = [
            ClientRequest::ProvisionOauthCredential {
                command_id,
                profile: profile.into(),
            },
            ClientRequest::ReprovisionOauthCredential {
                command_id: CommandId::try_from_uuid(Uuid::now_v7())?,
                profile: profile.into(),
            },
            ClientRequest::DeleteOauthCredential {
                command_id: CommandId::try_from_uuid(Uuid::now_v7())?,
                profile: profile.into(),
            },
        ];
        for request in requests {
            connection.request(1, request.clone()).await?;
            let receipt = response_within(&mut connection).await?;
            let request_id = match &request {
                ClientRequest::ProvisionOauthCredential { command_id, .. }
                | ClientRequest::ReprovisionOauthCredential { command_id, .. }
                | ClientRequest::DeleteOauthCredential { command_id, .. } => *command_id,
                _ => panic!("fixture constructs only OAuth requests"),
            };
            assert_eq!(
                receipt.message(),
                &ServerMessage::OauthCredentialReceipt {
                    command_id: request_id,
                    profile: profile.into(),
                    outcome: OauthCredentialOutcome::Failed {
                        reason: expected_reason
                    },
                }
            );
            retained.push((request, receipt.message().clone()));
        }
        connection
            .request(
                1,
                ClientRequest::ProvisionOauthCredential {
                    command_id,
                    profile: "changed".into(),
                },
            )
            .await?;
        assert!(matches!(
            response_within(&mut connection).await?.message(),
            ServerMessage::Error {
                code: ErrorCode::ConflictingReuse,
                ..
            }
        ));
    }
    drop(connection);
    runtime
        .restart_with_model_configuration(
            &MODEL_CONFIGURATION.replace("anthropic-primary", "renamed-profile"),
        )
        .await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    for (request, receipt) in retained {
        connection.request(1, request).await?;
        assert_eq!(response_within(&mut connection).await?.message(), &receipt);
    }
    drop(connection);
    runtime.stop().await
}
