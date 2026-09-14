//! Successor installation and retry preserve the unresolved tool round.

use super::*;
use signalbox_domain::{ReplaceLostRunner, ReplaceLostRunnerResult};
use signalbox_persistence::runner_protocol::RunnerRecoveryOutcome;

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn takeover_installs_successor_without_projecting_or_dispatching()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let facts = runner_recovery::prepare_runner_recovery_tool_round(
        &pool,
        authorized,
        catalog(),
        "effect_free",
    )
    .await?;
    runner_recovery::record_no_execution_lease_loss(&pool, &facts.lease).await?;
    runner_recovery::park_runner_recovery_tool_round(&pool, &facts).await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let predecessor = enrollment();
    let epoch = store
        .load_connection(predecessor.enrollment())
        .await?
        .expect("fixture authority exists")
        .epoch();
    store
        .transition_connection(
            predecessor.enrollment(),
            epoch,
            RunnerConnectionTransition::TransportClosed,
        )
        .await?;
    let successor = store
        .enroll_pristine(recovery_commands::enrollment_request())
        .await?
        .into_receipt();
    let successor_connection = store
        .open_connection(successor.identities().enrollment())
        .await?;
    let before: (i64, i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM tool_attempt), (SELECT count(*) FROM model_call),
                (SELECT count(*) FROM semantic_transcript_entry)",
    )
    .fetch_one(&pool)
    .await?;
    let command = ReplaceLostRunner {
        command_id: DurableCommandId::from_uuid(Uuid::now_v7()),
        session: facts.session,
        revision: None,
    };
    let result = store.replace_lost_runner(command.clone()).await?;
    assert_eq!(
        result,
        RunnerRecoveryOutcome::Recorded(ReplaceLostRunnerResult::Replaced {
            runner: successor.identities().runner(),
            placement_revision: facts
                .placement_revision
                .checked_next()
                .expect("fixture authority exists"),
        })
    );
    assert_eq!(store.replace_lost_runner(command).await?, result);
    let retained: (String, Uuid, String, i64, i64, i64) = sqlx::query_as(
        "SELECT turn.active_phase_kind, turn.runner_recovery_tool_attempt_id, attempt.state_kind,
            (SELECT count(*) FROM runner_replacement_stage),
            (SELECT count(*) FROM runner_recovery_takeover),
            (SELECT count(*) FROM runner_placement_boundary)
         FROM turn_lifecycle AS turn JOIN tool_attempt AS attempt
           ON attempt.attempt_id = turn.runner_recovery_tool_attempt_id
         WHERE turn.turn_id = $1",
    )
    .bind(facts.turn.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        retained,
        (
            "awaiting_runner_recovery".into(),
            facts.interrupted_attempt.into_uuid(),
            "in_flight".into(),
            0,
            1,
            0
        )
    );
    let after: (i64, i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM tool_attempt), (SELECT count(*) FROM model_call),
                (SELECT count(*) FROM semantic_transcript_entry)",
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(before, after);
    assert!(
        matches!(store.load_placement(facts.session).await?.expect("fixture authority exists").placement().state(),
        SessionRunnerPlacementState::Pinned(pin) if pin.runner == successor.identities().runner())
    );
    assert!(
        store
            .load_runner_recovery_wait(facts.session)
            .await?
            .is_some()
    );
    let retry = store
        .offer_runner_recovery_retry(
            successor.identities().enrollment(),
            successor_connection.epoch(),
            facts.turn,
        )
        .await?
        .expect("the installed takeover can offer its retained request");
    assert_eq!(retry.attempt(), facts.interrupted_attempt);
    assert_eq!(
        retry.generation(),
        facts
            .lease
            .generation()
            .checked_next()
            .expect("fixture authority exists")
    );
    assert_eq!(retry.runner(), successor.identities().runner());
    let batch_store =
        signalbox_persistence::tool_loop::PostgresToolLoopRepository::new(pool.clone());
    let batch = batch_store
        .load_active_batch(facts.session, facts.turn)
        .await?
        .expect("fixture authority exists");
    assert!(
        matches!(batch.phase(), signalbox_domain::ToolBatchPhase::Executing { turn_attempt }
        if turn_attempt == facts.turn_attempt)
    );
    store
        .claim_tool_lease(
            successor.identities().enrollment(),
            successor_connection.epoch(),
            retry.correlation(),
        )
        .await?;
    store
        .record_tool_lease_result(
            successor.identities().enrollment(),
            successor_connection.epoch(),
            retry.correlation(),
            signalbox_domain::ToolAttemptObservation::KnownFailed {
                error: signalbox_domain::ToolExecutionError::new(
                    signalbox_domain::ToolExecutionErrorKind::ExecutionFailed,
                    None,
                ),
            },
        )
        .await?;
    assert!(
        batch_store
            .reread_durable_completion(retry.correlation().dispatch)
            .await?
    );
    let repository = placement_loss::model_repository(&pool).with_runner_recovery(store.clone());
    let next_call = ModelCallId::from_uuid(Uuid::now_v7());
    let continuation = repository
        .tool_loop_repository()
        .prepare_continuation(
            facts.session,
            facts.turn,
            facts.producing_call,
            signalbox_application::ToolContinuationIdentities::new(
                vec![SemanticTranscriptEntryId::from_uuid(Uuid::now_v7())],
                ContextFrontierId::from_uuid(Uuid::now_v7()),
                next_call,
                signalbox_domain::FailedModelCallTurnIdentities::new(
                    SemanticTranscriptEntryId::from_uuid(Uuid::now_v7()),
                    ContextFrontierId::from_uuid(Uuid::now_v7()),
                ),
                ContextFrontierId::from_uuid(Uuid::now_v7()),
            ),
            |_| panic!("no steering is queued"),
        )
        .await?;
    assert_eq!(
        continuation,
        signalbox_application::PrepareToolContinuationOutcome::Checkpointed(next_call)
    );
    let suffix: Vec<String> = sqlx::query_scalar(
        "SELECT entry.payload_kind FROM model_call AS call
         JOIN LATERAL resolve_context_frontier_members(call.session_id, call.context_frontier_id) AS member ON true
         JOIN semantic_transcript_entry AS entry USING (source_session_id, semantic_entry_id)
         WHERE call.model_call_id = $1 ORDER BY member.member_position DESC LIMIT 2",
    ).bind(next_call.into_uuid()).fetch_all(&pool).await?;
    assert_eq!(
        suffix,
        ["runner_placement_changed", "tool_execution_result"]
    );
    let boundaries: i64 =
        sqlx::query_scalar("SELECT count(*) FROM runner_placement_boundary WHERE session_id = $1")
            .bind(facts.session.into_uuid())
            .fetch_one(&pool)
            .await?;
    assert_eq!(boundaries, 1);
    Ok(())
}

