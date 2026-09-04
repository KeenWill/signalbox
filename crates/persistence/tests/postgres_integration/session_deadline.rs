use std::{error::Error, time::Duration};

use crate::*;
use signalbox_domain::{
    DurableCommandId, SessionCreationCause, SessionCreationProvenance, SessionId,
    SessionLifecycleState, SessionOwnership, StartGate, TranscriptAncestry,
};
use signalbox_persistence::{
    create_session::CreateSessionRepository,
    scheduler::PostgresEligibilitySweep,
    session_deadline::{
        PostgresSessionDeadlineRepository, SessionDeadlineBounds, SessionDeadlinePassOutcome,
    },
    session_lifecycle::SessionLifecycleRepository,
};

const SEED: u128 = 0x11fe_9000;

fn owned_creation(seed: u128, gate: StartGate) -> PreparedCreateSession {
    CreateSession::new(
        DurableCommandId::from_uuid(Uuid::from_u128(SEED + seed)),
        SessionCreationProvenance::new(SessionCreationCause::Interactive, TranscriptAncestry::None),
        SessionConfigurationDefaults::new(direct(SEED + seed + 0x100)),
    )
    .with_lifecycle(gate, SessionOwnership::Owned, None)
    .prepare(SessionId::from_uuid(Uuid::from_u128(SEED + seed + 0x200)))
    .expect("the owned creation is preparable")
}

async fn queue_turn(
    pool: &PgPool,
    session: SessionId,
    seed: u128,
) -> Result<TurnId, Box<dyn Error>> {
    let turn = TurnId::from_uuid(Uuid::from_u128(SEED + seed + 0x300));
    SubmitInputRepository::new(pool.clone())
        .handle(
            SubmitInput::new(
                DurableCommandId::from_uuid(Uuid::from_u128(SEED + seed + 0x400)),
                session,
                UserContent::try_text(String::from("deadline fixture input"))
                    .expect("fixture input is admitted"),
                DeliveryRequest::StartWhenNoActiveTurn {
                    configuration: input_choices(1, ModelSelectionOverride::UseSessionDefault),
                },
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(SEED + seed + 0x500)),
            Some(turn),
        )
        .await?;
    Ok(turn)
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn admission_expiry_retires_the_held_session_and_queued_turn_together()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let creation = owned_creation(1, StartGate::Held);
    let session = creation.applied_result().session();
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(creation)
        .await?;
    let turn = queue_turn(&pool, session, 1).await?;

    let outcome = PostgresSessionDeadlineRepository::new(
        pool.clone(),
        SessionDeadlineBounds::new(Some(Duration::ZERO), None),
    )
    .expire_next()
    .await?;
    assert_eq!(outcome, SessionDeadlinePassOutcome::Retired { session });
    let lifecycle = SessionLifecycleRepository::new(pool.clone())
        .load(session)
        .await?
        .expect("the session retains its terminal lifecycle row");
    assert!(matches!(
        lifecycle.state(),
        SessionLifecycleState::Terminal { .. }
    ));
    let turn_state: (String, Option<String>) = sqlx::query_as(
        "SELECT state_kind, terminal_disposition_kind
           FROM turn_lifecycle WHERE turn_id = $1",
    )
    .bind(turn.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        turn_state,
        (String::from("terminal"), Some(String::from("retired")))
    );

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn waiting_expiry_parks_without_terminalizing_the_turn() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let creation = owned_creation(2, StartGate::Open);
    let session = creation.applied_result().session();
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(creation)
        .await?;
    sqlx::query(
        "UPDATE session_lifecycle
            SET state_kind = 'waiting',
                state_entered_at = statement_timestamp(),
                actor_kind = 'core',
                waiting_kind = 'external',
                waiting_waker = 'external_recheck'
          WHERE session_id = $1",
    )
    .bind(session.into_uuid())
    .execute(&pool)
    .await?;

    let outcome = PostgresSessionDeadlineRepository::new(
        pool.clone(),
        SessionDeadlineBounds::new(None, Some(Duration::ZERO)),
    )
    .expire_next()
    .await?;
    assert_eq!(outcome, SessionDeadlinePassOutcome::Parked { session });
    let lifecycle = SessionLifecycleRepository::new(pool.clone())
        .load(session)
        .await?
        .expect("the parked session retains its lifecycle row");
    assert!(matches!(
        lifecycle.state(),
        SessionLifecycleState::Parked { .. }
    ));
    let terminal_turns: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM turn_lifecycle
          WHERE session_id = $1 AND state_kind = 'terminal'",
    )
    .bind(session.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(terminal_turns, 0);

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn held_sessions_are_not_returned_by_the_eligibility_sweep() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let creation = owned_creation(3, StartGate::Held);
    let session = creation.applied_result().session();
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(creation)
        .await?;
    queue_turn(&pool, session, 3).await?;

    let batch = PostgresEligibilitySweep::new(pool.clone())
        .find_sessions()
        .await?;
    assert!(!batch.into_parts().0.contains(&session));

    pool.close().await;
    drop(container);
    Ok(())
}
