//! Staged replacement settles at model and complete-result observation boundaries.

use super::*;
use signalbox_domain::{FailedModelCallTurnIdentities, ReplaceLostRunner, ReplaceLostRunnerResult};
use signalbox_persistence::runner_protocol::RunnerRecoveryOutcome;

async fn completed_pinned_batch(
    pool: &PgPool,
) -> Result<(UndispatchedBatch, RunnerGeneration), Box<dyn Error>> {
    let (store, predecessor, registration, pin, epoch) =
        stored_active_pin_fixture_with_authorization(pool, ActivePinEffectCase::EffectFree).await?;
    let claimed = duplicate_lease(&pin.lease, registration.registration())
        .claim(pin.lease.correlation())
        .expect("the offered lease claims");
    store.store_lease(&claimed).await?;
    let completed = claimed
        .complete(pin.lease.correlation())
        .expect("the claimed lease completes");
    store.store_lease(&completed).await?;
    terminalize_physical_attempt(pool, INITIAL_PHYSICAL_ATTEMPT).await?;
    let mut transaction = pool.begin().await?;
    let continuation = Uuid::now_v7();
    sqlx::raw_sql("ALTER TABLE turn_attempt DISABLE TRIGGER ALL; ALTER TABLE turn_lifecycle DISABLE TRIGGER ALL; ALTER TABLE tool_attempt DISABLE TRIGGER ALL;")
        .execute(&mut *transaction).await?;
    sqlx::query("UPDATE turn_attempt SET state_kind = 'ended', end_variant = 'without_stop', end_disposition = 'yielded_to_durable_wait' WHERE turn_attempt_id = (SELECT current_attempt_id FROM turn_lifecycle WHERE session_id = $1)")
        .bind(pin.placement.session().into_uuid()).execute(&mut *transaction).await?;
    sqlx::query("INSERT INTO turn_attempt (turn_attempt_id, turn_id, session_id, continued_from_attempt_id, state_kind) SELECT $1, turn_id, session_id, current_attempt_id, 'prepared' FROM turn_lifecycle WHERE session_id = $2")
        .bind(continuation).bind(pin.placement.session().into_uuid()).execute(&mut *transaction).await?;
    sqlx::query("UPDATE turn_lifecycle SET current_attempt_id = $1 WHERE session_id = $2")
        .bind(continuation)
        .bind(pin.placement.session().into_uuid())
        .execute(&mut *transaction)
        .await?;
    sqlx::query("UPDATE tool_attempt SET issuing_turn_attempt_id = $1 WHERE attempt_id = $2")
        .bind(continuation)
        .bind(uuid(INITIAL_PHYSICAL_ATTEMPT.attempt))
        .execute(&mut *transaction)
        .await?;
    sqlx::raw_sql("ALTER TABLE tool_attempt ENABLE TRIGGER ALL; ALTER TABLE turn_attempt ENABLE TRIGGER ALL; ALTER TABLE turn_lifecycle ENABLE TRIGGER ALL;")
        .execute(&mut *transaction).await?;
    transaction.commit().await?;
    let connection = store
        .load_connection(predecessor.enrollment())
        .await?
        .expect("the predecessor is connected");
    assert_eq!(connection.epoch(), epoch);
    Ok((
        UndispatchedBatch {
            session: pin.placement.session(),
            turn: TurnId::from_uuid(uuid(INITIAL_PHYSICAL_ATTEMPT.turn)),
            request: ToolRequestId::from_uuid(uuid(INITIAL_PHYSICAL_ATTEMPT.request)),
            attempt: ToolAttemptId::from_uuid(uuid(INITIAL_PHYSICAL_ATTEMPT.attempt)),
            store,
            connection,
        },
        pin.placement.revision(),
    ))
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn staged_replacement_appends_one_boundary_after_all_batch_results()
-> Result<(), Box<dyn Error>> {
    assert_staged_batch_boundary(StagedBoundaryCase::Continue).await
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn staged_replacement_keeps_results_and_relocation_before_headroom_failure()
-> Result<(), Box<dyn Error>> {
    assert_staged_batch_boundary(StagedBoundaryCase::ExhaustHeadroom).await
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn staged_replacement_wait_releases_database_capacity_until_candidate_recovers()
-> Result<(), Box<dyn Error>> {
    assert_staged_batch_boundary(StagedBoundaryCase::RecoverCandidate).await
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum StagedBoundaryCase {
    Continue,
    ExhaustHeadroom,
    RecoverCandidate,
}

async fn assert_staged_batch_boundary(case: StagedBoundaryCase) -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (fixture, revision) = completed_pinned_batch(&pool).await?;
    lose_batch(&fixture).await?;
    let candidate = fixture
        .store
        .enroll_pristine(super::super::recovery_commands::enrollment_request())
        .await?
        .into_receipt();
    let candidate_connection = fixture
        .store
        .open_connection(candidate.identities().enrollment())
        .await?;
    let command = ReplaceLostRunner {
        command_id: DurableCommandId::from_uuid(Uuid::now_v7()),
        session: fixture.session,
        revision: None,
    };
    assert_eq!(
        fixture.store.replace_lost_runner(command.clone()).await?,
        RunnerRecoveryOutcome::Pending
    );
    let (notifications, receiver) = tokio::sync::watch::channel(());
    let continuation_pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect_with(pool.connect_options().as_ref().clone())
        .await?;
    let repository = model_repository(&continuation_pool)
        .with_runner_recovery(fixture.store.clone().with_recovery_notifications(receiver));
    let repository = if case == StagedBoundaryCase::ExhaustHeadroom {
        sqlx::raw_sql("ALTER TABLE model_call DISABLE TRIGGER ALL;")
            .execute(&pool)
            .await?;
        sqlx::query("UPDATE model_call SET usage_input_tokens = 100 WHERE turn_id = $1")
            .bind(fixture.turn.into_uuid())
            .execute(&pool)
            .await?;
        sqlx::raw_sql("ALTER TABLE model_call ENABLE TRIGGER ALL;")
            .execute(&pool)
            .await?;
        repository.with_continuation_usage_limits([
            signalbox_persistence::model_execution::ToolContinuationUsageLimit::new(
                signalbox_domain::ResolvedProviderTarget::naming(
                    signalbox_domain::ProviderModelIdentity::from_uuid(uuid(0xa159)),
                ),
                signalbox_domain::FastMode::Disabled,
                10,
                100,
            ),
        ])
    } else {
        repository
    };
    if case == StagedBoundaryCase::RecoverCandidate {
        fixture
            .store
            .transition_connection(
                candidate.identities().enrollment(),
                candidate_connection.epoch(),
                RunnerConnectionTransition::HeartbeatMissed,
            )
            .await?;
    }
    let batch = repository
        .tool_loop_repository()
        .load_active_batch(fixture.session, fixture.turn)
        .await?
        .expect("the completed batch remains active");
    let result = SemanticTranscriptEntryId::from_uuid(Uuid::now_v7());
    let call = ModelCallId::from_uuid(Uuid::now_v7());
    let tool_repository = repository.tool_loop_repository();
    let mut continuation = Box::pin(tool_repository.prepare_continuation(
        fixture.session,
        fixture.turn,
        batch.producing_call(),
        signalbox_application::ToolContinuationIdentities::new(
            vec![result],
            ContextFrontierId::from_uuid(Uuid::now_v7()),
            call,
            FailedModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::now_v7()),
                ContextFrontierId::from_uuid(Uuid::now_v7()),
            ),
            ContextFrontierId::from_uuid(Uuid::now_v7()),
        ),
        |_| panic!("no steering is queued"),
    ));
    if case == StagedBoundaryCase::RecoverCandidate {
        const WAIT_OBSERVATION: std::time::Duration = std::time::Duration::from_millis(100);
        const PROGRESS_DEADLINE: std::time::Duration = std::time::Duration::from_secs(5);
        assert!(
            tokio::time::timeout(WAIT_OBSERVATION, &mut continuation)
                .await
                .is_err()
        );
        tokio::select! {
            outcome = &mut continuation => {
                panic!("continuation waits for candidate recovery: {outcome:?}");
            }
            capacity = tokio::time::timeout(PROGRESS_DEADLINE, async {
                let mut transaction = continuation_pool.begin().await?;
                sqlx::query(
                    "SELECT session_id FROM session_scheduler WHERE session_id = $1 FOR UPDATE",
                )
                .bind(fixture.session.into_uuid())
                .fetch_one(&mut *transaction)
                .await?;
                transaction.rollback().await
            }) => {
                capacity.expect("waiting retains neither the only pool connection nor the scheduler lock")?;
            }
        }
        fixture
            .store
            .transition_connection(
                candidate.identities().enrollment(),
                candidate_connection.epoch(),
                RunnerConnectionTransition::HeartbeatRecovered,
            )
            .await?;
        notifications.send_replace(());
    }
    let outcome = continuation.await?;
    if case == StagedBoundaryCase::ExhaustHeadroom {
        assert!(matches!(
            outcome,
            signalbox_application::PrepareToolContinuationOutcome::ContextCompactionRequired(_)
        ));
    } else {
        assert_eq!(
            outcome,
            signalbox_application::PrepareToolContinuationOutcome::Checkpointed(call)
        );
    }
    assert_eq!(
        fixture.store.replace_lost_runner(command).await?,
        RunnerRecoveryOutcome::Recorded(ReplaceLostRunnerResult::Replaced {
            runner: candidate.identities().runner(),
            placement_revision: revision
                .checked_next()
                .expect("the successor revision fits"),
        })
    );
    let suffix: Vec<String> = sqlx::query_scalar("SELECT entry.payload_kind FROM turn_lifecycle AS turn LEFT JOIN model_call AS call ON call.model_call_id = $2 JOIN LATERAL resolve_context_frontier_members(turn.session_id, COALESCE(call.context_frontier_id, turn_lifecycle_effective_terminal_frontier(turn.session_id, turn.turn_id))) AS member ON true JOIN semantic_transcript_entry AS entry USING (source_session_id, semantic_entry_id) WHERE turn.turn_id = $1 ORDER BY member.member_position DESC LIMIT $3")
        .bind(fixture.turn.into_uuid()).bind(call.into_uuid()).bind(if case == StagedBoundaryCase::ExhaustHeadroom { 3_i64 } else { 2 }).fetch_all(&pool).await?;
    if case == StagedBoundaryCase::ExhaustHeadroom {
        assert_eq!(
            suffix,
            [
                "turn_failed",
                "runner_placement_changed",
                "tool_execution_result"
            ]
        );
    } else {
        assert_eq!(
            suffix,
            ["runner_placement_changed", "tool_execution_result"]
        );
    }
    let boundaries: i64 =
        sqlx::query_scalar("SELECT count(*) FROM runner_placement_boundary WHERE session_id = $1")
            .bind(fixture.session.into_uuid())
            .fetch_one(&pool)
            .await?;
    assert_eq!(boundaries, 1);
    if case != StagedBoundaryCase::ExhaustHeadroom {
        repository.authorize_send(fixture.session, call).await?;
    }
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn staged_pre_pin_replacement_settles_after_in_flight_known_failure()
-> Result<(), Box<dyn Error>> {
    assert_pre_pin_replacement_after_model_observation(PrePinObservation::KnownFailed).await
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn staged_replacement_wakes_queued_work_only_after_placement_recovers()
-> Result<(), Box<dyn Error>> {
    assert_pre_pin_replacement_after_model_observation(PrePinObservation::QueuedAfterFailure).await
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn staged_pre_pin_replacement_settles_without_clearing_model_ambiguity()
-> Result<(), Box<dyn Error>> {
    assert_pre_pin_replacement_after_model_observation(PrePinObservation::Ambiguous).await
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PrePinObservation {
    KnownFailed,
    QueuedAfterFailure,
    Ambiguous,
    DetachedCommand,
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn staged_replacement_resumes_from_a_turn_boundary_without_a_command_client()
-> Result<(), Box<dyn Error>> {
    assert_pre_pin_replacement_after_model_observation(PrePinObservation::DetachedCommand).await
}

async fn assert_pre_pin_replacement_after_model_observation(
    observation: PrePinObservation,
) -> Result<(), Box<dyn Error>> {
    let queued_work = observation == PrePinObservation::QueuedAfterFailure;
    let (_container, pool) = migrated_postgres().await?;
    let (session, turn, _) = insert_running_turn(&pool).await?;
    let mut connection = pool.acquire().await?;
    insert_empty_instruction_manifest(&mut connection, session, turn).await?;
    drop(connection);
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let predecessor = store
        .enroll_pristine(super::super::recovery_commands::enrollment_request())
        .await?
        .into_receipt();
    let connection = store
        .open_connection(predecessor.identities().enrollment())
        .await?;
    store
        .store_placement(
            &SessionRunnerPlacement::new(
                session,
                exact_runner_request(predecessor.identities().runner()),
            ),
            None,
            None,
        )
        .await?;
    let repository = model_repository(&pool).with_runner_recovery(store.clone());
    let call = ModelCallId::from_uuid(Uuid::now_v7());
    repository
        .prepare_initial_call(
            session,
            call,
            FailedModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::now_v7()),
                ContextFrontierId::from_uuid(Uuid::now_v7()),
            ),
            ContextFrontierId::from_uuid(Uuid::now_v7()),
            |_| panic!("no steering is queued"),
        )
        .await?;
    let signalbox_application::AuthorizeModelCallOutcome::Authorized(authorized) =
        repository.authorize_send(session, call).await?
    else {
        panic!("the model call is in flight");
    };
    if queued_work {
        SubmitInputRepository::new(pool.clone())
            .handle_with_candidates(
                SubmitInput::new(
                    DurableCommandId::from_uuid(Uuid::now_v7()),
                    session,
                    UserContent::try_text(String::from("Work after runner recovery"))
                        .expect("queued input is nonempty"),
                    DeliveryRequest::AfterCurrentTurn {
                        expected_active_turn: turn,
                        configuration: PerInputConfigurationChoices::new(
                            SessionConfigurationDefaultsVersion::first(),
                            ModelSelectionOverride::UseSessionDefault,
                        ),
                    },
                ),
                AcceptedInputId::from_uuid(Uuid::now_v7()),
                Some(TurnId::from_uuid(Uuid::now_v7())),
                CancelledModelCallTurnIdentities::new(
                    SemanticTranscriptEntryId::from_uuid(Uuid::now_v7()),
                    ContextFrontierId::from_uuid(Uuid::now_v7()),
                ),
                |_| TurnId::from_uuid(Uuid::now_v7()),
                |_| (Vec::new(), ContextFrontierId::from_uuid(Uuid::now_v7())),
            )
            .await?;
    }
    store
        .transition_connection(
            predecessor.identities().enrollment(),
            connection.epoch(),
            RunnerConnectionTransition::TransportClosed,
        )
        .await?;
    append_runner_lost_before_pin_projection(&pool, session).await?;
    let candidate = store
        .enroll_pristine(super::super::recovery_commands::enrollment_request())
        .await?
        .into_receipt();
    store
        .open_connection(candidate.identities().enrollment())
        .await?;
    let command = ReplaceLostRunner {
        command_id: DurableCommandId::from_uuid(Uuid::now_v7()),
        session,
        revision: None,
    };
    if !queued_work {
        assert_eq!(
            store.replace_lost_runner(command.clone()).await?,
            RunnerRecoveryOutcome::Pending
        );
    }
    let mut boundary_notifications = sqlx::postgres::PgListener::connect_with(&pool).await?;
    boundary_notifications.listen("runner_recovery").await?;
    repository
        .apply_terminal_observation(
            session,
            authorized
                .observation_correlation()
                .bind_terminal_observation(if observation == PrePinObservation::Ambiguous {
                    signalbox_domain::ModelCallTerminalObservation::Ambiguous
                } else {
                    signalbox_domain::ModelCallTerminalObservation::KnownFailed
                }),
            if observation == PrePinObservation::Ambiguous {
                signalbox_domain::ModelCallTerminalIdentities::Ambiguous(
                    signalbox_domain::AmbiguousModelCallTurnIdentities::new(
                        ContextFrontierId::from_uuid(Uuid::now_v7()),
                    ),
                )
            } else {
                signalbox_domain::ModelCallTerminalIdentities::Failed(
                    FailedModelCallTurnIdentities::new(
                        SemanticTranscriptEntryId::from_uuid(Uuid::now_v7()),
                        ContextFrontierId::from_uuid(Uuid::now_v7()),
                    ),
                )
            },
            |_| panic!("no steering is queued"),
        )
        .await?;
    if observation == PrePinObservation::DetachedCommand {
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            boundary_notifications.recv(),
        )
        .await??;
        store.resume_runner_replacements().await?;
    }
    if observation == PrePinObservation::Ambiguous {
        let phase: String = sqlx::query_scalar("SELECT active_phase_kind FROM turn_lifecycle WHERE turn_id = $1 AND state_kind = 'active'")
            .bind(turn.into_uuid()).fetch_one(&pool).await?;
        assert_eq!(phase, "awaiting_model_call_recovery");
    }
    let scheduler = StartEligibleTurnRepository::new(pool.clone());
    let activation = AcceptedInputTurnActivationIdentities::new(
        SemanticTranscriptEntryId::from_uuid(Uuid::now_v7()),
        SemanticTranscriptEntryId::from_uuid(Uuid::now_v7()),
        ContextFrontierId::from_uuid(Uuid::now_v7()),
        TurnAttemptId::from_uuid(Uuid::now_v7()),
    );
    if queued_work {
        assert!(scheduler.preview(session, activation).await?.is_none());
        assert_eq!(
            scheduler.handle(session, activation).await?,
            signalbox_application::StartEligibleTurnOutcome::NoEligibleTurn
        );
        let queued: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM turn_lifecycle WHERE session_id = $1 AND state_kind = 'queued'",
        )
        .bind(session.into_uuid())
        .fetch_one(&pool)
        .await?;
        assert_eq!(queued, 1);
    }
    assert!(
        matches!(store.replace_lost_runner(command).await?, RunnerRecoveryOutcome::Recorded(ReplaceLostRunnerResult::Replaced { runner, .. }) if runner == candidate.identities().runner())
    );
    let boundaries: i64 =
        sqlx::query_scalar("SELECT count(*) FROM runner_placement_boundary WHERE session_id = $1")
            .bind(session.into_uuid())
            .fetch_one(&pool)
            .await?;
    assert_eq!(boundaries, 0);
    if queued_work {
        assert!(matches!(
            scheduler.handle(session, activation).await?,
            signalbox_application::StartEligibleTurnOutcome::Activated(_)
        ));
    }
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn staged_replacement_retires_before_known_tool_crash_terminalizes_the_batch()
-> Result<(), Box<dyn Error>> {
    assert_terminal_batch_retires_replacement(false).await
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn staged_replacement_retires_before_ambiguous_tool_stop_terminalizes_the_batch()
-> Result<(), Box<dyn Error>> {
    assert_terminal_batch_retires_replacement(true).await
}

async fn assert_terminal_batch_retires_replacement(
    ambiguous_stop: bool,
) -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = prepared_batch(&pool).await?;
    if ambiguous_stop {
        let mut transaction = pool.begin().await?;
        sqlx::query("ALTER TABLE tool_attempt DISABLE TRIGGER ALL")
            .execute(&mut *transaction)
            .await?;
        sqlx::query(
            "UPDATE tool_attempt SET effect_class = 'external_effect' WHERE attempt_id = $1",
        )
        .bind(fixture.attempt.into_uuid())
        .execute(&mut *transaction)
        .await?;
        sqlx::query("ALTER TABLE tool_attempt ENABLE TRIGGER ALL")
            .execute(&mut *transaction)
            .await?;
        transaction.commit().await?;
        PostgresToolLoopRepository::new(pool.clone())
            .authorize_attempt(fixture.session, fixture.turn, fixture.attempt)
            .await?;
    }
    fixture
        .store
        .transition_connection(
            enrollment().enrollment(),
            fixture.connection.epoch(),
            RunnerConnectionTransition::TransportClosed,
        )
        .await?;
    append_runner_lost_before_pin_projection(&pool, fixture.session).await?;
    let candidate = fixture
        .store
        .enroll_pristine(super::super::recovery_commands::enrollment_request())
        .await?
        .into_receipt();
    fixture
        .store
        .open_connection(candidate.identities().enrollment())
        .await?;
    let command = ReplaceLostRunner {
        command_id: DurableCommandId::from_uuid(Uuid::now_v7()),
        session: fixture.session,
        revision: None,
    };
    assert_eq!(
        fixture.store.replace_lost_runner(command.clone()).await?,
        RunnerRecoveryOutcome::Pending
    );
    let repository = model_repository(&pool).with_runner_recovery(fixture.store.clone());
    let outcome = repository
        .tool_loop_repository()
        .classify_crash_loss_and_close(
            fixture.session,
            fixture.turn,
            fixture.attempt,
            signalbox_application::ToolCrashClosureIdentities::new(
                vec![SemanticTranscriptEntryId::from_uuid(Uuid::now_v7())],
                ContextFrontierId::from_uuid(Uuid::now_v7()),
                FailedModelCallTurnIdentities::new(
                    SemanticTranscriptEntryId::from_uuid(Uuid::now_v7()),
                    ContextFrontierId::from_uuid(Uuid::now_v7()),
                ),
            ),
            |_| panic!("no steering is pending"),
        )
        .await?;
    if ambiguous_stop {
        assert!(matches!(
            outcome,
            signalbox_domain::ToolAttemptCrashOutcome::Ambiguous(_)
        ));
        SubmitInputRepository::new(pool.clone())
            .handle_with_candidates(
                SubmitInput::new(
                    DurableCommandId::from_uuid(Uuid::now_v7()),
                    fixture.session,
                    UserContent::try_text(String::from("Stop the ambiguous tool"))
                        .expect("the stop content is nonempty"),
                    DeliveryRequest::Interrupt {
                        expected_active_turn: fixture.turn,
                        descendant_scope: DescendantTerminationScope::ParentAlone,
                        configuration: PerInputConfigurationChoices::new(
                            SessionConfigurationDefaultsVersion::first(),
                            ModelSelectionOverride::UseSessionDefault,
                        ),
                    },
                ),
                AcceptedInputId::from_uuid(Uuid::now_v7()),
                Some(TurnId::from_uuid(Uuid::now_v7())),
                signalbox_domain::CancelledModelCallTurnIdentities::new(
                    SemanticTranscriptEntryId::from_uuid(Uuid::now_v7()),
                    ContextFrontierId::from_uuid(Uuid::now_v7()),
                ),
                |_| TurnId::from_uuid(Uuid::now_v7()),
                |_| {
                    (
                        vec![SemanticTranscriptEntryId::from_uuid(Uuid::now_v7())],
                        ContextFrontierId::from_uuid(Uuid::now_v7()),
                    )
                },
            )
            .await?;
    } else {
        assert!(matches!(
            outcome,
            signalbox_domain::ToolAttemptCrashOutcome::KnownFailed(_)
        ));
    }
    assert_eq!(
        fixture.store.replace_lost_runner(command).await?,
        RunnerRecoveryOutcome::Recorded(ReplaceLostRunnerResult::Rejected(
            signalbox_domain::RunnerRecoveryRejection::TurnTerminalized
        ))
    );
    let stages: i64 =
        sqlx::query_scalar("SELECT count(*) FROM runner_replacement_stage WHERE session_id = $1")
            .bind(fixture.session.into_uuid())
            .fetch_one(&pool)
            .await?;
    assert_eq!(stages, 0);
    let state: String =
        sqlx::query_scalar("SELECT state_kind FROM turn_lifecycle WHERE turn_id = $1")
            .bind(fixture.turn.into_uuid())
            .fetch_one(&pool)
            .await?;
    assert_eq!(state, "terminal");
    Ok(())
}
