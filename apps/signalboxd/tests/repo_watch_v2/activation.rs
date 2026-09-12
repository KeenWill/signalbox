use super::*;
use signalbox_session_ownership::RepoWatchEventTarget;

fn activation_rule(version: u64) -> Result<RepoWatchRule, Box<dyn Error>> {
    Ok(RepoWatchRule::try_new(
        RepoWatchRuleId::try_new("activation".to_owned())?,
        RepoWatchRuleVersion::new(NonZeroU64::new(version).expect("revision")).expect("version"),
        RepoWatchMatcherV1::new(RepoWatchMatcherV1Input {
            event_kinds: vec![RepoWatchEventKindNameV1::ReviewSubmitted],
            labels: signalbox_session_ownership::RepoWatchLabelMatcher::new(
                signalbox_session_ownership::RepoWatchLabelMatcherInput {
                    none_of: vec![LabelName::try_new("no-auto".to_owned())?],
                    ..Default::default()
                },
            ),
            ..Default::default()
        }),
        vec![RepoWatchRuleActionV1::DispatchSession {
            template: SessionTemplateName::try_new("watch".to_owned())?,
        }],
        RepoWatchSingletonScope::PullRequest,
        Duration::from_secs(5),
    )?)
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL"]
async fn activation_admits_retired_revision_work_without_a_new_matching_event()
-> Result<(), Box<dyn Error>> {
    let (_database, core, url) = postgres().await?;
    sqlx::query("ALTER ROLE mod_repo_watch PASSWORD 'signalbox-test-only'")
        .execute(&core)
        .await?;
    let pool = module_pool(&url).await?;
    let store = RepoWatchStore::new(pool.clone());
    let source = signalbox_session_ownership::LifecycleEventSource::new(core);
    let repository = RepositorySlug::try_new("activation/project".to_owned())?;
    let now = OffsetDateTime::now_utc();
    let first_head = CommitSha::try_new("1111111111111111111111111111111111111111".to_owned())?;
    let later_head = CommitSha::try_new("2222222222222222222222222222222222222222".to_owned())?;
    let initial = super::retry::observation(
        &repository,
        MergeableState::Conflicting,
        RepoWatchPullRequestLifecycle::Open,
        &first_head,
        now,
    );
    store
        .ingest_observation(
            &store.ingest_baseline(&repository).await?,
            &initial,
            EventProducer::Poll,
            MERGED_RETENTION,
        )
        .await?;
    let old = activation_rule(1)?;
    store
        .reconcile_rules(
            &[RepositoryRuleSet::new(
                &repository,
                std::slice::from_ref(&old),
            )],
            now,
        )
        .await?;
    let mut ids = FixedDispatchIds {
        value: 81001,
        calls: 0,
    };
    let mut factory = FixtureSessionFactory {
        next_command: 82001,
        model: 83001,
    };
    let mut codec = FixtureCommandCodec;
    assert!(
        store
            .evaluate_next(&repository, &old, &mut ids, &mut factory, &mut codec, now)
            .await
            .expect("old activation")
    );
    let first = store.recover_pending_commands(&mut codec).await?;
    assert_eq!(first.len(), 1);
    let old_session = SessionId::from_uuid(Uuid::from_u128(84001));
    store
        .apply_lifecycle_event(&LifecycleEvent::session_created_for_test(
            1,
            now,
            old_session,
            SessionCreated {
                cause: SessionCreationCause::ModuleDispatched {
                    dispatch: ModuleDispatch::RepositoryWatch {
                        dispatch: first[0].dispatch(),
                    },
                },
                ownership: SessionOwnership::Owned,
            },
        ))
        .await?;
    sqlx::query("UPDATE dispatch_ledger SET session_terminal_at=$2,singleton_released_at=$2 WHERE created_session_id=$1")
        .bind(old_session.into_uuid()).bind(now).execute(&pool).await?;
    let rule = activation_rule(2)?;
    store
        .reconcile_rules(
            &[RepositoryRuleSet::new(
                &repository,
                std::slice::from_ref(&rule),
            )],
            now,
        )
        .await?;
    let advanced = super::retry::observation(
        &repository,
        MergeableState::Conflicting,
        RepoWatchPullRequestLifecycle::Open,
        &later_head,
        now,
    );
    store
        .ingest_observation(
            &store.ingest_baseline(&repository).await?,
            &advanced,
            EventProducer::Poll,
            MERGED_RETENTION,
        )
        .await?;
    let events_before: i64 = sqlx::query_scalar("SELECT count(*) FROM gh_event")
        .fetch_one(&pool)
        .await?;
    let restarted = RepoWatchStore::new(pool.clone());
    assert!(
        restarted
            .evaluate_next(&repository, &rule, &mut ids, &mut factory, &mut codec, now)
            .await
            .expect("new activation")
    );
    let pending = restarted.recover_pending_commands(&mut codec).await?;
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].rule_revision(), rule.version());
    let checkout = restarted
        .dispatch_checkout(pending[0].command().command_id())
        .await?
        .expect("checkout");
    let RepoWatchEventTarget::PullRequest(context) = checkout.event.target() else {
        panic!("pull request");
    };
    assert_eq!(context.head_sha(), &later_head);
    let session = SessionId::from_uuid(Uuid::from_u128(84002));
    restarted
        .apply_lifecycle_event(&LifecycleEvent::session_created_for_test(
            2,
            now,
            session,
            SessionCreated {
                cause: SessionCreationCause::ModuleDispatched {
                    dispatch: ModuleDispatch::RepositoryWatch {
                        dispatch: pending[0].dispatch(),
                    },
                },
                ownership: SessionOwnership::Owned,
            },
        ))
        .await?;
    sqlx::query("UPDATE dispatch_ledger SET session_terminal_at=$2,singleton_released_at=$2 WHERE created_session_id=$1")
        .bind(session.into_uuid()).bind(now).execute(&pool).await?;
    assert!(
        !restarted
            .retry_due(
                &repository,
                &rule,
                &mut ids,
                &mut factory,
                &mut codec,
                &source,
                now + Duration::from_secs(4)
            )
            .await
            .expect("cooldown")
    );
    assert!(
        restarted
            .retry_due(
                &repository,
                &rule,
                &mut ids,
                &mut factory,
                &mut codec,
                &source,
                now + Duration::from_secs(5)
            )
            .await
            .expect("activation retry")
    );
    assert!(
        !restarted
            .retry_due(
                &repository,
                &rule,
                &mut ids,
                &mut factory,
                &mut codec,
                &source,
                now + Duration::from_secs(6)
            )
            .await
            .expect("live singleton")
    );
    let events_after: i64 = sqlx::query_scalar("SELECT count(*) FROM gh_event")
        .fetch_one(&pool)
        .await?;
    assert_eq!(events_after, events_before);
    let candidates: i64 = sqlx::query_scalar("SELECT count(*) FROM rule_activation_candidate")
        .fetch_one(&pool)
        .await?;
    assert_eq!(candidates, 0);
    let dispatches: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM dispatch_ledger WHERE command_kind='create_session'",
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(dispatches, 3);
    Ok(())
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL"]
async fn activation_discards_work_that_finishes_before_admission() -> Result<(), Box<dyn Error>> {
    let (_database, core, url) = postgres().await?;
    sqlx::query("ALTER ROLE mod_repo_watch PASSWORD 'signalbox-test-only'")
        .execute(&core)
        .await?;
    let pool = module_pool(&url).await?;
    let store = RepoWatchStore::new(pool.clone());
    let repository = RepositorySlug::try_new("activation/project".to_owned())?;
    let now = OffsetDateTime::now_utc();
    let head = CommitSha::try_new("1111111111111111111111111111111111111111".to_owned())?;
    let conflict = super::retry::observation(
        &repository,
        MergeableState::Conflicting,
        RepoWatchPullRequestLifecycle::Open,
        &head,
        now,
    );
    store
        .ingest_observation(
            &store.ingest_baseline(&repository).await?,
            &conflict,
            EventProducer::Poll,
            MERGED_RETENTION,
        )
        .await?;
    let rule = activation_rule(1)?;
    store
        .reconcile_rules(
            &[RepositoryRuleSet::new(
                &repository,
                std::slice::from_ref(&rule),
            )],
            now,
        )
        .await?;
    let candidates_before: i64 =
        sqlx::query_scalar("SELECT count(*) FROM rule_activation_candidate")
            .fetch_one(&pool)
            .await?;
    assert_eq!(candidates_before, 1);
    let clean = super::retry::observation(
        &repository,
        MergeableState::Mergeable,
        RepoWatchPullRequestLifecycle::Open,
        &head,
        now,
    );
    store
        .ingest_observation(
            &store.ingest_baseline(&repository).await?,
            &clean,
            EventProducer::Poll,
            MERGED_RETENTION,
        )
        .await?;
    let mut ids = FixedDispatchIds {
        value: 81001,
        calls: 0,
    };
    let mut factory = FixtureSessionFactory {
        next_command: 82001,
        model: 83001,
    };
    let mut codec = FixtureCommandCodec;
    store
        .evaluate_next(&repository, &rule, &mut ids, &mut factory, &mut codec, now)
        .await
        .expect("clean candidate");
    assert!(store.recover_pending_commands(&mut codec).await?.is_empty());
    let candidates_after: i64 =
        sqlx::query_scalar("SELECT count(*) FROM rule_activation_candidate")
            .fetch_one(&pool)
            .await?;
    assert_eq!(candidates_after, 0);
    Ok(())
}
