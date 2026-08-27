//! PostgreSQL integration proof for bounded current-session snapshots.

use crate::*;
use signalbox_application::{
    SessionLiveActiveState, SessionLiveActiveTurn, SessionLiveReconciliation,
    max_session_live_queued_turns,
};
use signalbox_persistence::session_live::{SessionLiveRepository, SessionLiveRepositoryError};

const LIVE_SEED: u128 = 0x51_1e00;

async fn queue_fixture_turns(
    pool: &PgPool,
    session: SessionId,
    active: TurnId,
    count: u64,
) -> Result<Vec<TurnId>, Box<dyn Error>> {
    let mut turns = Vec::new();
    for ordinal in 1..=count {
        let turn = TurnId::from_uuid(Uuid::from_u128(LIVE_SEED + 0x400 + u128::from(ordinal)));
        let outcome = SubmitInputRepository::new(pool.clone())
            .handle(
                input_with_delivery(
                    LIVE_SEED + 0x100 + u128::from(ordinal),
                    session.into_uuid().as_u128(),
                    "bounded live queue fixture",
                    DeliveryRequest::AfterCurrentTurn {
                        expected_active_turn: active,
                        configuration: input_choices(1, ModelSelectionOverride::UseSessionDefault),
                    },
                ),
                AcceptedInputId::from_uuid(Uuid::from_u128(
                    LIVE_SEED + 0x200 + u128::from(ordinal),
                )),
                Some(turn),
            )
            .await?;
        let SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Applied(
            SubmitInputAppliedResult::TurnOrigin(_),
        )) = outcome
        else {
            return Err("fixture input was not queued".into());
        };
        turns.push(turn);
    }
    Ok(turns)
}

/// Recomputes the queue preview from the backfill predicate the migration
/// seeds `session_live_queued_turn` with, so a goal-event trigger that
/// diverges from that one definition fails here instead of surfacing as a
/// count/preview mismatch that fails a live read closed.
async fn backfilled_live_queue(
    pool: &PgPool,
    session: SessionId,
) -> Result<Vec<TurnId>, Box<dyn Error>> {
    let turns: Vec<Uuid> = sqlx::query_scalar(
        "SELECT turn_id
           FROM turn_lifecycle
          WHERE session_id = $1
            AND state_kind = 'queued'
            AND NOT delegation_runtime_terminal
            AND goal_turn_is_runtime_relevant(session_id, turn_id)
          ORDER BY acceptance_position",
    )
    .bind(session.into_uuid())
    .fetch_all(pool)
    .await?;
    Ok(turns.into_iter().map(TurnId::from_uuid).collect())
}

fn corruption_field(error: SessionLiveRepositoryError) -> Option<&'static str> {
    match error {
        SessionLiveRepositoryError::Corruption(field) => Some(field),
        SessionLiveRepositoryError::Database(_)
        | SessionLiveRepositoryError::Process(_)
        | SessionLiveRepositoryError::Unsupported { .. } => None,
    }
}

async fn create_live_session(pool: &PgPool) -> Result<SessionId, Box<dyn Error>> {
    let session = SessionId::from_uuid(Uuid::from_u128(LIVE_SEED));
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(prepared(
            LIVE_SEED + 0x300,
            LIVE_SEED,
            ModelSelectionRequest::Direct(DirectModelSelection::from_uuid(Uuid::from_u128(
                LIVE_SEED + 0x301,
            ))),
        ))
        .await?;
    Ok(session)
}

async fn submit_first_live_turn(
    pool: &PgPool,
    session: SessionId,
) -> Result<TurnId, Box<dyn Error>> {
    let turn = TurnId::from_uuid(Uuid::from_u128(LIVE_SEED + 1));
    SubmitInputRepository::new(pool.clone())
        .handle(
            start_input(
                LIVE_SEED + 0x100,
                session.into_uuid().as_u128(),
                "first live turn",
                1,
                ModelSelectionOverride::UseSessionDefault,
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(LIVE_SEED + 0x200)),
            Some(turn),
        )
        .await?;
    Ok(turn)
}

