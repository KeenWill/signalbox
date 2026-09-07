//! OAuth claim replay, cross-kind exclusion, and immutable record enforcement.

use super::*;
use signalbox_persistence::oauth_credential::*;

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn oauth_dispatch_serializes_token_copy_and_records_refresh_quarantine()
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
        panic!("initial exchange");
    };
    repository
        .complete_exchange(&exchange, Ok(&authorization()))
        .await?;
    let lease = repository
        .lock_dispatch(&command.profile)
        .await?
        .expect("profile");
    let competing = repository.lock_dispatch(&command.profile);
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(50), competing)
            .await
            .is_err(),
        "token copying retains the profile lock"
    );
    lease.mark_refresh().await?;
    let lease = repository
        .lock_dispatch(&command.profile)
        .await?
        .expect("profile");
    assert!(
        lease
            .authorization()
            .expect("authorization")
            .refresh_in_progress
    );
    lease.clear_refresh().await?;
    let lease = repository
        .lock_dispatch(&command.profile)
        .await?
        .expect("profile");
    assert!(
        !lease
            .authorization()
            .expect("authorization")
            .refresh_in_progress
    );
    lease.mark_refresh().await?;
    let lease = repository
        .lock_dispatch(&command.profile)
        .await?
        .expect("profile");
    let mut replacement = authorization();
    replacement.refresh_token = "replacement-refresh".into();
    replacement.identity_token = "replacement-identity".into();
    lease.replace_refresh(&replacement).await?;
    let lease = repository
        .lock_dispatch(&command.profile)
        .await?
        .expect("profile");
    let current = lease.authorization().expect("authorization");
    assert!(!current.refresh_in_progress);
    assert_eq!(
        current.authorization.identity_token,
        replacement.identity_token
    );
    assert_eq!(
        current.authorization.refresh_token,
        replacement.refresh_token
    );
    lease
        .quarantine(OauthQuarantineCause::RefreshAmbiguous)
        .await?;
    let (quarantined, cause, evidence): (bool, String, String) = sqlx::query_as("SELECT a.quarantined, a.quarantine_cause, f.cause FROM oauth_credential_authorization a JOIN oauth_credential_failure f USING (profile, generation) WHERE profile = $1")
        .bind(&command.profile).fetch_one(&pool).await?;
    assert!(quarantined);
    assert_eq!(cause, "refresh_ambiguous");
    assert_eq!(cause, evidence);
    Ok(())
}