#[derive(Clone, Copy, Debug)]
enum FixtureLoss {
    NoExecution(RunnerToolEffectClass),
    ExecutionPossible(RunnerToolEffectClass),
}

struct TakeoverFixture {
    store: RunnerProtocolStore,
    facts: runner_recovery::RunnerRecoveryToolRoundFacts,
    successor: RunnerEnrollmentId,
    epoch: RunnerConnectionEpoch,
}

async fn installed_takeover(
    pool: &PgPool,
    loss: FixtureLoss,
) -> Result<TakeoverFixture, Box<dyn Error>> {
    installed_takeover_with_neighbors(pool, loss, false).await
}

async fn installed_takeover_with_neighbors(
    pool: &PgPool,
    loss: FixtureLoss,
    neighbors: bool,
) -> Result<TakeoverFixture, Box<dyn Error>> {
    let effect = match loss {
        FixtureLoss::NoExecution(effect) | FixtureLoss::ExecutionPossible(effect) => effect,
    };
    let (catalog, authorize, effect_kind): (
        _,
        fn(PhysicalAttemptFacts) -> RunnerToolAttemptAuthorization,
        _,
    ) = match effect {
        RunnerToolEffectClass::Pure => (catalog(), authorized, "effect_free"),
        RunnerToolEffectClass::Idempotent => (
            idempotent_catalog(),
            idempotent_authorized,
            "external_effect",
        ),
        RunnerToolEffectClass::SideEffecting => (
            side_effecting_catalog(),
            external_authorized,
            "external_effect",
        ),
    };
    let facts = runner_recovery::prepare_runner_recovery_tool_round_in_sandbox(
        pool,
        authorize,
        catalog.clone(),
        effect_kind,
        if neighbors {
            RunnerSandboxProfile::Ambient
        } else {
            RunnerSandboxProfile::WorkspaceRestricted
        },
    )
    .await?;
    if neighbors {
        add_resolved_prefix_and_pending_suffix(pool, &facts).await?;
    }
    match loss {
        FixtureLoss::NoExecution(_) => {
            runner_recovery::record_no_execution_lease_loss(pool, &facts.lease).await?
        }
        FixtureLoss::ExecutionPossible(_) => {
            record_execution_possible_lease_loss(pool, &facts.lease).await?
        }
    }
    runner_recovery::park_runner_recovery_tool_round(pool, &facts).await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog);
    let predecessor = enrollment();
    let old_epoch = store
        .load_connection(predecessor.enrollment())
        .await?
        .expect("fixture authority exists")
        .epoch();
    store
        .transition_connection(
            predecessor.enrollment(),
            old_epoch,
            RunnerConnectionTransition::TransportClosed,
        )
        .await?;
    let successor = store
        .enroll_pristine(recovery_commands::enrollment_request())
        .await?
        .into_receipt();
    let connected = store
        .open_connection(successor.identities().enrollment())
        .await?;
    let result = store
        .replace_lost_runner(ReplaceLostRunner {
            command_id: DurableCommandId::from_uuid(Uuid::now_v7()),
            session: facts.session,
            revision: None,
        })
        .await?;
    assert!(
        matches!(
            result,
            RunnerRecoveryOutcome::Recorded(ReplaceLostRunnerResult::Replaced { .. })
        ),
        "{loss:?}: {result:?}"
    );
    Ok(TakeoverFixture {
        store,
        facts,
        successor: successor.identities().enrollment(),
        epoch: connected.epoch(),
    })
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn claimed_takeover_retires_the_old_attempt_atomically_with_the_retry()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = installed_takeover(
        &pool,
        FixtureLoss::ExecutionPossible(RunnerToolEffectClass::Pure),
    )
    .await?;
    let retry = fixture
        .store
        .offer_runner_recovery_retry(fixture.successor, fixture.epoch, fixture.facts.turn)
        .await?
        .expect("fixture authority exists");
    assert_ne!(retry.attempt(), fixture.facts.interrupted_attempt);
    assert_eq!(
        retry.correlation().lease,
        fixture.facts.lease.correlation().lease
    );
    assert_eq!(retry.generation().get(), 2);
    assert_eq!(retry.correlation().dispatch.generation().as_u64(), 1);
    let retired: (String, String, String) = sqlx::query_as(
        "SELECT state_kind, terminal_disposition_kind, error_kind FROM tool_attempt WHERE attempt_id = $1",
    ).bind(fixture.facts.interrupted_attempt.into_uuid()).fetch_one(&pool).await?;
    assert_eq!(
        retired,
        (
            "terminal".into(),
            "known_failed".into(),
            "crash_lost".into()
        )
    );
    let current: Vec<Uuid> = sqlx::query_scalar(
        "SELECT attempt_id FROM runner_current_tool_attempt WHERE request_id = $1",
    )
    .bind(fixture.facts.request.into_uuid())
    .fetch_all(&pool)
    .await?;
    assert_eq!(current, vec![retry.attempt().into_uuid()]);
    assert!(
        fixture
            .store
            .offer_runner_recovery_retry(fixture.successor, fixture.epoch, fixture.facts.turn)
            .await?
            .is_none()
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn idempotent_takeover_retains_ambiguous_history_without_an_extra_result()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = installed_takeover(
        &pool,
        FixtureLoss::ExecutionPossible(RunnerToolEffectClass::Idempotent),
    )
    .await?;
    let retry = fixture
        .store
        .offer_runner_recovery_retry(fixture.successor, fixture.epoch, fixture.facts.turn)
        .await?
        .expect("fixture authority exists");
    assert_ne!(retry.attempt(), fixture.facts.interrupted_attempt);
    let retired: (String, String, Option<String>) = sqlx::query_as(
        "SELECT state_kind, terminal_disposition_kind, error_kind FROM tool_attempt WHERE attempt_id = $1",
    ).bind(fixture.facts.interrupted_attempt.into_uuid()).fetch_one(&pool).await?;
    assert_eq!(retired, ("terminal".into(), "ambiguous".into(), None));
    let projected: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM semantic_transcript_entry WHERE tool_result_attempt_id IS NOT NULL",
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(projected, 0);
    Ok(())
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn side_effecting_takeover_with_no_execution_proof_retains_the_physical_attempt()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = installed_takeover(
        &pool,
        FixtureLoss::NoExecution(RunnerToolEffectClass::SideEffecting),
    )
    .await?;
    let retry = fixture
        .store
        .offer_runner_recovery_retry(fixture.successor, fixture.epoch, fixture.facts.turn)
        .await?
        .expect("fixture authority exists");
    assert_eq!(
        retry.correlation().dispatch,
        fixture.facts.lease.correlation().dispatch
    );
    assert_eq!(retry.generation().get(), 2);
    Ok(())
}

