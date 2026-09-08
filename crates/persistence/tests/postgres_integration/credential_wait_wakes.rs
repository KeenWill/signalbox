//! Wakes grant admission eligibility without preparing a call.
use super::*;
use signalbox_persistence::credential_exclusions::{self, ClearCredentialExclusion};
use signalbox_persistence::scheduler::PostgresEligibilitySweep;

async fn parked_member(
    pool: &sqlx::PgPool,
    quarantine: bool,
) -> Result<
    (
        SessionId,
        TurnId,
        PostgresModelCallRepository,
        signalbox_domain::CredentialAvailabilityWait,
        Uuid,
    ),
    Box<dyn Error>,
> {
    const SOURCE: u128 = 0x6001_1000;
    const WAITER: u128 = 0x6001_2000;
    const POOL: &str = "wake-pool";
    const MEMBER: &str = "waking-member";
    let (source_session, source_turn, source_repository) = active_credential_pool_fixture(
        pool,
        SOURCE,
        POOL,
        &[MEMBER],
        CredentialPoolRuntimeAction::Stay,
        CredentialPoolRuntimeAction::Quarantine,
    )
    .await?;
    let (source, _) =
        prepare_and_authorize_pool_call(&source_repository, source_session, SOURCE + 100).await?;
    let observation = source.observation_correlation().call().into_uuid();
    if quarantine {
        sqlx::query("INSERT INTO credential_pool_member_action (pool_name, credential_reference, action_kind, observed_session_id, observed_turn_id, observation_model_call_id, cause_kind) VALUES ($1,$2,'quarantine',$3,$4,$5,'credential_rejected')")
            .bind(POOL).bind(MEMBER).bind(source_session.into_uuid()).bind(source_turn.into_uuid()).bind(observation).execute(pool).await?;
    } else {
        sqlx::query("INSERT INTO credential_pool_transient_exclusion (observation_model_call_id, credential_reference, cause_kind, reset_at) VALUES ($1,$2,'overloaded',transaction_timestamp() + interval '1 hour')")
            .bind(observation).bind(MEMBER).execute(pool).await?;
    }
    let (session, turn, repository) = active_credential_pool_fixture(
        pool,
        WAITER,
        POOL,
        &[MEMBER],
        CredentialPoolRuntimeAction::SwitchNow,
        CredentialPoolRuntimeAction::Quarantine,
    )
    .await?;
    let target = ResolvedProviderTarget::naming(ProviderModelIdentity::from_uuid(Uuid::from_u128(
        WAITER + 4,
    )));
    let repository =
        repository.with_credential_pools(HashMap::from([(target, park_policy(POOL, &[MEMBER]))]));
    let PrepareInitialModelCallOutcome::CredentialWait(wait) =
        prepare_wait_admission(&repository, session, WAITER + 100).await?
    else {
        panic!("excluded member must park")
    };
    Ok((session, turn, repository, wait, observation))
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn credential_pool_wait_operator_clear_wakes_a_deadline_free_wait_but_restart_does_not()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _) = migrated_postgres().await?;
    let (session, turn, repository, wait, _) = parked_member(&pool, true).await?;
    let eligible: bool = sqlx::query_scalar("SELECT credential_wait_is_eligible($1)")
        .bind(wait.attempt().into_uuid())
        .fetch_one(&pool)
        .await?;
    assert!(!eligible);
    let mut ids = signalbox_application::UuidV7StartupScanIdGenerator;
    assert_eq!(
        PostgresStartupScanRepository::new(pool.clone())
            .recover(
                session,
                signalbox_domain::AcceptedInputTurnFailureIdentities::new(
                    SemanticTranscriptEntryId::from_uuid(Uuid::now_v7()),
                    ContextFrontierId::from_uuid(Uuid::now_v7())
                ),
                &mut ids
            )
            .await?,
        StartupScanSessionOutcome::NoActiveTurn
    );
    let eligible: bool = sqlx::query_scalar("SELECT credential_wait_is_eligible($1)")
        .bind(wait.attempt().into_uuid())
        .fetch_one(&pool)
        .await?;
    assert!(!eligible, "restart alone cannot grant eligibility");
    let target = credential_exclusions::list(&pool, 100, None)
        .await?
        .exclusions
        .remove(0);
    credential_exclusions::clear(
        &pool,
        ClearCredentialExclusion {
            command_id: DurableCommandId::from_uuid(Uuid::now_v7()),
            target,
        },
    )
    .await?;
    let eligible: bool = sqlx::query_scalar("SELECT credential_wait_is_eligible($1)")
        .bind(wait.attempt().into_uuid())
        .fetch_one(&pool)
        .await?;
    assert!(eligible);
    let calls: i64 = sqlx::query_scalar("SELECT count(*) FROM model_call WHERE turn_id = $1")
        .bind(turn.into_uuid())
        .fetch_one(&pool)
        .await?;
    assert_eq!(
        calls, 0,
        "the clear grants eligibility without releasing the wait"
    );
    assert!(
        PostgresEligibilitySweep::new(pool.clone())
            .find_sessions()
            .await?
            .into_parts()
            .0
            .contains(&session)
    );
    assert_eq!(
        PostgresToolLoopRepository::new(pool.clone())
            .find_resumable_turn(session)
            .await?,
        Some(turn)
    );
    assert!(matches!(
        prepare_wait_admission(&repository, session, 0x6001_2200).await?,
        PrepareInitialModelCallOutcome::Checkpointed(_)
    ));
    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn credential_pool_wait_deadline_reconciles_without_a_delivered_wake()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _) = migrated_postgres().await?;
    let (session, _, repository, wait, observation) = parked_member(&pool, false).await?;
    let original: i64 = sqlx::query_scalar("SELECT floor(extract(epoch FROM deadline) * 1000)::bigint FROM credential_availability_wait WHERE wait_attempt_id = $1")
        .bind(wait.attempt().into_uuid()).fetch_one(&pool).await?;
    sqlx::query("UPDATE credential_pool_transient_exclusion SET reset_at = transaction_timestamp() + interval '2 hours' WHERE observation_model_call_id = $1")
        .bind(observation).execute(&pool).await?;
    let PrepareInitialModelCallOutcome::CredentialWait(reparked) =
        prepare_wait_admission(&repository, session, 0x6001_2250).await?
    else {
        panic!("updated exclusion remains inadmissible")
    };
    assert_eq!(reparked.attempt(), wait.attempt());
    let refreshed: (i64, bool) = sqlx::query_as("SELECT floor(extract(epoch FROM deadline) * 1000)::bigint, eligible FROM credential_availability_wait WHERE wait_attempt_id = $1")
        .bind(wait.attempt().into_uuid()).fetch_one(&pool).await?;
    assert!(refreshed.0 > original);
    assert!(!refreshed.1);
    // Advance the retained exclusion and matching snapshot together to simulate elapsed time.
    let mut tx = pool.begin().await?;
    let expired: i64 = sqlx::query_scalar(
        "SELECT floor(extract(epoch FROM transaction_timestamp()) * 1000)::bigint - 1",
    )
    .fetch_one(&mut *tx)
    .await?;
    sqlx::query("UPDATE credential_pool_transient_exclusion SET reset_at = to_timestamp($2::double precision / 1000) WHERE observation_model_call_id = $1").bind(observation).bind(expired).execute(&mut *tx).await?;
    sqlx::query("UPDATE credential_availability_wait_member SET exclusions = jsonb_set(exclusions, '{0,reset}', to_jsonb($2::bigint)) WHERE wait_attempt_id = $1").bind(wait.attempt().into_uuid()).bind(expired).execute(&mut *tx).await?;
    sqlx::query("UPDATE credential_availability_wait SET deadline = to_timestamp($2::double precision / 1000), eligible = false WHERE wait_attempt_id = $1").bind(wait.attempt().into_uuid()).bind(expired).execute(&mut *tx).await?;
    tx.commit().await?;
    assert!(
        PostgresEligibilitySweep::new(pool.clone())
            .find_sessions()
            .await?
            .into_parts()
            .0
            .contains(&session)
    );
    assert!(matches!(
        prepare_wait_admission(&repository, session, 0x6001_2300).await?,
        PrepareInitialModelCallOutcome::Checkpointed(_)
    ));
    pool.close().await;
    drop(container);
    Ok(())
}
