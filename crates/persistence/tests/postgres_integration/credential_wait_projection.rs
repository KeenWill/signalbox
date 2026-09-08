//! A terminal wait release keeps the provider failure on its call-free successor.
use super::*;
use signalbox_persistence::credential_invocations;
use signalbox_persistence::process_read::{
    ProcessProviderModelCallFailureCause, ProcessReadRepository, ProcessTurnState,
};

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn credential_pool_terminal_wait_release_retains_predecessor_provider_failure()
-> Result<(), Box<dyn Error>> {
    const DONOR: u128 = 0x6003_1000;
    const WAITER: u128 = 0x6003_2000;
    const POOL: &str = "terminal-wait-pool";
    const FIRST: &str = "terminal-wait-first";
    const BOUNDED: &str = "terminal-wait-bounded";
    let (container, pool, _) = migrated_postgres().await?;
    credential_invocations::replace_registrations(
        &pool,
        &[(BOUNDED.to_owned(), NonZeroU32::new(1))],
    )
    .await?;
    let (donor_session, _, donor_repository) = active_credential_pool_fixture(
        &pool,
        DONOR,
        POOL,
        &[BOUNDED],
        CredentialPoolRuntimeAction::Stay,
        CredentialPoolRuntimeAction::Quarantine,
    )
    .await?;
    let (donor, _) =
        prepare_and_authorize_pool_call(&donor_repository, donor_session, DONOR + 100).await?;
    let (session, turn, mut repository) = active_credential_pool_fixture(
        &pool,
        WAITER,
        POOL,
        &[FIRST, BOUNDED],
        CredentialPoolRuntimeAction::SwitchNow,
        CredentialPoolRuntimeAction::Quarantine,
    )
    .await?;
    let (first, reference) =
        prepare_and_authorize_pool_call(&repository, session, WAITER + 100).await?;
    assert_eq!(reference, FIRST);
    let predecessor = first.observation_correlation().call();
    let outcome = repository
        .commit_observation(
            session,
            first
                .observation_correlation()
                .bind_provider_failure_observation_with_retry_after(
                    ProviderModelCallFailureCause::QuotaExhausted,
                    ProviderReportedTokenUsage::unreported(),
                    None,
                    true,
                ),
            signalbox_application::ModelCallTerminalIdentityCandidates::Availability {
                failed: FailedModelCallTurnIdentities::new(
                    SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(WAITER + 120)),
                    ContextFrontierId::from_uuid(Uuid::from_u128(WAITER + 121)),
                ),
                successor_attempt: TurnAttemptId::from_uuid(Uuid::from_u128(WAITER + 122)),
            },
            |_| panic!("no steering in this fixture"),
        )
        .await?;
    let wait = match outcome {
        Some(ModelCallObservationCommitOutcome::CredentialWait(wait)) => wait,
        Some(ModelCallObservationCommitOutcome::AvailabilitySuccessor(_)) => {
            let PrepareInitialModelCallOutcome::CredentialWait(wait) =
                prepare_wait_admission(&repository, session, WAITER + 130).await?
            else {
                panic!("bounded successor parks")
            };
            wait
        }
        other => panic!("provider failure must continue to contention: {other:?}"),
    };
    assert_eq!(wait.cause(), CredentialAvailabilityWaitCause::Contended);
    sqlx::query("INSERT INTO credential_pool_transient_exclusion (observation_model_call_id, credential_reference, cause_kind, reset_at) VALUES ($1,$2,'overloaded',transaction_timestamp() + interval '1 hour')")
        .bind(donor.observation_correlation().call().into_uuid()).bind(BOUNDED).execute(&pool).await?;
    let outcome = prepare_wait_admission(&repository, session, WAITER + 140).await?;
    assert!(matches!(
        outcome,
        PrepareInitialModelCallOutcome::WaitFailed(_)
    ));
    let snapshot = ProcessReadRepository::new(pool.clone())
        .read_transcript(session)
        .await?
        .expect("terminal session remains readable");
    let ProcessTurnState::FailedAfterCredentialWait {
        terminal_attempt,
        predecessor_call,
        provider_cause,
        ..
    } = snapshot.turns()[0].state()
    else {
        panic!("typed after-call wait failure")
    };
    assert_eq!(*predecessor_call, predecessor);
    assert_eq!(
        *provider_cause,
        ProcessProviderModelCallFailureCause::QuotaExhausted
    );
    assert_ne!(*terminal_attempt, wait.attempt());
    let terminal_calls: i64 =
        sqlx::query_scalar("SELECT count(*) FROM model_call WHERE turn_attempt_id = $1")
            .bind(terminal_attempt.into_uuid())
            .fetch_one(&pool)
            .await?;
    assert_eq!(terminal_calls, 0);
    let calls: i64 = sqlx::query_scalar("SELECT count(*) FROM model_call WHERE turn_id = $1")
        .bind(turn.into_uuid())
        .fetch_one(&pool)
        .await?;
    assert_eq!(calls, 1, "release never prepares the excluded fallback");
    let source: (String, Uuid) = sqlx::query_as("SELECT attempt.end_disposition, waiting.consumed_by_attempt_id FROM credential_availability_wait waiting JOIN turn_attempt attempt ON attempt.turn_attempt_id = waiting.wait_attempt_id WHERE waiting.wait_attempt_id = $1").bind(wait.attempt().into_uuid()).fetch_one(&pool).await?;
    assert_eq!(
        source,
        (
            "yielded_to_durable_wait".to_owned(),
            terminal_attempt.into_uuid()
        )
    );
    let exhaustion: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM credential_pool_terminal_exhaustion WHERE turn_id = $1",
    )
    .bind(turn.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        exhaustion, 0,
        "the predecessor provider cause is not pool exhaustion"
    );
    pool.close().await;
    drop(container);
    Ok(())
}
