use super::*;
use signalbox_session_ownership::RepoWatchEventTarget;

fn observation(
    repository: &RepositorySlug,
    state: MergeableState,
    lifecycle: RepoWatchPullRequestLifecycle,
    head: &CommitSha,
    now: OffsetDateTime,
) -> signalbox_module_repo_watch_v2::ingest::RepositoryObservation {
    let mut observed = goal_review_observation(repository, 1);
    let original = &observed.observation.state().pull_requests()[0];
    let mut context = original.context().clone();
    context = PullRequestEventContext::new(PullRequestEventContextInput {
        number: context.number(),
        head_sha: head.clone(),
        head_repository: repository.clone(),
        base_branch: context.base_branch().clone(),
        head_branch: context.head_branch().clone(),
        title: context.title().clone(),
        body: context.body().clone(),
        labels: vec![],
        draft: false,
        author: None,
    });
    observed.observed_at = now;
    observed.observation = RepoWatchObservation::new(
        vec![],
        RepoWatchRepositoryState::try_new(RepoWatchRepositoryStateInput {
            pull_requests: vec![
                ComparisonPullRequestState::try_new(RepoWatchPullRequestStateInput {
                    context,
                    lifecycle,
                    mergeable_state: state,
                    completed_check_suites: vec![],
                    completed_check_runs: vec![],
                    reviews: vec![],
                    threads: vec![],
                    reactions: vec![],
                })
                .expect("pull"),
            ],
            branch_heads: vec![],
            workflow_runs: vec![],
        })
        .expect("repository"),
    );
    observed
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL"]
async fn ended_conflict_dispatch_retries_after_cooldown_without_a_new_matching_event()
-> Result<(), Box<dyn Error>> {
    let (_database, core, url) = postgres().await?;
    let pool = module_pool(&url).await?;
    let store = RepoWatchStore::new(pool.clone());
    let source = signalbox_session_ownership::LifecycleEventSource::new(core);
    let repository = RepositorySlug::try_new("retry/project".to_owned())?;
    let now = OffsetDateTime::now_utc();
    let first_head = CommitSha::try_new("1111111111111111111111111111111111111111".to_owned())?;
    let later_head = CommitSha::try_new("2222222222222222222222222222222222222222".to_owned())?;
    let rule = RepoWatchRule::try_new(
        RepoWatchRuleId::try_new("retry".to_owned())?,
        RepoWatchRuleVersion::V1,
        RepoWatchMatcherV1::new(RepoWatchMatcherV1Input {
            event_kinds: vec![RepoWatchEventKindNameV1::MergeableStateChanged],
            mergeable_state: vec![MergeableState::Conflicting],
            ..Default::default()
        }),
        vec![RepoWatchRuleActionV1::DispatchSession {
            template: SessionTemplateName::try_new("watch".to_owned())?,
        }],
        RepoWatchSingletonScope::PullRequest,
        Duration::from_secs(5),
    )?;
    let initial = observation(
        &repository,
        MergeableState::Mergeable,
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
    store
        .reconcile_rules(
            &[RepositoryRuleSet::new(
                &repository,
                std::slice::from_ref(&rule),
            )],
            now,
        )
        .await?;
    let conflict = observation(
        &repository,
        MergeableState::Conflicting,
        RepoWatchPullRequestLifecycle::Open,
        &first_head,
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
    let mut ids = FixedDispatchIds {
        value: 71001,
        calls: 0,
    };
    let mut factory = FixtureSessionFactory {
        next_command: 72001,
        model: 73001,
    };
    let mut codec = FixtureCommandCodec;
    assert!(
        store
            .evaluate_next(&repository, &rule, &mut ids, &mut factory, &mut codec, now)
            .await
            .expect("evaluate")
    );
    let first = store.recover_pending_commands(&mut codec).await?;
    assert_eq!(first.len(), 1);
    let session = SessionId::from_uuid(Uuid::from_u128(74001));
    assert!(
        store
            .apply_lifecycle_event(&LifecycleEvent::session_created_for_test(
                1,
                now,
                session,
                SessionCreated {
                    cause: SessionCreationCause::ModuleDispatched {
                        dispatch: ModuleDispatch::RepositoryWatch {
                            dispatch: first[0].dispatch()
                        }
                    },
                    ownership: SessionOwnership::Owned,
                }
            ))
            .await?
    );
    // The session's nonsticky terminal settlement is retained by the lifecycle consumer.
    sqlx::query("UPDATE dispatch_ledger SET session_terminal_at=$2,singleton_released_at=$2 WHERE created_session_id=$1")
        .bind(session.into_uuid()).bind(now).execute(&pool).await?;
    assert!(
        !store
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
            .expect("retry")
    );
    // Closing and reopening before the retry must not retire the new session for the old closure.
    let closed = observation(
        &repository,
        MergeableState::Conflicting,
        RepoWatchPullRequestLifecycle::Closed,
        &first_head,
        now + Duration::from_secs(1),
    );
    store
        .ingest_observation(
            &store.ingest_baseline(&repository).await?,
            &closed,
            EventProducer::Poll,
            MERGED_RETENTION,
        )
        .await?;
    // A new remote head must be frozen into the retry's checkout, even though the conflict is unchanged.
    let advanced = observation(
        &repository,
        MergeableState::Conflicting,
        RepoWatchPullRequestLifecycle::Open,
        &later_head,
        now + Duration::from_secs(2),
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
    // A committed workflow evaluation still awaiting acknowledgement owns this rule's admission.
    sqlx::query("UPDATE rule_evaluation_cursor SET effect_id=$1, effect_input=$2, effect_result=$3 WHERE repository=$4 AND rule_id=$5")
        .bind(Uuid::from_u128(75001)).bind(b"pending evaluation".as_slice()).bind(b"suppressed".as_slice())
        .bind(repository.as_str()).bind(rule.id().as_str()).execute(&pool).await?;
    assert!(
        !restarted
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
            .expect("pending evaluation")
    );
    sqlx::query("UPDATE rule_evaluation_cursor SET effect_id=NULL, effect_input=NULL, effect_result=NULL WHERE repository=$1 AND rule_id=$2")
        .bind(repository.as_str()).bind(rule.id().as_str()).execute(&pool).await?;
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
            .expect("retry")
    );
    assert!(
        !store
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
            .expect("retry")
    );
    let pending = restarted.recover_pending_commands(&mut codec).await?;
    assert_eq!(pending.len(), 1);
    assert_ne!(pending[0].dispatch(), first[0].dispatch());
    let checkout = restarted
        .dispatch_checkout(pending[0].command().command_id())
        .await?
        .expect("retry checkout");
    let RepoWatchEventTarget::PullRequest(context) = checkout.event.target() else {
        panic!("pull request")
    };
    assert_eq!(context.head_sha(), &later_head);
    let events_after: i64 = sqlx::query_scalar("SELECT count(*) FROM gh_event")
        .fetch_one(&pool)
        .await?;
    assert_eq!(events_after, events_before);
    let parent: Uuid =
        sqlx::query_scalar("SELECT retry_of FROM dispatch_ledger WHERE command_id=$1")
            .bind(pending[0].command().command_id().into_uuid())
            .fetch_one(&pool)
            .await?;
    assert_eq!(parent, first[0].dispatch().into_uuid());
    let retry_session = SessionId::from_uuid(Uuid::from_u128(74002));
    restarted
        .apply_lifecycle_event(&LifecycleEvent::session_created_for_test(
            2,
            now,
            retry_session,
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
    restarted
        .react_to_pull_request_lifecycle(&mut factory, &mut codec, &source)
        .await?;
    let retirements: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM dispatch_ledger WHERE retirement_event_id IS NOT NULL",
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(retirements, 0);
    // A sticky terminal session does not release its singleton for another retry.
    sqlx::query("UPDATE dispatch_ledger SET session_terminal_at=$2 WHERE created_session_id=$1")
        .bind(retry_session.into_uuid())
        .bind(now)
        .execute(&pool)
        .await?;
    assert!(
        !restarted
            .retry_due(
                &repository,
                &rule,
                &mut ids,
                &mut factory,
                &mut codec,
                &source,
                now + Duration::from_secs(20)
            )
            .await
            .expect("sticky retry")
    );
    Ok(())
}
