use super::*;

fn rule(kind: RepoWatchEventKindNameV1) -> Result<RepoWatchRule, Box<dyn Error>> {
    Ok(RepoWatchRule::try_new(
        RepoWatchRuleId::try_new("pending-work".to_owned())?,
        RepoWatchRuleVersion::V1,
        RepoWatchMatcherV1::new(RepoWatchMatcherV1Input {
            event_kinds: vec![kind],
            mergeable_state: if kind == RepoWatchEventKindNameV1::MergeableStateChanged {
                vec![MergeableState::Conflicting]
            } else {
                vec![]
            },
            ..Default::default()
        }),
        vec![RepoWatchRuleActionV1::DispatchSession {
            template: SessionTemplateName::try_new("watch".to_owned())?,
        }],
        RepoWatchSingletonScope::PullRequest,
        Duration::ZERO,
    )?)
}

/// Arbitrary identities are confined to one test database.
async fn evaluate(
    store: &RepoWatchStore,
    repository: &RepositorySlug,
    rule: &RepoWatchRule,
) -> Result<(), Box<dyn Error>> {
    store
        .evaluate_pending(
            repository,
            rule,
            &mut FixedDispatchIds {
                value: 91001,
                calls: 0,
            },
            &mut FixtureSessionFactory {
                next_command: 92001,
                model: 93001,
            },
            &mut FixtureCommandCodec,
            OffsetDateTime::now_utc(),
        )
        .await
        .expect("pending evaluation");
    Ok(())
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL"]
async fn pending_evaluation_reaches_a_conflict_behind_unrelated_events_after_restart()
-> Result<(), Box<dyn Error>> {
    let (_database, _core, url) = postgres().await?;
    let pool = module_pool(&url).await?;
    let store = RepoWatchStore::new(pool.clone());
    let repository = RepositorySlug::try_new("pending/project".to_owned())?;
    let now = OffsetDateTime::now_utc();
    let head = CommitSha::try_new("1111111111111111111111111111111111111111".to_owned())?;
    let initial = super::retry::observation(
        &repository,
        MergeableState::Mergeable,
        RepoWatchPullRequestLifecycle::Open,
        &head,
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
    let rule = rule(RepoWatchEventKindNameV1::MergeableStateChanged)?;
    store
        .reconcile_rules(
            &[RepositoryRuleSet::new(
                &repository,
                std::slice::from_ref(&rule),
            )],
            now,
        )
        .await?;
    // A multi-event backlog must be consumed in this pass; the particular length is arbitrary.
    for run in 1..=32 {
        store
            .ingest_observation(
                &store.ingest_baseline(&repository).await?,
                &dispatch_observation(&repository, run, now),
                EventProducer::Poll,
                MERGED_RETENTION,
            )
            .await?;
    }
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
    let restarted = RepoWatchStore::new(pool);
    evaluate(&restarted, &repository, &rule).await?;
    let pending = restarted
        .recover_pending_commands(&mut FixtureCommandCodec)
        .await?;
    assert_eq!(
        pending.len(),
        1,
        "the retained conflict is admitted before provisioning"
    );
    assert!(
        restarted
            .next_rule_event(&repository, &rule)
            .await?
            .is_none(),
        "the backlog is consumed"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL"]
async fn pending_evaluation_admits_a_conflict_already_present_at_activation_after_restart()
-> Result<(), Box<dyn Error>> {
    let (_database, _core, url) = postgres().await?;
    let pool = module_pool(&url).await?;
    let store = RepoWatchStore::new(pool.clone());
    let repository = RepositorySlug::try_new("pending/project".to_owned())?;
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
    let rule = rule(RepoWatchEventKindNameV1::MergeableStateChanged)?;
    store
        .reconcile_rules(
            &[RepositoryRuleSet::new(
                &repository,
                std::slice::from_ref(&rule),
            )],
            now,
        )
        .await?;
    let restarted = RepoWatchStore::new(pool);
    evaluate(&restarted, &repository, &rule).await?;
    assert_eq!(
        restarted
            .recover_pending_commands(&mut FixtureCommandCodec)
            .await?
            .len(),
        1,
        "activation does not need a new provider event"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL"]
async fn pending_evaluation_coalesces_completed_checks_before_session_submission()
-> Result<(), Box<dyn Error>> {
    let (_database, _core, url) = postgres().await?;
    let pool = module_pool(&url).await?;
    let store = RepoWatchStore::new(pool);
    let repository = RepositorySlug::try_new("pending/project".to_owned())?;
    let now = OffsetDateTime::now_utc();
    let rule = rule(RepoWatchEventKindNameV1::ChecksCompleted)?;
    store
        .reconcile_rules(
            &[RepositoryRuleSet::new(
                &repository,
                std::slice::from_ref(&rule),
            )],
            now,
        )
        .await?;
    // Distinct completed suites share one pull request and head.
    for suite in 1..=3 {
        let mut observed = goal_review_observation(&repository, suite);
        let context = observed.observation.state().pull_requests()[0]
            .context()
            .clone();
        observed.observation = RepoWatchObservation::new(
            vec![],
            RepoWatchRepositoryState::try_new(RepoWatchRepositoryStateInput {
                pull_requests: vec![ComparisonPullRequestState::try_new(
                    RepoWatchPullRequestStateInput {
                        context,
                        lifecycle: RepoWatchPullRequestLifecycle::Open,
                        mergeable_state: MergeableState::Mergeable,
                        completed_check_suites: vec![RepoWatchCheckSuiteObservation::new(
                            GitHubObjectId::new(NonZeroU64::new(suite).expect("suite identity")),
                            RepoWatchCheckCompletionGeneration::try_new("completed".to_owned())?,
                            ChecksOutcome::Success,
                        )],
                        completed_check_runs: vec![],
                        reviews: vec![],
                        threads: vec![],
                        reactions: vec![],
                    },
                )?],
                branch_heads: vec![],
                workflow_runs: vec![],
            })?,
        );
        store
            .ingest_observation(
                &store.ingest_baseline(&repository).await?,
                &observed,
                EventProducer::Poll,
                MERGED_RETENTION,
            )
            .await?;
    }
    evaluate(&store, &repository, &rule).await?;
    assert_eq!(
        store
            .recover_pending_commands(&mut FixtureCommandCodec)
            .await?
            .len(),
        1,
        "the singleton suppresses other completions in the retained batch"
    );
    assert!(store.next_rule_event(&repository, &rule).await?.is_none());
    Ok(())
}
