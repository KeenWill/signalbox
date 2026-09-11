//! Provider-keyed facts retain their durable identity across presentation edits.

use super::*;

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn provider_facts_returning_on_the_same_head_after_presentation_edits_are_coalesced()
-> Result<(), Box<dyn Error>> {
    let (container, core_pool, url) = postgres().await?;
    migrate(&core_pool).await?;
    let pool = module_pool(&url).await?;
    let store = RepoWatchStore::new(pool.clone());
    let repository = RepositorySlug::try_new(String::from("provider-identity/project"))?;
    let head = CommitSha::try_new("a".repeat(40))?;
    let initial = PullRequestTitle::try_new(String::from("Initial title"))?;
    let edited = PullRequestTitle::try_new(String::from("Edited title"))?;
    let reviewer = RepoWatchAuthorLogin::try_new(String::from("reviewer"))?;
    let suite = RepoWatchCheckSuiteObservation::new(
        GitHubObjectId::new(NonZeroU64::MIN),
        RepoWatchCheckCompletionGeneration::try_new(String::from("completion-1"))?,
        ChecksOutcome::Success,
    );
    let review = RepoWatchReviewObservation::new(
        GitHubObjectId::new(NonZeroU64::MIN),
        reviewer.clone(),
        Some(ReviewState::Approved),
        head.clone(),
    );
    let labels = vec![LabelName::try_new(String::from("ready"))?];
    let mut admissions = Vec::new();
    for (observed_head, title, observed_labels, suites, reviews) in [
        (&head, &initial, Vec::new(), Vec::new(), Vec::new()),
        (
            &head,
            &initial,
            Vec::new(),
            vec![suite.clone()],
            vec![review.clone()],
        ),
        (&head, &edited, labels.clone(), Vec::new(), Vec::new()),
        (&head, &edited, labels, vec![suite], vec![review]),
    ] {
        let branch = BranchName::try_new(String::from("main"))?;
        let pull = ComparisonPullRequestState::try_new(RepoWatchPullRequestStateInput {
            context: PullRequestEventContext::new(PullRequestEventContextInput {
                number: PullRequestNumber::new(NonZeroU64::MIN),
                head_sha: observed_head.clone(),
                head_repository: repository.clone(),
                base_branch: branch.clone(),
                head_branch: BranchName::try_new(String::from("topic"))?,
                title: title.clone(),
                body: PullRequestBody::try_new(String::new())?,
                labels: observed_labels,
                draft: false,
                author: None,
            }),
            lifecycle: RepoWatchPullRequestLifecycle::Open,
            mergeable_state: MergeableState::Unknown,
            completed_check_suites: suites,
            completed_check_runs: Vec::new(),
            reviews,
            threads: Vec::new(),
            reactions: Vec::new(),
        })?;
        let observed = signalbox_module_repo_watch_v2::ingest::RepositoryObservation {
            repository: repository.clone(),
            default_branch: branch,
            default_head: head.clone(),
            observed_at: OffsetDateTime::now_utc(),
            merged_at: std::collections::BTreeMap::new(),
            observation: RepoWatchObservation::new(
                vec![reviewer.clone()],
                RepoWatchRepositoryState::try_new(RepoWatchRepositoryStateInput {
                    pull_requests: vec![pull],
                    workflow_runs: Vec::new(),
                    branch_heads: Vec::new(),
                })?,
            ),
        };
        let admission = store
            .ingest_observation(
                &store.ingest_baseline(&repository).await?,
                &observed,
                EventProducer::Poll,
                MERGED_RETENTION,
            )
            .await?;
        let FrontierEventAdmission::Committed { events, .. } = admission else {
            panic!("expected committed observation, got {admission:?}");
        };
        admissions.push(events);
    }
    assert_eq!(
        admissions.last().map(AsRef::as_ref),
        Some([EventAdmission::Replayed, EventAdmission::Replayed].as_slice()),
    );
    let kinds: Vec<String> = sqlx::query_scalar(
        "SELECT event_kind FROM gh_event WHERE repository = $1
           AND event_kind IN ('checks_completed', 'review_submitted')
         ORDER BY repository_event_ordinal",
    )
    .bind(repository.as_str())
    .fetch_all(&pool)
    .await?;
    assert_eq!(kinds, ["checks_completed", "review_submitted"]);
    let retained_titles: Vec<String> = sqlx::query_scalar(
        "SELECT convert_from(normalized_payload, 'UTF8')::jsonb #>> '{target,title}'
           FROM gh_event WHERE repository = $1
            AND event_kind IN ('checks_completed', 'review_submitted')",
    )
    .bind(repository.as_str())
    .fetch_all(&pool)
    .await?;
    assert_eq!(retained_titles, [initial.as_str(), initial.as_str()]);
    let title: String = sqlx::query_scalar("SELECT title FROM pr_state WHERE repository = $1")
        .bind(repository.as_str())
        .fetch_one(&pool)
        .await?;
    assert_eq!(title, edited.as_str());

    pool.close().await;
    core_pool.close().await;
    drop(container);
    Ok(())
}
