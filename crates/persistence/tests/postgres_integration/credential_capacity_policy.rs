//! Capacity selection and actions use persisted observations and frozen policy.

use crate::*;
use signalbox_domain::{ProviderRateLimitSnapshot, ProviderRateLimitWindow};
use signalbox_persistence::model_execution::{
    CredentialPoolRuntimeExhaustion, CredentialPoolRuntimeTieBreak,
};
use std::{
    collections::HashMap,
    num::NonZeroU32,
    time::{Duration, SystemTime},
};

const POOL: &str = "capacity-policy-fixture";
const FIRST: &str = "first-capacity-member";
const SECOND: &str = "second-capacity-member";

fn member(reference: &str, priority: u32) -> CredentialPoolRuntimeMember {
    CredentialPoolRuntimeMember::new(
        reference,
        NonZeroU32::new(priority).expect("nonzero priority"),
    )
}

fn policy(members: Vec<CredentialPoolRuntimeMember>) -> CredentialPoolRuntimePolicy {
    CredentialPoolRuntimePolicy::new(
        POOL,
        members,
        CredentialPoolRuntimeExhaustion::Fail,
        CredentialPoolRuntimeAction::Stay,
        CredentialPoolRuntimeAction::Stay,
        CredentialPoolRuntimeAction::Stay,
        CredentialPoolRuntimeAction::Stay,
    )
}

struct CapacityCall {
    session: SessionId,
    call: ModelCallId,
    repository: PostgresModelCallRepository,
    reference: String,
    terminal: FailedModelCallTurnIdentities,
}

/// Creates distinct session and call identities from an arbitrary fixture seed.
async fn prepare_capacity_call(
    pool: &PgPool,
    seed: u128,
    policy: CredentialPoolRuntimePolicy,
) -> Result<CapacityCall, Box<dyn Error>> {
    let session = SessionId::from_uuid(Uuid::from_u128(seed + 1));
    let turn = TurnId::from_uuid(Uuid::from_u128(seed + 2));
    let selection = DirectModelSelection::from_uuid(Uuid::from_u128(seed + 3));
    let target =
        ResolvedProviderTarget::naming(ProviderModelIdentity::from_uuid(Uuid::from_u128(seed + 4)));
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(prepared(
            seed + 5,
            seed + 1,
            ModelSelectionRequest::Direct(selection),
        ))
        .await?;
    SubmitInputRepository::new(pool.clone())
        .handle(
            start_input(
                seed + 6,
                seed + 1,
                "serialize shared locks",
                1,
                ModelSelectionOverride::UseSessionDefault,
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(seed + 7)),
            Some(turn),
        )
        .await?;
    activate_earliest_queued_turn(
        pool,
        EarliestQueuedTurnActivation {
            session: session.into_uuid(),
            origin_entry: Uuid::from_u128(seed + 8),
            starting_frontier: Uuid::from_u128(seed + 9),
            initial_attempt: Uuid::from_u128(seed + 10),
        },
    )
    .await?;
    let targets =
        ModelTargetCatalog::try_from_definitions([ModelTargetDefinition::new(selection, target)])
            .expect("one pool fixture target forms a catalog");
    let repository =
        PostgresModelCallRepository::new(pool.clone(), targets, model_credential_reference())
            .with_credential_pools(HashMap::from([(target, policy)]));
    let call = ModelCallId::from_uuid(Uuid::from_u128(seed + 11));
    let prepare = || {
        repository.prepare_initial_call(
            session,
            call,
            FailedModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 12)),
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 13)),
            ),
            ContextFrontierId::from_uuid(Uuid::from_u128(seed + 14)),
            |_| panic!("capacity fixture has no steering"),
        )
    };
    assert!(matches!(
        prepare().await?,
        PrepareInitialModelCallOutcome::Checkpointed(_)
    ));
    let PrepareInitialModelCallOutcome::Ready {
        credential_reference,
        ..
    } = prepare().await?
    else {
        panic!("checkpointed capacity call must become ready");
    };
    Ok(CapacityCall {
        session,
        call,
        repository,
        reference: credential_reference.as_str().to_owned(),
        terminal: FailedModelCallTurnIdentities::new(
            SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 17)),
            ContextFrontierId::from_uuid(Uuid::from_u128(seed + 18)),
        ),
    })
}

