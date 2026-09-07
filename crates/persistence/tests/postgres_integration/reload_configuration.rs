//! Durable reload claim, refusal replay, and immutable intent constraints.

use crate::*;
use signalbox_persistence::reload_configuration::{
    ReloadClaim, ReloadConfiguration, ReloadConfigurationRepository, ReloadIntent, ReloadLookup,
    ReloadPhase, ReloadResult,
};

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn reload_configuration_retains_pending_intent_and_replays_terminal_result()
-> Result<(), Box<dyn Error>> {
    let (_container, pool, _) = migrated_postgres().await?;
    let repository = ReloadConfigurationRepository::new(pool.clone());
    let request = ReloadConfiguration {
        command_id: DurableCommandId::from_uuid(next_test_submit_uuid()),
    };
    // These opaque checked payload fixtures differ so replacement/prior reversal is observable.
    let intent = ReloadIntent {
        replacement_snapshot: "{\"replacement\":true}".to_owned(),
        prior_snapshot: "{\"prior\":true}".to_owned(),
        rule_set_digest: [0; 32],
    };
    assert_eq!(repository.lookup(request).await?, ReloadLookup::Unclaimed);
    assert_eq!(
        repository.claim(request, Ok(&intent)).await?,
        ReloadClaim::Retained
    );
    let pending = repository.pending().await?;
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].0.command_id, request.command_id);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&pending[0].1.replacement_snapshot)?,
        serde_json::json!({"replacement":true})
    );
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&pending[0].1.prior_snapshot)?,
        serde_json::json!({"prior":true})
    );
    assert_eq!(pending[0].1.rule_set_digest, intent.rule_set_digest);
    let refused = ReloadResult::Failed {
        phase: ReloadPhase::Validate,
        reason: "startup-only configuration differs".to_owned(),
    };
    assert_eq!(
        repository.claim(request, Err(&refused)).await?,
        ReloadClaim::Settled(ReloadLookup::Pending)
    );
    repository.finish(request, &ReloadResult::Reloaded).await?;
    assert_eq!(
        repository.claim(request, Err(&refused)).await?,
        ReloadClaim::Settled(ReloadLookup::Recorded(ReloadResult::Reloaded))
    );
    assert!(repository.pending().await?.is_empty());
    assert!(
        sqlx::query(
            "UPDATE reload_configuration_command SET prior_snapshot = '{}' WHERE command_id = $1"
        )
        .bind(request.command_id.as_uuid())
        .execute(&pool)
        .await
        .is_err()
    );
    assert!(
        sqlx::query("DELETE FROM reload_configuration_result WHERE command_id = $1")
            .bind(request.command_id.as_uuid())
            .execute(&pool)
            .await
            .is_err()
    );
    pool.close().await;
    Ok(())
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn reload_configuration_refusal_claims_identity_and_requires_a_typed_record()
-> Result<(), Box<dyn Error>> {
    let (_container, pool, _) = migrated_postgres().await?;
    let repository = ReloadConfigurationRepository::new(pool.clone());
    let request = ReloadConfiguration {
        command_id: DurableCommandId::from_uuid(next_test_submit_uuid()),
    };
    let refused = ReloadResult::Failed {
        phase: ReloadPhase::Read,
        reason: "configuration could not be read".to_owned(),
    };
    assert_eq!(
        repository.claim(request, Err(&refused)).await?,
        ReloadClaim::Settled(ReloadLookup::Recorded(refused.clone()))
    );
    assert_eq!(
        repository.lookup(request).await?,
        ReloadLookup::Recorded(refused)
    );
    assert!(repository.pending().await?.is_empty());
    let missing = next_test_submit_uuid();
    assert!(sqlx::query("INSERT INTO durable_command (command_id, command_kind, storage_version, claimed_at, issuer_kind, issuer_module)
        VALUES ($1, 'reload_configuration', 1, now(), 'operator', NULL)")
        .bind(missing).execute(&pool).await.is_err());
    let existing = CreateSessionRepository::new(pool.clone(), test_session_credential_pin());
    let mut service = CreateSessionService::new(
        FixedSessionIds::new([SessionId::from_uuid(next_test_submit_uuid())]),
        existing,
    );
    let conflicting = service
        .execute(CreateSessionRequest::try_new(
            request.command_id,
            SessionConfigurationDefaults::new(direct(0x801)),
        )?)
        .await;
    // The create command must not be able to steal a reload claim.
    assert!(matches!(
        conflicting,
        Ok(CreateSessionOutcome::ConflictingReuse { .. })
    ));
    pool.close().await;
    Ok(())
}
