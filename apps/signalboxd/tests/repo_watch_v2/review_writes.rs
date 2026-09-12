use super::*;

fn review_and_thread(
    repository: &RepositorySlug,
    review: u64,
    resolved: bool,
) -> signalbox_module_repo_watch_v2::ingest::RepositoryObservation {
    let mut observed = goal_review_observation(repository, review);
    let pull = &observed.observation.state().pull_requests()[0];
    let author = RepoWatchAuthorLogin::try_new("reviewer".to_owned()).expect("fixture author");
    let thread = ReviewThreadId::try_new(format!("PRRT_review_{review}")).expect("fixture thread");
    let thread = if resolved {
        RepoWatchThreadObservation::resolved(thread, Some(author.clone()), Some(author))
    } else {
        RepoWatchThreadObservation::open(thread, Some(author))
    }
    .with_source_review(Some(GitHubObjectId::new(
        NonZeroU64::new(review).expect("fixture review"),
    )));
    let pull = ComparisonPullRequestState::try_new(RepoWatchPullRequestStateInput {
        context: pull.context().clone(),
        lifecycle: pull.lifecycle(),
        mergeable_state: pull.mergeable_state(),
        completed_check_suites: Vec::new(),
        completed_check_runs: Vec::new(),
        reviews: pull.reviews().to_vec(),
        threads: vec![thread],
        reactions: Vec::new(),
    })
    .expect("fixture pull request");
    observed.observation = RepoWatchObservation::new(
        Vec::new(),
        RepoWatchRepositoryState::try_new(RepoWatchRepositoryStateInput {
            pull_requests: vec![pull],
            branch_heads: Vec::new(),
            workflow_runs: Vec::new(),
        })
        .expect("fixture repository"),
    );
    observed
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL"]
async fn native_review_receipts_preserve_owner_reviews_and_later_thread_actions()
-> Result<(), Box<dyn Error>> {
    let (_container, core, url) = postgres().await?;
    migrate(&core).await?;
    let pool = module_pool(&url).await?;
    let store = RepoWatchStore::new(pool.clone());
    let repository = RepositorySlug::try_new("dispatch/project".to_owned())?;
    let now = OffsetDateTime::now_utc();
    let initial = goal_review_observation(&repository, 1);
    store
        .ingest_observation(
            &store.ingest_baseline(&repository).await?,
            &initial,
            EventProducer::Poll,
            MERGED_RETENTION,
        )
        .await?;
    let rule = RepoWatchRule::try_new(
        RepoWatchRuleId::try_new("reviews".to_owned())?,
        RepoWatchRuleVersion::V1,
        RepoWatchMatcherV1::new(RepoWatchMatcherV1Input {
            event_kinds: vec![
                RepoWatchEventKindNameV1::ReviewSubmitted,
                RepoWatchEventKindNameV1::ThreadOpened,
            ],
            repository: Some(repository.clone()),
            ..RepoWatchMatcherV1Input::default()
        }),
        vec![RepoWatchRuleActionV1::DispatchSession {
            template: SessionTemplateName::try_new("watch".to_owned())?,
        }],
        RepoWatchSingletonScope::PullRequest,
        Duration::ZERO,
    )?;
    store
        .reconcile_rules(
            &[RepositoryRuleSet::new(
                &repository,
                std::slice::from_ref(&rule),
            )],
            now,
        )
        .await?;

    // The tool and the owner use the same login; only the returned native ID differs.
    let native_review = GitHubObjectId::new(NonZeroU64::new(2).expect("native review"));
    let pending = store.begin_review_write(repository.clone()).await?;
    let lock_available: bool = sqlx::query_scalar(
        "SELECT pg_try_advisory_xact_lock(hashtextextended('frontier:' || $1, 0))",
    )
    .bind(repository.as_str())
    .fetch_one(&pool)
    .await?;
    assert!(!lock_available, "ingestion waits for the native receipt");
    pending
        .record(native_review, Some("PRRC_native_reply".to_owned()))
        .await?;

    // Reconstructing the store exercises durable provenance rather than process-local memory.
    let restarted = RepoWatchStore::new(pool.clone());
    let native = review_and_thread(&repository, native_review.get(), false);
    restarted
        .ingest_observation(
            &restarted.ingest_baseline(&repository).await?,
            &native,
            EventProducer::Poll,
            MERGED_RETENTION,
        )
        .await?;
    assert!(
        restarted
            .next_rule_event(&repository, &rule)
            .await?
            .is_none(),
        "native review and thread creation must not dispatch"
    );
    let retained: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM gh_event WHERE repository=$1 AND source_review_id=$2",
    )
    .bind(repository.as_str())
    .bind(Decimal::from(native_review.get()))
    .fetch_one(&pool)
    .await?;
    assert_eq!(retained, 2, "self-caused facts remain available for audit");

    let owner = review_and_thread(&repository, 3, false);
    restarted
        .ingest_observation(
            &restarted.ingest_baseline(&repository).await?,
            &owner,
            EventProducer::Poll,
            MERGED_RETENTION,
        )
        .await?;
    let next = restarted
        .next_rule_event(&repository, &rule)
        .await?
        .expect("genuine owner event");
    let review_id: Decimal =
        sqlx::query_scalar("SELECT source_review_id FROM gh_event WHERE event_id=$1")
            .bind(next.event.id().into_uuid())
            .fetch_one(&pool)
            .await?;
    assert_eq!(review_id, Decimal::from(3_u64));
    let owner_events: i64 = sqlx::query_scalar("SELECT count(*) FROM gh_readable_event AS readable JOIN gh_event AS stored USING (event_id) WHERE readable.repository=$1 AND stored.source_review_id=3")
        .bind(repository.as_str()).fetch_one(&pool).await?;
    assert_eq!(
        owner_events, 2,
        "the owner's review and thread are both eligible"
    );

    let resolved = review_and_thread(&repository, native_review.get(), true);
    restarted
        .ingest_observation(
            &restarted.ingest_baseline(&repository).await?,
            &resolved,
            EventProducer::Poll,
            MERGED_RETENTION,
        )
        .await?;
    let reopened = review_and_thread(&repository, native_review.get(), false);
    restarted
        .ingest_observation(
            &restarted.ingest_baseline(&repository).await?,
            &reopened,
            EventProducer::Poll,
            MERGED_RETENTION,
        )
        .await?;
    let reopening: i64 = sqlx::query_scalar("SELECT count(*) FROM gh_readable_event WHERE repository=$1 AND event_kind='thread_opened' AND convert_from(normalized_payload, 'UTF8') LIKE '%PRRT_review_2%'")
        .bind(repository.as_str()).fetch_one(&pool).await?;
    assert_eq!(
        reopening, 1,
        "an owner reopening a native thread is a distinct action"
    );
    pool.close().await;
    core.close().await;
    Ok(())
}