fn command(operation: OauthCredentialOperation) -> OauthCredentialCommand {
    OauthCredentialCommand {
        command_id: DurableCommandId::from_uuid(Uuid::now_v7()),
        operation,
        profile: "subscription".into(),
    }
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn oauth_delete_rejects_pool_only_rows_and_retains_real_administration_history()
-> Result<(), Box<dyn Error>> {
    let (_container, pool, _) = migrated_postgres().await?;
    let profile = "pool-only";
    super::credential_capacity_policy::prepare_capacity_call(&pool, 43000, oauth_pool(&[profile]))
        .await?;
    let repository = OauthCredentialRepository::new(pool.clone());
    for reason in [
        OauthCredentialFailure::NonOauthProfile,
        OauthCredentialFailure::UnknownProfile,
    ] {
        let command = OauthCredentialCommand {
            profile: profile.into(),
            ..command(OauthCredentialOperation::Delete)
        };
        assert_eq!(
            repository
                .delete(&command, reason, || panic!(
                    "a rejected deletion cannot discard cache"
                ))
                .await?,
            OauthCredentialHandlingOutcome::Recorded(OauthCredentialOutcome::Failed(reason))
        );
    }
    let generation: i64 =
        sqlx::query_scalar("SELECT generation FROM oauth_credential_profile WHERE profile = $1")
            .bind(profile)
            .fetch_one(&pool)
            .await?;
    assert_eq!(generation, 0);
    let registration = registration();
    repository
        .replace_registrations(&[
            ("registered-only".into(), registration.clone()),
            ("reprovision-only".into(), registration.clone()),
        ])
        .await?;
    assert_eq!(
        repository
            .delete(
                &OauthCredentialCommand {
                    profile: "registered-only".into(),
                    ..command(OauthCredentialOperation::Delete)
                },
                OauthCredentialFailure::UnknownProfile,
                || {}
            )
            .await?,
        OauthCredentialHandlingOutcome::Recorded(OauthCredentialOutcome::AlreadyDeleted)
    );
    assert!(matches!(
        repository
            .begin_exchange(
                &OauthCredentialCommand {
                    profile: "reprovision-only".into(),
                    ..command(OauthCredentialOperation::Reprovision)
                },
                Ok(&registration)
            )
            .await?,
        OauthStartOutcome::Existing(OauthCredentialHandlingOutcome::Recorded(
            OauthCredentialOutcome::NotProvisioned
        ))
    ));
    repository.replace_registrations(&[]).await?;
    for profile in ["registered-only", "reprovision-only"] {
        assert_eq!(
            repository
                .delete(
                    &OauthCredentialCommand {
                        profile: profile.into(),
                        ..command(OauthCredentialOperation::Delete)
                    },
                    OauthCredentialFailure::UnknownProfile,
                    || {}
                )
                .await?,
            OauthCredentialHandlingOutcome::Recorded(OauthCredentialOutcome::AlreadyDeleted)
        );
    }
    Ok(())
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn oauth_delete_serializes_copy_advances_generation_and_preserves_replay_and_history()
-> Result<(), Box<dyn Error>> {
    let (_container, pool, _) = migrated_postgres().await?;
    let repository = OauthCredentialRepository::new(pool.clone());
    let registration = registration();
    repository
        .replace_registrations(&[("subscription".into(), registration.clone())])
        .await?;
    let OauthStartOutcome::Started(exchange) = repository
        .begin_exchange(
            &command(OauthCredentialOperation::Provision),
            Ok(&registration),
        )
        .await?
    else {
        panic!("initial exchange");
    };
    repository
        .complete_exchange(&exchange, Ok(&authorization()))
        .await?;
    let OauthStartOutcome::Started(pending) = repository
        .begin_exchange(
            &command(OauthCredentialOperation::Reprovision),
            Ok(&registration),
        )
        .await?
    else {
        panic!("pending replacement");
    };
    repository
        .lock_dispatch("subscription")
        .await?
        .expect("profile")
        .quarantine(OauthQuarantineCause::RefreshRejected)
        .await?;
    repository.replace_registrations(&[]).await?;
    let deletion = command(OauthCredentialOperation::Delete);
    let copying = repository
        .lock_dispatch("subscription")
        .await?
        .expect("profile");
    assert!(
        tokio::time::timeout(
            std::time::Duration::from_millis(50),
            repository.delete(
                &deletion,
                OauthCredentialFailure::UnknownProfile,
                || panic!("copy lock excludes cache removal")
            )
        )
        .await
        .is_err()
    );
    copying.commit().await?;
    let mut discarded = false;
    assert_eq!(
        repository
            .delete(&deletion, OauthCredentialFailure::UnknownProfile, || {
                discarded = true
            })
            .await?,
        OauthCredentialHandlingOutcome::Recorded(OauthCredentialOutcome::Deleted)
    );
    assert!(discarded);
    assert_eq!(
        repository
            .complete_exchange(&pending, Ok(&authorization()))
            .await?,
        OauthCredentialOutcome::Superseded
    );
    assert_eq!(
        repository
            .delete(
                &command(OauthCredentialOperation::Delete),
                OauthCredentialFailure::NonOauthProfile,
                || {}
            )
            .await?,
        OauthCredentialHandlingOutcome::Recorded(OauthCredentialOutcome::AlreadyDeleted)
    );
    let generation: i64 = sqlx::query_scalar(
        "SELECT generation FROM oauth_credential_profile WHERE profile = 'subscription'",
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(generation, 3);
    let history: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM oauth_credential_failure WHERE profile = 'subscription'",
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(history, 1);
    repository
        .replace_registrations(&[("subscription".into(), registration.clone())])
        .await?;
    let OauthStartOutcome::Started(exchange) = repository
        .begin_exchange(
            &command(OauthCredentialOperation::Provision),
            Ok(&registration),
        )
        .await?
    else {
        panic!("new authorization");
    };
    repository
        .complete_exchange(&exchange, Ok(&authorization()))
        .await?;
    assert_eq!(
        repository
            .delete(
                &deletion,
                OauthCredentialFailure::NonOauthProfile,
                || panic!("replay cannot discard a newer token")
            )
            .await?,
        OauthCredentialHandlingOutcome::Recorded(OauthCredentialOutcome::Deleted)
    );
    let lease = repository
        .lock_dispatch("subscription")
        .await?
        .expect("profile");
    assert_eq!(
        lease
            .authorization()
            .expect("new authorization survives replay")
            .generation,
        4
    );
    Ok(())
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
        refresh_token_url: "https://authorization.example/oauth/token".into(),
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
    let changed_registration = OauthRegistration {
        refresh_token_url: "https://other-authorization.example/oauth/token".into(),
        ..registration
    };
    repository
        .replace_registrations(&[(first.profile.clone(), changed_registration)])
        .await?;
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