async fn activate_first_live_turn(pool: &PgPool, session: SessionId) -> Result<(), Box<dyn Error>> {
    activate_earliest_queued_turn(
        pool,
        EarliestQueuedTurnActivation {
            session: session.into_uuid(),
            origin_entry: Uuid::from_u128(LIVE_SEED + 0x302),
            starting_frontier: Uuid::from_u128(LIVE_SEED + 0x303),
            initial_attempt: Uuid::from_u128(LIVE_SEED + 0x304),
        },
    )
    .await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn live_snapshot_projects_the_initial_queue() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let session = create_live_session(&pool).await?;
    let first_turn = submit_first_live_turn(&pool, session).await?;
    let snapshot = SessionLiveRepository::new(pool.clone())
        .read_live_snapshot(session)
        .await?
        .expect("the created session has a live snapshot");

    assert_eq!(snapshot.queued_turn_count, 1);
    assert_eq!(snapshot.queued_turns, [first_turn]);
    assert_eq!(snapshot.active, None);

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn live_snapshot_tracks_turn_activation() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let session = create_live_session(&pool).await?;
    let first_turn = submit_first_live_turn(&pool, session).await?;
    activate_first_live_turn(&pool, session).await?;
    let snapshot = SessionLiveRepository::new(pool.clone())
        .read_live_snapshot(session)
        .await?
        .expect("the activated session has a live snapshot");

    assert_eq!(snapshot.queued_turn_count, 0);
    assert_eq!(snapshot.queued_turns, []);
    assert_eq!(
        snapshot.active,
        Some(SessionLiveActiveTurn {
            turn: first_turn,
            state: SessionLiveActiveState::Running { model_call: None },
        })
    );

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn live_snapshot_caps_the_queue_preview() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let session = create_live_session(&pool).await?;
    let first_turn = submit_first_live_turn(&pool, session).await?;
    activate_first_live_turn(&pool, session).await?;
    let turns = queue_fixture_turns(&pool, session, first_turn, 33).await?;
    let snapshot = SessionLiveRepository::new(pool.clone())
        .read_live_snapshot(session)
        .await?
        .expect("the occupied session has a bounded live snapshot");
    let preview_limit = usize::from(max_session_live_queued_turns());

    assert_eq!(snapshot.queued_turn_count, 33);
    assert_eq!(snapshot.queued_turns.len(), preview_limit);
    assert_eq!(snapshot.queued_turns, turns[..preview_limit]);

    pool.close().await;
    drop(container);
    Ok(())
}

