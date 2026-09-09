//! Program creation and ordinary turn-driving transactions.
use crate::*;
use signalbox_domain::{
    Actor, ProgramCapability, ProgramRunId,
    program_registration::{ProgramGrants, ProgramRegistrationRequest},
    program_session::{ProgramSessionCreate, ProgramSessionDisposition, ProgramSessionTurn},
};
use signalbox_persistence::{
    program_registration::ProgramRegistrationRepository,
    program_session::{ProgramSessionError, ProgramSessionRepository},
};

async fn registered_session_run(
    pool: &PgPool,
    grants: ProgramGrants,
) -> Result<ProgramRunId, Box<dyn Error>> {
    let registrations = ProgramRegistrationRepository::new(pool.clone());
    let registration = registrations
        .register_user(
            signalbox_domain::ProgramRegistrationId::from_uuid(Uuid::now_v7()),
            ProgramRegistrationRequest {
                name: Uuid::now_v7().to_string(),
                revision: "fixture-revision".into(),
                source: b"export {};".to_vec(),
                artifact: "export {};".into(),
                grants,
            },
        )
        .await?;
    Ok(registrations
        .start_run(
            ProgramRunId::from_uuid(Uuid::now_v7()),
            registration.id,
            &[],
        )
        .await?)
}

fn sessions(pool: &PgPool) -> ProgramSessionRepository {
    ProgramSessionRepository::new(
        pool.clone(),
        SubmitInputRepository::new(pool.clone()),
        CreateSessionRepository::new(pool.clone(), test_session_credential_pin()),
    )
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn workflow_creation_replays_its_session_and_reconstitutes_its_program_cause()
-> Result<(), Box<dyn Error>> {
    let (_container, pool, _) = migrated_postgres().await?;
    let run =
        registered_session_run(&pool, ProgramGrants::new([ProgramCapability::Session])).await?;
    let repository = sessions(&pool);
    let input = ProgramSessionCreate {
        command: DurableCommandId::from_uuid(Uuid::now_v7()),
        defaults: SessionConfigurationDefaults::new(direct(0x6200)),
    };
    assert_eq!(repository.adopt_creation(run, input.clone()).await?, None);
    let session = repository.create(run, input.clone()).await?;
    assert_eq!(repository.create(run, input.clone()).await?, session);
    assert_eq!(
        repository.adopt_creation(run, input.clone()).await?,
        Some(session)
    );
    let loaded = SessionRepository::new(pool.clone())
        .load_session(session)
        .await?
        .expect("workflow session exists");
    assert!(
        matches!(loaded.creation_provenance().cause(), SessionCreationCause::Workflow { run: actor } if actor.run() == run)
    );
    let stored: (String, Uuid, String) = sqlx::query_as("SELECT session.creation_cause, session.creating_program_run_id, registry.issuer_kind FROM session JOIN create_session_command AS command ON command.created_session_id = session.session_id JOIN durable_command AS registry USING (command_id) WHERE session.session_id = $1")
        .bind(session.into_uuid()).fetch_one(&pool).await?;
    assert_eq!(
        stored,
        ("workflow".into(), run.into_uuid(), "program".into())
    );
    let dispatcher = OutboxDispatcher::new(pool.clone());
    let mut observed = false;
    while dispatcher.dispatch_next(|event| {
        if let DispatchedOutboxEventKind::SessionCreated(creation) = event.kind() {
            assert!(matches!(creation.cause, SessionCreationCause::Workflow { run: actor } if actor.run() == run));
            assert_eq!(event.session(), Some(session)); observed = true;
        }
        OutboxDeliveryDecision::Delivered
    }).await? != OutboxDispatchOutcome::Idle {}
    assert!(observed);
    let mut old_version = pool.begin().await?;
    sqlx::query("ALTER TABLE session_created_outbox_event DISABLE TRIGGER USER")
        .execute(&mut *old_version)
        .await?;
    let error = sqlx::query(
        "UPDATE session_created_outbox_event SET storage_version = 2 WHERE session_id = $1",
    )
    .bind(session.into_uuid())
    .execute(&mut *old_version)
    .await
    .expect_err("workflow provenance requires the new event shape");
    assert_eq!(
        error
            .as_database_error()
            .and_then(|error| error.constraint()),
        Some("session_created_workflow_version")
    );
    old_version.rollback().await?;
    let ungranted = registered_session_run(&pool, ProgramGrants::new([])).await?;
    assert!(matches!(
        repository.create(ungranted, input).await,
        Err(ProgramSessionError::GrantDenied)
    ));
    pool.close().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn program_turn_waits_for_its_exact_terminal_outcome_and_adopts_it_after_restart()
-> Result<(), Box<dyn Error>> {
    let (_container, pool, _) = migrated_postgres().await?;
    let run =
        registered_session_run(&pool, ProgramGrants::new([ProgramCapability::Session])).await?;
    let repository = sessions(&pool);
    // These fixture identities and provider labels are arbitrary.
    let model = DirectModelSelection::from_uuid(Uuid::from_u128(0x6201));
    let target = ProviderModelIdentity::from_uuid(Uuid::from_u128(0x6202));
    let session = repository
        .create(
            run,
            ProgramSessionCreate {
                command: DurableCommandId::from_uuid(Uuid::now_v7()),
                defaults: SessionConfigurationDefaults::new(ModelSelectionRequest::Direct(model)),
            },
        )
        .await?;
    let input = ProgramSessionTurn {
        command: DurableCommandId::from_uuid(Uuid::now_v7()),
        session,
        content: UserContent::try_text("private turn input".to_owned())
            .expect("valid fixture input"),
        configuration: input_choices(1, ModelSelectionOverride::UseSessionDefault),
    };
    let targets = ModelTargetCatalog::try_from_definitions([ModelTargetDefinition::new(
        model,
        ResolvedProviderTarget::naming(target),
    )])
    .expect("one fixture model target");
    let (nudge, mut ready) = tokio::sync::mpsc::unbounded_channel();
    let drive = repository.drive_turn(
        run,
        input.clone(),
        |_| None,
        |session| {
            nudge.send(session).expect("fixture receiver exists");
        },
    );
    let complete = async {
        assert_eq!(ready.recv().await, Some(session));
        activate_earliest_queued_turn(
            &pool,
            EarliestQueuedTurnActivation {
                session: session.into_uuid(),
                origin_entry: Uuid::now_v7(),
                starting_frontier: Uuid::now_v7(),
                initial_attempt: Uuid::now_v7(),
            },
        )
        .await?;
        complete_text_turn(
            &pool,
            session,
            targets,
            ModelCallCredentialReference::new("fixture-provider"),
            0x62_000,
            "private model output",
        )
        .await
    };
    let (outcome, _) = tokio::time::timeout(std::time::Duration::from_secs(30), async {
        tokio::try_join!(
            async { drive.await.map_err(Box::<dyn Error>::from) },
            complete
        )
    })
    .await??;
    assert_eq!(outcome.session, session);
    assert_eq!(outcome.disposition, ProgramSessionDisposition::Completed);
    let receipt = SubmitInputRepository::new(pool.clone())
        .load(input.command)
        .await?
        .expect("program input receipt");
    let SubmitInputResult::Applied(SubmitInputAppliedResult::TurnOrigin(origin)) = receipt.result()
    else {
        panic!("turn origin receipt")
    };
    assert_eq!(outcome.turn, origin.turn());
    assert_eq!(outcome.accepted_input, origin.accepted_input());
    assert!(
        matches!(receipt.command().actor(), Actor::Program { run: actor } if actor.run() == run)
    );
    let restarted = sessions(&pool);
    assert_eq!(
        restarted.adopt_turn(run, input.clone(), |_| {}).await?,
        Some(outcome.clone())
    );
    assert_eq!(
        restarted.drive_turn(run, input, |_| None, |_| {}).await?,
        outcome
    );
    pool.close().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_program_cancel_wakes_a_waiting_session_operation() -> Result<(), Box<dyn Error>> {
    let (_container, pool, _) = migrated_postgres().await?;
    let run =
        registered_session_run(&pool, ProgramGrants::new([ProgramCapability::Session])).await?;
    let repository = sessions(&pool);
    let session = repository
        .create(
            run,
            ProgramSessionCreate {
                command: DurableCommandId::from_uuid(Uuid::now_v7()),
                defaults: SessionConfigurationDefaults::new(direct(0x6203)),
            },
        )
        .await?;
    let input = ProgramSessionTurn {
        command: DurableCommandId::from_uuid(Uuid::now_v7()),
        session,
        content: UserContent::try_text("waiting input".to_owned()).expect("valid fixture input"),
        configuration: input_choices(1, ModelSelectionOverride::UseSessionDefault),
    };
    let (nudge, mut ready) = tokio::sync::mpsc::unbounded_channel();
    let drive = repository.drive_turn(
        run,
        input,
        |_| None,
        |session| {
            nudge.send(session).expect("fixture receiver exists");
        },
    );
    let cancel = async {
        assert_eq!(ready.recv().await, Some(session));
        signalbox_persistence::program_cancellation::cancel(
            &pool,
            signalbox_persistence::program_cancellation::CancelProgramRun {
                command_id: DurableCommandId::from_uuid(Uuid::now_v7()),
                run_id: run,
            },
        )
        .await
    };
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        tokio::try_join!(
            async {
                match drive.await {
                    Err(ProgramSessionError::RunEnded) => Ok(()),
                    Err(error) => Err(Box::<dyn Error>::from(error)),
                    Ok(_) => panic!("cancelled program must stop waiting"),
                }
            },
            async { cancel.await.map_err(Box::<dyn Error>::from) }
        )
    })
    .await??;
    pool.close().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn concurrent_program_waiters_leave_a_single_connection_query_pool_available()
-> Result<(), Box<dyn Error>> {
    let (_container, migrated, _) = migrated_postgres().await?;
    let application = Uuid::now_v7().to_string();
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect_with(
            migrated
                .connect_options()
                .as_ref()
                .clone()
                .application_name(&application),
        )
        .await?;
    let repository = sessions(&pool);
    let mut inputs = Vec::new();
    // Three waiting programs exceed the query pool's single available connection.
    for _ in 0..3 {
        let run =
            registered_session_run(&pool, ProgramGrants::new([ProgramCapability::Session])).await?;
        let session = repository
            .create(
                run,
                ProgramSessionCreate {
                    command: DurableCommandId::from_uuid(Uuid::now_v7()),
                    defaults: SessionConfigurationDefaults::new(direct(0x6204)),
                },
            )
            .await?;
        inputs.push((
            run,
            ProgramSessionTurn {
                command: DurableCommandId::from_uuid(Uuid::now_v7()),
                session,
                content: UserContent::try_text("waiting input".to_owned())
                    .expect("valid fixture input"),
                configuration: input_choices(1, ModelSelectionOverride::UseSessionDefault),
            },
        ));
    }
    let (nudge, mut ready) = tokio::sync::mpsc::unbounded_channel();
    let mut waiters = tokio::task::JoinSet::new();
    for (run, input) in &inputs {
        let repository = repository.clone();
        let nudge = nudge.clone();
        let run = *run;
        let input = input.clone();
        waiters.spawn(async move {
            repository
                .drive_turn(
                    run,
                    input,
                    |_| None,
                    |session| {
                        nudge.send(session).expect("fixture receiver exists");
                    },
                )
                .await
        });
    }
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        for _ in &inputs {
            ready.recv().await.expect("waiting program admitted");
        }
        for (run, _) in &inputs {
            signalbox_persistence::program_cancellation::cancel(
                &pool,
                signalbox_persistence::program_cancellation::CancelProgramRun {
                    command_id: DurableCommandId::from_uuid(Uuid::now_v7()),
                    run_id: *run,
                },
            )
            .await?;
        }
        while let Some(result) = waiters.join_next().await {
            assert!(matches!(result?, Err(ProgramSessionError::RunEnded)));
        }
        Ok::<_, Box<dyn Error>>(())
    })
    .await??;
    let connections: i64 =
        sqlx::query_scalar("SELECT count(*) FROM pg_stat_activity WHERE application_name = $1")
            .bind(&application)
            .fetch_one(&pool)
            .await?;
    assert_eq!(
        connections, 2,
        "one query connection and one shared listener"
    );
    pool.close().await;
    migrated.close().await;
    Ok(())
}
