//! Concurrent waiters share capacity only through selecting preparations.
use super::*;
use signalbox_persistence::credential_invocations;
use std::num::NonZeroU32;

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn credential_pool_contended_wait_release_admits_exactly_one_competitor()
-> Result<(), Box<dyn Error>> {
    // Each session has independent deterministic fixture identities and shares one home.
    const OWNER: u128 = 0x6002_1000;
    const LEFT: u128 = 0x6002_2000;
    const RIGHT: u128 = 0x6002_3000;
    const MEMBER: &str = "bounded-home";
    const POOL: &str = "contended-pool";
    let (container, pool, _) = migrated_postgres().await?;
    let registrations = vec![(MEMBER.to_owned(), NonZeroU32::new(1))];
    credential_invocations::replace_registrations(&pool, &registrations).await?;
    let (owner, _, owner_repository) = active_credential_pool_fixture(
        &pool,
        OWNER,
        POOL,
        &[MEMBER],
        CredentialPoolRuntimeAction::Stay,
        CredentialPoolRuntimeAction::Quarantine,
    )
    .await?;
    let (call, _) = prepare_and_authorize_pool_call(&owner_repository, owner, OWNER + 100).await?;
    let call = call.observation_correlation().call();
    let (left, left_turn, left_repository) = active_credential_pool_fixture(
        &pool,
        LEFT,
        POOL,
        &[MEMBER],
        CredentialPoolRuntimeAction::Stay,
        CredentialPoolRuntimeAction::Quarantine,
    )
    .await?;
    let (right, _, right_repository) = active_credential_pool_fixture(
        &pool,
        RIGHT,
        POOL,
        &[MEMBER],
        CredentialPoolRuntimeAction::Stay,
        CredentialPoolRuntimeAction::Quarantine,
    )
    .await?;
    let PrepareInitialModelCallOutcome::CredentialWait(left_wait) =
        prepare_wait_admission(&left_repository, left, LEFT + 100).await?
    else {
        panic!("a saturated fail pool still waits")
    };
    let PrepareInitialModelCallOutcome::CredentialWait(right_wait) =
        prepare_wait_admission(&right_repository, right, RIGHT + 100).await?
    else {
        panic!("both waiters retain contention")
    };
    assert_eq!(
        left_wait.cause(),
        CredentialAvailabilityWaitCause::Contended
    );
    let recorded: Vec<Uuid> = sqlx::query_scalar("SELECT reservation_ids FROM credential_availability_wait_member WHERE wait_attempt_id = $1").bind(left_wait.attempt().into_uuid()).fetch_one(&pool).await?;
    assert_eq!(recorded, vec![call.into_uuid()]);
    credential_invocations::replace_registrations(&pool, &registrations).await?;
    let eligible: bool = sqlx::query_scalar("SELECT credential_wait_is_eligible($1)")
        .bind(left_wait.attempt().into_uuid())
        .fetch_one(&pool)
        .await?;
    assert!(!eligible, "restart with unchanged capacity is not a wake");
    credential_invocations::release(&pool, call).await?;
    let (left_outcome, right_outcome) = tokio::join!(
        prepare_wait_admission(&left_repository, left, LEFT + 120),
        prepare_wait_admission(&right_repository, right, RIGHT + 120)
    );
    let outcomes = [left_outcome?, right_outcome?];
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| matches!(outcome, PrepareInitialModelCallOutcome::Checkpointed(_)))
            .count(),
        1
    );
    let remaining_wait = outcomes
        .iter()
        .find_map(|outcome| match outcome {
            PrepareInitialModelCallOutcome::CredentialWait(wait) => Some(*wait),
            _ => None,
        })
        .expect("one waiter reparks");
    assert!(
        remaining_wait.attempt() == left_wait.attempt()
            || remaining_wait.attempt() == right_wait.attempt()
    );
    let active: i64 = sqlx::query_scalar("SELECT count(*) FROM credential_invocation_reservation WHERE profile = $1 AND released_at IS NULL").bind(MEMBER).fetch_one(&pool).await?;
    assert_eq!(active, 1);
    let attempts: i64 = sqlx::query_scalar("SELECT count(*) FROM turn_attempt WHERE turn_id = $1")
        .bind(left_turn.into_uuid())
        .fetch_one(&pool)
        .await?;
    assert_eq!(
        attempts,
        if remaining_wait.attempt() == left_wait.attempt() {
            1
        } else {
            2
        }
    );
    let current: Vec<Uuid> = sqlx::query_scalar("SELECT reservation_ids FROM credential_availability_wait_member WHERE wait_attempt_id = $1").bind(remaining_wait.attempt().into_uuid()).fetch_one(&pool).await?;
    assert_ne!(current, recorded, "repark retains the competing invocation");
    let wider = vec![(MEMBER.to_owned(), NonZeroU32::new(2))];
    credential_invocations::replace_registrations(&pool, &wider).await?;
    let eligible: bool = sqlx::query_scalar("SELECT credential_wait_is_eligible($1)")
        .bind(remaining_wait.attempt().into_uuid())
        .fetch_one(&pool)
        .await?;
    assert!(
        eligible,
        "startup re-evaluates against current registrations"
    );
    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn credential_pool_contended_wait_rechecks_exhaustion_policy_when_member_is_excluded()
