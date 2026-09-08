//! Credential admission parks without a call and releases through a fresh attempt.
use super::*;
use signalbox_domain::CredentialAvailabilityWaitCause;
use signalbox_persistence::model_execution::CredentialPoolRuntimeExhaustion;

pub(super) fn park_policy(name: &str, members: &[&str]) -> CredentialPoolRuntimePolicy {
    CredentialPoolRuntimePolicy::new(
        name.to_owned(),
        members
            .iter()
            .enumerate()
            .map(|(ordinal, member)| {
                CredentialPoolRuntimeMember::new(
                    (*member).to_owned(),
                    nonzero_priority(u32::try_from(ordinal + 1).unwrap()),
                )
            })
            .collect::<Vec<_>>(),
        CredentialPoolRuntimeExhaustion::Park,
        CredentialPoolRuntimeAction::SwitchNow,
        CredentialPoolRuntimeAction::SwitchNow,
        CredentialPoolRuntimeAction::SwitchNow,
        CredentialPoolRuntimeAction::Quarantine,
    )
}

async fn prepare_wait_admission(
    repository: &PostgresModelCallRepository,
    session: SessionId,
    seed: u128,
) -> Result<PrepareInitialModelCallOutcome, Box<dyn Error>> {
    Ok(repository
        .prepare_initial_call(
            session,
            ModelCallId::from_uuid(Uuid::from_u128(seed)),
            FailedModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 1)),
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 2)),
            ),
            ContextFrontierId::from_uuid(Uuid::from_u128(seed + 3)),
            |_| panic!("no pending steering in this fixture"),
        )
        .await?)
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn credential_pool_wait_reparks_without_an_attempt_and_releases_atomically()
-> Result<(), Box<dyn Error>> {
    // Disjoint synthetic identities keep the exclusion's observation outside the parked turn.
    const SOURCE: u128 = 0x6000_1000;
    const WAITER: u128 = 0x6000_2000;
    const POOL: &str = "wait-pool";
    const MEMBER: &str = "cooling-member";
    let (container, pool, _) = migrated_postgres().await?;
    let (source_session, _, source_repository) = active_credential_pool_fixture(
        &pool,
        SOURCE,
        POOL,
        &[MEMBER],
        CredentialPoolRuntimeAction::Stay,
        CredentialPoolRuntimeAction::Stay,
    )
    .await?;
    let (source, _) =
        prepare_and_authorize_pool_call(&source_repository, source_session, SOURCE + 100).await?;
    let observation = source.observation_correlation().call().into_uuid();
    sqlx::query("INSERT INTO credential_pool_transient_exclusion (observation_model_call_id, credential_reference, cause_kind, reset_at) VALUES ($1,$2,'overloaded',transaction_timestamp() + interval '1 hour')")
        .bind(observation).bind(MEMBER).execute(&pool).await?;
    let (session, turn, repository) = active_credential_pool_fixture(
        &pool,
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
        panic!("exhaustion must park")
    };
    assert_eq!(wait.cause(), CredentialAvailabilityWaitCause::Exhausted);
    let phase: String =
        sqlx::query_scalar("SELECT active_phase_kind FROM turn_lifecycle WHERE turn_id = $1")
            .bind(turn.into_uuid())
            .fetch_one(&pool)
            .await?;
    assert_eq!(phase, "awaiting_credential_availability");
    let calls: i64 = sqlx::query_scalar("SELECT count(*) FROM model_call WHERE turn_id = $1")
        .bind(turn.into_uuid())
        .fetch_one(&pool)
        .await?;
    assert_eq!(calls, 0);
    let terminal_entries: i64 = sqlx::query_scalar("SELECT count(*) FROM semantic_transcript_entry WHERE source_session_id = $1 AND payload_kind = 'turn_failed'").bind(session.into_uuid()).fetch_one(&pool).await?;
    assert_eq!(terminal_entries, 0);
    // The wake transport is independent of the admission transaction.
    sqlx::query(
        "UPDATE credential_availability_wait SET eligible = true WHERE wait_attempt_id = $1",
    )
    .bind(wait.attempt().into_uuid())
    .execute(&pool)
    .await?;
    let PrepareInitialModelCallOutcome::CredentialWait(reparked) =
        prepare_wait_admission(&repository, session, WAITER + 110).await?
    else {
        panic!("an unchanged exclusion must repark")
    };
    assert_eq!(reparked.attempt(), wait.attempt());
    let attempts: i64 = sqlx::query_scalar("SELECT count(*) FROM turn_attempt WHERE turn_id = $1")
        .bind(turn.into_uuid())
        .fetch_one(&pool)
        .await?;
    assert_eq!(attempts, 1);
    sqlx::query("UPDATE credential_pool_transient_exclusion SET reset_at = transaction_timestamp() WHERE observation_model_call_id = $1").bind(observation).execute(&pool).await?;
    sqlx::query(
        "UPDATE credential_availability_wait SET eligible = true WHERE wait_attempt_id = $1",
    )
    .bind(wait.attempt().into_uuid())
    .execute(&pool)
    .await?;
    let PrepareInitialModelCallOutcome::Checkpointed(call) =
        prepare_wait_admission(&repository, session, WAITER + 120).await?
    else {
        panic!("the cleared member must prepare on release")
    };
    let released: (Uuid, Uuid, String) = sqlx::query_as("SELECT waiting.consumed_by_attempt_id, attempt.continued_from_attempt_id, call.state_kind FROM credential_availability_wait waiting JOIN turn_attempt attempt ON attempt.turn_attempt_id = waiting.consumed_by_attempt_id JOIN model_call call ON call.turn_attempt_id = attempt.turn_attempt_id WHERE waiting.wait_attempt_id = $1 AND call.model_call_id = $2")
        .bind(wait.attempt().into_uuid()).bind(call.into_uuid()).fetch_one(&pool).await?;
    assert_ne!(released.0, wait.attempt().into_uuid());
    assert_eq!(released.1, wait.attempt().into_uuid());
    assert_eq!(released.2, "prepared");
    pool.close().await;
    drop(container);
    Ok(())
}
