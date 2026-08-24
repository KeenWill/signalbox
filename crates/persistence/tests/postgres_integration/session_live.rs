//! PostgreSQL integration proof for bounded current-session snapshots.

use crate::*;
use signalbox_application::{
    SessionLiveActiveState, SessionLiveActiveTurn, max_session_live_queued_turns,
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

fn corruption_field(error: SessionLiveRepositoryError) -> Option<&'static str> {
    match error {
        SessionLiveRepositoryError::Corruption(field) => Some(field),
        SessionLiveRepositoryError::Database(_)
        | SessionLiveRepositoryError::Process(_)
        | SessionLiveRepositoryError::Unsupported { .. } => None,
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn snapshot_caps_queue_preview_and_tracks_activation() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
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
    let repository = SessionLiveRepository::new(pool.clone());
    let first_turn = TurnId::from_uuid(Uuid::from_u128(LIVE_SEED + 1));
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
            Some(first_turn),
        )
        .await?;
    let queued = repository
        .read_live_snapshot(session)
        .await?
        .expect("the created session has a live snapshot");
    activate_earliest_queued_turn(
        &pool,
        EarliestQueuedTurnActivation {
            session: session.into_uuid(),
            origin_entry: Uuid::from_u128(LIVE_SEED + 0x302),
            starting_frontier: Uuid::from_u128(LIVE_SEED + 0x303),
            initial_attempt: Uuid::from_u128(LIVE_SEED + 0x304),
        },
    )
    .await?;
    let active = repository
        .read_live_snapshot(session)
        .await?
        .expect("the activated session has a live snapshot");
    let turns = queue_fixture_turns(&pool, session, first_turn, 33).await?;
    let bounded = repository
        .read_live_snapshot(session)
        .await?
        .expect("the occupied session has a bounded live snapshot");
    let absent = repository
        .read_live_snapshot(SessionId::from_uuid(Uuid::from_u128(LIVE_SEED + 0xffff)))
        .await?;
    let preview_limit = usize::from(max_session_live_queued_turns());

    assert_eq!(queued.queued_turn_count, 1);
    assert_eq!(queued.queued_turns, [first_turn]);
    assert_eq!(queued.active, None);
    assert_eq!(active.queued_turn_count, 0);
    assert_eq!(active.queued_turns, []);
    assert_eq!(bounded.queued_turn_count, 33);
    assert_eq!(bounded.queued_turns.len(), preview_limit);
    assert_eq!(bounded.queued_turns, turns[..preview_limit]);
    assert_eq!(
        active.active,
        Some(SessionLiveActiveTurn {
            turn: first_turn,
            state: SessionLiveActiveState::Running { model_call: None },
        })
    );
    assert_eq!(bounded.active, active.active);
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
