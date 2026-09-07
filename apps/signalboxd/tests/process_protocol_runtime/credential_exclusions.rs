use super::*;
use signalbox_process_protocol::{CredentialExclusionClearOutcome, CredentialExclusionTarget};

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn credential_exclusions_list_clear_and_replay_over_the_socket() -> Result<(), Box<dyn Error>>
{
    let runtime = RunningRuntime::start().await?;
    let generation: i64 = sqlx::query_scalar("INSERT INTO credential_exclusion(kind,profile,origin) VALUES ('profile_quarantine','home','codex_home') RETURNING record_generation").fetch_one(&runtime.pool).await?;
    sqlx::query("INSERT INTO credential_exclusion(kind,profile,origin) VALUES ('profile_quarantine','oauth','oauth_refresh')").execute(&runtime.pool).await?;
    let target = CredentialExclusionTarget::ProfileQuarantine {
        profile: "home".into(),
        record_generation: CanonicalU64::new(generation as u64),
    };
    let mut connection = Connection::connect(runtime.socket()).await?;
    connection
        .request_version(
            ProtocolVersion::One,
            1,
            ClientRequest::ListCredentialExclusions {
                page_size: 1,
                after: None,
            },
        )
        .await?;
    assert_eq!(
        response_within(&mut connection).await?.message(),
        &ServerMessage::CredentialExclusionStart {}
    );
    assert_eq!(
        response_within(&mut connection).await?.message(),
        &ServerMessage::CredentialExclusion {
            target: target.clone()
        }
    );
    assert_eq!(
        response_within(&mut connection).await?.message(),
        &ServerMessage::CredentialExclusionEnd {
            exclusion_count: CanonicalU64::new(1),
            next_after: None
        }
    );
    let request = ClientRequest::ClearCredentialExclusion {
        command_id: CommandId::try_from_uuid(Uuid::now_v7())?,
        target: target.clone(),
    };
    connection
        .request_version(ProtocolVersion::One, 2, request.clone())
        .await?;
    let receipt = response_within(&mut connection).await?;
    assert_eq!(
        receipt.message(),
        &ServerMessage::CredentialExclusionCleared {
            target,
            outcome: CredentialExclusionClearOutcome::Cleared
        }
    );
    connection
        .request_version(ProtocolVersion::One, 3, request)
        .await?;
    assert_eq!(
        response_within(&mut connection).await?.message(),
        receipt.message()
    );
    connection
        .request_version(
            ProtocolVersion::One,
            4,
            ClientRequest::ListCredentialExclusions {
                page_size: 100,
                after: None,
            },
        )
        .await?;
    assert_eq!(
        response_within(&mut connection).await?.message(),
        &ServerMessage::CredentialExclusionStart {}
    );
    assert_eq!(
        response_within(&mut connection).await?.message(),
        &ServerMessage::CredentialExclusionEnd {
            exclusion_count: CanonicalU64::new(0),
            next_after: None
        }
    );
    drop(connection);
    runtime.stop().await
}