fn snapshot(primary: i64, secondary: i64) -> ProviderRateLimitSnapshot {
    let now = SystemTime::now();
    let reset = Some(now + Duration::from_secs(3600));
    ProviderRateLimitSnapshot::new(
        now,
        vec![
            ProviderRateLimitWindow::new(primary, Some(Duration::from_secs(18000)), reset),
            ProviderRateLimitWindow::new(secondary, Some(Duration::from_secs(604800)), reset),
        ],
    )
}

async fn observe_capacity(
    call: CapacityCall,
    snapshot: ProviderRateLimitSnapshot,
) -> Result<(), Box<dyn Error>> {
    let AuthorizeModelCallOutcome::Authorized(authorized) = call
        .repository
        .authorize_send(call.session, call.call)
        .await?
    else {
        panic!("prepared capacity call authorizes");
    };
    call.repository
        .apply_terminal_observation(
            call.session,
            authorized
                .observation_correlation()
                .bind_terminal_observation(ModelCallTerminalObservation::KnownFailed)
                .with_rate_limits(Some(snapshot)),
            ModelCallTerminalIdentities::Failed(call.terminal),
            |_| panic!("capacity fixture has no steering"),
        )
        .await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn credential_capacity_policy_ranks_known_binding_windows_then_configured_order()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _) = migrated_postgres().await?;
    let unknown = || ProviderRateLimitSnapshot::new(SystemTime::now(), Vec::new());
    let expired = || {
        ProviderRateLimitSnapshot::new(
            SystemTime::now(),
            vec![ProviderRateLimitWindow::new(
                99,
                None,
                Some(SystemTime::UNIX_EPOCH),
            )],
        )
    };
    let cases = [
        (
            "known beats unknown",
            unknown(),
            snapshot(30, 60),
            1,
            SECOND,
        ),
        (
            "secondary window binds",
            snapshot(90, 20),
            snapshot(30, 60),
            1,
            SECOND,
        ),
        (
            "equal capacity uses configured order",
            snapshot(30, 60),
            snapshot(60, 30),
            1,
            FIRST,
        ),
        (
            "expired capacity is unknown",
            expired(),
            snapshot(30, 60),
            1,
            SECOND,
        ),
        (
            "unknown ties use configured order",
            unknown(),
            unknown(),
            1,
            FIRST,
        ),
        (
            "priority precedes capacity",
            unknown(),
            snapshot(100, 100),
            2,
            FIRST,
        ),
    ];
    for (index, (name, first, second, second_priority, expected)) in cases.into_iter().enumerate() {
        // Each case owns three disjoint session/call identity ranges.
        let seed = 0xdc00_0000 + (index as u128) * 0x1000;
        let first_call = prepare_capacity_call(&pool, seed, policy(vec![member(FIRST, 1)])).await?;
        observe_capacity(first_call, first).await?;
        let second_call =
            prepare_capacity_call(&pool, seed + 0x100, policy(vec![member(SECOND, 1)])).await?;
        observe_capacity(second_call, second).await?;
        let selection = prepare_capacity_call(
            &pool,
            seed + 0x200,
            policy(vec![member(FIRST, 1), member(SECOND, second_priority)]).with_capacity_policy(
                CredentialPoolRuntimeTieBreak::LeastUsed,
                None,
                CredentialPoolRuntimeAction::Stay,
            ),
        )
        .await?;
        assert_eq!(selection.reference, expected, "{name}");
    }
    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn credential_capacity_policy_reserves_exclude_at_threshold_with_member_override()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _) = migrated_postgres().await?;
    let first = prepare_capacity_call(&pool, 0xdd00_0000, policy(vec![member(FIRST, 1)])).await?;
    observe_capacity(first, snapshot(10, 80)).await?;
    let second = prepare_capacity_call(&pool, 0xdd00_0100, policy(vec![member(SECOND, 1)])).await?;
    observe_capacity(second, snapshot(30, 80)).await?;
    let overridden = prepare_capacity_call(
        &pool,
        0xdd00_0200,
        policy(vec![
            member(FIRST, 1).with_headroom_reserve(Some(5)),
            member(SECOND, 1),
        ])
        .with_capacity_policy(
            CredentialPoolRuntimeTieBreak::FirstListed,
            Some(10),
            CredentialPoolRuntimeAction::Stay,
        ),
    )
    .await?;
    assert_eq!(overridden.reference, FIRST);
    let excluded = prepare_capacity_call(
        &pool,
        0xdd00_0300,
        policy(vec![member(FIRST, 1), member(SECOND, 1)]).with_capacity_policy(
            CredentialPoolRuntimeTieBreak::FirstListed,
            Some(10),
            CredentialPoolRuntimeAction::Stay,
        ),
    )
    .await?;
    assert_eq!(excluded.reference, SECOND);
    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn credential_capacity_policy_observation_uses_frozen_headroom_action()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _) = migrated_postgres().await?;
    let actions = [
        (CredentialPoolRuntimeAction::Stay, None),
        (
            CredentialPoolRuntimeAction::SwitchNextTurn,
            Some("switch_next_turn"),
        ),
        (
            CredentialPoolRuntimeAction::AvoidNewSessions,
            Some("avoid_new_sessions"),
        ),
        (CredentialPoolRuntimeAction::Quarantine, Some("quarantine")),
    ];
    for (index, (action, expected)) in actions.into_iter().enumerate() {
        let reference = format!("observing-member-{index}");
        let mut call = prepare_capacity_call(
            &pool,
            0xde00_0000 + (index as u128) * 0x1000,
            policy(vec![member(&reference, 1).with_headroom_reserve(Some(20))])
                .with_capacity_policy(CredentialPoolRuntimeTieBreak::LeastUsed, Some(5), action),
        )
        .await?;
        let call_id = call.call;
        // Composition changed after preparation; the observation must use its frozen policy.
        call.repository = call.repository.with_credential_pools(HashMap::new());
        observe_capacity(call, snapshot(20, 80)).await?;
        let actual: Option<(String, String)> = sqlx::query_as(
            "SELECT action_kind, cause_kind FROM credential_pool_member_action WHERE observation_model_call_id = $1")
            .bind(call_id.into_uuid()).fetch_optional(&pool).await?;
        assert_eq!(actual.as_ref().map(|(action, _)| action.as_str()), expected);
        assert_eq!(
            actual.as_ref().map(|(_, cause)| cause.as_str()),
            expected.map(|_| "headroom_low")
        );
    }
    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn credential_capacity_policy_stale_low_report_cannot_quarantine_newer_capacity()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _) = migrated_postgres().await?;
    let high = prepare_capacity_call(&pool, 0xdf00_0000, policy(vec![member(FIRST, 1)])).await?;
    let low = prepare_capacity_call(
        &pool,
        0xdf00_0100,
        policy(vec![member(FIRST, 1)]).with_capacity_policy(
            CredentialPoolRuntimeTieBreak::LeastUsed,
            Some(10),
            CredentialPoolRuntimeAction::Quarantine,
        ),
    )
    .await?;
    let older = snapshot(1, 80);
    let newer = ProviderRateLimitSnapshot::new(
        *older.observed_at() + Duration::from_secs(1),
        snapshot(80, 80).windows().to_vec(),
    );
    observe_capacity(high, newer).await?;
    observe_capacity(low, older).await?;
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM credential_pool_member_action")
        .fetch_one(&pool)
        .await?;
    assert_eq!(count, 0);
    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn credential_capacity_policy_unknown_capacity_neither_excludes_nor_quarantines()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _) = migrated_postgres().await?;
    let call = prepare_capacity_call(
        &pool,
        0xe000_0000,
        policy(vec![member(FIRST, 1)]).with_capacity_policy(
            CredentialPoolRuntimeTieBreak::LeastUsed,
            Some(10),
            CredentialPoolRuntimeAction::Quarantine,
        ),
    )
    .await?;
    let missing_reset = ProviderRateLimitSnapshot::new(
        SystemTime::now(),
        vec![ProviderRateLimitWindow::new(0, None, None)],
    );
    observe_capacity(call, missing_reset).await?;
    let next = prepare_capacity_call(
        &pool,
        0xe000_0100,
        policy(vec![member(FIRST, 1), member(SECOND, 1)]).with_capacity_policy(
            CredentialPoolRuntimeTieBreak::FirstListed,
            Some(10),
            CredentialPoolRuntimeAction::Quarantine,
        ),
    )
    .await?;
    assert_eq!(next.reference, FIRST);
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM credential_pool_member_action")
        .fetch_one(&pool)
        .await?;
    assert_eq!(count, 0);
    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn credential_capacity_policy_failure_action_precedes_low_headroom()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _) = migrated_postgres().await?;
    let cases = [
        (
            CredentialPoolRuntimeAction::SwitchNextTurn,
            Some("switch_next_turn"),
        ),
        (
            CredentialPoolRuntimeAction::AvoidNewSessions,
            Some("avoid_new_sessions"),
        ),
        (CredentialPoolRuntimeAction::Quarantine, Some("quarantine")),
        (CredentialPoolRuntimeAction::Stay, None),
    ];
    for (index, (failure_action, expected)) in cases.into_iter().enumerate() {
        // Each collision case owns disjoint identities and one credential reference.
        let seed = 0xe100_0000 + (index as u128) * 0x1000;
        let reference = format!("failure-capacity-member-{index}");
        let mut call = prepare_capacity_call(
            &pool,
            seed,
            CredentialPoolRuntimePolicy::new(
                POOL,
                vec![member(&reference, 1)],
                CredentialPoolRuntimeExhaustion::Fail,
                failure_action,
                CredentialPoolRuntimeAction::Stay,
                CredentialPoolRuntimeAction::Stay,
                CredentialPoolRuntimeAction::Stay,
            )
            .with_capacity_policy(
                CredentialPoolRuntimeTieBreak::LeastUsed,
                Some(10),
                CredentialPoolRuntimeAction::Quarantine,
            ),
        )
        .await?;
        let AuthorizeModelCallOutcome::Authorized(authorized) = call
            .repository
            .authorize_send(call.session, call.call)
            .await?
        else {
            panic!("prepared capacity call authorizes");
        };
        let reported = snapshot(5, 80);
        let outcome = call
            .repository
            .commit_observation(
                call.session,
                authorized
                    .observation_correlation()
                    .bind_provider_failure_observation_with_retry_after(
                        ProviderModelCallFailureCause::QuotaExhausted,
                        ProviderReportedTokenUsage::unreported(),
                        None,
                        false,
                    )
                    .with_rate_limits(Some(reported.clone())),
                signalbox_application::ModelCallTerminalIdentityCandidates::Availability {
                    failed: call.terminal,
                    successor_attempt: TurnAttemptId::from_uuid(Uuid::from_u128(seed + 19)),
                },
                |_| panic!("capacity fixture has no steering"),
            )
            .await?;
        assert!(matches!(
            outcome,
            Some(ModelCallObservationCommitOutcome::Terminal(_))
        ));
        let action: Option<(String, String)> = sqlx::query_as(
            "SELECT action_kind, cause_kind FROM credential_pool_member_action WHERE observation_model_call_id = $1")
            .bind(call.call.into_uuid()).fetch_optional(&pool).await?;
        assert_eq!(action.as_ref().map(|(action, _)| action.as_str()), expected);
        assert_eq!(
            action.as_ref().map(|(_, cause)| cause.as_str()),
            expected.map(|_| "quota_exhausted")
        );
        let retained = signalbox_persistence::credential_capacity::load_credential_rate_limits(
            &mut *pool.acquire().await?,
            &reference,
        )
        .await?;
        assert_eq!(retained, Some(reported));
    }
    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn credential_exclusion_clear_replays_before_newer_generations_and_filters_origins()
