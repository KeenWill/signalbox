//! Changed-profile reloads release authentication waits without replacing their turns.
use super::*;
use signalbox_persistence::reload_configuration::{
    ReloadConfiguration, ReloadConfigurationRepository, ReloadIntent,
};

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn authentication_wait_requires_a_changed_profile_and_replays_release_once()
-> Result<(), Box<dyn Error>> {
    // Disjoint synthetic identities for the single turn and its successive attempts.
    const SEED: u128 = 0x6007_1000;
    const POOL: &str = "authentication-wait";
    const MEMBER: &str = "changed-home";
    let (container, pool, _) = migrated_postgres().await?;
    signalbox_persistence::credential_invocations::replace_registrations(
        &pool,
        &[(MEMBER.to_owned(), None)],
    )
    .await?;
    let (session, turn, mut repository) = active_credential_pool_fixture(
        &pool,
        SEED,
        POOL,
        &[MEMBER],
        CredentialPoolRuntimeAction::SwitchNow,
        CredentialPoolRuntimeAction::SwitchNow,
    )
    .await?;
    let target =
        ResolvedProviderTarget::naming(ProviderModelIdentity::from_uuid(Uuid::from_u128(SEED + 4)));
    let policy = CredentialPoolRuntimePolicy::new(
        POOL.to_owned(),
        vec![CredentialPoolRuntimeMember::new(
            MEMBER.to_owned(),
            nonzero_priority(1),
        )],
        CredentialPoolRuntimeExhaustion::Park,
        CredentialPoolRuntimeAction::SwitchNow,
        CredentialPoolRuntimeAction::SwitchNow,
        CredentialPoolRuntimeAction::SwitchNow,
        CredentialPoolRuntimeAction::SwitchNow,
    );
    repository = repository.with_credential_pools(HashMap::from([(target, policy)]));
    let (first, _) = prepare_and_authorize_pool_call(&repository, session, SEED + 100).await?;
    let predecessor = first.observation_correlation().call();
    let parked = repository
        .commit_observation(
            session,
            first
                .observation_correlation()
                .bind_provider_failure_observation_with_retry_after(
                    ProviderModelCallFailureCause::CredentialRejected,
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
    let Some(ModelCallObservationCommitOutcome::CredentialWait(wait)) = parked else {
        panic!("authentication exhaustion must retain its turn in a wait")
    };
    let reloads = ReloadConfigurationRepository::new(pool.clone());
    let intent = ReloadIntent {
        // The repository retains opaque checked catalogs; profile comparison belongs to the daemon.
        replacement_snapshot: "{}".to_owned(),
        prior_snapshot: "{}".to_owned(),
        rule_set_digest: [0; 32],
    };
    let unchanged = ReloadConfiguration {
        command_id: DurableCommandId::from_uuid(Uuid::from_u128(SEED + 130)),
    };
    reloads.claim(unchanged, Ok(&intent)).await?;
    reloads.finish_profile_reload(unchanged, &[]).await?;
    let eligible: bool = sqlx::query_scalar("SELECT credential_wait_is_eligible($1)")
        .bind(wait.attempt().into_uuid())
        .fetch_one(&pool)
        .await?;
    assert!(
        !eligible,
        "unchanged profiles cannot wake authentication waits"
    );
    let changed = ReloadConfiguration {
        command_id: DurableCommandId::from_uuid(Uuid::from_u128(SEED + 131)),
    };
    reloads.claim(changed, Ok(&intent)).await?;
    reloads
        .finish_profile_reload(changed, &[MEMBER.to_owned()])
        .await?;
    reloads
        .finish_profile_reload(changed, &[MEMBER.to_owned()])
        .await?;
    let eligible: bool = sqlx::query_scalar("SELECT credential_wait_is_eligible($1)")
        .bind(wait.attempt().into_uuid())
        .fetch_one(&pool)
        .await?;
    assert!(eligible);
    let releases: i64 = sqlx::query_scalar("SELECT count(*) FROM credential_authentication_release WHERE predecessor_model_call_id = $1")
        .bind(predecessor.into_uuid()).fetch_one(&pool).await?;
    assert_eq!(releases, 1);
    let (left, right) = tokio::join!(
        prepare_wait_admission(&repository, session, SEED + 140),
        prepare_wait_admission(&repository, session, SEED + 150),
    );
    let call = [left?, right?]
        .into_iter()
        .find_map(|outcome| match outcome {
            PrepareInitialModelCallOutcome::Checkpointed(call) => Some(call),
            _ => None,
        })
        .expect("a competing release prepares the successor");
    let calls: i64 = sqlx::query_scalar("SELECT count(*) FROM model_call WHERE turn_id = $1")
        .bind(turn.into_uuid())
        .fetch_one(&pool)
        .await?;
    assert_eq!(calls, 2, "one failed call and one successor");
    let AuthorizeModelCallOutcome::Authorized(authorized) =
        repository.authorize_send(session, call).await?
    else {
        panic!("released call must authorize")
    };
    let rejected_again = repository
        .commit_observation(
            session,
            authorized
                .observation_correlation()
                .bind_provider_failure_observation_with_retry_after(
                    ProviderModelCallFailureCause::CredentialRejected,
                    ProviderReportedTokenUsage::unreported(),
                    None,
                    false,
                ),
            signalbox_application::ModelCallTerminalIdentityCandidates::Availability {
                failed: FailedModelCallTurnIdentities::new(
                    SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(SEED + 170)),
                    ContextFrontierId::from_uuid(Uuid::from_u128(SEED + 171)),
                ),
                successor_attempt: TurnAttemptId::from_uuid(Uuid::from_u128(SEED + 172)),
            },
            |_| panic!("no steering in this fixture"),
        )
        .await?;
    let Some(ModelCallObservationCommitOutcome::CredentialWait(second_wait)) = rejected_again
    else {
        panic!("another rejection must park the retained turn again")
    };
    reloads
        .finish_profile_reload(changed, &[MEMBER.to_owned()])
        .await?;
    let eligible: bool = sqlx::query_scalar("SELECT credential_wait_is_eligible($1)")
        .bind(second_wait.attempt().into_uuid())
        .fetch_one(&pool)
        .await?;
    assert!(
        !eligible,
        "replaying an old reload cannot release a newer rejection"
    );
    let changed_again = ReloadConfiguration {
        command_id: DurableCommandId::from_uuid(Uuid::from_u128(SEED + 180)),
    };
    reloads.claim(changed_again, Ok(&intent)).await?;
    reloads
        .finish_profile_reload(changed_again, &[MEMBER.to_owned()])
        .await?;
    let PrepareInitialModelCallOutcome::Checkpointed(call) =
        prepare_wait_admission(&repository, session, SEED + 190).await?
    else {
        panic!("a new profile replacement releases the new rejection")
    };
    let AuthorizeModelCallOutcome::Authorized(authorized) =
        repository.authorize_send(session, call).await?
    else {
        panic!("the next released call must authorize")
    };
    let completed = repository
        .commit_observation(
            session,
            authorized
                .observation_correlation()
                .bind_terminal_observation(ModelCallTerminalObservation::Completed {
                    assistant_text: vec![
                        AssistantText::try_new("resumed after profile replacement".to_owned())
                            .expect("nonempty recovered reply"),
                    ],
                }),
            signalbox_application::ModelCallTerminalIdentityCandidates::Exact(
                ModelCallTerminalIdentities::Completed(CompletedModelCallIdentities::new(
                    vec![SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(
                        SEED + 160,
                    ))],
                    SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(SEED + 161)),
                    ContextFrontierId::from_uuid(Uuid::from_u128(SEED + 162)),
                )),
            ),
            |_| panic!("no steering in this fixture"),
        )
        .await?;
    assert!(matches!(
        completed,
        Some(ModelCallObservationCommitOutcome::Terminal(_))
    ));
    let state: (String, String) = sqlx::query_as(
        "SELECT state_kind, terminal_disposition_kind FROM turn_lifecycle WHERE turn_id = $1",
    )
    .bind(turn.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(state, ("terminal".to_owned(), "completed".to_owned()));
    signalbox_persistence::process_read::ProcessReadRepository::new(pool.clone())
        .read_transcript(session)
        .await?
        .expect("recovered turn remains readable");
    pool.close().await;
    drop(container);
    Ok(())
}
