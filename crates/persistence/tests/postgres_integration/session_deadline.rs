use std::{error::Error, time::Duration};

use crate::*;
use signalbox_domain::{
    DispatchingModule, DurableCommandId, LifecycleActor, SessionCreationCause,
    SessionCreationProvenance, SessionId, SessionLifecycleState, SessionOwnership,
    SessionParkCause, SessionParkResponder, StartGate, TranscriptAncestry,
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

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn admission_clock_survives_the_created_to_dispatched_transition()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let creation = owned_creation(4, StartGate::Open);
    let session = creation.applied_result().session();
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(creation)
        .await?;
    let created_armed_at: sqlx::types::time::OffsetDateTime =
        sqlx::query_scalar("SELECT armed_at FROM session_deadline WHERE session_id = $1")
            .bind(session.into_uuid())
            .fetch_one(&pool)
            .await?;

    queue_turn(&pool, session, 4).await?;
    let dispatched_armed_at: sqlx::types::time::OffsetDateTime =
        sqlx::query_scalar("SELECT armed_at FROM session_deadline WHERE session_id = $1")
            .bind(session.into_uuid())
            .fetch_one(&pool)
            .await?;

    assert_eq!(dispatched_armed_at, created_armed_at);
    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn held_start_gate_survives_a_module_park_and_resume() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let creation = owned_creation(5, StartGate::Held);
    let session = creation.applied_result().session();
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(creation)
        .await?;
    queue_turn(&pool, session, 5).await?;

    let repository = SessionLifecycleRepository::new(pool.clone());
    let parked = repository
        .park(
            session,
            SessionParkCause::ModulePark,
            SessionParkResponder::Module {
                module: DispatchingModule::CommissionedDispatch,
            },
            None,
            LifecycleActor::Module {
                module: DispatchingModule::CommissionedDispatch,
            },
        )
        .await?;

    assert!(matches!(
        parked,
        SessionLifecycleState::Parked {
            cause: SessionParkCause::ModulePark,
            ..
        }
    ));
    assert_eq!(
        repository.resume(session).await?,
        SessionLifecycleState::Created
    );
    let resumed: (String, bool) = sqlx::query_as(
        "SELECT state_kind, start_gate_held FROM session_lifecycle WHERE session_id = $1",
    )
    .bind(session.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(resumed, (String::from("created"), true));
    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn migration_rewrites_stored_admission_retirement_causes_before_validation()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = unmigrated_postgres().await?;
    signalbox_persistence::MIGRATOR
        .run_to(202609020021, &pool)
        .await?;
    let terminal_creation = owned_creation(6, StartGate::Open);
    let terminal_session = terminal_creation.applied_result().session();
    let pending_creation = owned_creation(7, StartGate::Open);
    let pending_session = pending_creation.applied_result().session();
    let creation_repository =
        CreateSessionRepository::new(pool.clone(), test_session_credential_pin());
    creation_repository.handle(terminal_creation).await?;
    creation_repository.handle(pending_creation).await?;
    sqlx::query(
        "UPDATE session_lifecycle
            SET state_kind = 'terminal',
                state_entered_at = statement_timestamp(),
                ended_at = statement_timestamp(),
                terminal_outcome_kind = 'retired',
                terminal_cause_kind = 'dispatch_deadline_expired'
          WHERE session_id = $1",
    )
    .bind(terminal_session.into_uuid())
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE session_lifecycle
            SET pending_terminal_outcome_kind = 'retired',
                pending_terminal_cause_kind = 'start_gate_deadline_expired',
                pending_terminal_actor_kind = 'operator'
          WHERE session_id = $1",
    )
    .bind(pending_session.into_uuid())
    .execute(&pool)
    .await?;

    signalbox_persistence::MIGRATOR.run(&pool).await?;
    let causes: (String, String) = sqlx::query_as(
        "SELECT terminal.terminal_cause_kind, pending.pending_terminal_cause_kind
           FROM session_lifecycle AS terminal
           JOIN session_lifecycle AS pending ON pending.session_id = $2
          WHERE terminal.session_id = $1",
    )
    .bind(terminal_session.into_uuid())
    .bind(pending_session.into_uuid())
    .fetch_one(&pool)
    .await?;

    assert_eq!(
        causes,
        (
            String::from("admission_deadline_expired"),
            String::from("admission_deadline_expired"),
        )
    );
    pool.close().await;
    drop(container);
    Ok(())
}