-> Result<(), Box<dyn Error>> {
    use signalbox_persistence::credential_exclusions::{
        self as exclusions, ClearCredentialExclusion as Clear,
        ClearCredentialExclusionOutcome as Outcome, ClearCredentialExclusionResult as Result,
        CredentialExclusionTarget as Target,
    };
    let (container, pool, _) = migrated_postgres().await?;
    let old_generation: i64 = sqlx::query_scalar("INSERT INTO credential_exclusion(kind,profile,origin) VALUES ('profile_quarantine','home','codex_home') RETURNING record_generation").fetch_one(&pool).await?;
    let oauth_generation: i64 = sqlx::query_scalar("INSERT INTO credential_exclusion(kind,profile,origin) VALUES ('profile_quarantine','oauth','oauth_refresh') RETURNING record_generation").fetch_one(&pool).await?;
    let final_generation: i64 = sqlx::query_scalar("INSERT INTO credential_exclusion(kind,profile,origin) VALUES ('profile_quarantine','z-home','codex_home') RETURNING record_generation").fetch_one(&pool).await?;
    let old = Target::ProfileQuarantine {
        profile: "home".into(),
        record_generation: old_generation as u64,
    };
    let last = Target::ProfileQuarantine {
        profile: "z-home".into(),
        record_generation: final_generation as u64,
    };
    let page = exclusions::list(&pool, 1, None).await?;
    assert_eq!(page.exclusions, vec![old.clone()]);
    assert_eq!(page.next_after, Some(old.clone()));
    let page = exclusions::list(&pool, 1, page.next_after.as_ref()).await?;
    assert_eq!(page.exclusions, vec![last]);
    assert_eq!(page.next_after, None);
    let clear = Clear {
        command_id: DurableCommandId::from_uuid(Uuid::now_v7()),
        target: old.clone(),
    };
    assert_eq!(
        exclusions::clear(&pool, clear.clone()).await?,
        Result::Recorded(Outcome::Cleared)
    );
    assert_eq!(
        exclusions::clear(
            &pool,
            Clear {
                command_id: DurableCommandId::from_uuid(Uuid::now_v7()),
                target: old.clone()
            }
        )
        .await?,
        Result::Recorded(Outcome::AlreadyCleared)
    );
    sqlx::query("INSERT INTO credential_exclusion(kind,profile,origin) VALUES ('profile_quarantine','home','codex_home')").execute(&pool).await?;
    assert_eq!(
        exclusions::clear(&pool, clear.clone()).await?,
        Result::Recorded(Outcome::Cleared)
    );
    assert_eq!(
        exclusions::clear(
            &pool,
            Clear {
                command_id: DurableCommandId::from_uuid(Uuid::now_v7()),
                target: old
            }
        )
        .await?,
        Result::Recorded(Outcome::StaleGeneration)
    );
    let oauth = Target::ProfileQuarantine {
        profile: "oauth".into(),
        record_generation: oauth_generation as u64,
    };
    assert_eq!(
        exclusions::clear(
            &pool,
            Clear {
                command_id: clear.command_id,
                target: oauth.clone()
            }
        )
        .await?,
        Result::ConflictingReuse
    );
    assert_eq!(
        exclusions::clear(
            &pool,
            Clear {
                command_id: DurableCommandId::from_uuid(Uuid::now_v7()),
                target: oauth
            }
        )
        .await?,
        Result::Recorded(Outcome::UnknownCredentialExclusion)
    );
    let retained: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM credential_exclusion WHERE record_generation = $1",
    )
    .bind(old_generation)
    .fetch_one(&pool)
    .await?;
    assert_eq!(retained, 1, "clearing retains the exact generation");
    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn credential_exclusion_clear_restores_a_quarantined_pool_member()
