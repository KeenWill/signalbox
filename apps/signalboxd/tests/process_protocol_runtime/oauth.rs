//! OAuth administration admission and durable rejection replay.

use super::*;
use signalbox_process_protocol::{OauthCredentialFailure, OauthCredentialOutcome};

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn oauth_delete_uses_retained_identity_without_a_current_declaration()
-> Result<(), Box<dyn Error>> {
    use signalbox_persistence::oauth_credential::{OauthCredentialRepository, OauthRegistration};
    let runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    connection
        .request(1, ClientRequest::ListSessions {})
        .await?;
    assert_eq!(
        response_within(&mut connection).await?.message(),
        &ServerMessage::SessionsStart {}
    );
    assert!(matches!(
        response_within(&mut connection).await?.message(),
        ServerMessage::SessionsEnd { .. }
    ));
    let repository = OauthCredentialRepository::new(runtime.pool.clone());
    repository
        .replace_registrations(&[(
            "retired-oauth".into(),
            OauthRegistration {
                client_id: "fixture-client".into(),
                token_url: "https://authorization.example/token".into(),
                refresh_token_url: "https://authorization.example/oauth/token".into(),
                device_authorization_url: "https://authorization.example/device".into(),
                scopes: vec!["openid".into()],
            },
        )])
        .await?;
    use signalbox_persistence::oauth_credential as stored;
    let initial = repository
        .delete(
            &stored::OauthCredentialCommand {
                command_id: signalbox_domain::DurableCommandId::from_uuid(Uuid::now_v7()),
                operation: stored::OauthCredentialOperation::Delete,
                profile: "retired-oauth".into(),
            },
            stored::OauthCredentialFailure::UnknownProfile,
            || {},
        )
        .await?;
    assert_eq!(
        initial,
        stored::OauthCredentialHandlingOutcome::Recorded(
            stored::OauthCredentialOutcome::AlreadyDeleted
        )
    );
    repository.replace_registrations(&[]).await?;
    let command_id = CommandId::try_from_uuid(Uuid::now_v7())?;
    let request = ClientRequest::DeleteOauthCredential {
        command_id,
        profile: "retired-oauth".into(),
    };
    for _ in 0..2 {
        connection.request(2, request.clone()).await?;
        assert_eq!(
            response_within(&mut connection).await?.message(),
            &ServerMessage::OauthCredentialReceipt {
                command_id,
                profile: "retired-oauth".into(),
                outcome: OauthCredentialOutcome::AlreadyDeleted {}
            }
        );
    }
    let generation: i64 = sqlx::query_scalar(
        "SELECT generation FROM oauth_credential_profile WHERE profile = 'retired-oauth'",
    )
    .fetch_one(&runtime.pool)
    .await?;
    assert_eq!(generation, 2);
    drop(connection);
    runtime.stop().await
}

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
