//! Exhausted network retries park and resume from fresh capacity evidence.
use super::*;
use signalbox_domain::{ProviderRateLimitSnapshot, ProviderRateLimitWindow};
use signalbox_persistence::credential_capacity::retain_credential_capacity_probe;
use std::time::SystemTime;

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn every_network_exhausted_member_parks_until_a_fresh_observation()
-> Result<(), Box<dyn Error>> {
    // Synthetic disjoint identities for two pool members and their retained turn.
    const SEED: u128 = 0x6007_2000;
    const POOL: &str = "network-wait";
    const MEMBERS: &[&str] = &["network-first", "network-second"];
    let (container, pool, _) = migrated_postgres().await?;
    let stale = SystemTime::now();
    let (session, turn, repository) = active_credential_pool_fixture(
        &pool,
        SEED,
        POOL,
        MEMBERS,
        CredentialPoolRuntimeAction::Stay,
        CredentialPoolRuntimeAction::Stay,
    )
    .await?;
    let target =
        ResolvedProviderTarget::naming(ProviderModelIdentity::from_uuid(Uuid::from_u128(SEED + 4)));
    let mut repository = repository
        .with_same_credential_attempt_bound(Some(std::num::NonZeroUsize::MIN))
        .with_credential_pools(HashMap::from([(target, park_policy(POOL, MEMBERS))]));
    let (first_member, first_outcome) =
        fail_network_call(&pool, &mut repository, session, SEED + 100).await?;
    assert!(matches!(
        first_outcome,
        ModelCallObservationCommitOutcome::AvailabilitySuccessor(_)
    ));
    let (second_member, second_outcome) =
        fail_network_call(&pool, &mut repository, session, SEED + 200).await?;
    let ModelCallObservationCommitOutcome::CredentialWait(wait) = second_outcome else {
        panic!("all exhausted members must park")
    };
    assert_eq!(
        wait.cause(),
        CredentialAvailabilityWaitCause::NetworkUnavailable
    );
    assert_ne!(
        first_member, second_member,
        "the failed member rotates at its bound"
    );
    let profiles = MEMBERS
        .iter()
        .map(|profile| (*profile).to_owned())
        .collect::<Vec<_>>();
    assert_eq!(
        signalbox_persistence::credential_capacity::waiting_capacity_profiles(&pool, &profiles)
            .await?,
        profiles,
        "the existing capacity loop observes both network-excluded members"
    );
    let live = signalbox_persistence::session_live::SessionLiveRepository::new(pool.clone())
        .read_live_snapshot(session)
        .await?
        .expect("parked session live state");
    assert!(matches!(live.active.expect("retained active turn").state,
        signalbox_application::SessionLiveActiveState::AwaitingCredentialAvailability {
            attempt, cause: CredentialAvailabilityWaitCause::NetworkUnavailable,
        } if attempt == wait.attempt()));
    let snapshot = signalbox_persistence::process_read::ProcessReadRepository::new(pool.clone())
        .read_transcript(session)
        .await?
        .expect("parked session remains readable");
    assert!(matches!(snapshot.turns()[0].state(),
        signalbox_persistence::process_read::ProcessTurnState::ActiveAwaitingCredentialAvailability { wait: projected } if *projected == wait));
    for observation in [
        ProviderRateLimitSnapshot::new(stale, vec![ProviderRateLimitWindow::new(100, None, None)]),
        ProviderRateLimitSnapshot::new(SystemTime::now(), vec![]),
    ] {
        retain_credential_capacity_probe(&pool, &first_member, &observation).await?;
        assert!(
            matches!(prepare_wait_admission(&repository, session, SEED + 300).await?,
            PrepareInitialModelCallOutcome::CredentialWait(parked) if parked == wait)
        );
    }
    retain_credential_capacity_probe(
        &pool,
        &first_member,
        &ProviderRateLimitSnapshot::new(
            SystemTime::now(),
            vec![ProviderRateLimitWindow::new(100, None, None)],
        ),
    )
    .await?;
    let (left, right) = tokio::join!(
        prepare_wait_admission(&repository, session, SEED + 400),
        prepare_wait_admission(&repository, session, SEED + 500),
    );
    let outcomes = [left?, right?];
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| matches!(outcome, PrepareInitialModelCallOutcome::Checkpointed(_)))
            .count(),
        1
    );
    let calls: i64 = sqlx::query_scalar("SELECT count(*) FROM model_call WHERE turn_id = $1")
        .bind(turn.into_uuid())
        .fetch_one(&pool)
        .await?;
    assert_eq!(
        calls, 3,
        "two failed calls and one resumed call on the same turn"
    );
    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn non_park_pool_network_failure_terminalizes_at_the_retry_bound()
