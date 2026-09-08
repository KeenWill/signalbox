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
    exercise_wait(false).await
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn credential_pool_wait_stop_consumes_the_wait_and_cancels_a_fresh_attempt()
-> Result<(), Box<dyn Error>> {
    exercise_wait(true).await
}

async fn exercise_wait(stop: bool) -> Result<(), Box<dyn Error>> {
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
    let mut repository =
        repository.with_credential_pools(HashMap::from([(target, park_policy(POOL, &[MEMBER]))]));
    let PrepareInitialModelCallOutcome::CredentialWait(wait) =
        prepare_wait_admission(&repository, session, WAITER + 100).await?
    else {
        panic!("exhaustion must park")
    };
    assert_eq!(wait.cause(), CredentialAvailabilityWaitCause::Exhausted);
    let snapshot = signalbox_persistence::process_read::ProcessReadRepository::new(pool.clone())
        .read_transcript(session)
        .await?
        .expect("parked session stays readable");
    assert!(
        matches!(snapshot.turns()[0].state(), signalbox_persistence::process_read::ProcessTurnState::ActiveAwaitingCredentialAvailability { wait: projected } if *projected == wait)
    );
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
    if stop {
        let outcome = SubmitInputRepository::new(pool.clone())
            .handle(
                input_with_delivery(
                    WAITER + 130,
                    WAITER + 1,
                    "stop credential wait",
                    DeliveryRequest::Interrupt {
                        expected_active_turn: turn,
                        descendant_scope: DescendantTerminationScope::ParentAlone,
                        configuration: input_choices(1, ModelSelectionOverride::UseSessionDefault),
                    },
                ),
                AcceptedInputId::from_uuid(Uuid::from_u128(WAITER + 131)),
                Some(TurnId::from_uuid(Uuid::from_u128(WAITER + 132))),
            )
            .await?;
        assert!(matches!(
            outcome,
            SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Applied(_))
        ));
        let ended: (String, String, String, String, Uuid) = sqlx::query_as("SELECT lifecycle.terminal_disposition_kind, attempt.end_variant, attempt.end_disposition, source.end_disposition, attempt.continued_from_attempt_id FROM turn_lifecycle lifecycle JOIN turn_attempt attempt ON attempt.turn_attempt_id = lifecycle.terminal_attempt_id JOIN turn_attempt source ON source.turn_attempt_id = attempt.continued_from_attempt_id WHERE lifecycle.turn_id = $1")
            .bind(turn.into_uuid()).fetch_one(&pool).await?;
        assert_eq!(
            ended,
            (
                "cancelled".to_owned(),
                "after_cancellation".to_owned(),
                "cancelled".to_owned(),
                "yielded_to_durable_wait".to_owned(),
                wait.attempt().into_uuid()
            )
        );
        let cancelled_entries: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM semantic_transcript_entry WHERE cancelled_turn_id = $1",
        )
        .bind(turn.into_uuid())
        .fetch_one(&pool)
        .await?;
        assert_eq!(cancelled_entries, 1);
        pool.close().await;
        drop(container);
        return Ok(());
    }
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
    let AuthorizeModelCallOutcome::Authorized(authorized) =
        repository.authorize_send(session, call).await?
    else {
        panic!("released call must authorize")
    };
    let completed = repository
        .commit_observation(
            session,
            authorized
                .observation_correlation()
                .bind_terminal_observation(ModelCallTerminalObservation::Completed {
                    assistant_text: vec![
                        AssistantText::try_new("resumed reply".to_owned()).unwrap(),
                    ],
                }),
            signalbox_application::ModelCallTerminalIdentityCandidates::Exact(
                ModelCallTerminalIdentities::Completed(CompletedModelCallIdentities::new(
                    vec![SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(
                        WAITER + 140,
                    ))],
                    SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(WAITER + 141)),
                    ContextFrontierId::from_uuid(Uuid::from_u128(WAITER + 142)),
                )),
            ),
            |_| panic!("no pending steering in this fixture"),
        )
        .await?;
    assert!(matches!(
        completed,
        Some(ModelCallObservationCommitOutcome::Terminal(_))
    ));
    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn credential_pool_wait_retains_the_failed_predecessor_and_never_readmits_it()
