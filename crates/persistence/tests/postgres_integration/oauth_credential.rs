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

/// Arbitrary HTTPS registration; spelling equality is asserted against this fixture.
fn registration() -> OauthRegistration {
    OauthRegistration {
        client_id: "test-client".into(),
        token_url: "https://authorization.example/token".into(),
        device_authorization_url: "https://authorization.example/device".into(),
        scopes: vec!["openid".into(), "offline_access".into()],
    }
}

/// Synthetic authorization material, never a provider credential.
fn authorization() -> OauthAuthorization {
    OauthAuthorization {
        refresh_token: "synthetic-refresh".into(),
        identity_token: "synthetic-identity".into(),
        account_identity: serde_json::json!({"subject": "test-subject", "account_id": "test-account"}),
    }
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn oauth_provisioning_retains_progress_and_atomically_replays_authorization()
-> Result<(), Box<dyn Error>> {
    let (_container, pool, _) = migrated_postgres().await?;
    let repository = OauthCredentialRepository::new(pool.clone());
    let command = command(OauthCredentialOperation::Provision);
    let registration = registration();
    repository
        .replace_registrations(&[(command.profile.clone(), registration.clone())])
        .await?;
    let OauthStartOutcome::Started(exchange) = repository
        .begin_exchange(&command, Ok(&registration))
        .await?
    else {
        panic!("initial provision must own an exchange");
    };
    let progress = OauthProgress {
        user_code: "OPERATOR-CODE".into(),
        verification_uri: "https://authorization.example/verify".into(),
    };
    repository.retain_progress(&exchange, &progress).await?;
    assert_eq!(
        repository.progress(command.command_id).await?,
        Some(progress)
    );
    assert!(matches!(
        repository
            .begin_exchange(&command, Ok(&registration))
            .await?,
        OauthStartOutcome::Existing(OauthCredentialHandlingOutcome::Pending)
    ));
    let authorization = authorization();
    assert_eq!(
        repository
            .complete_exchange(&exchange, Ok(&authorization))
            .await?,
        OauthCredentialOutcome::Provisioned
    );
    let stored: (String, String, i64) = sqlx::query_as("SELECT refresh_token, identity_token, generation FROM oauth_credential_authorization WHERE profile = $1")
        .bind(&command.profile).fetch_one(&pool).await?;
    assert_eq!(
        stored,
        (authorization.refresh_token, authorization.identity_token, 1)
    );
    assert!(matches!(
        repository
            .begin_exchange(&command, Err(OauthCredentialFailure::UnknownProfile))
            .await?,
        OauthStartOutcome::Existing(OauthCredentialHandlingOutcome::Recorded(
            OauthCredentialOutcome::Provisioned
        ))
    ));
    let another = OauthCredentialCommand {
        command_id: DurableCommandId::from_uuid(Uuid::now_v7()),
        ..command
    };
    assert!(matches!(
        repository
            .begin_exchange(&another, Ok(&registration))
            .await?,
        OauthStartOutcome::Existing(OauthCredentialHandlingOutcome::Recorded(
            OauthCredentialOutcome::AlreadyProvisioned
        ))
    ));
    Ok(())
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn oauth_provisioning_rejects_superseded_generations_and_changed_registration()
-> Result<(), Box<dyn Error>> {
    let (_container, pool, _) = migrated_postgres().await?;
    let repository = OauthCredentialRepository::new(pool.clone());
    let first = command(OauthCredentialOperation::Provision);
    let second = command(OauthCredentialOperation::Provision);
    let registration = registration();
    repository
        .replace_registrations(&[(first.profile.clone(), registration.clone())])
        .await?;
    let OauthStartOutcome::Started(first_exchange) =
        repository.begin_exchange(&first, Ok(&registration)).await?
    else {
        panic!("first exchange");
    };
    let OauthStartOutcome::Started(second_exchange) = repository
        .begin_exchange(&second, Ok(&registration))
        .await?
    else {
        panic!("second exchange");
    };
    assert_eq!(
        repository
            .complete_exchange(&first_exchange, Ok(&authorization()))
            .await?,
        OauthCredentialOutcome::Provisioned
    );
    assert_eq!(
        repository
            .complete_exchange(&second_exchange, Ok(&authorization()))
            .await?,
        OauthCredentialOutcome::Superseded
    );
    let replacement = command(OauthCredentialOperation::Reprovision);
    let OauthStartOutcome::Started(replacement_exchange) = repository
        .begin_exchange(&replacement, Ok(&registration))
        .await?
    else {
        panic!("replacement exchange");
    };
    repository.replace_registrations(&[]).await?;
    assert_eq!(
        repository
            .complete_exchange(&replacement_exchange, Ok(&authorization()))
            .await?,
        OauthCredentialOutcome::Failed(OauthCredentialFailure::RegistrationChanged)
    );
    let generation: i64 =
        sqlx::query_scalar("SELECT generation FROM oauth_credential_profile WHERE profile = $1")
            .bind(&first.profile)
            .fetch_one(&pool)
            .await?;
    assert_eq!(
        generation, 1,
        "rejected exchanges must not advance the generation"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn oauth_startup_abandons_pending_claims_without_retaining_authorization()
-> Result<(), Box<dyn Error>> {
    let (_container, pool, _) = migrated_postgres().await?;
    let repository = OauthCredentialRepository::new(pool.clone());
    let command = command(OauthCredentialOperation::Provision);
    let registration = registration();
    repository
        .replace_registrations(&[(command.profile.clone(), registration.clone())])
        .await?;
    let OauthStartOutcome::Started(exchange) = repository
        .begin_exchange(&command, Ok(&registration))
        .await?
    else {
        panic!("pending exchange");
    };
    repository.abandon_pending().await?;
    assert_eq!(
        repository
            .complete_exchange(&exchange, Ok(&authorization()))
            .await?,
        OauthCredentialOutcome::Abandoned
    );
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM oauth_credential_authorization")
        .fetch_one(&pool)
        .await?;
    assert_eq!(count, 0);
    assert!(matches!(
        repository
            .begin_exchange(&command, Ok(&registration))
            .await?,
        OauthStartOutcome::Existing(OauthCredentialHandlingOutcome::Recorded(
            OauthCredentialOutcome::Abandoned
        ))
    ));
    Ok(())
}

/// Both members must denote independent accounts; all pool actions are inert in this fixture.
fn oauth_pool(profiles: &[&str]) -> CredentialPoolRuntimePolicy {
    use signalbox_persistence::model_execution::CredentialPoolRuntimeExhaustion;
    CredentialPoolRuntimePolicy::new(
        "oauth-independence",
        profiles
            .iter()
            .map(|profile| CredentialPoolRuntimeMember::new(*profile, std::num::NonZeroU32::MIN))
            .collect::<Vec<_>>(),
        CredentialPoolRuntimeExhaustion::Fail,
        CredentialPoolRuntimeAction::Stay,
        CredentialPoolRuntimeAction::Stay,
        CredentialPoolRuntimeAction::Stay,
        CredentialPoolRuntimeAction::Stay,
    )
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn oauth_concurrent_pool_members_cannot_provision_the_same_account()
-> Result<(), Box<dyn Error>> {
    let (_container, pool, _) = migrated_postgres().await?;
    const FIRST: &str = "first-oauth";
    const SECOND: &str = "second-oauth";
    const CALL_SEED: u128 = 41000; // Arbitrary identities for the retained pool revision.
    super::credential_capacity_policy::prepare_capacity_call(
        &pool,
        CALL_SEED,
        oauth_pool(&[FIRST, SECOND]),
    )
    .await?;
    let repository = OauthCredentialRepository::new(pool.clone());
    let registration = registration();
    repository
        .replace_registrations(&[
            (FIRST.into(), registration.clone()),
            (SECOND.into(), registration.clone()),
        ])
        .await?;
    let first = OauthCredentialCommand {
        profile: FIRST.into(),
        ..command(OauthCredentialOperation::Provision)
    };
    let second = OauthCredentialCommand {
        profile: SECOND.into(),
        ..command(OauthCredentialOperation::Provision)
    };
    let OauthStartOutcome::Started(first) =
        repository.begin_exchange(&first, Ok(&registration)).await?
    else {
        panic!("first exchange");
    };
    let OauthStartOutcome::Started(second) = repository
        .begin_exchange(&second, Ok(&registration))
        .await?
    else {
        panic!("second exchange");
    };
    let authorization = authorization();
    let (first, second) = tokio::join!(
        repository.complete_exchange(&first, Ok(&authorization)),
        repository.complete_exchange(&second, Ok(&authorization))
    );
    let outcomes = [first?, second?];
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| **outcome == OauthCredentialOutcome::Provisioned)
            .count(),
        1
    );
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| **outcome
                == OauthCredentialOutcome::Failed(
                    OauthCredentialFailure::AccountIndependenceFailed
                ))
            .count(),
        1
    );
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM oauth_credential_authorization")
        .fetch_one(&pool)
        .await?;
    assert_eq!(
        count, 1,
        "account collision must retain only the winning authorization"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn oauth_pool_interning_rejects_already_provisioned_account_aliases()
-> Result<(), Box<dyn Error>> {
    let (_container, pool, _) = migrated_postgres().await?;
    const FIRST: &str = "first-oauth";
    const SECOND: &str = "second-oauth";
    let repository = OauthCredentialRepository::new(pool.clone());
    let registration = registration();
    repository
        .replace_registrations(&[
            (FIRST.into(), registration.clone()),
            (SECOND.into(), registration.clone()),
        ])
        .await?;
    for profile in [FIRST, SECOND] {
        let command = OauthCredentialCommand {
            profile: profile.into(),
            ..command(OauthCredentialOperation::Provision)
        };
        let OauthStartOutcome::Started(exchange) = repository
            .begin_exchange(&command, Ok(&registration))
            .await?
        else {
            panic!("independent profile exchange");
        };
        assert_eq!(
            repository
                .complete_exchange(&exchange, Ok(&authorization()))
                .await?,
            OauthCredentialOutcome::Provisioned
        );
    }
    const CALL_SEED: u128 = 42000; // Arbitrary identities for the rejected pool revision.
    let result = super::credential_capacity_policy::prepare_capacity_call(
        &pool,
        CALL_SEED,
        oauth_pool(&[FIRST, SECOND]),
    )
    .await;
    assert!(
        result.is_err(),
        "interning must reject two names for the same OAuth account"
    );
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM model_call_credential_pool_member")
        .fetch_one(&pool)
        .await?;
    assert_eq!(count, 0, "rejected membership must roll back atomically");
    Ok(())
}