-> Result<(), Box<dyn Error>> {
    // One synthetic member with a one-call retry bound and fail exhaustion policy.
    const SEED: u128 = 0x6007_3000;
    let (container, pool, _) = migrated_postgres().await?;
    let (session, turn, repository) = active_credential_pool_fixture(
        &pool,
        SEED,
        "network-fail",
        &["failing-member"],
        CredentialPoolRuntimeAction::Stay,
        CredentialPoolRuntimeAction::Stay,
    )
    .await?;
    let mut repository =
        repository.with_same_credential_attempt_bound(Some(std::num::NonZeroUsize::MIN));
    let (authorized, _) = prepare_and_authorize_pool_call(&repository, session, SEED + 100).await?;
    let outcome = repository
        .commit_observation(
            session,
            authorized
                .observation_correlation()
                .bind_provider_failure_observation_with_retry_after(
                    ProviderModelCallFailureCause::ProviderInternal,
                    ProviderReportedTokenUsage::unreported(),
                    None,
                    false,
                ),
            signalbox_application::ModelCallTerminalIdentityCandidates::Availability {
                failed: FailedModelCallTurnIdentities::new(
                    SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(SEED + 120)),
                    ContextFrontierId::from_uuid(Uuid::from_u128(SEED + 121)),
                ),
                successor_attempt: TurnAttemptId::from_uuid(Uuid::from_u128(SEED + 122)),
            },
            |_| panic!("no steering in this fixture"),
        )
        .await?;
    assert!(matches!(
        outcome,
        Some(ModelCallObservationCommitOutcome::Terminal(_))
    ));
    let disposition: String = sqlx::query_scalar(
        "SELECT terminal_disposition_kind FROM turn_lifecycle WHERE turn_id = $1",
    )
    .bind(turn.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(disposition, "failed");
    let waits: i64 =
        sqlx::query_scalar("SELECT count(*) FROM credential_availability_wait WHERE turn_id = $1")
            .bind(turn.into_uuid())
            .fetch_one(&pool)
            .await?;
    assert_eq!(waits, 0);
    pool.close().await;
    drop(container);
    Ok(())
}

async fn fail_network_call(
    pool: &sqlx::PgPool,
    repository: &mut PostgresModelCallRepository,
    session: SessionId,
    seed: u128,
) -> Result<(String, ModelCallObservationCommitOutcome), Box<dyn Error>> {
    let (authorized, member) = prepare_and_authorize_pool_call(repository, session, seed).await?;
    let successor = TurnAttemptId::from_uuid(Uuid::from_u128(seed + 22));
    let outcome = repository
        .commit_observation(
            session,
            authorized
                .observation_correlation()
                .bind_provider_failure_observation_with_retry_after(
                    ProviderModelCallFailureCause::ProviderInternal,
                    ProviderReportedTokenUsage::unreported(),
                    None,
                    false,
                ),
            signalbox_application::ModelCallTerminalIdentityCandidates::Availability {
                failed: FailedModelCallTurnIdentities::new(
                    SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 20)),
                    ContextFrontierId::from_uuid(Uuid::from_u128(seed + 21)),
                ),
                successor_attempt: successor,
            },
            |_| panic!("no steering in this fixture"),
        )
        .await?
        .expect("fresh issued failure commits");
    authentication::wait_for_retry_deadline(pool, successor).await?;
    Ok((member, outcome))
}
