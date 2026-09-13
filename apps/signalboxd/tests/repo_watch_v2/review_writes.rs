use super::*;
use std::sync::Arc;

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
        required_check_conclusions: None,
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

struct IdentityClient(signalbox_module_repo_watch_v2::github::GitHubClient);

impl signalbox_module_repo_watch_v2::provider::RepositoryClientLoader for IdentityClient {
    type Error = std::convert::Infallible;
    async fn load_client(
        &self,
    ) -> Result<signalbox_module_repo_watch_v2::github::GitHubClient, Self::Error> {
        Ok(self.0.clone())
    }
}

pub(super) async fn identity_attempt(
    store: &RepoWatchStore,
    repository: &RepositorySlug,
    login: Option<&str>,
) -> bool {
    use signalbox_module_repo_watch_v2::{
        github::GitHubClient, measurements::PollOutcome, provider::GitHubRepositoryTask,
    };
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    let requests = Arc::new(AtomicUsize::new(0));
    let observed = requests.clone();
    let body = serde_json::json!({"data":{"viewer":{"login":login}}}).to_string();
    let client = GitHubClient::try_with_request_sender(
        "identity-fixture",
        Arc::new(move |request, _| {
            let body = body.clone();
            assert_eq!(request.build().expect("request").url().path(), "/graphql");
            observed.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                Ok(http::Response::builder()
                    .status(200)
                    .body(body)
                    .expect("identity response")
                    .into())
            })
        }),
    )
    .expect("fixture client");
    let mut task = GitHubRepositoryTask {
        repository: repository.clone(),
        signal_reviewers: Vec::new(),
        subject_retention: MERGED_RETENTION,
        poll_request_budget: std::num::NonZeroUsize::MIN,
        clients: IdentityClient(client),
        store: store.clone(),
    };
    let result = task.poll_outcome(EventProducer::Webhook).await;
    assert_eq!(
        requests.load(Ordering::SeqCst),
        1,
        "identity lookup stays within the one-request attempt budget"
    );
    if login.is_some() {
        assert!(matches!(result, Ok(PollOutcome::Succeeded)));
        true
    } else {
        assert!(result.is_err());
        false
    }
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL"]
async fn observer_identity_gates_evaluation_and_self_review_exclusion_survives_rotation()
-> Result<(), Box<dyn Error>> {
    let (_container, core, url) = postgres().await?;
    migrate(&core).await?;
    let pool = module_pool(&url).await?;
    let store = RepoWatchStore::new(pool.clone());
    let repository = RepositorySlug::try_new("identity/project".to_owned())?;
    let rule = RepoWatchRule::try_new(
        RepoWatchRuleId::try_new("identity-gate".to_owned())?,
        RepoWatchRuleVersion::V1,
        RepoWatchMatcherV1::new(RepoWatchMatcherV1Input {
            event_kinds: vec![RepoWatchEventKindNameV1::ReviewSubmitted],
            ..Default::default()
        }),
        vec![RepoWatchRuleActionV1::DispatchSession {
            template: SessionTemplateName::try_new("watch".to_owned())?,
        }],
        RepoWatchSingletonScope::PullRequest,
        Duration::ZERO,
    )?;
    for review in [1, 2] {
        store
            .ingest_observation(
                &store.ingest_baseline(&repository).await?,
                &goal_review_observation(&repository, review),
                EventProducer::Poll,
                MERGED_RETENTION,
            )
            .await?;
        if review == 1 {
            store
                .reconcile_rules(
                    &[RepositoryRuleSet::new(
                        &repository,
                        std::slice::from_ref(&rule),
                    )],
                    OffsetDateTime::now_utc(),
                )
                .await?;
        }
    }
    assert!(store.next_rule_event(&repository, &rule).await?.is_some());

    store.prepare_observer_identity(&repository).await?;
    assert!(
        store.next_rule_event(&repository, &rule).await?.is_none(),
        "startup cannot evaluate ahead of identity resolution"
    );
    assert!(!identity_attempt(&store, &repository, None).await);
    assert!(
        store.next_rule_event(&repository, &rule).await?.is_none(),
        "a failed lookup keeps evaluation paused"
    );
    assert!(identity_attempt(&store, &repository, Some("Reviewer")).await);
    let own: bool = sqlx::query_scalar("SELECT self_review FROM gh_event WHERE repository=$1 AND event_kind='review_submitted' AND source_review_id=2")
        .bind(repository.as_str()).fetch_one(&pool).await?;
    assert!(
        own,
        "the first identity resolves retained review provenance"
    );
    store.prepare_observer_identity(&repository).await?;
    assert!(identity_attempt(&store, &repository, Some("another-account")).await);
    store
        .ingest_observation(
            &store.ingest_baseline(&repository).await?,
            &goal_review_observation(&repository, 3),
            EventProducer::Poll,
            MERGED_RETENTION,
        )
        .await?;
    let reviews: Vec<Decimal> = sqlx::query_scalar("SELECT stored.source_review_id FROM gh_readable_event readable JOIN gh_event stored USING(event_id) WHERE readable.repository=$1 AND readable.event_kind='review_submitted' ORDER BY readable.repository_event_ordinal")
        .bind(repository.as_str()).fetch_all(&pool).await?;
    assert_eq!(
        reviews,
        vec![Decimal::from(3_u64)],
        "old self reviews stay excluded; new reviews from the former account are eligible"
    );
    let rule = RepoWatchRule::try_new(
        RepoWatchRuleId::try_new("reviews".to_owned())?,
        RepoWatchRuleVersion::V1,
        RepoWatchMatcherV1::new(RepoWatchMatcherV1Input {
            event_kinds: vec![RepoWatchEventKindNameV1::ReviewSubmitted],
            ..Default::default()
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
            OffsetDateTime::now_utc(),
        )
        .await?;
    store
        .ingest_observation(
            &store.ingest_baseline(&repository).await?,
            &goal_review_observation(&repository, 4),
            EventProducer::Poll,
            MERGED_RETENTION,
        )
        .await?;
    let poisoned: uuid::Uuid = sqlx::query_scalar("UPDATE gh_event SET normalized_payload=$2 WHERE repository=$1 AND source_review_id=4 AND event_kind='review_submitted' RETURNING event_id")
        .bind(repository.as_str()).bind(b"not json".as_slice()).fetch_one(&pool).await?;
    assert!(store.next_rule_event(&repository, &rule).await?.is_none());
    let quarantined: bool =
        sqlx::query_scalar("SELECT decode_error IS NOT NULL FROM gh_event WHERE event_id=$1")
            .bind(poisoned)
            .fetch_one(&pool)
            .await?;
    assert!(
        quarantined,
        "identity filtering preserves ordinary event quarantine"
    );
    pool.close().await;
    core.close().await;
    Ok(())
}

fn observation_identity_client(requests: Arc<std::sync::Mutex<Vec<String>>>) -> IdentityClient {
    use signalbox_module_repo_watch_v2::github::GitHubClient;
    let pages = ConditionalPollFixture::new().pages;
    IdentityClient(
        GitHubClient::try_with_request_sender(
            "observation-identity-fixture",
            Arc::new(move |request, _| {
                let request = request.build().expect("fixture request");
                let path = request.url().path();
                let key = match request.url().query() {
                    Some(query) => format!("{path}?{query}"),
                    None => path.to_owned(),
                };
                requests.lock().expect("request log").push(key.clone());
                let body = if path == "/graphql" {
                    let body: serde_json::Value = serde_json::from_slice(
                        request.body().expect("GraphQL body").as_bytes().expect("request bytes"),
                    ).expect("GraphQL request");
                    if body["query"].as_str().expect("query").contains("RepositoryWatchActor") {
                        serde_json::json!({"data":{"viewer":{"login":"daemon"}}})
                    } else if body["query"].as_str().expect("query").contains("RequiredChecks") {
                        let head = &pages["/repos/example/project/pulls/1"].0["head"]["sha"];
                        serde_json::json!({"data":{"repository":{"pullRequest":{"headRefOid":head,"commits":{"nodes":[{"commit":{"oid":head,"statusCheckRollup":null}}]}}}}})
                    } else {
                        serde_json::json!({"data":{"repository":{"pullRequest":{"reviewThreads":{"nodes":[],"pageInfo":{"hasNextPage":false}}}}}})
                    }
                } else {
                    pages.get(&key).expect("fixture page").0.clone()
                };
                Box::pin(async move {
                    Ok(http::Response::builder()
                        .status(200)
                        .header("x-ratelimit-remaining", "5000")
                        .body(body.to_string())
                        .expect("fixture response")
                        .into())
                })
            }),
        ).expect("fixture client"),
    )
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL"]
async fn initial_identity_lookup_continues_the_poll_without_waiting_for_its_interval()
-> Result<(), Box<dyn Error>> {
    use signalbox_module_repo_watch_v2::{
        measurements::PollOutcome, provider::GitHubRepositoryTask,
    };
    let (_container, core, url) = postgres().await?;
    migrate(&core).await?;
    let pool = module_pool(&url).await?;
    let store = RepoWatchStore::new(pool.clone());
    let repository = RepositorySlug::try_new("example/project".to_owned())?;
    store.prepare_poll_cache(&repository, &[]).await?;
    store.prepare_observer_identity(&repository).await?;
    let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut task = GitHubRepositoryTask {
        repository,
        signal_reviewers: Vec::new(),
        subject_retention: MERGED_RETENTION,
        poll_request_budget: std::num::NonZeroUsize::MIN,
        clients: observation_identity_client(requests.clone()),
        store,
    };
    assert_eq!(
        task.poll_outcome(EventProducer::Poll)
            .await
            .map_err(|error| std::io::Error::other(error.to_string()))?,
        PollOutcome::Partial
    );
    assert_eq!(
        *requests.lock().expect("request log"),
        ["/graphql", "/rate_limit"],
        "identity and the resumed poll each spend their one-request allowance"
    );
    pool.close().await;
    core.close().await;
    Ok(())
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL"]
async fn identity_recovery_drains_the_triggering_webhook_wake() -> Result<(), Box<dyn Error>> {
    use signalbox_module_repo_watch_v2::{
        measurements::PollOutcome, provider::GitHubRepositoryTask,
    };
    let (_container, core, url) = postgres().await?;
    migrate(&core).await?;
    let pool = module_pool(&url).await?;
    let store = RepoWatchStore::new(pool.clone());
    let repository = RepositorySlug::try_new("example/project".to_owned())?;
    store.prepare_observer_identity(&repository).await?;
    assert!(!identity_attempt(&store, &repository, None).await);
    let now = OffsetDateTime::now_utc();
    let delivery = Uuid::now_v7();
    store
        .admit_webhook(WebhookDelivery {
            repository: &repository,
            hook_id: 1,
            delivery_id: delivery,
            event: "pull_request",
            action: Some("labeled"),
            body: br#"{"pull_request":{"number":1}}"#,
            received_at: now,
            expires_at: now + MERGED_RETENTION,
        })
        .await?;
    store
        .settle_webhook(1, delivery, WebhookDisposition::Applied, now)
        .await?;
    let mut task = GitHubRepositoryTask {
        repository: repository.clone(),
        signal_reviewers: Vec::new(),
        subject_retention: MERGED_RETENTION,
        poll_request_budget: std::num::NonZeroUsize::MIN,
        clients: observation_identity_client(Arc::new(std::sync::Mutex::new(Vec::new()))),
        store,
    };
    assert_eq!(
        task.poll_outcome(EventProducer::Webhook)
            .await
            .map_err(|error| std::io::Error::other(error.to_string()))?,
        PollOutcome::Succeeded
    );
    let remaining: i64 =
        sqlx::query_scalar("SELECT count(*) FROM webhook_pull_wake WHERE repository=$1")
            .bind(repository.as_str())
            .fetch_one(&pool)
            .await?;
    assert_eq!(
        remaining, 0,
        "the wake that resolved identity also observes its queued PR"
    );
    let webhook_events: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM gh_event WHERE repository=$1 AND producer='webhook'",
    )
    .bind(repository.as_str())
    .fetch_one(&pool)
    .await?;
    assert!(
        webhook_events > 0,
        "the observation preserves webhook provenance"
    );
    pool.close().await;
    core.close().await;
    Ok(())
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL"]
async fn a_new_polling_account_replaces_the_ready_identity_without_reload()
-> Result<(), Box<dyn Error>> {
    let (_container, core, url) = postgres().await?;
    let pool = module_pool(&url).await?;
    let store = RepoWatchStore::new(pool.clone());
    let repository = RepositorySlug::try_new("rotated/project".to_owned())?;
    assert!(identity_attempt(&store, &repository, Some("first-account")).await);
    assert!(identity_attempt(&store, &repository, Some("second-account")).await);
    let actor: (String, bool) =
        sqlx::query_as("SELECT login,ready FROM observer_actor WHERE repository=$1")
            .bind(repository.as_str())
            .fetch_one(&pool)
            .await?;
    assert_eq!(actor, ("second-account".to_owned(), true));
    assert!(!identity_attempt(&store, &repository, None).await);
    let ready: bool = sqlx::query_scalar("SELECT ready FROM observer_actor WHERE repository=$1")
        .bind(repository.as_str())
        .fetch_one(&pool)
        .await?;
    assert!(
        !ready,
        "a failed identity lookup cannot use the preceding account"
    );
    pool.close().await;
    core.close().await;
    Ok(())
}