async fn stop_retained_turn(
    pool: &PgPool,
    facts: &runner_recovery::RunnerRecoveryToolRoundFacts,
) -> Result<(), Box<dyn Error>> {
    stop_turn(pool, facts.session, facts.turn).await
}

async fn stop_turn(pool: &PgPool, session: SessionId, turn: TurnId) -> Result<(), Box<dyn Error>> {
    let terminal_frontier = ContextFrontierId::from_uuid(Uuid::now_v7());
    let interrupt = SubmitInput::new(
        DurableCommandId::from_uuid(Uuid::now_v7()),
        session,
        UserContent::try_text("stop recovery retry".to_owned()).expect("nonempty stop input"),
        DeliveryRequest::Interrupt {
            expected_active_turn: turn,
            descendant_scope: DescendantTerminationScope::ParentAlone,
            configuration: PerInputConfigurationChoices::new(
                SessionConfigurationDefaultsVersion::try_from_u64(1)
                    .expect("initial defaults version"),
                ModelSelectionOverride::UseSessionDefault,
            ),
        },
    );
    SubmitInputRepository::new(pool.clone())
        .handle_with_candidates(
            interrupt,
            AcceptedInputId::from_uuid(Uuid::now_v7()),
            Some(TurnId::from_uuid(Uuid::now_v7())),
            CancelledModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::now_v7()),
                terminal_frontier,
            ),
            |_| TurnId::from_uuid(Uuid::now_v7()),
            |requests| {
                (
                    requests
                        .iter()
                        .map(|_| SemanticTranscriptEntryId::from_uuid(Uuid::now_v7()))
                        .collect(),
                    terminal_frontier,
                )
            },
        )
        .await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn terminalization_before_retry_closes_the_request_and_suppresses_dispatch()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture =
        installed_takeover(&pool, FixtureLoss::NoExecution(RunnerToolEffectClass::Pure)).await?;
    stop_retained_turn(&pool, &fixture.facts).await?;
    assert!(
        fixture
            .store
            .offer_runner_recovery_retry(fixture.successor, fixture.epoch, fixture.facts.turn)
            .await?
            .is_none()
    );
    let turn: (String, String, Option<String>) = sqlx::query_as(
        "SELECT state_kind, terminal_disposition_kind, active_phase_kind FROM turn_lifecycle WHERE turn_id = $1",
    ).bind(fixture.facts.turn.into_uuid()).fetch_one(&pool).await?;
    assert_eq!(turn, ("terminal".into(), "cancelled".into(), None));
    let result: String = sqlx::query_scalar(
        "SELECT payload_kind FROM semantic_transcript_entry WHERE tool_result_request_id = $1",
    )
    .bind(fixture.facts.request.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(result, "tool_closed_by_turn_end");
    let relocation: i64 =
        sqlx::query_scalar("SELECT count(*) FROM runner_placement_boundary WHERE session_id = $1")
            .bind(fixture.facts.session.into_uuid())
            .fetch_one(&pool)
            .await?;
    assert_eq!(relocation, 1);
    let suffix: Vec<String> = sqlx::query_scalar(
        "SELECT entry.payload_kind FROM turn_lifecycle AS turn
         JOIN LATERAL resolve_context_frontier_members(turn.session_id, turn.terminal_frontier_id) AS member ON true
         JOIN semantic_transcript_entry AS entry USING (source_session_id, semantic_entry_id)
         WHERE turn.turn_id = $1 ORDER BY member.member_position DESC LIMIT 3",
    ).bind(fixture.facts.turn.into_uuid()).fetch_all(&pool).await?;
    assert_eq!(
        suffix,
        [
            "turn_cancelled",
            "runner_placement_changed",
            "tool_closed_by_turn_end"
        ]
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn retry_dispatch_keeps_terminalization_waiting_for_its_result() -> Result<(), Box<dyn Error>>
{
    let (_container, pool) = migrated_postgres().await?;
    let fixture =
        installed_takeover(&pool, FixtureLoss::NoExecution(RunnerToolEffectClass::Pure)).await?;
    let gate = signalbox_application::InProcessToolDispatchGate::default();
    let dispatch_permit = gate.acquire(fixture.facts.turn).await;
    let retry = fixture
        .store
        .offer_runner_recovery_retry(fixture.successor, fixture.epoch, fixture.facts.turn)
        .await?
        .expect("fixture authority exists");
    let mut stopping = Box::pin(async {
        let _stop_permit = gate.acquire(fixture.facts.turn).await;
        stop_retained_turn(&pool, &fixture.facts).await
    });
    let first_poll = std::future::poll_fn(|context| {
        std::task::Poll::Ready(std::future::Future::poll(stopping.as_mut(), context))
    })
    .await;
    assert!(first_poll.is_pending());
    let state: String =
        sqlx::query_scalar("SELECT state_kind FROM turn_lifecycle WHERE turn_id = $1")
            .bind(fixture.facts.turn.into_uuid())
            .fetch_one(&pool)
            .await?;
    assert_eq!(state, "active");
    fixture
        .store
        .claim_tool_lease(fixture.successor, fixture.epoch, retry.correlation())
        .await?;
    fixture
        .store
        .record_tool_lease_result(
            fixture.successor,
            fixture.epoch,
            retry.correlation(),
            signalbox_domain::ToolAttemptObservation::KnownFailed {
                error: signalbox_domain::ToolExecutionError::new(
                    signalbox_domain::ToolExecutionErrorKind::ExecutionFailed,
                    None,
                ),
            },
        )
        .await?;
    drop(dispatch_permit);
    stopping.await?;
    let turn: (String, String) = sqlx::query_as(
        "SELECT state_kind, terminal_disposition_kind FROM turn_lifecycle WHERE turn_id = $1",
    )
    .bind(fixture.facts.turn.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(turn, ("terminal".into(), "cancelled".into()));
    Ok(())
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn stopping_an_idempotent_takeover_closes_the_retry_and_retains_ambiguous_history()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = installed_takeover(
        &pool,
        FixtureLoss::ExecutionPossible(RunnerToolEffectClass::Idempotent),
    )
    .await?;
    stop_retained_turn(&pool, &fixture.facts).await?;
    assert!(
        fixture
            .store
            .offer_runner_recovery_retry(fixture.successor, fixture.epoch, fixture.facts.turn)
            .await?
            .is_none()
    );
    let retained: (String, String, String) = sqlx::query_as(
        "SELECT turn.terminal_disposition_kind, attempt.terminal_disposition_kind, entry.payload_kind
         FROM turn_lifecycle AS turn JOIN tool_attempt AS attempt ON attempt.turn_id = turn.turn_id
         JOIN semantic_transcript_entry AS entry ON entry.tool_result_request_id = attempt.request_id
         WHERE turn.turn_id = $1",
    ).bind(fixture.facts.turn.into_uuid()).fetch_one(&pool).await?;
    assert_eq!(
        retained,
        (
            "cancelled".into(),
            "ambiguous".into(),
            "tool_closed_by_turn_end".into()
        )
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn replacement_without_an_interrupted_tool_resumes_a_fresh_turn_attempt()
-> Result<(), Box<dyn Error>> {
    check_no_tool_recovery(false, true, false).await
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn pinned_recovery_before_a_model_call_retains_relocation_through_terminalization()
-> Result<(), Box<dyn Error>> {
    check_no_tool_recovery(true, true, false).await
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn repeated_no_tool_recovery_can_stop_before_its_first_call() -> Result<(), Box<dyn Error>> {
    check_no_tool_recovery(true, false, true).await
}

async fn check_no_tool_recovery(
    pinned: bool,
    prepare_call: bool,
    repeat: bool,
) -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let predecessor = enrollment();
    let (session, turn, mut yielded, store, connection) = if pinned {
        let (session, previous, store, connection) =
            placement_loss::completed_pinned_turn(&pool).await?;
        stop_turn(&pool, session, previous).await?;
        let turn: Uuid = sqlx::query_scalar(
            "SELECT turn_id FROM turn_lifecycle WHERE session_id = $1 AND state_kind = 'queued'",
        )
        .bind(session.into_uuid())
        .fetch_one(&pool)
        .await?;
        let yielded = TurnAttemptId::from_uuid(Uuid::now_v7());
        StartEligibleTurnRepository::new(pool.clone())
            .handle(
                session,
                AcceptedInputTurnActivationIdentities::new(
                    SemanticTranscriptEntryId::from_uuid(Uuid::now_v7()),
                    SemanticTranscriptEntryId::from_uuid(Uuid::now_v7()),
                    ContextFrontierId::from_uuid(Uuid::now_v7()),
                    yielded,
                ),
            )
            .await?;
        (session, TurnId::from_uuid(turn), yielded, store, connection)
    } else {
        let (session, turn, yielded) = insert_running_turn(&pool).await?;
        let store = RunnerProtocolStore::new(pool.clone(), catalog());
        store.insert_enrollment(&predecessor).await?;
        store.register(&predecessor, advertisement()).await?;
        let connection = store.open_connection(predecessor.enrollment()).await?;
        store
            .store_placement(
                &SessionRunnerPlacement::new(session, exact_runner_request(predecessor.runner())),
                None,
                None,
            )
            .await?;
        (session, turn, yielded, store, connection)
    };
    let before_work: (i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM model_call), (SELECT count(*) FROM runner_lease_generation)",
    )
    .fetch_one(&pool)
    .await?;
    store
        .transition_connection(
            predecessor.enrollment(),
            connection.epoch(),
            RunnerConnectionTransition::TransportClosed,
        )
        .await?;
    let loss = store
        .load_current_connection_loss(predecessor.enrollment())
        .await?
        .expect("fixture authority exists");
    store
        .propagate_connection_loss_session(loss, session)
        .await?;
    let mut parked = pool.begin().await?;
    sqlx::query("UPDATE turn_attempt SET state_kind = 'ended', end_variant = 'without_stop', end_disposition = 'yielded_to_durable_wait' WHERE turn_attempt_id = $1")
        .bind(yielded.into_uuid()).execute(&mut *parked).await?;
    sqlx::query("UPDATE turn_lifecycle SET active_phase_kind = 'awaiting_runner_recovery', current_attempt_id = NULL,
        runner_recovery_runner_id = $1, runner_recovery_placement_revision = 1 WHERE turn_id = $2")
        .bind(predecessor.runner().into_uuid()).bind(turn.into_uuid()).execute(&mut *parked).await?;
    parked.commit().await?;
    assert!(store.load_runner_recovery_wait(session).await?.is_some());
    let successor = store
        .enroll_pristine(recovery_commands::enrollment_request())
        .await?
        .into_receipt();
    store
        .open_connection(successor.identities().enrollment())
        .await?;
    let result = store
        .replace_lost_runner(ReplaceLostRunner {
            command_id: DurableCommandId::from_uuid(Uuid::now_v7()),
            session,
            revision: None,
        })
        .await?;
    assert!(matches!(
        result,
        RunnerRecoveryOutcome::Recorded(ReplaceLostRunnerResult::Replaced { .. })
    ));
    if repeat {
        let current: Uuid =
            sqlx::query_scalar("SELECT current_attempt_id FROM turn_lifecycle WHERE turn_id = $1")
                .bind(turn.into_uuid())
                .fetch_one(&pool)
                .await?;
        yielded = TurnAttemptId::from_uuid(current);
        let enrolled = successor.identities().enrollment();
        let connected = store
            .load_connection(enrolled)
            .await?
            .expect("installed successor");
        store
            .transition_connection(
                enrolled,
                connected.epoch(),
                RunnerConnectionTransition::TransportClosed,
            )
            .await?;
        let loss = store
            .load_current_connection_loss(enrolled)
            .await?
            .expect("second loss");
        store
            .propagate_connection_loss_session(loss, session)
            .await?;
        let mut parked = pool.begin().await?;
        sqlx::query("UPDATE turn_attempt SET state_kind = 'ended', end_variant = 'without_stop', end_disposition = 'yielded_to_durable_wait' WHERE turn_attempt_id = $1")
            .bind(current).execute(&mut *parked).await?;
        sqlx::query("UPDATE turn_lifecycle SET active_phase_kind = 'awaiting_runner_recovery', current_attempt_id = NULL,
            runner_recovery_runner_id = $1, runner_recovery_placement_revision = 2 WHERE turn_id = $2")
            .bind(successor.identities().runner().into_uuid()).bind(turn.into_uuid()).execute(&mut *parked).await?;
        parked.commit().await?;
        let next = store
            .enroll_pristine(recovery_commands::enrollment_request())
            .await?
            .into_receipt();
        store
            .open_connection(next.identities().enrollment())
            .await?;
        let replaced = store
            .replace_lost_runner(ReplaceLostRunner {
                command_id: DurableCommandId::from_uuid(Uuid::now_v7()),
                session,
                revision: None,
            })
            .await?;
        assert!(matches!(
            replaced,
            RunnerRecoveryOutcome::Recorded(ReplaceLostRunnerResult::Replaced { .. })
        ));
    }
    let resumed: (String, Uuid, Uuid, String) = sqlx::query_as(
        "SELECT turn.active_phase_kind, attempt.turn_attempt_id, attempt.continued_from_attempt_id, attempt.state_kind
         FROM turn_lifecycle AS turn JOIN turn_attempt AS attempt ON attempt.turn_attempt_id = turn.current_attempt_id
         WHERE turn.turn_id = $1",
    ).bind(turn.into_uuid()).fetch_one(&pool).await?;
    assert_eq!(resumed.0, "running");
    assert_ne!(resumed.1, yielded.into_uuid());
    assert_eq!(resumed.2, yielded.into_uuid());
    assert_eq!(resumed.3, "prepared");
    let work: (i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM model_call), (SELECT count(*) FROM runner_lease_generation)",
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(work, before_work);
    let mut connection = pool.acquire().await?;
    insert_empty_instruction_manifest(&mut connection, session, turn).await?;
    drop(connection);
    let call = ModelCallId::from_uuid(Uuid::now_v7());
    if prepare_call {
        placement_loss::model_repository(&pool)
            .with_runner_recovery(store.clone())
            .prepare_initial_call(
                session,
                call,
                signalbox_domain::FailedModelCallTurnIdentities::new(
                    SemanticTranscriptEntryId::from_uuid(Uuid::now_v7()),
                    ContextFrontierId::from_uuid(Uuid::now_v7()),
                ),
                ContextFrontierId::from_uuid(Uuid::now_v7()),
                |_| panic!("no steering is queued"),
            )
            .await?;
        let prepared: String =
            sqlx::query_scalar("SELECT state_kind FROM model_call WHERE model_call_id = $1")
                .bind(call.into_uuid())
                .fetch_one(&pool)
                .await?;
        assert_eq!(prepared, "prepared");
    }
    stop_turn(&pool, session, turn).await?;
    let terminal: String =
        sqlx::query_scalar("SELECT state_kind FROM turn_lifecycle WHERE turn_id = $1")
            .bind(turn.into_uuid())
            .fetch_one(&pool)
            .await?;
    assert_eq!(terminal, "terminal");
    let boundaries: i64 =
        sqlx::query_scalar("SELECT count(*) FROM runner_placement_boundary WHERE session_id = $1")
            .bind(session.into_uuid())
            .fetch_one(&pool)
            .await?;
    assert_eq!(boundaries, i64::from(pinned) * (1 + i64::from(repeat)));
    if pinned && prepare_call {
        let last: String = sqlx::query_scalar("SELECT entry.payload_kind FROM model_call AS call
            JOIN LATERAL resolve_context_frontier_members(call.session_id, call.context_frontier_id) AS member ON true
            JOIN semantic_transcript_entry AS entry USING (source_session_id, semantic_entry_id)
            WHERE call.model_call_id = $1 ORDER BY member.member_position DESC LIMIT 1")
            .bind(call.into_uuid()).fetch_one(&pool).await?;
        assert_eq!(last, "runner_placement_changed");
    }
    if !prepare_call {
        let suffix: Vec<String> = sqlx::query_scalar("SELECT entry.payload_kind FROM turn_lifecycle AS turn
            JOIN LATERAL resolve_context_frontier_members(turn.session_id, turn.terminal_frontier_id) AS member ON true
            JOIN semantic_transcript_entry AS entry USING (source_session_id, semantic_entry_id)
            WHERE turn.turn_id = $1 ORDER BY member.member_position DESC LIMIT 3")
            .bind(turn.into_uuid()).fetch_all(&pool).await?;
        assert_eq!(
            suffix,
            [
                "turn_cancelled",
                "runner_placement_changed",
                "runner_placement_changed"
            ]
        );
    }
    Ok(())
}

async fn replace_disconnected_successor(
    fixture: &TakeoverFixture,
    enrollment: RunnerEnrollmentId,
    epoch: RunnerConnectionEpoch,
) -> Result<(RunnerEnrollmentId, RunnerConnectionEpoch), Box<dyn Error>> {
    fixture
        .store
        .transition_connection(
            enrollment,
            epoch,
            RunnerConnectionTransition::TransportClosed,
        )
        .await?;
    let loss = fixture
        .store
        .load_current_connection_loss(enrollment)
        .await?
        .expect("fixture authority exists");
    fixture
        .store
        .propagate_connection_loss_session(loss, fixture.facts.session)
        .await?;
    let successor = fixture
        .store
        .enroll_pristine(recovery_commands::enrollment_request())
        .await?
        .into_receipt();
    let enrollment = successor.identities().enrollment();
    let connection = fixture.store.open_connection(enrollment).await?;
    let result = fixture
        .store
        .replace_lost_runner(ReplaceLostRunner {
            command_id: DurableCommandId::from_uuid(Uuid::now_v7()),
            session: fixture.facts.session,
            revision: None,
        })
        .await?;
    assert!(
        matches!(
            result,
            RunnerRecoveryOutcome::Recorded(ReplaceLostRunnerResult::Replaced { .. })
        ),
        "{result:?}"
    );
    Ok((enrollment, connection.epoch()))
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn successor_loss_before_offer_keeps_the_original_request_recoverable()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture =
        installed_takeover(&pool, FixtureLoss::NoExecution(RunnerToolEffectClass::Pure)).await?;
    let (successor, epoch) =
        replace_disconnected_successor(&fixture, fixture.successor, fixture.epoch).await?;
    let retry = fixture
        .store
        .offer_runner_recovery_retry(successor, epoch, fixture.facts.turn)
        .await?
        .expect("fixture authority exists");
    assert_eq!(retry.attempt(), fixture.facts.interrupted_attempt);
    assert_eq!(retry.generation().get(), 2);
    assert_eq!(retry.correlation().placement_revision.get(), 3);
    fixture
        .store
        .claim_tool_lease(successor, epoch, retry.correlation())
        .await?;
    fixture
        .store
        .record_tool_lease_result(
            successor,
            epoch,
            retry.correlation(),
            signalbox_domain::ToolAttemptObservation::KnownFailed {
                error: signalbox_domain::ToolExecutionError::new(
                    signalbox_domain::ToolExecutionErrorKind::ExecutionFailed,
                    None,
                ),
            },
        )
        .await?;
    assert!(
        fixture
            .store
            .load_runner_recovery_wait(fixture.facts.session)
            .await?
            .is_none()
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn repeated_retry_loss_keeps_one_yield_until_the_request_settles()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture =
        installed_takeover(&pool, FixtureLoss::NoExecution(RunnerToolEffectClass::Pure)).await?;
    let (mut successor, mut epoch) = (fixture.successor, fixture.epoch);
    let mut previous = fixture.facts.interrupted_attempt;
    for claim in [false, true] {
        let retry = fixture
            .store
            .offer_runner_recovery_retry(successor, epoch, fixture.facts.turn)
            .await?
            .expect("fixture authority exists");
        assert_eq!(retry.attempt(), previous);
        if claim {
            fixture
                .store
                .claim_tool_lease(successor, epoch, retry.correlation())
                .await?;
        }
        (successor, epoch) = replace_disconnected_successor(&fixture, successor, epoch).await?;
        let attempts: i64 =
            sqlx::query_scalar("SELECT count(*) FROM turn_attempt WHERE turn_id = $1")
                .bind(fixture.facts.turn.into_uuid())
                .fetch_one(&pool)
                .await?;
        assert_eq!(
            attempts, 1,
            "loss must not reopen and yield another turn attempt"
        );
        previous = retry.attempt();
    }
    let retry = fixture
        .store
        .offer_runner_recovery_retry(successor, epoch, fixture.facts.turn)
        .await?
        .expect("fixture authority exists");
    assert_ne!(retry.attempt(), previous);
    assert_eq!(retry.generation().get(), 4);
    fixture
        .store
        .claim_tool_lease(successor, epoch, retry.correlation())
        .await?;
    fixture
        .store
        .record_tool_lease_result(
            successor,
            epoch,
            retry.correlation(),
            signalbox_domain::ToolAttemptObservation::KnownFailed {
                error: signalbox_domain::ToolExecutionError::new(
                    signalbox_domain::ToolExecutionErrorKind::ExecutionFailed,
                    None,
                ),
            },
        )
        .await?;
    let attempts: i64 = sqlx::query_scalar("SELECT count(*) FROM turn_attempt WHERE turn_id = $1")
        .bind(fixture.facts.turn.into_uuid())
        .fetch_one(&pool)
        .await?;
    assert_eq!(attempts, 2);
    let next_call = ModelCallId::from_uuid(Uuid::now_v7());
    let repository =
        placement_loss::model_repository(&pool).with_runner_recovery(fixture.store.clone());
    let continuation = repository
        .tool_loop_repository()
        .prepare_continuation(
            fixture.facts.session,
            fixture.facts.turn,
            fixture.facts.producing_call,
            signalbox_application::ToolContinuationIdentities::new(
                vec![SemanticTranscriptEntryId::from_uuid(Uuid::now_v7())],
                ContextFrontierId::from_uuid(Uuid::now_v7()),
                next_call,
                signalbox_domain::FailedModelCallTurnIdentities::new(
                    SemanticTranscriptEntryId::from_uuid(Uuid::now_v7()),
                    ContextFrontierId::from_uuid(Uuid::now_v7()),
                ),
                ContextFrontierId::from_uuid(Uuid::now_v7()),
            ),
            |_| panic!("no steering is queued"),
        )
        .await?;
    assert_eq!(
        continuation,
        signalbox_application::PrepareToolContinuationOutcome::Checkpointed(next_call)
    );
    let suffix: Vec<String> = sqlx::query_scalar(
        "SELECT entry.payload_kind FROM model_call AS call
         JOIN LATERAL resolve_context_frontier_members(call.session_id, call.context_frontier_id) AS member ON true
         JOIN semantic_transcript_entry AS entry USING (source_session_id, semantic_entry_id)
         WHERE call.model_call_id = $1 ORDER BY member.member_position DESC LIMIT 4",
    ).bind(next_call.into_uuid()).fetch_all(&pool).await?;
    assert_eq!(
        suffix,
        [
            "runner_placement_changed",
            "runner_placement_changed",
            "runner_placement_changed",
            "tool_execution_result"
        ]
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn retry_refusal_resumes_the_turn_with_the_retained_failure() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture =
        installed_takeover(&pool, FixtureLoss::NoExecution(RunnerToolEffectClass::Pure)).await?;
    let retry = fixture
        .store
        .offer_runner_recovery_retry(fixture.successor, fixture.epoch, fixture.facts.turn)
        .await?
        .expect("fixture authority exists");
    fixture.store.record_tool_lease_failure(fixture.successor, fixture.epoch, retry.correlation(),
        signalbox_persistence::runner_protocol::RunnerLeaseFailureKind::LeaseAdmissionRefused,
        &serde_json::json!({"code":"admission_unavailable","message":"cannot admit","payload":{}}),
    ).await?;
    assert!(
        fixture
            .store
            .load_runner_recovery_wait(fixture.facts.session)
            .await?
            .is_none()
    );
    let batch = signalbox_persistence::tool_loop::PostgresToolLoopRepository::new(pool.clone())
        .load_active_batch(fixture.facts.session, fixture.facts.turn)
        .await?
        .expect("fixture authority exists");
    assert!(
        matches!(batch.attempt(fixture.facts.request), Some(signalbox_domain::ReconstitutedToolAttempt::Ended(ended))
        if matches!(ended.end(), signalbox_domain::ToolAttemptEnd::KnownFailed { error }
            if error.kind() == signalbox_domain::ToolExecutionErrorKind::ExecutionFailed))
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn ambiguous_retry_enters_tool_recovery_without_rewriting_the_yielded_attempt()
-> Result<(), Box<dyn Error>> {
    reconcile_ambiguous_retry(false).await
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn automatic_reconciliation_projects_the_pending_takeover_boundary()
-> Result<(), Box<dyn Error>> {
    reconcile_ambiguous_retry(true).await
}

async fn reconcile_ambiguous_retry(automatic: bool) -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = installed_takeover(
        &pool,
        FixtureLoss::ExecutionPossible(RunnerToolEffectClass::Idempotent),
    )
    .await?;
    let retry = fixture
        .store
        .offer_runner_recovery_retry(fixture.successor, fixture.epoch, fixture.facts.turn)
        .await?
        .expect("fixture authority exists");
    fixture
        .store
        .claim_tool_lease(fixture.successor, fixture.epoch, retry.correlation())
        .await?;
    fixture
        .store
        .record_tool_lease_result(
            fixture.successor,
            fixture.epoch,
            retry.correlation(),
            signalbox_domain::ToolAttemptObservation::Ambiguous,
        )
        .await?;
    assert!(
        fixture
            .store
            .load_runner_recovery_wait(fixture.facts.session)
            .await?
            .is_none()
    );
    let batch = signalbox_persistence::tool_loop::PostgresToolLoopRepository::new(pool.clone())
        .load_active_batch(fixture.facts.session, fixture.facts.turn)
        .await?
        .expect("fixture authority exists");
    assert!(
        matches!(batch.phase(), signalbox_domain::ToolBatchPhase::AwaitingRecovery { attempt } if attempt == retry.attempt())
    );
    let retained: (String, Uuid, String) = sqlx::query_as(
        "SELECT turn.active_phase_kind, turn.current_attempt_id, attempt.end_disposition
         FROM turn_lifecycle AS turn JOIN turn_attempt AS attempt ON attempt.turn_attempt_id = turn.current_attempt_id
         WHERE turn.turn_id = $1",
    ).bind(fixture.facts.turn.into_uuid()).fetch_one(&pool).await?;
    assert_eq!(
        retained,
        (
            "awaiting_tool_recovery".into(),
            fixture.facts.turn_attempt.into_uuid(),
            "yielded_to_durable_wait".into()
        )
    );
    if automatic {
        let repository = signalbox_persistence::automatic_reconciliation::PostgresAutomaticReconciliationRepository::new(pool.clone());
        let claimed = repository.claim_due().await?;
        assert_eq!(claimed.claimed().len(), 1);
        assert_eq!(claimed.claimed()[0].turn(), fixture.facts.turn);
        assert_eq!(
            repository.reconcile(claimed.claimed()[0]).await?,
            signalbox_application::AutomaticReconciliationOutcome::Reconciled,
        );
        let kinds: Vec<String> = sqlx::query_scalar(
            "SELECT entry.payload_kind FROM turn_lifecycle AS turn
             JOIN LATERAL resolve_context_frontier_members(turn.session_id, turn.terminal_frontier_id) AS member ON true
             JOIN semantic_transcript_entry AS entry USING (source_session_id, semantic_entry_id)
             WHERE turn.turn_id = $1 ORDER BY member.member_position DESC LIMIT 2",
        ).bind(fixture.facts.turn.into_uuid()).fetch_all(&pool).await?;
        assert_eq!(
            kinds,
            ["runner_placement_changed", "tool_closed_by_turn_end"]
        );
        let boundaries: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM runner_placement_boundary WHERE session_id = $1",
        )
        .bind(fixture.facts.session.into_uuid())
        .fetch_one(&pool)
        .await?;
        assert_eq!(boundaries, 1);
    } else {
        stop_retained_turn(&pool, &fixture.facts).await?;
    }
    let terminal: String = sqlx::query_scalar(
        "SELECT terminal_disposition_kind FROM turn_lifecycle WHERE turn_id = $1",
    )
    .bind(fixture.facts.turn.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(terminal, "reconciliation_required");
    Ok(())
}

async fn add_resolved_prefix_and_pending_suffix(
    pool: &PgPool,
    facts: &runner_recovery::RunnerRecoveryToolRoundFacts,
) -> Result<(), Box<dyn Error>> {
    let mut transaction = pool.begin().await?;
    sqlx::raw_sql("ALTER TABLE tool_request DISABLE TRIGGER ALL; ALTER TABLE tool_attempt DISABLE TRIGGER ALL;
        ALTER TABLE tool_approval_decision DISABLE TRIGGER ALL; ALTER TABLE semantic_transcript_entry DISABLE TRIGGER ALL;
        ALTER TABLE context_frontier DISABLE TRIGGER ALL; ALTER TABLE context_frontier_delta DISABLE TRIGGER ALL;
        ALTER TABLE tool_round DISABLE TRIGGER ALL;") .execute(&mut *transaction).await?;
    sqlx::query("UPDATE tool_request SET request_ordinal = 1 WHERE request_id = $1")
        .bind(facts.request.into_uuid())
        .execute(&mut *transaction)
        .await?;
    sqlx::query("UPDATE semantic_transcript_entry SET assistant_response_part_ordinal = 1 WHERE assistant_tool_request_id = $1")
        .bind(facts.request.into_uuid()).execute(&mut *transaction).await?;
    let base_position: Decimal = sqlx::query_scalar(
        "SELECT member_position FROM context_frontier_delta WHERE context_frontier_id = $1",
    )
    .bind(facts.boundary.into_uuid())
    .fetch_one(&mut *transaction)
    .await?;
    sqlx::query("UPDATE context_frontier_delta SET member_position = member_position + 1 WHERE context_frontier_id = $1")
        .bind(facts.boundary.into_uuid()).execute(&mut *transaction).await?;
    for ordinal in [0_u64, 2] {
        let request = Uuid::now_v7();
        let entry = Uuid::now_v7();
        sqlx::query("INSERT INTO tool_request (request_id, session_id, turn_id, producing_model_call_id, request_ordinal, tool_name, arguments_kind, arguments_text, approval_posture)
            SELECT $1, session_id, turn_id, producing_model_call_id, $2, tool_name, arguments_kind, arguments_text, approval_posture FROM tool_request WHERE request_id = $3")
            .bind(request).bind(Decimal::from(ordinal)).bind(facts.request.into_uuid()).execute(&mut *transaction).await?;
        sqlx::query("INSERT INTO tool_approval_decision (request_id, decision_kind, decision_source) VALUES ($1, 'approve', 'policy_auto')")
            .bind(request).execute(&mut *transaction).await?;
        sqlx::query("INSERT INTO semantic_transcript_entry (source_session_id, semantic_entry_id, payload_kind, producing_model_call_id, assistant_tool_request_id, assistant_response_part_ordinal)
            VALUES ($1, $2, 'assistant_tool_use', $3, $4, $5)")
            .bind(facts.session.into_uuid()).bind(entry).bind(facts.producing_call.into_uuid()).bind(request).bind(Decimal::from(ordinal))
            .execute(&mut *transaction).await?;
        sqlx::query("INSERT INTO context_frontier_delta (owning_session_id, context_frontier_id, member_position, source_session_id, semantic_entry_id) VALUES ($1, $2, $3, $1, $4)")
            .bind(facts.session.into_uuid()).bind(facts.boundary.into_uuid()).bind(base_position + Decimal::from(ordinal)).bind(entry).execute(&mut *transaction).await?;
        if ordinal == 0 {
            sqlx::query("INSERT INTO tool_attempt (attempt_id, request_id, session_id, turn_id, issuing_turn_attempt_id, effect_class, dispatch_generation, state_kind, terminal_disposition_kind, error_kind)
                SELECT $1, $2, session_id, turn_id, issuing_turn_attempt_id, effect_class, dispatch_generation, 'terminal', 'known_failed', 'execution_failed' FROM tool_attempt WHERE attempt_id = $3")
                .bind(Uuid::now_v7()).bind(request).bind(facts.interrupted_attempt.into_uuid()).execute(&mut *transaction).await?;
        }
    }
    sqlx::query("UPDATE context_frontier SET member_count = member_count + 2 WHERE context_frontier_id = $1")
        .bind(facts.boundary.into_uuid()).execute(&mut *transaction).await?;
    sqlx::query("UPDATE tool_round SET response_part_count = 3, request_count = 3 WHERE producing_model_call_id = $1")
        .bind(facts.producing_call.into_uuid()).execute(&mut *transaction).await?;
    sqlx::raw_sql("ALTER TABLE tool_request ENABLE TRIGGER ALL; ALTER TABLE tool_attempt ENABLE TRIGGER ALL;
        ALTER TABLE tool_approval_decision ENABLE TRIGGER ALL; ALTER TABLE semantic_transcript_entry ENABLE TRIGGER ALL;
        ALTER TABLE context_frontier ENABLE TRIGGER ALL; ALTER TABLE context_frontier_delta ENABLE TRIGGER ALL;
        ALTER TABLE tool_round ENABLE TRIGGER ALL;") .execute(&mut *transaction).await?;
    transaction.commit().await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn takeover_preserves_proposal_order_through_the_remaining_batch()
-> Result<(), Box<dyn Error>> {
    check_remaining_batch(false).await
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_later_request_can_lose_and_retry_after_an_earlier_takeover() -> Result<(), Box<dyn Error>>
{
    check_remaining_batch(true).await
}

async fn check_remaining_batch(lose_later: bool) -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = installed_takeover_with_neighbors(
        &pool,
        FixtureLoss::NoExecution(RunnerToolEffectClass::Pure),
        true,
    )
    .await?;
    let retry = fixture
        .store
        .offer_runner_recovery_retry(fixture.successor, fixture.epoch, fixture.facts.turn)
        .await?
        .expect("retained middle request");
    fixture
        .store
        .claim_tool_lease(fixture.successor, fixture.epoch, retry.correlation())
        .await?;
    fixture
        .store
        .record_tool_lease_result(
            fixture.successor,
            fixture.epoch,
            retry.correlation(),
            signalbox_domain::ToolAttemptObservation::KnownFailed {
                error: signalbox_domain::ToolExecutionError::new(
                    signalbox_domain::ToolExecutionErrorKind::ExecutionFailed,
                    None,
                ),
            },
        )
        .await?;
    let repository =
        placement_loss::model_repository(&pool).with_runner_recovery(fixture.store.clone());
    let batch_store = repository.tool_loop_repository();
    let next = batch_store
        .prepare_next_attempt(
            fixture.facts.session,
            fixture.facts.turn,
            ToolAttemptId::from_uuid(Uuid::now_v7()),
            signalbox_domain::ToolEffectClass::EffectFree,
        )
        .await?
        .expect("the following request becomes executable");
    let ordinal: Decimal =
        sqlx::query_scalar("SELECT request_ordinal FROM tool_request WHERE request_id = $1")
            .bind(next.request().into_uuid())
            .fetch_one(&pool)
            .await?;
    assert_eq!(ordinal, Decimal::from(2));
    let authority = batch_store
        .authorize_attempt(fixture.facts.session, fixture.facts.turn, next.attempt())
        .await?;
    let observation = signalbox_domain::ToolAttemptObservation::KnownFailed {
        error: signalbox_domain::ToolExecutionError::new(
            signalbox_domain::ToolExecutionErrorKind::ExecutionFailed,
            None,
        ),
    };
    if lose_later {
        let offered = fixture
            .store
            .offer_tool_dispatch(&authority, RunnerLeaseId::from_uuid(Uuid::now_v7()))
            .await
            .expect("later request takeover step")
            .expect("later request uses the installed runner");
        fixture
            .store
            .claim_tool_lease(fixture.successor, fixture.epoch, offered.correlation())
            .await
            .expect("later request takeover step");
        let (successor, epoch) =
            replace_disconnected_successor(&fixture, fixture.successor, fixture.epoch)
                .await
                .expect("later request takeover step");
        let retry = fixture
            .store
            .offer_runner_recovery_retry(successor, epoch, fixture.facts.turn)
            .await
            .expect("later request takeover step")
            .expect("later loss retains its own request for takeover");
        assert_ne!(retry.attempt(), next.attempt());
        fixture
            .store
            .claim_tool_lease(successor, epoch, retry.correlation())
            .await
            .expect("later request takeover step");
        fixture
            .store
            .record_tool_lease_result(successor, epoch, retry.correlation(), observation)
            .await
            .expect("later request takeover step");
    } else {
        batch_store
            .commit_observation(authority.executor_fence().bind(observation))
            .await?;
    }
    let next_call = ModelCallId::from_uuid(Uuid::now_v7());
    let outcome = batch_store
        .prepare_continuation(
            fixture.facts.session,
            fixture.facts.turn,
            fixture.facts.producing_call,
            signalbox_application::ToolContinuationIdentities::new(
                (0..3)
                    .map(|_| SemanticTranscriptEntryId::from_uuid(Uuid::now_v7()))
                    .collect(),
                ContextFrontierId::from_uuid(Uuid::now_v7()),
                next_call,
                signalbox_domain::FailedModelCallTurnIdentities::new(
                    SemanticTranscriptEntryId::from_uuid(Uuid::now_v7()),
                    ContextFrontierId::from_uuid(Uuid::now_v7()),
                ),
                ContextFrontierId::from_uuid(Uuid::now_v7()),
            ),
            |_| panic!("no steering is queued"),
        )
        .await?;
    assert_eq!(
        outcome,
        signalbox_application::PrepareToolContinuationOutcome::Checkpointed(next_call)
    );
    let results: Vec<Decimal> = sqlx::query_scalar("SELECT request.request_ordinal FROM model_call AS call
        JOIN LATERAL resolve_context_frontier_members(call.session_id, call.context_frontier_id) AS member ON true
        JOIN semantic_transcript_entry AS entry USING (source_session_id, semantic_entry_id)
        JOIN tool_attempt AS attempt ON attempt.attempt_id = entry.tool_result_attempt_id
        JOIN tool_request AS request ON request.request_id = attempt.request_id
        WHERE call.model_call_id = $1 ORDER BY member.member_position")
        .bind(next_call.into_uuid()).fetch_all(&pool).await?;
    assert_eq!(results, [Decimal::ZERO, Decimal::ONE, Decimal::from(2)]);
    Ok(())
}