-> Result<(), Box<dyn Error>> {
    const SOURCE: u128 = 0x6000_3000;
    const WAITER: u128 = 0x6000_4000;
    const POOL: &str = "post-failure-wait";
    let (container, pool, _) = migrated_postgres().await?;
    let (source_session, _, source_repository) = active_credential_pool_fixture(
        &pool,
        SOURCE,
        POOL,
        &["member-b"],
        CredentialPoolRuntimeAction::Stay,
        CredentialPoolRuntimeAction::Stay,
    )
    .await?;
    let (source, _) =
        prepare_and_authorize_pool_call(&source_repository, source_session, SOURCE + 100).await?;
    let observation = source.observation_correlation().call().into_uuid();
    sqlx::query("INSERT INTO credential_pool_transient_exclusion (observation_model_call_id, credential_reference, cause_kind, reset_at) VALUES ($1,'member-b','overloaded',transaction_timestamp() + interval '1 hour')")
        .bind(observation).execute(&pool).await?;
    let (session, _, repository) = active_credential_pool_fixture(
        &pool,
        WAITER,
        POOL,
        &["member-a", "member-b"],
        CredentialPoolRuntimeAction::SwitchNow,
        CredentialPoolRuntimeAction::Quarantine,
    )
    .await?;
    let target = ResolvedProviderTarget::naming(ProviderModelIdentity::from_uuid(Uuid::from_u128(
        WAITER + 4,
    )));
    let mut repository = repository.with_credential_pools(HashMap::from([(
        target,
        park_policy(POOL, &["member-a", "member-b"]),
    )]));
    let (first, reference) =
        prepare_and_authorize_pool_call(&repository, session, WAITER + 100).await?;
    assert_eq!(reference, "member-a");
    let predecessor = first.observation_correlation().call();
    let parked = repository
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
    let Some(ModelCallObservationCommitOutcome::CredentialWait(wait)) = parked else {
        panic!("the remaining member can become admissible at its reset")
    };
    let proof: (Uuid, bool) = sqlx::query_as("SELECT predecessor_model_call_id, predecessor_non_acceptance_proven FROM credential_availability_wait WHERE wait_attempt_id = $1")
        .bind(wait.attempt().into_uuid()).fetch_one(&pool).await?;
    assert_eq!(proof, (predecessor.into_uuid(), true));
    sqlx::query("UPDATE credential_pool_transient_exclusion SET reset_at = transaction_timestamp() WHERE observation_model_call_id = $1").bind(observation).execute(&pool).await?;
    sqlx::query(
        "UPDATE credential_availability_wait SET eligible = true WHERE wait_attempt_id = $1",
    )
    .bind(wait.attempt().into_uuid())
    .execute(&pool)
    .await?;
    let PrepareInitialModelCallOutcome::Checkpointed(call) =
        prepare_wait_admission(&repository, session, WAITER + 130).await?
    else {
        panic!("the remaining member must resume")
    };
    let reference: String =
        sqlx::query_scalar("SELECT credential_reference FROM model_call WHERE model_call_id = $1")
            .bind(call.into_uuid())
            .fetch_one(&pool)
            .await?;
    assert_eq!(reference, "member-b");
    let AuthorizeModelCallOutcome::Authorized(authorized) =
        repository.authorize_send(session, call).await?
    else {
        panic!("released fallback must authorize")
    };
    let terminal = repository
        .commit_observation(
            session,
            authorized
                .observation_correlation()
                .bind_provider_failure_observation_with_retry_after(
                    ProviderModelCallFailureCause::QuotaExhausted,
                    ProviderReportedTokenUsage::unreported(),
                    None,
                    true,
                ),
            signalbox_application::ModelCallTerminalIdentityCandidates::Availability {
                failed: FailedModelCallTurnIdentities::new(
                    SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(WAITER + 140)),
                    ContextFrontierId::from_uuid(Uuid::from_u128(WAITER + 141)),
                ),
                successor_attempt: TurnAttemptId::from_uuid(Uuid::from_u128(WAITER + 142)),
            },
            |_| panic!("no steering in this fixture"),
        )
        .await?;
    assert!(matches!(
        terminal,
        Some(ModelCallObservationCommitOutcome::PoolExhausted(
            signalbox_application::CredentialPoolExhaustedOutcome::AfterCall { .. }
        ))
    ));

    pool.close().await;
    drop(container);
    Ok(())
}

#[path = "credential_wait_wakes.rs"]
mod wakes;
