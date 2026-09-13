use super::*;
use signalbox_session_ownership::LifecycleEventSource;

fn review_observation(
    repository: &RepositorySlug,
    review: u64,
    at: OffsetDateTime,
) -> signalbox_module_repo_watch_v2::ingest::RepositoryObservation {
    let mut observed = goal_review_observation(repository, review);
    let pull = &observed.observation.state().pull_requests()[0];
    observed.observed_at = at;
    observed.observation = RepoWatchObservation::new(
        vec![],
        RepoWatchRepositoryState::try_new(RepoWatchRepositoryStateInput {
            pull_requests: vec![
                ComparisonPullRequestState::try_new(RepoWatchPullRequestStateInput {
                    context: pull.context().clone(),
                    lifecycle: RepoWatchPullRequestLifecycle::Open,
                    mergeable_state: MergeableState::Mergeable,
                    required_check_conclusions: None,
                    completed_check_suites: vec![],
                    completed_check_runs: vec![],
                    reviews: pull.reviews().to_vec(),
                    threads: vec![RepoWatchThreadObservation::open(
                        ReviewThreadId::try_new("unresolved-review".to_owned()).expect("thread"),
                        None,
                    )],
                    reactions: vec![],
                })
                .expect("pull request"),
            ],
            branch_heads: vec![],
            workflow_runs: vec![],
        })
        .expect("repository"),
    );
    observed
}

/// The lifecycle reader consumes a completed-push projection in an isolated schema;
/// the module's dispatch, event, cursor and observation tables use their migrations.
async fn pushed_session_source(
    core: &PgPool,
    url: &str,
    session: SessionId,
) -> Result<LifecycleEventSource, Box<dyn Error>> {
    sqlx::raw_sql("CREATE SCHEMA push_evidence;
        CREATE TABLE push_evidence.tool_request (request_id uuid PRIMARY KEY, tool_name text);
        CREATE TABLE push_evidence.tool_attempt (request_id uuid, session_id uuid, terminal_disposition_kind text)")
        .execute(core).await?;
    let request = Uuid::new_v4();
    sqlx::query("INSERT INTO push_evidence.tool_request VALUES ($1,'git_push_configured')")
        .bind(request)
        .execute(core)
        .await?;
    sqlx::query("INSERT INTO push_evidence.tool_attempt VALUES ($1,$2,'completed')")
        .bind(request)
        .bind(session.into_uuid())
        .execute(core)
        .await?;
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .after_connect(|connection, _| {
            Box::pin(async move {
                sqlx::query("SET search_path=push_evidence,pg_catalog")
                    .execute(connection)
                    .await?;
                Ok(())
            })
        })
        .connect_with(local_test_connection_options(url)?)
        .await?;
    Ok(LifecycleEventSource::new(pool))
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL"]
async fn fresh_review_consumed_during_cooldown_dispatches_after_a_successful_push()
-> Result<(), Box<dyn Error>> {
    let (_database, core, url) = postgres().await?;
    let pool = module_pool(&url).await?;
    let store = RepoWatchStore::new(pool.clone());
    let repository = RepositorySlug::try_new("cooldown/project".to_owned())?;
    let now = OffsetDateTime::now_utc();
    let cooldown = Duration::from_secs(300);
    let rule = RepoWatchRule::try_new(
        RepoWatchRuleId::try_new("review-response".to_owned())?,
        RepoWatchRuleVersion::V1,
        RepoWatchMatcherV1::new(RepoWatchMatcherV1Input {
            event_kinds: vec![RepoWatchEventKindNameV1::ReviewSubmitted],
            ..Default::default()
        }),
        vec![RepoWatchRuleActionV1::DispatchSession {
            template: SessionTemplateName::try_new("watch".to_owned())?,
        }],
        RepoWatchSingletonScope::PullRequest,
        cooldown,
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
    let initial = review_observation(&repository, 1, now);
    store
        .ingest_observation(
            &store.ingest_baseline(&repository).await?,
            &initial,
            EventProducer::Poll,
            MERGED_RETENTION,
        )
        .await?;
    let mut ids = FixedDispatchIds {
        value: 91001,
        calls: 0,
    };
    let mut factory = FixtureSessionFactory {
        next_command: 92001,
        model: 93001,
    };
    let mut codec = FixtureCommandCodec;
    while store
        .evaluate_next(&repository, &rule, &mut ids, &mut factory, &mut codec, now)
        .await
        .expect("evaluate initial review")
    {}
    let first = store.recover_pending_commands(&mut codec).await?;
    assert_eq!(first.len(), 1);
    let session = SessionId::from_uuid(Uuid::from_u128(94001));
    store
        .apply_lifecycle_event(&LifecycleEvent::session_created_for_test(
            1,
            now,
            session,
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
    let released = now + Duration::from_secs(60);
    sqlx::query("UPDATE dispatch_ledger SET session_terminal_at=$2,singleton_released_at=$2 WHERE created_session_id=$1")
        .bind(session.into_uuid()).bind(released).execute(&pool).await?;
    let source = pushed_session_source(&core, &url, session).await?;
    assert!(source.session_pushed(session).await?);
    assert!(
        !store
            .retry_due(
                &repository,
                &rule,
                &mut ids,
                &mut factory,
                &mut codec,
                &source,
                released + cooldown
            )
            .await
            .expect("successful push alone must not retry")
    );

    let reviewed = released + Duration::from_secs(270);
    let fresh = review_observation(&repository, 2, reviewed);
    store
        .ingest_observation(
            &store.ingest_baseline(&repository).await?,
            &fresh,
            EventProducer::Poll,
            MERGED_RETENTION,
        )
        .await?;
    let event: Uuid = sqlx::query_scalar("SELECT event_id FROM gh_event WHERE event_kind='review_submitted' ORDER BY repository_event_ordinal DESC LIMIT 1").fetch_one(&pool).await?;
    while store
        .evaluate_next(
            &repository,
            &rule,
            &mut ids,
            &mut factory,
            &mut codec,
            reviewed,
        )
        .await
        .expect("consume review during cooldown")
    {}
    assert!(store.recover_pending_commands(&mut codec).await?.is_empty());
    assert!(
        !store
            .retry_due(
                &repository,
                &rule,
                &mut ids,
                &mut factory,
                &mut codec,
                &source,
                released + cooldown - Duration::from_secs(1)
            )
            .await
            .expect("cooldown must hold")
    );

    let restarted = RepoWatchStore::new(pool.clone());
    assert!(
        restarted
            .retry_due(
                &repository,
                &rule,
                &mut ids,
                &mut factory,
                &mut codec,
                &source,
                released + cooldown
            )
            .await
            .expect("fresh review must survive cooldown after a push")
    );
    let pending = restarted.recover_pending_commands(&mut codec).await?;
    assert_eq!(pending.len(), 1);
    let (retained_event, parent): (Uuid, Uuid) =
        sqlx::query_as("SELECT event_id,retry_of FROM dispatch_ledger WHERE command_id=$1")
            .bind(pending[0].command().command_id().into_uuid())
            .fetch_one(&pool)
            .await?;
    assert_eq!(
        retained_event, event,
        "admission must retain the fresh review provenance"
    );
    assert_eq!(parent, first[0].dispatch().into_uuid());
    assert!(
        !restarted
            .retry_due(
                &repository,
                &rule,
                &mut ids,
                &mut factory,
                &mut codec,
                &source,
                released + cooldown
            )
            .await
            .expect("fresh review must not dispatch twice")
    );
    Ok(())
}
