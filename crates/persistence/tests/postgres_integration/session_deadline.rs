use std::{error::Error, time::Duration};

use crate::*;
use signalbox_domain::{
    CommandPrincipal, DispatchingModule, DurableCommandId, GoalCommandResult, GoalStatement,
    GoalUserAction, GoalUserCommand, LifecycleActor, SessionCreationCause,
    SessionCreationProvenance, SessionId, SessionLifecycleApplication, SessionLifecycleCommand,
    SessionLifecycleCommandResult, SessionLifecycleOperation, SessionLifecycleState,
    SessionOwnership, SessionParkCause, SessionParkResponder, StartGate, TranscriptAncestry,
};
use signalbox_persistence::{
    create_session::CreateSessionRepository,
    goal::{GoalCommandHandlingOutcome, GoalRepository},
    goal_turn::GoalTurnCandidates,
    scheduler::PostgresEligibilitySweep,
    session_deadline::{
        PostgresSessionDeadlineRepository, SessionDeadlineBounds, SessionDeadlinePassOutcome,
    },
    session_lifecycle::SessionLifecycleRepository,
    session_lifecycle_command::{
        SessionLifecycleCommandHandlingOutcome, SessionLifecycleCommandRepository,
    },
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
/// expiry waits for the session scheduler lock before retiring turns.
async fn admission_expiry_retires_the_held_session_and_queued_turn_together()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let creation = owned_creation(1, StartGate::Held);
    let session = creation.applied_result().session();
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(creation)
        .await?;
    let turn = queue_turn(&pool, session, 1).await?;

    let mut scheduler_blocker = pool.begin().await?;
    sqlx::query("SELECT session_id FROM session_scheduler WHERE session_id = $1 FOR UPDATE")
        .bind(session.into_uuid())
        .execute(&mut *scheduler_blocker)
        .await?;
    let expiry = tokio::spawn({
        let repository = PostgresSessionDeadlineRepository::new(
            pool.clone(),
            SessionDeadlineBounds::new(Some(Duration::ZERO), None),
        );
        async move { repository.expire_next().await }
    });
    assert!(
        blocked_backends_reached(&pool, 1).await?,
        "admission expiry must block on the held scheduler row"
    );
    let queued_state: String =
        sqlx::query_scalar("SELECT state_kind FROM turn_lifecycle WHERE turn_id = $1")
            .bind(turn.into_uuid())
            .fetch_one(&pool)
            .await?;
    assert_eq!(queued_state, "queued");

    scheduler_blocker.rollback().await?;
    let outcome = expiry.await??;
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
    GoalRepository::new(pool.clone())
        .handle_user_command(
            GoalUserCommand::new(
                DurableCommandId::from_uuid(Uuid::from_u128(SEED + 0x2a00)),
                session,
                GoalUserAction::Attach(
                    GoalStatement::try_new(String::from("continue after the expired wait"))
                        .expect("the fixture goal is admitted"),
                ),
            ),
            Some(GoalTurnCandidates::new(
                AcceptedInputId::from_uuid(Uuid::from_u128(SEED + 0x2b00)),
                TurnId::from_uuid(Uuid::from_u128(SEED + 0x2c00)),
            )),
            |_| None,
        )
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
    let resume = SessionLifecycleCommandRepository::new(pool.clone())
        .handle(
            SessionLifecycleCommand::new(
                DurableCommandId::from_uuid(Uuid::from_u128(SEED + 0x2d00)),
                session,
                SessionLifecycleOperation::Resume,
            ),
            CommandPrincipal::Operator,
        )
        .await?;
    assert_eq!(
        resume,
        SessionLifecycleCommandHandlingOutcome::Recorded(SessionLifecycleCommandResult::Applied(
            SessionLifecycleApplication::Resumed {
                state: SessionLifecycleState::Dispatched,
            }
        ))
    );

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
async fn retired_never_started_turn_restores_dispatched_admission() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let creation = owned_creation(6, StartGate::Open);
    let session = creation.applied_result().session();
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(creation)
        .await?;
    let turn = TurnId::from_uuid(Uuid::from_u128(SEED + 0x6c00));
    let goal_repository = GoalRepository::new(pool.clone());
    let attached = goal_repository
        .handle_user_command(
            GoalUserCommand::new(
                DurableCommandId::from_uuid(Uuid::from_u128(SEED + 0x6a00)),
                session,
                GoalUserAction::Attach(
                    GoalStatement::try_new(String::from("never started parked goal"))
                        .expect("the fixture goal is admitted"),
                ),
            ),
            Some(GoalTurnCandidates::new(
                AcceptedInputId::from_uuid(Uuid::from_u128(SEED + 0x6b00)),
                turn,
            )),
            |_| None,
        )
        .await?;
    let lifecycle_repository = SessionLifecycleRepository::new(pool.clone());
    lifecycle_repository
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
    let stopped = goal_repository
        .handle_user_command(
            GoalUserCommand::new(
                DurableCommandId::from_uuid(Uuid::from_u128(SEED + 0x6d00)),
                session,
                GoalUserAction::Stop {
                    descendant_scope: signalbox_domain::DescendantTerminationScope::ParentAlone,
                },
            ),
            None,
            |_| None,
        )
        .await?;

    assert!(matches!(
        attached,
        GoalCommandHandlingOutcome::Recorded(GoalCommandResult::Applied(_))
    ));
    assert!(matches!(
        stopped,
        GoalCommandHandlingOutcome::Recorded(GoalCommandResult::Applied(_))
    ));
    let retired: (String, Option<String>, Option<String>) = sqlx::query_as(
        "SELECT state_kind, terminal_disposition_kind, start_lineage_kind
           FROM turn_lifecycle
          WHERE session_id = $1 AND turn_id = $2",
    )
    .bind(session.into_uuid())
    .bind(turn.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        retired,
        (
            String::from("terminal"),
            Some(String::from("retired")),
            None,
        )
    );
    assert_eq!(
        lifecycle_repository.resume(session).await?,
        SessionLifecycleState::Dispatched
    );
    let deadline_kind: String =
        sqlx::query_scalar("SELECT deadline_kind FROM session_deadline WHERE session_id = $1")
            .bind(session.into_uuid())
            .fetch_one(&pool)
            .await?;
    assert_eq!(deadline_kind, "admission");

    pool.close().await;
    drop(container);
    Ok(())
}
