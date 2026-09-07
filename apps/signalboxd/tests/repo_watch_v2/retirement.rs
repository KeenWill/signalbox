use super::*;
use signalbox_ownership_seam::{
    LifecycleActor, LifecycleEventKind, SessionStateKind, SessionTerminal, SessionTerminalOutcome,
};

fn pull_observation(
    repository: &RepositorySlug,
    lifecycle: RepoWatchPullRequestLifecycle,
) -> signalbox_module_repo_watch_v2::ingest::RepositoryObservation {
    let mut observed = dispatch_observation(repository, 1, OffsetDateTime::now_utc());
    let pull = ComparisonPullRequestState::try_new(RepoWatchPullRequestStateInput {
        context: PullRequestEventContext::new(PullRequestEventContextInput {
            number: PullRequestNumber::new(NonZeroU64::new(1).expect("fixture PR")),
            head_sha: observed.default_head.clone(),
            head_repository: repository.clone(),
            base_branch: observed.default_branch.clone(),
            head_branch: BranchName::try_new(String::from("feature")).expect("branch"),
            title: PullRequestTitle::try_new(String::from("Retirement fixture")).expect("title"),
            body: PullRequestBody::try_new(String::new()).expect("body"),
            labels: Vec::new(),
            draft: false,
            author: None,
        }),
        lifecycle,
        mergeable_state: MergeableState::Unknown,
        completed_check_suites: Vec::new(),
        completed_check_runs: Vec::new(),
        reviews: Vec::new(),
        threads: Vec::new(),
        reactions: Vec::new(),
    })
    .expect("pull state");
    observed.observation = RepoWatchObservation::new(
        Vec::new(),
        RepoWatchRepositoryState::try_new(RepoWatchRepositoryStateInput {
            pull_requests: vec![pull],
            workflow_runs: Vec::new(),
            branch_heads: observed.observation.state().branch_heads().to_vec(),
        })
        .expect("observation"),
    );
    observed
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL"]
async fn closing_or_merging_retires_only_live_dispatched_sessions_and_replays_after_restart()
-> Result<(), Box<dyn Error>> {
    let (container, core_pool, url) = postgres().await?;
    migrate(&core_pool).await?;
    sqlx::query("ALTER ROLE mod_repo_watch PASSWORD 'signalbox-test-only'")
        .execute(&core_pool)
        .await?;
    let pool = module_pool(&url).await?;
    let store = RepoWatchStore::new(pool.clone());
    // Distinct command/session identities are arbitrary fixture data.
    let mut factory = FixtureSessionFactory {
        next_command: 10001,
        model: 20001,
    };
    let mut ids = FixedDispatchIds {
        value: 30001,
        calls: 0,
    };
    let mut codec = FixtureCommandCodec;
    for (name, lifecycle, reason) in [
        (
            "closed/project",
            RepoWatchPullRequestLifecycle::Closed,
            "pull_request_closed",
        ),
        (
            "merged/project",
            RepoWatchPullRequestLifecycle::Merged,
            "pull_request_merged",
        ),
    ] {
        let repository = RepositorySlug::try_new(name.to_owned())?;
        let now = OffsetDateTime::now_utc();
        let rule = RepoWatchRule::try_new(
            RepoWatchRuleId::try_new(String::from("opened"))?,
            RepoWatchRuleVersion::V1,
            RepoWatchMatcherV1::new(RepoWatchMatcherV1Input {
                repository: Some(repository.clone()),
                event_kinds: vec![RepoWatchEventKindNameV1::PullRequestOpened],
                ..RepoWatchMatcherV1Input::default()
            }),
            vec![
                RepoWatchRuleActionV1::DispatchSession {
                    template: SessionTemplateName::try_new(String::from("watch"))?
                };
                2
            ],
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
        store
            .ingest_observation(
                &store.ingest_baseline(&repository).await?,
                &pull_observation(&repository, RepoWatchPullRequestLifecycle::Open),
                EventProducer::Poll,
            )
            .await?;
        store
            .evaluate_next(&repository, &rule, &mut ids, &mut factory, &mut codec, now)
            .await
            .expect("dispatch");
        let pending = store.recover_pending_commands(&mut codec).await?;
        let creation = pending
            .iter()
            .find(|p| p.repository() == &repository)
            .expect("create command");
        let terminal_session = SessionId::from_uuid(Uuid::now_v7());
        store
            .react_to_lifecycle(
                &LifecycleEvent::session_created_for_test(
                    1,
                    now,
                    terminal_session,
                    SessionCreated {
                        cause: SessionCreationCause::ModuleDispatched {
                            dispatch: ModuleDispatch::RepositoryWatch {
                                dispatch: creation.dispatch(),
                            },
                        },
                        ownership: SessionOwnership::Owned,
                    },
                ),
                &mut factory,
                &mut codec,
            )
            .await?;
        store
            .react_to_lifecycle(
                &LifecycleEvent::for_test(
                    2,
                    now,
                    Some(terminal_session),
                    LifecycleEventKind::SessionTerminal(SessionTerminal {
                        prior: SessionStateKind::Created,
                        outcome: SessionTerminalOutcome::Stopped {
                            sticky: StopStickiness::Sticky,
                        },
                        standing: None,
                        actor: LifecycleActor::Operator,
                    }),
                ),
                &mut factory,
                &mut codec,
            )
            .await?;
        let session = SessionId::from_uuid(Uuid::now_v7());
        let created = LifecycleEvent::session_created_for_test(
            3,
            now,
            session,
            SessionCreated {
                cause: SessionCreationCause::ModuleDispatched {
                    dispatch: ModuleDispatch::RepositoryWatch {
                        dispatch: creation.dispatch(),
                    },
                },
                ownership: SessionOwnership::Owned,
            },
        );
        // Close before creation settles; retained facts must still retire the late session.
        store
            .ingest_observation(
                &store.ingest_baseline(&repository).await?,
                &pull_observation(&repository, lifecycle),
                EventProducer::Poll,
            )
            .await?;
        store
            .reconcile_rules(&[RepositoryRuleSet::new(&repository, &[])], now)
            .await?;
        store
            .react_to_pull_request_lifecycle(&mut factory, &mut codec)
            .await?;
        let before: i64 = sqlx::query_scalar("SELECT count(*) FROM dispatch_ledger WHERE repository = $1 AND retirement_reason IS NOT NULL").bind(name).fetch_one(&pool).await?;
        assert_eq!(before, 0, "unsettled creation is left pending");
        store
            .react_to_lifecycle(&created, &mut factory, &mut codec)
            .await?;
        store
            .react_to_pull_request_lifecycle(&mut factory, &mut codec)
            .await?;
        let restarted = RepoWatchStore::new(pool.clone());
        let commands = restarted.recover_pending_commands(&mut codec).await?;
        let command = commands
            .iter()
            .find(|p| p.repository() == &repository)
            .expect("retirement command")
            .command();
        let command_id = command.command_id();
        let SessionCommandPayload::Lifecycle(stop) = command.clone().into_payload() else {
            panic!("retirement uses lifecycle command");
        };
        assert_eq!(stop.session(), session);
        assert_eq!(
            *stop.operation(),
            SessionLifecycleOperation::Stop {
                sticky: StopStickiness::Sticky,
                descendant_scope: DescendantTerminationScope::ParentAlone
            }
        );
        restarted
            .react_to_pull_request_lifecycle(&mut factory, &mut codec)
            .await?;
        restarted
            .react_to_lifecycle(&created, &mut factory, &mut codec)
            .await?;
        let rows: Vec<(Uuid, String)> = sqlx::query_as("SELECT command_id, retirement_reason FROM dispatch_ledger WHERE repository = $1 AND retirement_event_id IS NOT NULL").bind(name).fetch_all(&pool).await?;
        assert_eq!(
            rows,
            vec![(command_id.into_uuid(), reason.to_owned())],
            "restart and replay retain one exact stop and reason after rule removal"
        );
    }
    pool.close().await;
    core_pool.close().await;
    drop(container);
    Ok(())
}