/// A queued successor must not hide the latest outstanding terminal
/// reconciliation: reconciliation does not gate later queue admission, so the
/// projection filters for the reconciliation-required terminal shape instead
/// of inspecting only the turn with the greatest acceptance position.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn live_snapshot_reports_reconciliation_behind_a_queued_successor()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let parked =
        crate::model_call_execution_and_recovery::park_restart_ambiguity(&pool, 0xD7_0000).await?;
    let reconciliation = PostgresAutomaticReconciliationRepository::new(pool.clone());
    let batch = reconciliation.claim_due().await?;
    let outcome = reconciliation.reconcile(batch.claimed()[0]).await?;
    assert_eq!(outcome, AutomaticReconciliationOutcome::Reconciled);
    let successor = TurnId::from_uuid(Uuid::from_u128(0xD7_1003));
    let queued = SubmitInputRepository::new(pool.clone())
        .handle(
            start_input(
                0xD7_1001,
                parked.session.into_uuid().as_u128(),
                "successor queued behind the reconciliation",
                1,
                ModelSelectionOverride::UseSessionDefault,
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(0xD7_1002)),
            Some(successor),
        )
        .await?;
    let SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Applied(
        SubmitInputAppliedResult::TurnOrigin(_),
    )) = queued
    else {
        return Err("the successor input was not queued".into());
    };
    let snapshot = SessionLiveRepository::new(pool.clone())
        .read_live_snapshot(parked.session)
        .await?
        .expect("the reconciliation-parked session has a live snapshot");

    assert_eq!(snapshot.active, None);
    assert_eq!(snapshot.queued_turns, [successor]);
    assert_eq!(
        snapshot.reconciliation,
        Some(SessionLiveReconciliation::ModelCall {
            turn: parked.turn,
            call: parked.call,
        })
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// A delegation termination cascade marks a turn logically terminal while its
/// underlying `state_kind` deliberately stays `active`, so the live current
/// projection must exclude it exactly as the timeline-fact and scheduler
/// predicates do — otherwise a later active turn makes two selected rows and
/// every live read fails with an active-cardinality corruption.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn live_snapshot_excludes_a_delegation_terminated_active_turn() -> Result<(), Box<dyn Error>>
{
    let (container, pool, _database_url) = migrated_postgres().await?;
    let session = create_live_session(&pool).await?;
    submit_first_live_turn(&pool, session).await?;
    activate_first_live_turn(&pool, session).await?;
    // A real release runs through the delegation termination cascade with its
    // logical-terminal proof rows; this narrows the fixture to the released
    // runtime slot itself, as the runner-protocol release test does.
    sqlx::query("ALTER TABLE turn_lifecycle DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query(
        "UPDATE turn_lifecycle
            SET origin_kind = 'delegation', origin_accepted_input_id = NULL,
                delegation_runtime_terminal = true
          WHERE session_id = $1",
    )
    .bind(session.into_uuid())
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE turn_lifecycle ENABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    let snapshot = SessionLiveRepository::new(pool.clone())
        .read_live_snapshot(session)
        .await?
        .expect("the terminated session still has a live snapshot");

    assert_eq!(snapshot.active, None);
    assert_eq!(snapshot.queued_turn_count, 0);
    assert_eq!(snapshot.reconciliation, None);

    pool.close().await;
    drop(container);
    Ok(())
}

/// Goal events reconcile the live queue preview relation through the same
/// runtime-relevance predicate that recomputes `queued_turn_count`, so a
/// retiring event can never leave the preview holding more rows than the
/// exact count, which a live read would fail closed on.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn goal_transitions_keep_the_live_queue_preview_reconciled() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let session = create_live_session(&pool).await?;
    commission_fixture_session_goal(&pool, session, LIVE_SEED + 0x2000).await?;
    let commissioned = SessionLiveRepository::new(pool.clone())
        .read_live_snapshot(session)
        .await?
        .expect("the commissioned session has a live snapshot");
    let commissioned_backfill = backfilled_live_queue(&pool, session).await?;
    stop_fixture_session_goal(&pool, session, LIVE_SEED + 0x2100).await?;
    let stopped = SessionLiveRepository::new(pool.clone())
        .read_live_snapshot(session)
        .await?
        .expect("the stopped session has a live snapshot");
    let stopped_backfill = backfilled_live_queue(&pool, session).await?;

    assert_eq!(commissioned.queued_turn_count, 1);
    assert_eq!(commissioned.queued_turns, commissioned_backfill);
    assert_eq!(stopped.queued_turn_count, 0);
    assert_eq!(stopped.queued_turns, stopped_backfill);
    assert_eq!(stopped.queued_turns, []);

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn live_snapshot_returns_none_for_an_absent_session() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let absent = SessionLiveRepository::new(pool.clone())
        .read_live_snapshot(SessionId::from_uuid(Uuid::from_u128(LIVE_SEED + 0xffff)))
        .await?;

    assert_eq!(absent, None);

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn existing_session_with_missing_live_facts_fails_closed() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let session = SessionId::from_uuid(Uuid::from_u128(LIVE_SEED + 0x1000));
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(prepared(
            LIVE_SEED + 0x1001,
            LIVE_SEED + 0x1000,
            ModelSelectionRequest::Direct(DirectModelSelection::from_uuid(Uuid::from_u128(
                LIVE_SEED + 0x1002,
            ))),
        ))
        .await?;
    sqlx::query("DELETE FROM session_timeline_fact WHERE session_id = $1")
        .bind(session.into_uuid())
        .execute(&pool)
        .await?;
    let error = SessionLiveRepository::new(pool.clone())
        .read_live_snapshot(session)
        .await
        .expect_err("missing bounded facts are corruption, not absence");

    assert_eq!(corruption_field(error), Some("queued_turn_count"));

    pool.close().await;
    drop(container);
    Ok(())
}
