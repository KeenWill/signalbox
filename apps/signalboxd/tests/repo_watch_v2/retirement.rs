use super::*;
use signalbox_session_ownership::{
    GoalChange, GoalEventKind, LifecycleActor, LifecycleEventKind, SessionStateKind,
    SessionTerminal, SessionTerminalOutcome,
};

async fn persist_creation(
    pool: &PgPool,
    command: &SessionCommand,
) -> Result<SessionId, Box<dyn Error>> {
    let SessionCommandPayload::CreateSession(command) = command.clone().into_payload() else {
        panic!("creation command");
    };
    let models = signalboxd::HubModelConfiguration::parse(
        &include_str!("../../../../config/signalboxd.example.toml").replace(
            "/usr/local/bin/signalbox-exec-supervisor",
            std::env::current_exe()?.to_string_lossy().as_ref(),
        ),
    )?;
    let session = SessionId::from_uuid(Uuid::now_v7());
    let repository = signalbox_persistence::create_session::CreateSessionRepository::new(
        pool.clone(),
        models.session_credential_pin(),
    )
    .with_principal(signalbox_domain::CommandPrincipal::Module {
        module: signalbox_domain::DispatchingModule::RepositoryWatch,
    });
    repository
        .handle(command.prepare(session).expect("prepared fixture session"))
        .await?;
    Ok(session)
}

async fn close_in_core(pool: &PgPool, session: SessionId) -> Result<(), Box<dyn Error>> {
    signalbox_persistence::session_lifecycle_command::SessionLifecycleCommandRepository::new(
        pool.clone(),
    )
    .handle(
        SessionLifecycleCommand::new(
            DurableCommandId::from_uuid(Uuid::now_v7()),
            session,
            SessionLifecycleOperation::Stop {
                sticky: StopStickiness::Sticky,
                descendant_scope: DescendantTerminationScope::ParentAlone,
            },
        ),
        signalbox_domain::CommandPrincipal::Operator,
    )
    .await?;
    Ok(())
}

struct NoSubmission;

impl signalbox_module_repo_watch_v2::dispatch::SessionCommandSink for NoSubmission {
    type Error = std::convert::Infallible;

    async fn submit(
        &mut self,
        _: SessionCommand,
    ) -> Result<signalbox_module_repo_watch_v2::dispatch::CommandSubmission, Self::Error> {
        panic!("a retirement for a terminal session must not reach the core command sink")
    }
}

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
    if lifecycle == RepoWatchPullRequestLifecycle::Merged {
        observed.merged_at.insert(
            observed.observation.state().pull_requests()[0]
                .context()
                .number(),
            observed
                .observed_at
                .replace_nanosecond(0)
                .expect("second precision"),
        );
    }
    observed
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL"]
async fn closing_or_merging_retires_only_live_dispatched_sessions_and_replays_after_restart()
-> Result<(), Box<dyn Error>> {
    let (container, core_pool, url) = postgres().await?;
    migrate(&core_pool).await?;
    let source = signalbox_session_ownership::LifecycleEventSource::new(core_pool.clone());
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
                MERGED_RETENTION,
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
        let terminal_session = persist_creation(&core_pool, creation.command()).await?;
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
                &source,
            )
            .await?;
        close_in_core(&core_pool, terminal_session).await?;
        let terminal_at = source
            .session_terminal_at(terminal_session)
            .await?
            .expect("durable terminal time");
        if lifecycle == RepoWatchPullRequestLifecycle::Closed {
            store
                .react_to_lifecycle(
                    &LifecycleEvent::for_test(
                        2,
                        terminal_at,
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
                    &source,
                )
                .await?;
        }
        let known_terminal: bool = sqlx::query_scalar(
            "SELECT session_terminal_at IS NOT NULL FROM dispatch_ledger WHERE created_session_id = $1",
        )
        .bind(terminal_session.into_uuid())
        .fetch_one(&pool)
        .await?;
        assert_eq!(
            known_terminal,
            lifecycle == RepoWatchPullRequestLifecycle::Closed
        );
        let second_creation = pending
            .iter()
            .filter(|p| p.repository() == &repository)
            .nth(1)
            .expect("second creation");
        let session = persist_creation(&core_pool, second_creation.command()).await?;
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
                MERGED_RETENTION,
            )
            .await?;
        store
            .reconcile_rules(&[RepositoryRuleSet::new(&repository, &[])], now)
            .await?;
        store
            .react_to_pull_request_lifecycle(&mut factory, &mut codec, &source)
            .await?;
        let retained_terminal_at: Option<OffsetDateTime> = sqlx::query_scalar(
            "SELECT session_terminal_at FROM dispatch_ledger WHERE created_session_id = $1",
        )
        .bind(terminal_session.into_uuid())
        .fetch_one(&pool)
        .await?;
        assert_eq!(
            retained_terminal_at,
            Some(terminal_at),
            "durable terminal facts leave the live retirement scan even before cursor delivery"
        );
        let before: i64 = sqlx::query_scalar("SELECT count(*) FROM dispatch_ledger WHERE repository = $1 AND retirement_reason IS NOT NULL").bind(name).fetch_one(&pool).await?;
        assert_eq!(before, 0, "unsettled creation is left pending");
        store
            .react_to_lifecycle(&created, &mut factory, &mut codec, &source)
            .await?;
        let restarted = RepoWatchStore::new(pool.clone());
        let commands = restarted.recover_pending_commands(&mut codec).await?;
        let command = commands
            .iter()
            .find(|p| p.repository() == &repository)
            .expect("processing creation retires the session without an idle worker tick")
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
            .react_to_pull_request_lifecycle(&mut factory, &mut codec, &source)
            .await?;
        restarted
            .react_to_lifecycle(&created, &mut factory, &mut codec, &source)
            .await?;
        let rows: Vec<(Uuid, String)> = sqlx::query_as("SELECT command_id, retirement_reason FROM dispatch_ledger WHERE repository = $1 AND retirement_event_id IS NOT NULL").bind(name).fetch_all(&pool).await?;
        assert_eq!(
            rows,
            vec![(command_id.into_uuid(), reason.to_owned())],
            "restart and replay retain one exact stop and reason after rule removal"
        );
        close_in_core(&core_pool, session).await?;
        assert!(source.session_terminal_at(session).await?.is_some());
        restarted
            .submit_pending(&mut codec, &mut NoSubmission, &source)
            .await
            .expect("terminal retirement is discarded before submission");
        assert!(
            restarted
                .recover_pending_commands(&mut codec)
                .await?
                .is_empty()
        );
        let rejection: String =
            sqlx::query_scalar("SELECT rejection_kind FROM dispatch_ledger WHERE command_id = $1")
                .bind(command_id.into_uuid())
                .fetch_one(&pool)
                .await?;
        assert_eq!(rejection, "session_already_terminal");
    }
    pool.close().await;
    core_pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL"]
