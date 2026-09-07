//! Frozen pool exhaustion records and their authenticated projections.
use super::*;
use signalbox_persistence::credential_pool_exhaustion as evidence;

async fn fail_before_call(
    repository: &PostgresModelCallRepository,
    session: SessionId,
    seed: u128,
) -> Result<(), Box<dyn Error>> {
    for _ in 0..2 {
        match repository
            .prepare_initial_call(
                session,
                ModelCallId::from_uuid(Uuid::from_u128(seed)),
                FailedModelCallTurnIdentities::new(
                    SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 1)),
                    ContextFrontierId::from_uuid(Uuid::from_u128(seed + 2)),
                ),
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 3)),
                |_| panic!("no steering in exhaustion fixture"),
            )
            .await?
        {
            PrepareInitialModelCallOutcome::PoolExhausted(_) => return Ok(()),
            PrepareInitialModelCallOutcome::Checkpointed(_) => {}
            other => panic!("excluded members cannot prepare a call: {other:?}"),
        }
    }
    panic!("exhaustion must commit");
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn pool_projection_freezes_generation_and_unprojected_action_evidence()
-> Result<(), Box<dyn Error>> {
    use signalbox_persistence::credential_exclusions::{
        self as exclusions, ClearCredentialExclusion, ClearCredentialExclusionOutcome,
        ClearCredentialExclusionResult,
    };
    use signalbox_persistence::outbox::{DispatchedOutboxEventKind, OutboxConsumerReader};
    for projected in [true, false] {
        let (container, pool, _) = migrated_postgres().await?;
        let (source_session, source_turn, source_repository) = active_credential_pool_fixture(
            &pool,
            0x4605_0000,
            "projection-pool",
            &["excluded-profile"],
            CredentialPoolRuntimeAction::SwitchNow,
            CredentialPoolRuntimeAction::Quarantine,
        )
        .await?;
        let (source, _) =
            prepare_and_authorize_pool_call(&source_repository, source_session, 0x4605_0100)
                .await?;
        let (session, turn, repository) = active_credential_pool_fixture(
            &pool,
            0x4605_0200,
            "projection-pool",
            &["excluded-profile"],
            CredentialPoolRuntimeAction::SwitchNow,
            CredentialPoolRuntimeAction::Quarantine,
        )
        .await?;
        if !projected {
            sqlx::query("ALTER TABLE credential_pool_member_action DISABLE TRIGGER credential_action_exclusion").execute(&pool).await?;
        }
        sqlx::query("INSERT INTO credential_pool_member_action (pool_name, credential_reference, action_kind, observed_session_id, observed_turn_id, observation_model_call_id, cause_kind) VALUES ('projection-pool','excluded-profile','quarantine',$1,$2,$3,'credential_rejected')")
            .bind(source_session.into_uuid()).bind(source_turn.into_uuid()).bind(source.observation_correlation().call().into_uuid()).execute(&pool).await?;
        if !projected {
            sqlx::query("ALTER TABLE credential_pool_member_action ENABLE TRIGGER credential_action_exclusion").execute(&pool).await?;
        }
        fail_before_call(&repository, session, 0x4605_0300).await?;
        let mut connection = pool.acquire().await?;
        let captured = evidence::load(&mut connection, session.into_uuid(), turn.into_uuid())
            .await?
            .expect("exhaustion evidence");
        assert_eq!(captured.policy_members, ["excluded-profile"]);
        assert_eq!(captured.members.len(), 1);
        let evidence::CredentialPoolExclusion::ProfileQuarantine { record_generation } =
            captured.members[0].exclusion
        else {
            panic!("quarantine evidence");
        };
        assert_eq!(record_generation.is_some(), projected);
        assert_eq!(captured.members[0].reset_at_unix_ms, None);
        assert_eq!(
            evidence::read_policy(
                &mut connection,
                session.into_uuid(),
                turn.into_uuid(),
                captured.pool_policy_id
            )
            .await?,
            Some(captured.policy_members.clone())
        );
        assert_eq!(
            evidence::read_policy(
                &mut connection,
                source_session.into_uuid(),
                turn.into_uuid(),
                captured.pool_policy_id
            )
            .await?,
            None
        );
        drop(connection);
        if projected {
            let target = exclusions::list(&pool, 100, None)
                .await?
                .exclusions
                .remove(0);
            assert_eq!(
                exclusions::clear(
                    &pool,
                    ClearCredentialExclusion {
                        command_id: DurableCommandId::from_uuid(Uuid::now_v7()),
                        target
                    }
                )
                .await?,
                ClearCredentialExclusionResult::Recorded(ClearCredentialExclusionOutcome::Cleared)
            );
        }
        let mut connection = pool.acquire().await?;
        assert_eq!(
            evidence::load(&mut connection, session.into_uuid(), turn.into_uuid()).await?,
            Some(captured.clone()),
            "later clearing cannot rewrite failure evidence"
        );
        drop(connection);
        let snapshot =
            signalbox_persistence::process_read::ProcessReadRepository::new(pool.clone())
                .read_transcript(session)
                .await?
                .expect("session snapshot");
        assert!(
            matches!(snapshot.turns()[0].state(), signalbox_persistence::process_read::ProcessTurnState::FailedCredentialPoolExhausted(value) if **value == captured)
        );
        let reader = OutboxConsumerReader::new(
            pool.clone(),
            signalbox_persistence::outbox::OutboxConsumer::RepoWatch,
        );
        let mut found = false;
        while let Some(event) = reader.read_next().await? {
            if let DispatchedOutboxEventKind::CredentialPoolExhausted(value) = event.kind() {
                assert_eq!(**value, captured);
                let address = signalbox_application::TimelineAddress::new(
                    std::num::NonZeroU64::new(event.sequence()).expect("durable event sequence"),
                );
                let detail =
                    signalbox_persistence::session_timeline::SessionTimelineRepository::new(
                        pool.clone(),
                    )
                    .read_item_details(
                        session,
                        address,
                        None,
                        signalbox_application::TimelineDetailLimits::new(1, 256)
                            .expect("bounded terminal detail"),
                    )
                    .await?
                    .expect("exhaustion timeline detail");
                assert_eq!(detail.items.len(), 1);
                assert_eq!(
                    detail.items[0].kind,
                    signalbox_application::SessionTimelineEventKind::TurnFailed
                );
                assert_eq!(
                    detail.items[0].body,
                    signalbox_application::SessionTimelineDetailBody::TurnLifecycle {
                        turn_id: turn,
                        lifecycle: signalbox_application::TimelineTurnLifecycleKind::Terminalized,
                        cause_code: "failed".to_owned(),
                    }
                );
                found = true;
            }
            reader.acknowledge(event.sequence()).await?;
        }
        assert!(found, "the typed live event shares the snapshot evidence");
        sqlx::query("ALTER TABLE credential_pool_exhaustion_member DISABLE TRIGGER credential_pool_exhaustion_member_immutable").execute(&pool).await?;
        sqlx::query("DELETE FROM credential_pool_exhaustion_member WHERE terminal_attempt_id = $1")
            .bind(captured.terminal_attempt_id)
            .execute(&pool)
            .await?;
        let mut connection = pool.acquire().await?;
        assert!(
            matches!(
                evidence::load(&mut connection, session.into_uuid(), turn.into_uuid()).await,
                Err(evidence::CredentialPoolEvidenceError::Corruption)
            ),
            "partial evidence must fail closed"
        );
        drop(connection);
        pool.close().await;
        drop(container);
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn pool_projection_prefers_quarantine_over_transient_exclusion() -> Result<(), Box<dyn Error>>
{
    use signalbox_persistence::credential_exclusions::{
        self as exclusions, ClearCredentialExclusion, ClearCredentialExclusionOutcome,
        ClearCredentialExclusionResult,
    };
    const SOURCE_SEED: u128 = 0x4605_1000;
    const TRANSIENT_SEED: u128 = 0x4605_2000;
    const QUARANTINE_SEED: u128 = 0x4605_3000;
    const POOL: &str = "transient-projection";
    const PROFILE: &str = "cooling-member";
    let (container, pool, _) = migrated_postgres().await?;
    let (source_session, _, source_repository) = active_credential_pool_fixture(
        &pool,
        SOURCE_SEED,
        POOL,
        &[PROFILE],
        CredentialPoolRuntimeAction::Stay,
        CredentialPoolRuntimeAction::Stay,
    )
    .await?;
    let (source, _) =
        prepare_and_authorize_pool_call(&source_repository, source_session, SOURCE_SEED + 100)
            .await?;
    let observation = source.observation_correlation().call().into_uuid();
    let reset_ms: i64 = sqlx::query_scalar("INSERT INTO credential_pool_transient_exclusion (observation_model_call_id, credential_reference, cause_kind, reset_at) VALUES ($1,$2,'overloaded',transaction_timestamp() + interval '1 hour') RETURNING floor(extract(epoch FROM reset_at) * 1000)::bigint")
        .bind(observation).bind(PROFILE).fetch_one(&pool).await?;
    let (transient_session, transient_turn, transient_repository) = active_credential_pool_fixture(
        &pool,
        TRANSIENT_SEED,
        POOL,
        &[PROFILE],
        CredentialPoolRuntimeAction::Stay,
        CredentialPoolRuntimeAction::Stay,
    )
    .await?;
    fail_before_call(
        &transient_repository,
        transient_session,
        TRANSIENT_SEED + 100,
    )
    .await?;
    let mut connection = pool.acquire().await?;
    let captured = evidence::load(
        &mut connection,
        transient_session.into_uuid(),
        transient_turn.into_uuid(),
    )
    .await?
    .expect("transient evidence");
    assert_eq!(
        captured.members[0].exclusion,
        evidence::CredentialPoolExclusion::TransientExclusion {
            observation_model_call_id: observation
        }
    );
    assert_eq!(captured.members[0].reset_at_unix_ms, Some(reset_ms));
    drop(connection);
    sqlx::query("ALTER TABLE credential_pool_exhaustion_member DISABLE TRIGGER credential_pool_exhaustion_member_immutable").execute(&pool).await?;
    for forged_reset in [reset_ms - 1, reset_ms + 1] {
        sqlx::query("UPDATE credential_pool_exhaustion_member SET evidence = jsonb_set(evidence, '{reset_at_unix_ms}', to_jsonb($2::bigint)) WHERE terminal_attempt_id = $1")
            .bind(captured.terminal_attempt_id).bind(forged_reset).execute(&pool).await?;
        let mut connection = pool.acquire().await?;
        assert!(
            matches!(
                evidence::load(
                    &mut connection,
                    transient_session.into_uuid(),
                    transient_turn.into_uuid()
                )
                .await,
                Err(evidence::CredentialPoolEvidenceError::Corruption)
            ),
            "forged reset {forged_reset} must fail closed"
        );
    }
    sqlx::query("UPDATE credential_pool_exhaustion_member SET evidence = jsonb_set(evidence, '{reset_at_unix_ms}', to_jsonb($2::bigint)) WHERE terminal_attempt_id = $1")
        .bind(captured.terminal_attempt_id).bind(reset_ms).execute(&pool).await?;
    sqlx::query("ALTER TABLE credential_pool_exhaustion_member ENABLE TRIGGER credential_pool_exhaustion_member_immutable").execute(&pool).await?;
    let generation: i64 = sqlx::query_scalar("INSERT INTO credential_exclusion (kind,profile,origin) VALUES ('profile_quarantine',$1,'codex_home') RETURNING record_generation").bind(PROFILE).fetch_one(&pool).await?;
    let (session, turn, repository) = active_credential_pool_fixture(
        &pool,
        QUARANTINE_SEED,
        POOL,
        &[PROFILE],
        CredentialPoolRuntimeAction::Stay,
        CredentialPoolRuntimeAction::Stay,
    )
    .await?;
    fail_before_call(&repository, session, QUARANTINE_SEED + 100).await?;
    let mut connection = pool.acquire().await?;
    let widest = evidence::load(&mut connection, session.into_uuid(), turn.into_uuid())
        .await?
        .expect("widest evidence");
    assert_eq!(
        widest.members[0].exclusion,
        evidence::CredentialPoolExclusion::ProfileQuarantine {
            record_generation: Some(u64::try_from(generation)?)
        }
    );
    assert_eq!(widest.members[0].reset_at_unix_ms, None);
    assert_eq!(
        evidence::load(
            &mut connection,
            transient_session.into_uuid(),
            transient_turn.into_uuid()
        )
        .await?,
        Some(captured)
    );
    drop(connection);
    let quarantine = exclusions::list(&pool, 100, None)
        .await?
        .exclusions
        .remove(0);
    assert_eq!(
        exclusions::clear(
            &pool,
            ClearCredentialExclusion {
                command_id: DurableCommandId::from_uuid(Uuid::now_v7()),
                target: quarantine,
            }
        )
        .await?,
        ClearCredentialExclusionResult::Recorded(ClearCredentialExclusionOutcome::Cleared)
    );
    let mut connection = pool.acquire().await?;
    assert_eq!(
        evidence::load(&mut connection, session.into_uuid(), turn.into_uuid()).await?,
        Some(widest.clone()),
        "a later clear preserves the selected quarantine"
    );
    drop(connection);
    sqlx::query("ALTER TABLE credential_pool_exhaustion_member DISABLE TRIGGER credential_pool_exhaustion_member_immutable").execute(&pool).await?;
    sqlx::query("UPDATE credential_pool_exhaustion_member SET evidence = jsonb_build_object('profile',profile,'reset_at_unix_ms',$2::bigint,'exclusion',jsonb_build_object('kind','transient_exclusion','observation_model_call_id',$3::uuid)) WHERE terminal_attempt_id = $1")
        .bind(widest.terminal_attempt_id).bind(reset_ms).bind(observation).execute(&pool).await?;
    let mut connection = pool.acquire().await?;
    assert!(
        matches!(
            evidence::load(&mut connection, session.into_uuid(), turn.into_uuid()).await,
            Err(evidence::CredentialPoolEvidenceError::Corruption)
        ),
        "a transient source cannot replace a quarantine active at the captured ceiling"
    );
    drop(connection);
    let (after_clear_session, after_clear_turn, after_clear_repository) =
        active_credential_pool_fixture(
            &pool,
            QUARANTINE_SEED + 1000,
            POOL,
            &[PROFILE],
            CredentialPoolRuntimeAction::Stay,
            CredentialPoolRuntimeAction::Stay,
        )
        .await?;
    fail_before_call(
        &after_clear_repository,
        after_clear_session,
        QUARANTINE_SEED + 1100,
    )
    .await?;
    let mut connection = pool.acquire().await?;
    let after_clear = evidence::load(
        &mut connection,
        after_clear_session.into_uuid(),
        after_clear_turn.into_uuid(),
    )
    .await?
    .expect("transient evidence after quarantine clear");
    assert_eq!(
        after_clear.members[0].exclusion,
        evidence::CredentialPoolExclusion::TransientExclusion {
            observation_model_call_id: observation,
        }
    );
    drop(connection);
    let cleared_evidence = serde_json::json!({"profile": PROFILE, "reset_at_unix_ms": null,
        "exclusion": {"kind": "profile_quarantine", "record_generation": generation}});
    sqlx::query(
        "UPDATE credential_pool_exhaustion_member SET evidence = $2 WHERE terminal_attempt_id = $1",
    )
    .bind(after_clear.terminal_attempt_id)
    .bind(cleared_evidence)
    .execute(&pool)
    .await?;
    let mut connection = pool.acquire().await?;
    assert!(
        matches!(
            evidence::load(
                &mut connection,
                after_clear_session.into_uuid(),
                after_clear_turn.into_uuid()
            )
            .await,
            Err(evidence::CredentialPoolEvidenceError::Corruption)
        ),
        "a quarantine cleared before observation cannot be restored as active"
    );
    drop(connection);
    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn pool_projection_records_the_observed_headroom_reserve() -> Result<(), Box<dyn Error>> {
    use signalbox_domain::{ProviderRateLimitSnapshot, ProviderRateLimitWindow};
    use signalbox_persistence::model_execution::CredentialPoolRuntimeExhaustion;
    use std::time::SystemTime;
    const SOURCE_SEED: u128 = 0x4605_4000;
    const TARGET_SEED: u128 = 0x4605_5000;
    const POOL: &str = "headroom-projection";
    const PROFILE: &str = "capacity-member";
    let (container, pool, _) = migrated_postgres().await?;
    let (source_session, _, source_repository) = active_credential_pool_fixture(
        &pool,
        SOURCE_SEED,
        POOL,
        &[PROFILE],
        CredentialPoolRuntimeAction::Stay,
        CredentialPoolRuntimeAction::Stay,
    )
    .await?;
    let (source, _) =
        prepare_and_authorize_pool_call(&source_repository, source_session, SOURCE_SEED + 100)
            .await?;
    let now = SystemTime::now();
    let reset = now + Duration::from_secs(3600);
    let reset_ms = i64::try_from(reset.duration_since(SystemTime::UNIX_EPOCH)?.as_millis())?;
    source_repository
        .apply_terminal_observation(
            source_session,
            source
                .observation_correlation()
                .bind_terminal_observation(ModelCallTerminalObservation::KnownFailed)
                .with_rate_limits(Some(ProviderRateLimitSnapshot::new(
                    now,
                    vec![
                        ProviderRateLimitWindow::new(10, None, Some(reset)),
                        ProviderRateLimitWindow::new(
                            10,
                            None,
                            Some(now + Duration::from_secs(1800)),
                        ),
                        ProviderRateLimitWindow::new(
                            11,
                            None,
                            Some(now + Duration::from_secs(7200)),
                        ),
                    ],
                ))),
            ModelCallTerminalIdentities::Failed(FailedModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::now_v7()),
                ContextFrontierId::from_uuid(Uuid::now_v7()),
            )),
            |_| panic!("fixture has no steering"),
        )
        .await?;
    let (session, turn, repository) = active_credential_pool_fixture(
        &pool,
        TARGET_SEED,
        POOL,
        &[PROFILE],
        CredentialPoolRuntimeAction::Stay,
        CredentialPoolRuntimeAction::Stay,
    )
    .await?;
    let target = ResolvedProviderTarget::naming(ProviderModelIdentity::from_uuid(Uuid::from_u128(
        TARGET_SEED + 4,
    )));
    let policy = CredentialPoolRuntimePolicy::new(
        POOL,
        vec![
            CredentialPoolRuntimeMember::new(PROFILE, nonzero_priority(1))
                .with_headroom_reserve(Some(10)),
        ],
        CredentialPoolRuntimeExhaustion::Fail,
        CredentialPoolRuntimeAction::Stay,
        CredentialPoolRuntimeAction::Stay,
        CredentialPoolRuntimeAction::Stay,
        CredentialPoolRuntimeAction::Stay,
    );
    let repository = repository.with_credential_pools(HashMap::from([(target, policy)]));
    fail_before_call(&repository, session, TARGET_SEED + 100).await?;
    let mut connection = pool.acquire().await?;
    let captured = evidence::load(&mut connection, session.into_uuid(), turn.into_uuid())
        .await?
        .expect("headroom evidence");
    assert_eq!(
        captured.members[0].exclusion,
        evidence::CredentialPoolExclusion::HeadroomReserve {
            observed_headroom_percent: 10,
            reserve_percent: 10
        }
    );
    assert_eq!(captured.members[0].reset_at_unix_ms, Some(reset_ms));
    drop(connection);
    sqlx::query("ALTER TABLE credential_pool_exhaustion_member DISABLE TRIGGER credential_pool_exhaustion_member_immutable").execute(&pool).await?;
    for forged_reset in [reset_ms - 1, reset_ms + 1] {
        sqlx::query("UPDATE credential_pool_exhaustion_member SET evidence = jsonb_set(evidence, '{reset_at_unix_ms}', to_jsonb($2::bigint)) WHERE terminal_attempt_id = $1")
            .bind(captured.terminal_attempt_id).bind(forged_reset).execute(&pool).await?;
        let mut connection = pool.acquire().await?;
        assert!(
            matches!(
                evidence::load(&mut connection, session.into_uuid(), turn.into_uuid()).await,
                Err(evidence::CredentialPoolEvidenceError::Corruption)
            ),
            "forged reset {forged_reset} must fail closed"
        );
    }
    sqlx::query("UPDATE credential_pool_exhaustion_member SET evidence = jsonb_set(evidence, '{reset_at_unix_ms}', to_jsonb($2::bigint)) WHERE terminal_attempt_id = $1")
        .bind(captured.terminal_attempt_id).bind(reset_ms).execute(&pool).await?;
    for observed in [9_i64, 11] {
        sqlx::query("UPDATE credential_pool_exhaustion_member SET evidence = jsonb_set(evidence, '{exclusion,observed_headroom_percent}', to_jsonb($2::bigint)) WHERE terminal_attempt_id = $1")
            .bind(captured.terminal_attempt_id).bind(observed).execute(&pool).await?;
        let mut connection = pool.acquire().await?;
        assert!(
            matches!(
                evidence::load(&mut connection, session.into_uuid(), turn.into_uuid()).await,
                Err(evidence::CredentialPoolEvidenceError::Corruption)
            ),
            "forged observed headroom {observed}"
        );
    }
    sqlx::query("UPDATE credential_pool_exhaustion_member SET evidence = jsonb_set(evidence, '{exclusion,observed_headroom_percent}', '10'::jsonb) WHERE terminal_attempt_id = $1")
        .bind(captured.terminal_attempt_id).execute(&pool).await?;
    sqlx::query("ALTER TABLE credential_pool_exhaustion_member ENABLE TRIGGER credential_pool_exhaustion_member_immutable").execute(&pool).await?;
    sqlx::query("UPDATE credential_rate_limit_snapshot SET windows = '[]'::jsonb WHERE credential_reference = $1")
        .bind(PROFILE).execute(&pool).await?;
    let mut connection = pool.acquire().await?;
    assert_eq!(
        evidence::load(&mut connection, session.into_uuid(), turn.into_uuid()).await?,
        Some(captured),
        "later capacity observations cannot rewrite the captured reset"
    );
    drop(connection);
    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn pool_projection_rejects_foreign_and_stale_generations() -> Result<(), Box<dyn Error>> {
    const SEED: u128 = 0x4605_6000;
    const PROFILE: &str = "generation-member";
    let (container, pool, _) = migrated_postgres().await?;
    let stale: i64 = sqlx::query_scalar("INSERT INTO credential_exclusion (kind,profile,origin) VALUES ('profile_quarantine',$1,'codex_home') RETURNING record_generation").bind(PROFILE).fetch_one(&pool).await?;
    let foreign: i64 = sqlx::query_scalar("INSERT INTO credential_exclusion (kind,profile,origin) VALUES ('profile_quarantine','another-member','codex_home') RETURNING record_generation").fetch_one(&pool).await?;
    let current: i64 = sqlx::query_scalar("INSERT INTO credential_exclusion (kind,profile,origin) VALUES ('profile_quarantine',$1,'codex_home') RETURNING record_generation").bind(PROFILE).fetch_one(&pool).await?;
    let (session, turn, repository) = active_credential_pool_fixture(
        &pool,
        SEED,
        "generation-projection",
        &[PROFILE],
        CredentialPoolRuntimeAction::Stay,
        CredentialPoolRuntimeAction::Stay,
    )
    .await?;
    fail_before_call(&repository, session, SEED + 100).await?;
    let mut connection = pool.acquire().await?;
    let captured = evidence::load(&mut connection, session.into_uuid(), turn.into_uuid())
        .await?
        .expect("current generation evidence");
    assert_eq!(
        captured.members[0].exclusion,
        evidence::CredentialPoolExclusion::ProfileQuarantine {
            record_generation: Some(u64::try_from(current)?)
        }
    );
    drop(connection);
    sqlx::query("ALTER TABLE credential_pool_exhaustion_member DISABLE TRIGGER credential_pool_exhaustion_member_immutable").execute(&pool).await?;
    for (case, generation) in [("stale", stale), ("foreign", foreign)] {
        sqlx::query("UPDATE credential_pool_exhaustion_member SET evidence = jsonb_set(evidence, '{exclusion,record_generation}', to_jsonb($2::bigint)) WHERE terminal_attempt_id = $1").bind(captured.terminal_attempt_id).bind(generation).execute(&pool).await?;
        let mut connection = pool.acquire().await?;
        assert!(
            matches!(
                evidence::load(&mut connection, session.into_uuid(), turn.into_uuid()).await,
                Err(evidence::CredentialPoolEvidenceError::Corruption)
            ),
            "{case} evidence must fail closed"
        );
    }
    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn pool_projection_reconstructs_the_widest_applicable_source() -> Result<(), Box<dyn Error>> {
    use signalbox_persistence::model_execution::{
        CredentialPoolRuntimeExhaustion, CredentialPoolRuntimeTieBreak,
    };
    for (action, chain, expected_kind, expected_transient_result) in [
        (
            Some("avoid_new_sessions"),
            true,
            "membership_exclusion",
            (false, true),
        ),
        (
            Some("switch_next_turn"),
            true,
            "session_displacement",
            (false, true),
        ),
        (None, true, "chain_exclusion", (false, true)),
        (None, false, "transient_exclusion", (true, false)),
    ] {
        // Arbitrary identities; all sources apply to one member of the second turn.
        const SEED: u128 = 0x4606_0000;
        const PROFILE: &str = "reconstructed-member";
        const POOL: &str = "reconstruction-pool";
        let (container, pool, _) = migrated_postgres().await?;
        let (session, first_turn, repository) = active_credential_pool_fixture(
            &pool,
            SEED,
            POOL,
            &[PROFILE],
            CredentialPoolRuntimeAction::Stay,
            CredentialPoolRuntimeAction::Stay,
        )
        .await?;
        let target = ResolvedProviderTarget::naming(ProviderModelIdentity::from_uuid(
            Uuid::from_u128(SEED + 4),
        ));
        let policy = CredentialPoolRuntimePolicy::new(
            POOL,
            vec![CredentialPoolRuntimeMember::new(
                PROFILE,
                nonzero_priority(1),
            )],
            CredentialPoolRuntimeExhaustion::Fail,
            CredentialPoolRuntimeAction::Stay,
            CredentialPoolRuntimeAction::Stay,
            CredentialPoolRuntimeAction::Stay,
            CredentialPoolRuntimeAction::Stay,
        )
        .with_capacity_policy(
            CredentialPoolRuntimeTieBreak::FirstListed,
            Some(10),
            CredentialPoolRuntimeAction::Stay,
        );
        let repository = repository.with_credential_pools(HashMap::from([(target, policy)]));
        let (call, _) = prepare_and_authorize_pool_call(&repository, session, SEED + 100).await?;
        let observation = call.observation_correlation().call().into_uuid();
        repository
            .apply_terminal_observation(
                session,
                call.observation_correlation()
                    .bind_terminal_observation(ModelCallTerminalObservation::KnownFailed),
                ModelCallTerminalIdentities::Failed(FailedModelCallTurnIdentities::new(
                    SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(SEED + 200)),
                    ContextFrontierId::from_uuid(Uuid::from_u128(SEED + 201)),
                )),
                |_| panic!("fixture has no steering"),
            )
            .await?;
        let turn = TurnId::from_uuid(Uuid::from_u128(SEED + 300));
        SubmitInputRepository::new(pool.clone())
            .handle(
                start_input(
                    SEED + 301,
                    SEED + 1,
                    "observe reconstructed exclusions",
                    1,
                    ModelSelectionOverride::UseSessionDefault,
                ),
                AcceptedInputId::from_uuid(Uuid::from_u128(SEED + 302)),
                Some(turn),
            )
            .await?;
        activate_earliest_queued_turn(
            &pool,
            EarliestQueuedTurnActivation {
                session: session.into_uuid(),
                origin_entry: Uuid::from_u128(SEED + 303),
                starting_frontier: Uuid::from_u128(SEED + 304),
                initial_attempt: Uuid::from_u128(SEED + 305),
            },
        )
        .await?;
        sqlx::query("INSERT INTO credential_pool_member_action (pool_name, credential_reference, action_kind, observed_session_id, observed_turn_id, observation_model_call_id, cause_kind) SELECT $1,$2,$3,$4,$5,$6,'credential_rejected' WHERE $3::text IS NOT NULL")
            .bind(POOL).bind(PROFILE).bind(action).bind(session.into_uuid()).bind(first_turn.into_uuid()).bind(observation).execute(&pool).await?;
        sqlx::query("INSERT INTO credential_pool_chain_exclusion (session_id,turn_id,credential_reference,predecessor_model_call_id,cause_kind) SELECT $1,$2,$3,$4,'overloaded' WHERE $5")
            .bind(session.into_uuid()).bind(turn.into_uuid()).bind(PROFILE).bind(observation).bind(chain).execute(&pool).await?;
        let reset_ms: i64 = sqlx::query_scalar("INSERT INTO credential_pool_transient_exclusion (observation_model_call_id,credential_reference,cause_kind,reset_at) VALUES ($1,$2,'overloaded',transaction_timestamp() + interval '1 hour') RETURNING floor(extract(epoch FROM reset_at) * 1000)::bigint")
            .bind(observation).bind(PROFILE).fetch_one(&pool).await?;
        fail_before_call(&repository, session, SEED + 400).await?;
        let mut connection = pool.acquire().await?;
        let captured = evidence::load(&mut connection, session.into_uuid(), turn.into_uuid())
            .await?
            .expect("reconstructed evidence");
        assert_eq!(
            serde_json::to_value(&captured.members[0].exclusion)?["kind"],
            expected_kind
        );
        drop(connection);
        sqlx::query("ALTER TABLE credential_pool_exhaustion_member DISABLE TRIGGER credential_pool_exhaustion_member_immutable").execute(&pool).await?;
        // The forged headroom arm has a policy reserve but no captured capacity windows.
        let forged = serde_json::json!({"profile": PROFILE, "reset_at_unix_ms": reset_ms,
            "exclusion": {"kind": "headroom_reserve", "observed_headroom_percent": 0, "reserve_percent": 10}});
        sqlx::query("UPDATE credential_pool_exhaustion_member SET evidence = $2, action_id = NULL WHERE terminal_attempt_id = $1")
            .bind(captured.terminal_attempt_id).bind(forged).execute(&pool).await?;
        let mut connection = pool.acquire().await?;
        assert!(
            matches!(
                evidence::load(&mut connection, session.into_uuid(), turn.into_uuid()).await,
                Err(evidence::CredentialPoolEvidenceError::Corruption)
            ),
            "fabricated headroom after {expected_kind}"
        );
        drop(connection);
        // A valid transient source cannot replace any earlier arm; with no earlier arm it must still load.
        let forged = serde_json::json!({"profile": PROFILE, "reset_at_unix_ms": reset_ms,
            "exclusion": {"kind": "transient_exclusion", "observation_model_call_id": observation.to_string()}});
        sqlx::query("UPDATE credential_pool_exhaustion_member SET evidence = $2 WHERE terminal_attempt_id = $1")
            .bind(captured.terminal_attempt_id).bind(forged).execute(&pool).await?;
        let mut connection = pool.acquire().await?;
        let result = evidence::load(&mut connection, session.into_uuid(), turn.into_uuid()).await;
        assert_eq!(
            (
                matches!(result, Ok(Some(_))),
                matches!(
                    result,
                    Err(evidence::CredentialPoolEvidenceError::Corruption)
                )
            ),
            expected_transient_result,
            "transient replacement after {expected_kind}"
        );
        drop(connection);
        pool.close().await;
        drop(container);
    }
    Ok(())
}