-> Result<(), Box<dyn Error>> {
    const OWNER: u128 = 0x6002_4000;
    const WAITER: u128 = 0x6002_5000;
    const MEMBER: &str = "excluded-bounded-home";
    const POOL: &str = "contended-fail-pool";
    let (container, pool, _) = migrated_postgres().await?;
    credential_invocations::replace_registrations(
        &pool,
        &[(MEMBER.to_owned(), NonZeroU32::new(1))],
    )
    .await?;
    let (owner, _, owner_repository) = active_credential_pool_fixture(
        &pool,
        OWNER,
        POOL,
        &[MEMBER],
        CredentialPoolRuntimeAction::Stay,
        CredentialPoolRuntimeAction::Quarantine,
    )
    .await?;
    let (call, _) = prepare_and_authorize_pool_call(&owner_repository, owner, OWNER + 100).await?;
    let call = call.observation_correlation().call();
    let (session, turn, repository) = active_credential_pool_fixture(
        &pool,
        WAITER,
        POOL,
        &[MEMBER],
        CredentialPoolRuntimeAction::Stay,
        CredentialPoolRuntimeAction::Quarantine,
    )
    .await?;
    let PrepareInitialModelCallOutcome::CredentialWait(wait) =
        prepare_wait_admission(&repository, session, WAITER + 100).await?
    else {
        panic!("saturated member waits")
    };
    sqlx::query("INSERT INTO credential_pool_transient_exclusion (observation_model_call_id, credential_reference, cause_kind, reset_at) VALUES ($1,$2,'overloaded',transaction_timestamp() + interval '1 hour')").bind(call.into_uuid()).bind(MEMBER).execute(&pool).await?;
    let _ = prepare_wait_admission(&repository, session, WAITER + 110).await?;
    let ended: (String, String, Uuid) = sqlx::query_as("SELECT lifecycle.state_kind, attempt.end_disposition, attempt.continued_from_attempt_id FROM turn_lifecycle lifecycle JOIN turn_attempt attempt ON attempt.turn_attempt_id = lifecycle.terminal_attempt_id WHERE lifecycle.turn_id = $1").bind(turn.into_uuid()).fetch_one(&pool).await?;
    assert_eq!(
        ended,
        (
            "terminal".to_owned(),
            "known_failure".to_owned(),
            wait.attempt().into_uuid()
        )
    );
    let calls: i64 = sqlx::query_scalar("SELECT count(*) FROM model_call WHERE turn_id = $1")
        .bind(turn.into_uuid())
        .fetch_one(&pool)
        .await?;
    assert_eq!(calls, 0);
    pool.close().await;
    drop(container);
    Ok(())
}