async fn quarantined_dispatch_origin_still_releases_and_retires_its_session()
-> Result<(), Box<dyn Error>> {
    let (container, core_pool, url) = postgres().await?;
    migrate(&core_pool).await?;
    let source = signalbox_session_ownership::LifecycleEventSource::new(core_pool.clone());
    let pool = module_pool(&url).await?;
    let store = RepoWatchStore::new(pool.clone());
    let repository = RepositorySlug::try_new(String::from("quarantine/project"))?;
    let now = OffsetDateTime::now_utc();
    let rule = RepoWatchRule::try_new(
        RepoWatchRuleId::try_new(String::from("opened"))?,
        RepoWatchRuleVersion::V1,
        RepoWatchMatcherV1::new(RepoWatchMatcherV1Input {
            repository: Some(repository.clone()),
            event_kinds: vec![RepoWatchEventKindNameV1::PullRequestOpened],
            ..RepoWatchMatcherV1Input::default()
        }),
        vec![RepoWatchRuleActionV1::DispatchSession {
            template: SessionTemplateName::try_new(String::from("watch"))?,
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
    store
        .ingest_observation(
            &store.ingest_baseline(&repository).await?,
            &pull_observation(&repository, RepoWatchPullRequestLifecycle::Open),
            EventProducer::Poll,
            MERGED_RETENTION,
        )
        .await?;
    let mut ids = FixedDispatchIds {
        value: 70001,
        calls: 0,
    };
    let mut factory = FixtureSessionFactory {
        next_command: 80001,
        model: 90001,
    };
    let mut codec = FixtureCommandCodec;
    store
        .evaluate_next(&repository, &rule, &mut ids, &mut factory, &mut codec, now)
        .await
        .expect("dispatch");
    let pending = store.recover_pending_commands(&mut codec).await?;
    let creation = pending.first().expect("create command");
    let session = persist_creation(&core_pool, creation.command()).await?;
    store
        .react_to_lifecycle(
            &LifecycleEvent::session_created_for_test(
                1,
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
            ),
            &mut factory,
            &mut codec,
            &source,
        )
        .await?;
    sqlx::query(
        "UPDATE gh_event SET decode_error = 'fixture quarantine'
          WHERE event_id = (
              SELECT event_id FROM dispatch_ledger WHERE command_id = $1)",
    )
    .bind(creation.command().command_id().into_uuid())
    .execute(&pool)
    .await?;

    store
        .react_to_lifecycle(
            &LifecycleEvent::for_test(
                2,
                now,
                Some(session),
                LifecycleEventKind::GoalChanged(GoalChange {
                    event_ordinal: 1,
                    generation: 1,
                    kind: GoalEventKind::Commissioned,
                }),
            ),
            &mut factory,
            &mut codec,
            &source,
        )
        .await?;
    store
        .ingest_observation(
            &store.ingest_baseline(&repository).await?,
            &pull_observation(&repository, RepoWatchPullRequestLifecycle::Closed),
            EventProducer::Poll,
            MERGED_RETENTION,
        )
        .await?;
    store
        .react_to_pull_request_lifecycle(&mut factory, &mut codec, &source)
        .await?;

    let reactions = store.recover_pending_commands(&mut codec).await?;
    assert!(
        reactions.iter().any(|planned| matches!(
            planned.command().clone().into_payload(),
            SessionCommandPayload::Lifecycle(command)
                if *command.operation() == SessionLifecycleOperation::ReleaseStart
        )),
        "the quarantined origin still commissions its release"
    );
    assert!(
        reactions.iter().any(|planned| matches!(
            planned.command().clone().into_payload(),
            SessionCommandPayload::Lifecycle(command)
                if *command.operation() == SessionLifecycleOperation::Stop {
                    sticky: StopStickiness::Sticky,
                    descendant_scope: DescendantTerminationScope::ParentAlone,
                }
        )),
        "a later closing fact still retires the quarantined origin"
    );

    pool.close().await;
    core_pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL"]
async fn terminal_triggered_dispatches_do_not_retire_themselves() -> Result<(), Box<dyn Error>> {
    let (container, core_pool, url) = postgres().await?;
    migrate(&core_pool).await?;
    let pool = module_pool(&url).await?;
    let store = RepoWatchStore::new(pool.clone());
    let source = signalbox_session_ownership::LifecycleEventSource::new(core_pool.clone());
    // Command, model and dispatch identities distinguish this fixture's actions.
    let mut factory = FixtureSessionFactory {
        next_command: 40001,
        model: 50001,
    };
    let mut ids = FixedDispatchIds {
        value: 60001,
        calls: 0,
    };
    let mut codec = FixtureCommandCodec;
    for (sequence, name, lifecycle, event_kind) in [
        (
            1,
            "close-trigger/project",
            RepoWatchPullRequestLifecycle::Closed,
            RepoWatchEventKindNameV1::PullRequestClosed,
        ),
        (
            2,
            "merge-trigger/project",
            RepoWatchPullRequestLifecycle::Merged,
            RepoWatchEventKindNameV1::PullRequestMerged,
        ),
    ] {
        let repository = RepositorySlug::try_new(name.to_owned())?;
        store
            .ingest_observation(
                &store.ingest_baseline(&repository).await?,
                &pull_observation(&repository, RepoWatchPullRequestLifecycle::Open),
                EventProducer::Poll,
                MERGED_RETENTION,
            )
            .await?;
        let now = OffsetDateTime::now_utc();
        let rule = RepoWatchRule::try_new(
            RepoWatchRuleId::try_new(String::from("terminal"))?,
            RepoWatchRuleVersion::V1,
            RepoWatchMatcherV1::new(RepoWatchMatcherV1Input {
                repository: Some(repository.clone()),
                event_kinds: vec![event_kind],
                ..RepoWatchMatcherV1Input::default()
            }),
            vec![RepoWatchRuleActionV1::DispatchSession {
                template: SessionTemplateName::try_new(String::from("watch"))?,
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
        store
            .ingest_observation(
                &store.ingest_baseline(&repository).await?,
                &pull_observation(&repository, lifecycle),
                EventProducer::Poll,
                MERGED_RETENTION,
            )
            .await?;
        assert!(
            store
                .evaluate_next(&repository, &rule, &mut ids, &mut factory, &mut codec, now)
                .await
                .expect("terminal rule dispatch")
        );
        let pending = store.recover_pending_commands(&mut codec).await?;
        let creation = pending
            .iter()
            .find(|p| p.repository() == &repository)
            .expect("terminal-triggered creation");
        let session = persist_creation(&core_pool, creation.command()).await?;
        store
            .react_to_lifecycle(
                &LifecycleEvent::session_created_for_test(
                    sequence,
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
                ),
                &mut factory,
                &mut codec,
                &source,
            )
            .await?;
        let restarted = RepoWatchStore::new(pool.clone());
        restarted
            .react_to_pull_request_lifecycle(&mut factory, &mut codec, &source)
            .await?;
        let retirements: i64 = sqlx::query_scalar("SELECT count(*) FROM dispatch_ledger WHERE repository=$1 AND retirement_event_id IS NOT NULL")
            .bind(repository.as_str()).fetch_one(&pool).await?;
        assert_eq!(
            retirements, 0,
            "the dispatch's own terminal trigger cannot retire its session"
        );
        assert!(
            restarted
                .recover_pending_commands(&mut codec)
                .await?
                .is_empty()
        );
        assert!(source.session_terminal_at(session).await?.is_none());
    }
    pool.close().await;
    core_pool.close().await;
    drop(container);
    Ok(())
}
