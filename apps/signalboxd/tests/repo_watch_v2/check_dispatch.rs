use super::*;

fn observation(
    repository: &RepositorySlug,
    head: &CommitSha,
    generation: u64,
    review: bool,
) -> Result<signalbox_module_repo_watch_v2::ingest::RepositoryObservation, Box<dyn Error>> {
    let mut observed = goal_review_observation(repository, generation);
    let mut pull = RepoWatchPullRequestStateInput {
        context: PullRequestEventContext::new(PullRequestEventContextInput {
            number: PullRequestNumber::new(NonZeroU64::MIN),
            head_sha: head.clone(),
            head_repository: repository.clone(),
            base_branch: observed.default_branch.clone(),
            head_branch: BranchName::try_new("fix-checks".to_owned())?,
            title: PullRequestTitle::try_new("Check completion".to_owned())?,
            body: PullRequestBody::try_new(String::new())?,
            labels: vec![],
            draft: false,
            author: None,
        }),
        lifecycle: RepoWatchPullRequestLifecycle::Open,
        mergeable_state: MergeableState::Mergeable,
        completed_check_suites: vec![],
        completed_check_runs: vec![],
        reviews: vec![],
        threads: vec![],
        reactions: vec![],
    };
    let id = GitHubObjectId::new(NonZeroU64::new(generation).expect("positive generation"));
    if review {
        pull.reviews.push(RepoWatchReviewObservation::new(
            id,
            RepoWatchAuthorLogin::try_new("reviewer".to_owned())?,
            Some(ReviewState::ChangesRequested),
            head.clone(),
        ));
    } else {
        // A suite and a run are separate facts for the same completed workflow.
        let completion = RepoWatchCheckCompletionGeneration::try_new(generation.to_string())?;
        pull.completed_check_suites
            .push(RepoWatchCheckSuiteObservation::new(
                id,
                completion.clone(),
                ChecksOutcome::Success,
            ));
        pull.completed_check_runs
            .push(RepoWatchCheckRunObservation::new(
                id,
                completion,
                CheckRunName::try_new("validate".to_owned())?,
                CheckConclusion::Success,
            ));
    }
    observed.observation = RepoWatchObservation::new(
        vec![],
        RepoWatchRepositoryState::try_new(RepoWatchRepositoryStateInput {
            pull_requests: vec![ComparisonPullRequestState::try_new(pull)?],
            branch_heads: vec![],
            workflow_runs: vec![],
        })?,
    );
    Ok(observed)
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL"]
async fn check_completions_coalesce_across_cooldown_and_restart_but_new_heads_and_reviews_dispatch()
-> Result<(), Box<dyn Error>> {
    let (_database, _core, url) = postgres().await?;
    let pool = module_pool(&url).await?;
    let repository = RepositorySlug::try_new("checks/project".to_owned())?;
    let now = OffsetDateTime::now_utc();
    let rule = RepoWatchRule::try_new(
        RepoWatchRuleId::try_new("labeled-review-response".to_owned())?,
        RepoWatchRuleVersion::V1,
        RepoWatchMatcherV1::new(RepoWatchMatcherV1Input {
            event_kinds: vec![
                RepoWatchEventKindNameV1::ChecksCompleted,
                RepoWatchEventKindNameV1::CheckRunCompleted,
                RepoWatchEventKindNameV1::ReviewSubmitted,
            ],
            ..Default::default()
        }),
        vec![RepoWatchRuleActionV1::DispatchSession {
            template: SessionTemplateName::try_new("watch".to_owned())?,
        }],
        RepoWatchSingletonScope::PullRequest,
        Duration::from_secs(300),
    )?;
    RepoWatchStore::new(pool.clone())
        .reconcile_rules(
            &[RepositoryRuleSet::new(
                &repository,
                std::slice::from_ref(&rule),
            )],
            now,
        )
        .await?;
    // Distinct fixture heads represent two pushes. Each step occurs beyond cooldown.
    let first = CommitSha::try_new("1".repeat(40))?;
    let second = CommitSha::try_new("2".repeat(40))?;
    // Arbitrary identities belong only to this disposable database.
    let mut ids = FixedDispatchIds {
        value: 81001,
        calls: 0,
    };
    let mut factory = FixtureSessionFactory {
        next_command: 82001,
        model: 83001,
    };
    let mut codec = FixtureCommandCodec;
    for (generation, head, review, expected) in [
        (1, &first, false, 1_i64),
        (2, &first, false, 1),
        (3, &first, true, 2),
        (4, &second, false, 3),
        (5, &second, false, 3),
    ] {
        let store = RepoWatchStore::new(pool.clone());
        let at = now + Duration::from_secs(generation * 600);
        store
            .ingest_observation(
                &store.ingest_baseline(&repository).await?,
                &observation(&repository, head, generation, review)?,
                EventProducer::Poll,
                MERGED_RETENTION,
            )
            .await?;
        while store
            .evaluate_next(&repository, &rule, &mut ids, &mut factory, &mut codec, at)
            .await
            .expect("evaluate completion")
        {}
        let count: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM dispatch_ledger WHERE command_kind='create_session'",
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(count, expected, "generation {generation}");
        // Retain a provisioned head after nonsticky completion and checkout removal.
        sqlx::query(
            "UPDATE dispatch_ledger SET checkout_path='.', checkout_head_sha=$1,
            checkout_removed=true, singleton_released_at=$2 WHERE singleton_released_at IS NULL",
        )
        .bind(head.as_str())
        .bind(at)
        .execute(&pool)
        .await?;
    }
    Ok(())
}
