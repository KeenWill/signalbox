//! OAuth claim replay, cross-kind exclusion, and immutable record enforcement.

use super::*;
use signalbox_persistence::oauth_credential::*;

fn command(operation: OauthCredentialOperation) -> OauthCredentialCommand {
    OauthCredentialCommand {
        command_id: DurableCommandId::from_uuid(Uuid::now_v7()),
        operation,
        profile: "subscription".into(),
    }
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn oauth_command_registry_replays_receipts_and_rejects_changed_meaning()
-> Result<(), Box<dyn Error>> {
    let (_container, pool, _) = migrated_postgres().await?;
    let repository = OauthCredentialRepository::new(pool.clone());
    for (operation, different_operation) in [
        (
            OauthCredentialOperation::Provision,
            OauthCredentialOperation::Delete,
        ),
        (
            OauthCredentialOperation::Reprovision,
            OauthCredentialOperation::Provision,
        ),
        (
            OauthCredentialOperation::Delete,
            OauthCredentialOperation::Provision,
        ),
    ] {
        let command = command(operation);
        let outcome = OauthCredentialOutcome::Failed(OauthCredentialFailure::UnknownProfile);
        assert_eq!(
            repository.record(&command, || outcome.clone()).await?,
            OauthCredentialHandlingOutcome::Recorded(outcome.clone())
        );
        assert_eq!(
            repository
                .record(&command, || panic!(
                    "equal replay must not evaluate current registration"
                ))
                .await?,
            OauthCredentialHandlingOutcome::Recorded(outcome)
        );
        let changed = OauthCredentialCommand {
            profile: "different".into(),
            ..command.clone()
        };
        assert_eq!(
            repository
                .record(&changed, || OauthCredentialOutcome::AlreadyDeleted)
                .await?,
            OauthCredentialHandlingOutcome::ConflictingReuse
        );
        let changed = OauthCredentialCommand {
            operation: different_operation,
            ..command
        };
        assert_eq!(
            repository
                .record(&changed, || OauthCredentialOutcome::AlreadyDeleted)
                .await?,
            OauthCredentialHandlingOutcome::ConflictingReuse
        );
    }
    pool.close().await;
    Ok(())
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn oauth_command_registry_requires_typed_records_and_preserves_receipts()
-> Result<(), Box<dyn Error>> {
    let (_container, pool, _) = migrated_postgres().await?;
    for (kind, operation) in [
        (
            "provision_oauth_credential",
            OauthCredentialOperation::Provision,
        ),
        (
            "reprovision_oauth_credential",
            OauthCredentialOperation::Reprovision,
        ),
        ("delete_oauth_credential", OauthCredentialOperation::Delete),
    ] {
        let mut tx = pool.begin().await?;
        sqlx::query("INSERT INTO durable_command (command_id, command_kind, storage_version, claimed_at, issuer_kind, issuer_module) VALUES ($1, $2, 1, transaction_timestamp(), 'operator', NULL)")
            .bind(Uuid::now_v7()).bind(kind).execute(&mut *tx).await?;
        let error = tx
            .commit()
            .await
            .expect_err("a registry claim requires its typed request at commit");
        assert_eq!(
            error.as_database_error().and_then(|e| e.code()).as_deref(),
            Some("23503")
        );

        let command = command(operation);
        OauthCredentialRepository::new(pool.clone())
            .record(&command, || {
                OauthCredentialOutcome::Failed(OauthCredentialFailure::UnknownProfile)
            })
            .await?;
        for suffix in ["command", "result"] {
            for mutation in [
                format!("DELETE FROM {kind}_{suffix} WHERE command_id = $1"),
                format!("UPDATE {kind}_{suffix} SET command_id = command_id WHERE command_id = $1"),
            ] {
                assert!(
                    sqlx::query(sqlx::AssertSqlSafe(mutation.as_str()))
                        .bind(command.command_id.into_uuid())
                        .execute(&pool)
                        .await
                        .is_err(),
                    "{kind}_{suffix} must be append-only"
                );
            }
        }
    }
    pool.close().await;
    Ok(())
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn oauth_concurrent_equal_claims_return_the_single_winners_receipt()
-> Result<(), Box<dyn Error>> {
    let (_container, pool, _) = migrated_postgres().await?;
    let repository = OauthCredentialRepository::new(pool.clone());
    let command = command(OauthCredentialOperation::Provision);
    let first = OauthCredentialOutcome::Failed(OauthCredentialFailure::UnknownProfile);
    let second = OauthCredentialOutcome::Failed(OauthCredentialFailure::NonOauthProfile);
    let (left, right) = tokio::join!(
        repository.record(&command, || first),
        repository.record(&command, || second)
    );
    assert_eq!(left?, right?);
    let receipts: i64 =
        sqlx::query_scalar("SELECT count(*) FROM provision_oauth_credential_result")
            .fetch_one(&pool)
            .await?;
    assert_eq!(receipts, 1);
    pool.close().await;
    Ok(())
}