-> Result<(), Box<dyn Error>> {
    use signalbox_persistence::credential_exclusions::{
        self as exclusions, ClearCredentialExclusion as Clear,
        ClearCredentialExclusionOutcome as Outcome, ClearCredentialExclusionResult as Result,
    };
    let (container, pool, _) = migrated_postgres().await?;
    let call = prepare_capacity_call(
        &pool,
        0xf046_0000,
        policy(vec![member(FIRST, 1)]).with_capacity_policy(
            CredentialPoolRuntimeTieBreak::FirstListed,
            Some(10),
            CredentialPoolRuntimeAction::Quarantine,
        ),
    )
    .await?;
    observe_capacity(call, snapshot(1, 80)).await?;
    let next = prepare_capacity_call(
        &pool,
        0xf046_0100,
        policy(vec![member(FIRST, 1), member(SECOND, 1)]),
    )
    .await?;
    assert_eq!(next.reference, SECOND);
    let targets = exclusions::list(&pool, 100, None).await?;
    assert_eq!(targets.exclusions.len(), 1);
    assert_eq!(targets.exclusions[0].profile(), FIRST);
    let clear = Clear {
        command_id: DurableCommandId::from_uuid(Uuid::now_v7()),
        target: targets.exclusions[0].clone(),
    };
    assert_eq!(
        exclusions::clear(&pool, clear).await?,
        Result::Recorded(Outcome::Cleared)
    );
    let next = prepare_capacity_call(
        &pool,
        0xf046_0200,
        policy(vec![member(FIRST, 1), member(SECOND, 1)]),
    )
    .await?;
    assert_eq!(
        next.reference, FIRST,
        "clearing changes durable selection eligibility"
    );
    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn credential_exclusion_generations_are_scoped_to_pool_policy() -> Result<(), Box<dyn Error>>
{
    use signalbox_persistence::credential_exclusions::{
        self as exclusions, ClearCredentialExclusion as Clear,
        ClearCredentialExclusionOutcome as Outcome, ClearCredentialExclusionResult as Result,
        CredentialExclusionTarget as Target,
    };
    let (container, pool, _) = migrated_postgres().await?;
    let first = prepare_capacity_call(
        &pool,
        0xf046_0300,
        policy(vec![member(FIRST, 1)]).with_capacity_policy(
            CredentialPoolRuntimeTieBreak::FirstListed,
            Some(10),
            CredentialPoolRuntimeAction::AvoidNewSessions,
        ),
    )
    .await?;
    observe_capacity(first, snapshot(1, 80)).await?;
    let old = exclusions::list(&pool, 100, None)
        .await?
        .exclusions
        .remove(0);
    let second = prepare_capacity_call(
        &pool,
        0xf046_0400,
        policy(vec![member(FIRST, 1)]).with_capacity_policy(
            CredentialPoolRuntimeTieBreak::FirstListed,
            Some(0),
            CredentialPoolRuntimeAction::AvoidNewSessions,
        ),
    )
    .await?;
    observe_capacity(second, snapshot(0, 80)).await?;
    let before = exclusions::list(&pool, 100, None).await?;
    assert_eq!(before.exclusions.len(), 2);
    assert!(matches!(old, Target::MembershipExclusion { .. }));
    assert_eq!(
        exclusions::clear(
            &pool,
            Clear {
                command_id: DurableCommandId::from_uuid(Uuid::now_v7()),
                target: old.clone(),
            }
        )
        .await?,
        Result::Recorded(Outcome::Cleared),
        "a newer generation in another policy cannot make this target stale",
    );
    let after = exclusions::list(&pool, 100, None).await?;
    assert_eq!(after.exclusions.len(), 1);
    assert_ne!(after.exclusions[0], old);
    assert!(before.exclusions.contains(&after.exclusions[0]));
    pool.close().await;
    drop(container);
    Ok(())
}
