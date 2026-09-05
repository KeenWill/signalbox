//! PostgreSQL proof for the session lifecycle satellite
//! (docs/spec/session-lifecycle.md): the state mapping, the armed deadline,
//! the parked override, the closures, and the provenance and ownership
//! journal.

use std::collections::BTreeSet;
use std::num::NonZeroU64;
use std::time::Duration;

use crate::*;
use signalbox_domain::{
    CoreAgency, DescendantTerminationScope, DispatchingModule, GoalEventOrdinal, GoalStatement,
    GoalUserAction, GoalUserCommand, LifecycleActor, ModuleDispatch, RepoWatchDispatchId,
    SessionCreationProvenance, SessionFailureCause, SessionLifecycleState, SessionOwnership,
    SessionParkCause, SessionParkResponder, SessionRetirementCause, SessionRetryableCause,
    SessionStructuralCause, SessionTerminalOutcome, StopStickiness,
};
use signalbox_persistence::{
    session_lifecycle::{
        SessionLifecycleRejection, SessionLifecycleRepository, SessionLifecycleRepositoryError,
    },
    turn_liveness::{PostgresTurnLivenessRepository, TurnLivenessPersistenceBounds},
};
use sqlx::error::DatabaseError;
use sqlx::types::time::OffsetDateTime;

const LIFECYCLE_SEED: u128 = 0x11fe_0000;

/// Builds one interactive creation, recorded as unmonitored.
fn interactive_creation(seed: u128) -> PreparedCreateSession {
    prepared(
        LIFECYCLE_SEED + seed,
        LIFECYCLE_SEED + seed + 0x100,
        direct(LIFECYCLE_SEED + seed + 0x200),
    )
}

/// Builds one repository-watch dispatch, recorded as owned work.
fn dispatched_creation(seed: u128) -> PreparedCreateSession {
    CreateSession::new(
        DurableCommandId::from_uuid(Uuid::from_u128(LIFECYCLE_SEED + seed)),
        SessionCreationProvenance::module_dispatched(ModuleDispatch::RepositoryWatch {
            dispatch: RepoWatchDispatchId::from_uuid(Uuid::from_u128(
                LIFECYCLE_SEED + seed + 0x300,
            )),
        }),
        SessionConfigurationDefaults::new(direct(LIFECYCLE_SEED + seed + 0x200)),
    )
    .prepare(SessionId::from_uuid(Uuid::from_u128(
        LIFECYCLE_SEED + seed + 0x100,
    )))
    .expect("a module-dispatched creation without ancestry is preparable")
}

fn creation_session(seed: u128) -> SessionId {
    SessionId::from_uuid(Uuid::from_u128(LIFECYCLE_SEED + seed + 0x100))
}

/// Reads which deadline the session has armed, if any.
async fn armed_deadline(pool: &PgPool, session: SessionId) -> Result<Option<String>, sqlx::Error> {
    sqlx::query_scalar("SELECT deadline_kind FROM session_deadline WHERE session_id = $1")
        .bind(session.into_uuid())
        .fetch_optional(pool)
        .await
}

/// Reads the ownership journal in the order it was written.
async fn ownership_journal(
    pool: &PgPool,
    session: SessionId,
) -> Result<Vec<(i64, String, bool, String)>, sqlx::Error> {
    sqlx::query_as(
        "SELECT event_ordinal, transition_kind, owned_after, actor_kind
           FROM session_ownership_event
          WHERE session_id = $1
          ORDER BY event_ordinal",
    )
    .bind(session.into_uuid())
    .fetch_all(pool)
    .await
}

/// Reads the closed spellings one database constraint admits, from the
/// constraint itself rather than restating them here.
async fn admitted_spellings(
    pool: &PgPool,
    constraint: &str,
) -> Result<BTreeSet<String>, sqlx::Error> {
    let spellings: Vec<String> = sqlx::query_scalar(
        "SELECT DISTINCT spelling.captures[1]
           FROM pg_constraint AS closure
           CROSS JOIN LATERAL regexp_matches(
                    pg_get_constraintdef(closure.oid),
                    $$'([a-z_]+)'::text$$,
                    'g'
                ) AS spelling(captures)
          WHERE closure.conname = $1",
    )
    .bind(constraint)
    .fetch_all(pool)
    .await?;
    Ok(spellings.into_iter().collect())
}

/// Commissions one goal on a session that already exists.
async fn attach_goal(pool: &PgPool, session: SessionId, seed: u128) -> Result<(), Box<dyn Error>> {
    GoalRepository::new(pool.clone())
        .handle_user_command(
            GoalUserCommand::new(
                DurableCommandId::from_uuid(Uuid::from_u128(LIFECYCLE_SEED + seed + 0xa00)),
                session,
                GoalUserAction::Attach(
                    GoalStatement::try_new(String::from("converge the fixture branch"))
                        .expect("the fixture statement is admitted"),
                ),
            ),
            Some(GoalTurnCandidates::new(
                AcceptedInputId::from_uuid(Uuid::from_u128(LIFECYCLE_SEED + seed + 0xb00)),
                TurnId::from_uuid(Uuid::from_u128(LIFECYCLE_SEED + seed + 0xc00)),
            )),
            |_| None,
        )
        .await?;
    Ok(())
}

/// Blocks the commissioned goal by statement, standing in for the scheduler's
/// own execution-failure block, whose turn machinery this test does not need.
async fn block_goal_by_statement(
    pool: &PgPool,
    session: SessionId,
    scheduler_turn: u128,
) -> Result<(), sqlx::Error> {
    sqlx::query("ALTER TABLE turn_lifecycle DISABLE TRIGGER ALL")
        .execute(pool)
        .await?;
    sqlx::query(
        "UPDATE turn_lifecycle
            SET state_kind = 'terminal', start_lineage_kind = 'first_in_session',
                immediate_predecessor_turn_id = NULL, starting_frontier_id = $2,
                terminal_frontier_id = $3, terminal_disposition_kind = 'failed',
                terminal_cause_kind = 'unclassified_failure'
          WHERE session_id = $1",
    )
    .bind(session.into_uuid())
    .bind(Uuid::from_u128(scheduler_turn + 0x10))
    .bind(Uuid::from_u128(scheduler_turn + 0x11))
    .execute(pool)
    .await?;
    sqlx::query("ALTER TABLE turn_lifecycle ENABLE TRIGGER ALL")
        .execute(pool)
        .await?;
    sqlx::query(
        "INSERT INTO goal_event
            (session_id, event_ordinal, generation, event_kind,
             blocked_reason, need, scheduler_turn_id)
         VALUES ($1, 2, 1, 'blocked', 'execution_failure', 'fixture need', $2)",
    )
    .bind(session.into_uuid())
    .bind(Uuid::from_u128(scheduler_turn))
    .execute(pool)
    .await?;
    Ok(())
}

/// Parks one session by statement, standing in for the module-park path.
async fn park_by_statement(pool: &PgPool, session: SessionId) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE session_lifecycle
            SET state_kind = 'parked',
                state_entered_at = statement_timestamp(),
                waiting_kind = NULL,
                waiting_waker = NULL,
                waiting_subject_session_id = NULL,
                recovering_op = NULL,
                blocked_reason = NULL,
                blocked_cycle = NULL,
                parked_cause = 'module_park',
                parked_responder = 'repo_watch',
                parked_since = statement_timestamp()
          WHERE session_id = $1",
    )
    .bind(session.into_uuid())
    .execute(pool)
    .await?;
    Ok(())
}

/// Queues one turn for a session that already exists.
async fn queue_first_turn(
    pool: &PgPool,
    session: SessionId,
    seed: u128,
) -> Result<TurnId, Box<dyn Error>> {
    let turn = TurnId::from_uuid(Uuid::from_u128(LIFECYCLE_SEED + seed + 0x400));
    SubmitInputRepository::new(pool.clone())
        .handle(
            SubmitInput::new(
                DurableCommandId::from_uuid(Uuid::from_u128(LIFECYCLE_SEED + seed + 0x500)),
                session,
                UserContent::try_text(String::from("lifecycle fixture input"))
                    .expect("fixture content is admitted"),
                DeliveryRequest::StartWhenNoActiveTurn {
                    configuration: input_choices(1, ModelSelectionOverride::UseSessionDefault),
                },
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(LIFECYCLE_SEED + seed + 0x600)),
            Some(turn),
        )
        .await?;
    Ok(turn)
}

/// Queues one more turn beneath a session's live turn.
async fn queue_successor_turn(
    pool: &PgPool,
    session: SessionId,
    seed: u128,
) -> Result<(), Box<dyn Error>> {
    SubmitInputRepository::new(pool.clone())
        .handle(
            SubmitInput::new(
                DurableCommandId::from_uuid(Uuid::from_u128(LIFECYCLE_SEED + seed + 0xd00)),
                session,
                UserContent::try_text(String::from("lifecycle fixture successor"))
                    .expect("fixture content is admitted"),
                DeliveryRequest::StartWhenNoActiveTurn {
                    configuration: input_choices(2, ModelSelectionOverride::UseSessionDefault),
                },
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(LIFECYCLE_SEED + seed + 0xe00)),
            Some(TurnId::from_uuid(Uuid::from_u128(
                LIFECYCLE_SEED + seed + 0xf00,
            ))),
        )
        .await?;
    Ok(())
}

/// Queues and activates one turn for a session that already exists.
async fn activate_first_turn(
    pool: &PgPool,
    session: SessionId,
    seed: u128,
) -> Result<TurnId, Box<dyn Error>> {
    let turn = queue_first_turn(pool, session, seed).await?;
    activate_earliest_queued_turn(
        pool,
        EarliestQueuedTurnActivation {
            session: session.into_uuid(),
            origin_entry: Uuid::from_u128(LIFECYCLE_SEED + seed + 0x700),
            starting_frontier: Uuid::from_u128(LIFECYCLE_SEED + seed + 0x800),
            initial_attempt: Uuid::from_u128(LIFECYCLE_SEED + seed + 0x900),
        },
    )
    .await?;
    Ok(turn)
}

fn lifecycle_rejection(error: SessionLifecycleRepositoryError) -> SessionLifecycleRejection {
    match error {
        SessionLifecycleRepositoryError::Rejected(rejection) => rejection,
        other => panic!("expected a refused transition, got {other}"),
    }
}

/// An interactive creation is a conversation. It records the unmonitored
/// bit, opens its ownership journal, and carries no armed deadline, because a
/// deadline on a person's chat window is exactly what the unmonitored bit forbids.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn an_interactive_creation_is_unmonitored_and_arms_no_deadline() -> Result<(), Box<dyn Error>>
{
    let (container, pool, _database_url) = migrated_postgres().await?;
    let session = creation_session(1);
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(interactive_creation(1))
        .await?;

    let record = SessionLifecycleRepository::new(pool.clone())
        .load(session)
        .await?
        .expect("every session owns a lifecycle row");

    assert_eq!(record.state(), SessionLifecycleState::Created);
    assert_eq!(record.ownership(), SessionOwnership::Unmonitored);
    assert_eq!(record.actor(), LifecycleActor::Operator);
    assert_eq!(armed_deadline(&pool, session).await?, None);
    assert_eq!(
        ownership_journal(&pool, session).await?,
        vec![(
            1,
            String::from("created_unmonitored"),
            false,
            String::from("operator")
        )]
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// The command records the dispatch it created its session for, and the two
/// cannot disagree. The committed composite foreign key cannot say this — its
/// dispatch members are null for every interactive creation, and a composite
/// key with a null member checks nothing — so a deferred trigger says it and
/// fires whether or not the pair is null.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_command_naming_another_dispatch_than_its_session_is_rejected()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let session = creation_session(21);
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(dispatched_creation(21))
        .await?;

    let error = sqlx::query(
        "UPDATE create_session_command
            SET dispatch_ref = $2
          WHERE created_session_id = $1",
    )
    .bind(session.into_uuid())
    .bind(Uuid::from_u128(LIFECYCLE_SEED + 0xdead))
    .execute(&pool)
    .await
    .expect_err("a command cannot name a dispatch its session does not");

    assert_eq!(
        error
            .as_database_error()
            .and_then(DatabaseError::code)
            .as_deref(),
        Some("23514")
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// A module-dispatched creation records the module and its exact dispatch,
/// is owned, and arms the admission deadline an owned creation is given.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_dispatched_creation_is_owned_and_holds_its_admission_deadline()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let session = creation_session(2);
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(dispatched_creation(2))
        .await?;

    let record = SessionLifecycleRepository::new(pool.clone())
        .load(session)
        .await?
        .expect("every session owns a lifecycle row");

    assert_eq!(record.state(), SessionLifecycleState::Created);
    assert_eq!(record.ownership(), SessionOwnership::Owned);
    assert_eq!(
        record.actor(),
        LifecycleActor::Module {
            module: signalbox_domain::DispatchingModule::RepositoryWatch,
        }
    );
    assert_eq!(
        armed_deadline(&pool, session).await?,
        Some(String::from("admission"))
    );

    let stored: (String, Option<String>, Option<Uuid>) = sqlx::query_as(
        "SELECT creation_cause, dispatching_module, dispatch_ref
           FROM session WHERE session_id = $1",
    )
    .bind(session.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        stored,
        (
            String::from("module_dispatched"),
            Some(String::from("repo_watch")),
            Some(Uuid::from_u128(LIFECYCLE_SEED + 2 + 0x300))
        )
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// The first accepted input moves the session to `dispatched`, not
/// `active` — the turn is queued, and only activation makes it active. Each
/// state arms the deadline that state defines.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn the_mapping_follows_the_turn_from_dispatch_to_activation() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let repository = SessionLifecycleRepository::new(pool.clone());
    let session = creation_session(3);
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(dispatched_creation(3))
        .await?;

    SubmitInputRepository::new(pool.clone())
        .handle(
            SubmitInput::new(
                DurableCommandId::from_uuid(Uuid::from_u128(LIFECYCLE_SEED + 3 + 0x500)),
                session,
                UserContent::try_text(String::from("lifecycle fixture input"))
                    .expect("fixture content is admitted"),
                DeliveryRequest::StartWhenNoActiveTurn {
                    configuration: input_choices(1, ModelSelectionOverride::UseSessionDefault),
                },
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(LIFECYCLE_SEED + 3 + 0x600)),
            Some(TurnId::from_uuid(Uuid::from_u128(
                LIFECYCLE_SEED + 3 + 0x400,
            ))),
        )
        .await?;

    let dispatched = repository
        .load(session)
        .await?
        .expect("the session keeps its lifecycle row");
    assert_eq!(dispatched.state(), SessionLifecycleState::Dispatched);
    assert_eq!(
        armed_deadline(&pool, session).await?,
        Some(String::from("admission"))
    );

    activate_earliest_queued_turn(
        &pool,
        EarliestQueuedTurnActivation {
            session: session.into_uuid(),
            origin_entry: Uuid::from_u128(LIFECYCLE_SEED + 3 + 0x700),
            starting_frontier: Uuid::from_u128(LIFECYCLE_SEED + 3 + 0x800),
            initial_attempt: Uuid::from_u128(LIFECYCLE_SEED + 3 + 0x900),
        },
    )
    .await?;

    let active = repository
        .load(session)
        .await?
        .expect("the session keeps its lifecycle row");
    assert_eq!(active.state(), SessionLifecycleState::Active);
    assert_eq!(
        armed_deadline(&pool, session).await?,
        Some(String::from("active_stall"))
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// A parked session's rows are not eligibility-sweep candidates. Parking
/// suspends the session's work in place, so a sweep that still hinted it would
/// hand the scheduler a session no pass may run.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_parked_session_leaves_the_eligibility_sweep() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let session = creation_session(4);
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(dispatched_creation(4))
        .await?;
    queue_first_turn(&pool, session, 4).await?;

    let (before_park, _, _) = PostgresEligibilitySweep::new(pool.clone())
        .find_sessions()
        .await?
        .into_parts();
    assert!(before_park.contains(&session));

    // The dispatched session is parked by statement rather than through the
    // store: the lifecycle admits a park only from the states the turn mapping derives,
    // and the module-park unification that drives a dispatched session to core
    // `parked` lands with the expiry engine. The sweep's exclusion is what this
    // test is about, and it reads the state column either way.
    park_by_statement(&pool, session).await?;

    let (after_park, _, _) = PostgresEligibilitySweep::new(pool.clone())
        .find_sessions()
        .await?
        .into_parts();
    assert!(!after_park.contains(&session));

    pool.close().await;
    drop(container);
    Ok(())
}

/// A parked session's turn is not a liveness-watchdog candidate either.
/// Parking keeps the turn's phase, so a watchdog that still saw it would read
/// a deliberately held turn as a stalled one and reap the work an operator is
/// holding — the safety-backfire class this conjunct exists to prevent.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_parked_session_leaves_the_liveness_scans() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let session = creation_session(20);
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(dispatched_creation(20))
        .await?;
    activate_first_turn(&pool, session, 20).await?;
    let liveness = PostgresTurnLivenessRepository::new(
        pool.clone(),
        TurnLivenessPersistenceBounds::new(
            Some(Duration::from_millis(7)),
            Some(Duration::from_secs(5)),
            Some(Duration::from_millis(13)),
        ),
    );
    assert_eq!(
        liveness
            .quiescent_active_turns(None)
            .await?
            .candidates()
            .len(),
        1
    );

    let parked = SessionLifecycleRepository::new(pool.clone())
        .park(
            session,
            SessionParkCause::OperatorHold,
            SessionParkResponder::Operator,
            None,
            LifecycleActor::Operator,
        )
        .await?;

    assert!(parked.is_parked());
    // A parked session waits on a human, so it runs no deadline at all.
    assert_eq!(armed_deadline(&pool, session).await?, None);
    assert_eq!(
        liveness
            .quiescent_active_turns(None)
            .await?
            .candidates()
            .len(),
        0
    );
    assert_eq!(
        liveness
            .slot_held_active_turns(None)
            .await?
            .candidates()
            .len(),
        0
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// Leaving `parked` re-enters the state the mapping gives the suspended
/// turn's phase. The turn kept its phase through the park, so the phase is
/// what decides where the session belongs — not a remembered previous state.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn leaving_a_park_re_enters_the_mapped_state() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let repository = SessionLifecycleRepository::new(pool.clone());
    let session = creation_session(5);
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(dispatched_creation(5))
        .await?;
    activate_first_turn(&pool, session, 5).await?;
    repository
        .park(
            session,
            SessionParkCause::ActiveStallDeadlineExpired,
            SessionParkResponder::Operator,
            None,
            LifecycleActor::Operator,
        )
        .await?;

    let resumed = repository.resume(session).await?;

    assert_eq!(resumed, SessionLifecycleState::Active);
    assert_eq!(
        armed_deadline(&pool, session).await?,
        Some(String::from("active_stall"))
    );

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn retired_unactivated_turn_does_not_make_a_park_resume_active() -> Result<(), Box<dyn Error>>
{
    let (container, pool, _database_url) = migrated_postgres().await?;
    let repository = SessionLifecycleRepository::new(pool.clone());
    let session = creation_session(30);
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(dispatched_creation(30))
        .await?;
    attach_goal(&pool, session, 30).await?;
    repository
        .park(
            session,
            SessionParkCause::ModulePark,
            SessionParkResponder::Module {
                module: DispatchingModule::RepositoryWatch,
            },
            None,
            LifecycleActor::Module {
                module: DispatchingModule::RepositoryWatch,
            },
        )
        .await?;
    GoalRepository::new(pool.clone())
        .handle_user_command(
            GoalUserCommand::new(
                DurableCommandId::from_uuid(Uuid::from_u128(LIFECYCLE_SEED + 0x30d0)),
                session,
                GoalUserAction::Supersede(
                    GoalStatement::try_new(String::from("continue with the replacement"))
                        .expect("the replacement statement is admitted"),
                ),
            ),
            Some(GoalTurnCandidates::new(
                AcceptedInputId::from_uuid(Uuid::from_u128(LIFECYCLE_SEED + 0x30e0)),
                TurnId::from_uuid(Uuid::from_u128(LIFECYCLE_SEED + 0x30f0)),
            )),
            |_| None,
        )
        .await?;

    let resumed = repository.resume(session).await?;

    assert_eq!(resumed, SessionLifecycleState::Dispatched);
    pool.close().await;
    drop(container);
    Ok(())
}

/// No terminal session leaves a non-terminal turn behind. A closure
/// issued over a live turn is refused rather than committing a terminal
/// session whose turn is still running.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_closure_over_a_live_turn_is_refused() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let session = creation_session(6);
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(dispatched_creation(6))
        .await?;
    activate_first_turn(&pool, session, 6).await?;

    let error = SessionLifecycleRepository::new(pool.clone())
        .close(
            session,
            SessionTerminalOutcome::Stopped {
                sticky: StopStickiness::Redispatchable,
            },
            LifecycleActor::Operator,
        )
        .await
        .expect_err("a terminal session cannot leave a live turn behind");
    assert!(matches!(
        error,
        SessionLifecycleRepositoryError::Database(_)
    ));

    let state = SessionLifecycleRepository::new(pool.clone())
        .load(session)
        .await?
        .expect("the refused closure left the session alone")
        .state();
    assert_eq!(state, SessionLifecycleState::Active);

    pool.close().await;
    drop(container);
    Ok(())
}

/// Session terminalization settles the live goal generation in the same
/// closure. Goal state is the sole continuation-stopping condition in the
/// goal contract, so a pursuing goal beneath a terminal session would keep
/// scheduling work no one owns.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_closure_settles_the_live_goal_generation() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let session = creation_session(7);
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(dispatched_creation(7))
        .await?;
    GoalRepository::new(pool.clone())
        .handle_user_command(
            GoalUserCommand::new(
                DurableCommandId::from_uuid(Uuid::from_u128(LIFECYCLE_SEED + 7 + 0xa00)),
                session,
                GoalUserAction::Attach(
                    GoalStatement::try_new(String::from("converge the fixture branch"))
                        .expect("the fixture statement is admitted"),
                ),
            ),
            Some(GoalTurnCandidates::new(
                AcceptedInputId::from_uuid(Uuid::from_u128(LIFECYCLE_SEED + 7 + 0xb00)),
                TurnId::from_uuid(Uuid::from_u128(LIFECYCLE_SEED + 7 + 0xc00)),
            )),
            |_| None,
        )
        .await?;

    let terminal = SessionLifecycleRepository::new(pool.clone())
        .close(
            session,
            SessionTerminalOutcome::FailedStructural {
                cause: SessionStructuralCause::ContextCompactionWall,
            },
            LifecycleActor::Watchdog,
        )
        .await?;

    assert_eq!(
        terminal,
        SessionLifecycleState::Terminal {
            outcome: SessionTerminalOutcome::FailedStructural {
                cause: SessionStructuralCause::ContextCompactionWall,
            },
        }
    );
    let settled: (String, Option<String>, Option<String>) = sqlx::query_as(
        "SELECT event_kind, session_outcome_kind, closure_actor_kind
           FROM goal_event
          WHERE session_id = $1
          ORDER BY event_ordinal DESC
          LIMIT 1",
    )
    .bind(session.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        settled,
        (
            String::from("session_closed"),
            Some(String::from("failed_structural")),
            Some(String::from("watchdog"))
        )
    );
    assert_eq!(armed_deadline(&pool, session).await?, None);
    let retired: Option<Uuid> = sqlx::query_scalar(
        "SELECT turn_id FROM turn_terminal_outbox_event
          WHERE session_id = $1 AND disposition_kind = 'retired'",
    )
    .bind(session.into_uuid())
    .fetch_optional(&pool)
    .await?;
    assert_eq!(
        retired,
        Some(Uuid::from_u128(LIFECYCLE_SEED + 7 + 0xc00)),
        "the closure retires the queued goal turn it leaves behind"
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// A verified achievement and a stop are the goal contract's own events,
/// so a session closure naming one over a generation still open is refused
/// rather than recording the same closure twice.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn an_achievement_closure_over_an_open_generation_is_refused() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let session = creation_session(8);
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(dispatched_creation(8))
        .await?;
    GoalRepository::new(pool.clone())
        .handle_user_command(
            GoalUserCommand::new(
                DurableCommandId::from_uuid(Uuid::from_u128(LIFECYCLE_SEED + 8 + 0xa00)),
                session,
                GoalUserAction::Attach(
                    GoalStatement::try_new(String::from("converge the fixture branch"))
                        .expect("the fixture statement is admitted"),
                ),
            ),
            Some(GoalTurnCandidates::new(
                AcceptedInputId::from_uuid(Uuid::from_u128(LIFECYCLE_SEED + 8 + 0xb00)),
                TurnId::from_uuid(Uuid::from_u128(LIFECYCLE_SEED + 8 + 0xc00)),
            )),
            |_| None,
        )
        .await?;

    let error = SessionLifecycleRepository::new(pool.clone())
        .close(
            session,
            SessionTerminalOutcome::AchievedVerified,
            LifecycleActor::Operator,
        )
        .await
        .expect_err("an achievement is the goal command's own event");

    assert_eq!(
        lifecycle_rejection(error),
        SessionLifecycleRejection::GoalGenerationStillOpen
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// Adopting takes the liveness obligation and arms the deadline the state
/// defines; releasing drops the forward obligations immediately, disarming the
/// deadline with the bit. Both transitions are journaled with their actor.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn ownership_flips_arm_and_disarm_the_deadline_and_journal_themselves()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let repository = SessionLifecycleRepository::new(pool.clone());
    let session = creation_session(11);
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(interactive_creation(11))
        .await?;
    assert_eq!(armed_deadline(&pool, session).await?, None);

    repository.adopt(session, LifecycleActor::Operator).await?;
    assert_eq!(
        armed_deadline(&pool, session).await?,
        Some(String::from("admission"))
    );

    repository
        .release(session, LifecycleActor::Operator)
        .await?;
    assert_eq!(armed_deadline(&pool, session).await?, None);

    assert_eq!(
        ownership_journal(&pool, session).await?,
        vec![
            (
                1,
                String::from("created_unmonitored"),
                false,
                String::from("operator")
            ),
            (2, String::from("adopted"), true, String::from("operator")),
            (3, String::from("released"), false, String::from("operator")),
        ]
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// `release` on a `parked` session is rejected. `parked` is an owned-only
/// state, so the park is closed or resumed first; releasing it would strand a
/// session waiting on a human with nothing left watching it.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn releasing_a_parked_session_is_rejected() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let repository = SessionLifecycleRepository::new(pool.clone());
    let session = creation_session(12);
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(dispatched_creation(12))
        .await?;
    activate_first_turn(&pool, session, 12).await?;
    repository
        .park(
            session,
            SessionParkCause::StructuralFailure,
            SessionParkResponder::Operator,
            Some(SessionFailureCause::Structural(
                SessionStructuralCause::BrokenToolchain,
            )),
            LifecycleActor::Operator,
        )
        .await?;

    let error = repository
        .release(session, LifecycleActor::Operator)
        .await
        .expect_err("a parked session is owned-only");

    assert_eq!(
        lifecycle_rejection(error),
        SessionLifecycleRejection::ReleaseWhileParked
    );
    assert_eq!(armed_deadline(&pool, session).await?, None);

    pool.close().await;
    drop(container);
    Ok(())
}

/// Terminal is final. Without this a later transition could reopen a closed
/// session and move every metric cohort built on `ended_at` underneath the week
/// that already reported it.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_terminal_session_admits_no_further_transition() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let repository = SessionLifecycleRepository::new(pool.clone());
    let session = creation_session(15);
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(dispatched_creation(15))
        .await?;
    repository
        .close(
            session,
            SessionTerminalOutcome::Retired {
                cause: SessionRetirementCause::AdmissionDeadlineExpired,
            },
            LifecycleActor::Watchdog,
        )
        .await?;

    let error = repository
        .park(
            session,
            SessionParkCause::OperatorHold,
            SessionParkResponder::Operator,
            None,
            LifecycleActor::Operator,
        )
        .await
        .expect_err("a terminal session cannot park");

    assert_eq!(
        lifecycle_rejection(error),
        SessionLifecycleRejection::TransitionNotAdmitted
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// A closure may commit to its outcome while the live turn still settles.
/// The handoff is what lets a command say what it decided without recording a
/// terminal session over a running turn.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_pending_terminal_settles_once_the_turn_does() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let repository = SessionLifecycleRepository::new(pool.clone());
    let session = creation_session(16);
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(dispatched_creation(16))
        .await?;
    let outcome = SessionTerminalOutcome::Stopped {
        sticky: StopStickiness::Sticky,
    };

    repository
        .commit_pending_terminal(session, outcome, LifecycleActor::Operator)
        .await?;
    let committed = repository
        .load(session)
        .await?
        .expect("the session keeps its lifecycle row");
    assert_eq!(committed.pending_terminal(), Some(outcome));
    assert_eq!(committed.state(), SessionLifecycleState::Created);

    let settled = repository.settle_pending_terminal(session).await?;
    assert_eq!(settled, SessionLifecycleState::Terminal { outcome });

    pool.close().await;
    drop(container);
    Ok(())
}

/// A committed closure disposes steering still pending on its live turn.
/// The turn then settles without creating a successor that would hold the
/// session open beneath its terminal handoff.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_closure_handoff_disposes_pending_steering_before_settlement()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let repository = SessionLifecycleRepository::new(pool.clone());
    let session = creation_session(65);
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(dispatched_creation(65))
        .await?;
    let turn = activate_first_turn(&pool, session, 65).await?;
    let accepted_input = AcceptedInputId::from_uuid(Uuid::from_u128(LIFECYCLE_SEED + 65 + 0xb00));
    SubmitInputRepository::new(pool.clone())
        .handle(
            SubmitInput::new(
                DurableCommandId::from_uuid(Uuid::from_u128(LIFECYCLE_SEED + 65 + 0xa00)),
                session,
                UserContent::try_text(String::from("steer before closing"))
                    .expect("fixture content is admitted"),
                DeliveryRequest::NextSafePoint {
                    expected_active_turn: turn,
                },
            ),
            accepted_input,
            None,
        )
        .await?;
    let outcome = SessionTerminalOutcome::Stopped {
        sticky: StopStickiness::Sticky,
    };

    repository
        .commit_pending_terminal(session, outcome, LifecycleActor::Operator)
        .await?;
    let disposition: String = sqlx::query_scalar(
        "SELECT disposition_kind
           FROM accepted_input
          WHERE accepted_input_id = $1",
    )
    .bind(accepted_input.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(disposition, String::from("closed_not_delivered"));

    let liveness = PostgresTurnLivenessRepository::new(
        pool.clone(),
        TurnLivenessPersistenceBounds::new(None, None, None),
    );
    let candidate = *liveness
        .quiescent_active_turns(None)
        .await?
        .candidates()
        .first()
        .expect("the live turn is quiescent");
    assert_eq!(candidate.turn(), turn);
    assert_eq!(
        liveness
            .terminalize_stale_turn(
                candidate,
                signalbox_domain::AcceptedInputTurnFailureIdentities::new(
                    SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(
                        LIFECYCLE_SEED + 65 + 0xc00,
                    )),
                    ContextFrontierId::from_uuid(Uuid::from_u128(LIFECYCLE_SEED + 65 + 0xd00)),
                ),
                &mut signalbox_application::UuidV7StartupScanIdGenerator,
            )
            .await?,
        signalbox_application::StaleTurnOutcome::Terminalized
    );
    let settled = repository
        .load(session)
        .await?
        .expect("the session keeps its lifecycle row");
    assert_eq!(settled.state(), SessionLifecycleState::Terminal { outcome });

    pool.close().await;
    drop(container);
    Ok(())
}

/// The state vocabulary the domain carries and the vocabulary the database
/// admits are the same closed set, read from the constraint itself rather than
/// restated by the reader.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn the_state_vocabulary_matches_its_database_constraint() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;

    let admitted = admitted_spellings(&pool, "session_lifecycle_state_closed").await?;

    assert_eq!(
        admitted,
        BTreeSet::from([
            String::from("created"),
            String::from("dispatched"),
            String::from("active"),
            String::from("waiting"),
            String::from("recovering"),
            String::from("blocked"),
            String::from("parked"),
            String::from("terminal"),
        ])
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// The closed terminal-outcome vocabulary, likewise read from the constraint.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn the_terminal_outcome_vocabulary_matches_its_database_constraint()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;

    let admitted = admitted_spellings(&pool, "session_lifecycle_terminal_outcome_closed").await?;

    assert_eq!(
        admitted,
        BTreeSet::from([
            String::from("achieved_verified"),
            String::from("achieved_declared"),
            String::from("failed_retryable"),
            String::from("failed_structural"),
            String::from("failed_unknown"),
            String::from("stopped"),
            String::from("superseded"),
            String::from("abandoned"),
            String::from("retired"),
        ])
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// The satellite's declared lock position holds under interleaving: one
/// transaction taking the session row then the satellite, and another taking
/// the satellite then the scheduler row, both commit. A satellite acquired
/// after the scheduler row would deadlock against every session-first path,
/// which is why the scheduler statements acquire it first.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn the_satellite_lock_position_survives_interleaving() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let session = creation_session(17);
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(dispatched_creation(17))
        .await?;
    activate_first_turn(&pool, session, 17).await?;

    let mut scheduler_first = pool.begin().await?;
    sqlx::query(
        "WITH satellite AS (
             SELECT session_id FROM session_lifecycle
              WHERE session_id = $1 FOR NO KEY UPDATE
         )
         SELECT session_id FROM session_scheduler
          WHERE session_id = (SELECT session_id FROM satellite)
          FOR UPDATE",
    )
    .bind(session.into_uuid())
    .fetch_optional(&mut *scheduler_first)
    .await?;

    let park_pool = pool.clone();
    let parking = tokio::spawn(async move {
        SessionLifecycleRepository::new(park_pool)
            .park(
                session,
                SessionParkCause::OperatorHold,
                SessionParkResponder::Operator,
                None,
                LifecycleActor::Operator,
            )
            .await
    });

    scheduler_first.commit().await?;
    let parked = parking.await.expect("the park task runs to completion")?;

    assert!(parked.is_parked());

    pool.close().await;
    drop(container);
    Ok(())
}

/// The eligibility sweep reports each candidate's ownership, so an
/// unmonitored conversation's pass never counts toward the occupancy the
/// daemon reports as driven work.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn the_sweep_reports_which_candidates_are_unmonitored() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let conversation = creation_session(18);
    let dispatched = creation_session(19);
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(interactive_creation(18))
        .await?;
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(dispatched_creation(19))
        .await?;
    queue_first_turn(&pool, conversation, 18).await?;
    queue_first_turn(&pool, dispatched, 19).await?;

    let batch = PostgresEligibilitySweep::new(pool.clone())
        .find_sessions()
        .await?;

    assert!(batch.unmonitored().contains(&conversation));
    assert!(!batch.unmonitored().contains(&dispatched));

    pool.close().await;
    drop(container);
    Ok(())
}

/// `parked` is an owned-only state, so an unmonitored conversation
/// cannot be parked. The park would delete rather than arm the re-notification
/// deadline — an unmonitored session holds none — while the sweep and both
/// watchdogs stopped seeing it, leaving the conversation with nothing at all
/// scheduled to revisit it.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn parking_an_unmonitored_session_is_rejected() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let session = creation_session(22);
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(interactive_creation(22))
        .await?;
    activate_first_turn(&pool, session, 22).await?;

    let error = SessionLifecycleRepository::new(pool.clone())
        .park(
            session,
            SessionParkCause::OperatorHold,
            SessionParkResponder::Operator,
            None,
            LifecycleActor::Operator,
        )
        .await
        .expect_err("an unmonitored conversation is not parkable");

    assert_eq!(
        lifecycle_rejection(error),
        SessionLifecycleRejection::ParkWhileUnmonitored
    );
    assert_eq!(armed_deadline(&pool, session).await?, None);

    pool.close().await;
    drop(container);
    Ok(())
}

/// The first committed closure decision is the one that settles. A second
/// closure naming a different outcome is refused rather than silently
/// replacing the decision that already started tearing the turn down; an
/// identical replay is the caller's idempotent retry and settles the same way.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_second_pending_terminal_cannot_replace_the_first() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let repository = SessionLifecycleRepository::new(pool.clone());
    let session = creation_session(23);
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(dispatched_creation(23))
        .await?;
    let committed = SessionTerminalOutcome::Stopped {
        sticky: StopStickiness::Sticky,
    };
    repository
        .commit_pending_terminal(session, committed, LifecycleActor::Operator)
        .await?;

    repository
        .commit_pending_terminal(session, committed, LifecycleActor::Operator)
        .await?;
    let error = repository
        .commit_pending_terminal(
            session,
            SessionTerminalOutcome::FailedUnknown,
            LifecycleActor::Operator,
        )
        .await
        .expect_err("a second outcome cannot replace the committed decision");

    assert_eq!(
        lifecycle_rejection(error),
        SessionLifecycleRejection::PendingTerminalConflict
    );
    assert_eq!(
        repository
            .load(session)
            .await?
            .expect("the session keeps its lifecycle row")
            .pending_terminal(),
        Some(committed)
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// A session outcome and its goal's terminal state are two records of one
/// ending. A goal the user stopped and a session claiming a verified
/// achievement disagree, and nothing downstream could say which is true, so
/// the closure is refused instead of committing both.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_closure_contradicting_its_settled_goal_is_rejected() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let session = creation_session(24);
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(dispatched_creation(24))
        .await?;
    attach_goal(&pool, session, 24).await?;
    GoalRepository::new(pool.clone())
        .handle_user_command(
            GoalUserCommand::new(
                DurableCommandId::from_uuid(Uuid::from_u128(LIFECYCLE_SEED + 24 + 0xe00)),
                session,
                GoalUserAction::Stop {
                    descendant_scope: DescendantTerminationScope::ParentAlone,
                },
            ),
            None,
            |_| None,
        )
        .await?;

    let error = SessionLifecycleRepository::new(pool.clone())
        .close(
            session,
            SessionTerminalOutcome::AchievedVerified,
            LifecycleActor::Operator,
        )
        .await
        .expect_err("a stopped goal cannot close as a verified achievement");

    assert_eq!(
        lifecycle_rejection(error),
        SessionLifecycleRejection::GoalOutcomeMismatch
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// A park closes "as failed with the cause standing". A closure naming a
/// different cause would record a fabricated one in both the terminal outcome
/// and the goal event, and the same write clears the park that would have
/// contradicted it.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_failed_closure_must_carry_the_parks_standing_cause() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let repository = SessionLifecycleRepository::new(pool.clone());
    let session = creation_session(25);
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(dispatched_creation(25))
        .await?;
    activate_first_turn(&pool, session, 25).await?;
    repository
        .park(
            session,
            SessionParkCause::StructuralFailure,
            SessionParkResponder::Operator,
            Some(SessionFailureCause::Structural(
                SessionStructuralCause::ContextCompactionWall,
            )),
            LifecycleActor::Operator,
        )
        .await?;

    let error = repository
        .close(
            session,
            SessionTerminalOutcome::FailedStructural {
                cause: SessionStructuralCause::BrokenToolchain,
            },
            LifecycleActor::Operator,
        )
        .await
        .expect_err("the closure cause is not the one standing");

    assert_eq!(
        lifecycle_rejection(error),
        SessionLifecycleRejection::StandingCauseMismatch
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// An unknown-failure park cannot close under a classification the park
/// explicitly did not establish.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn an_unknown_failure_park_refuses_a_classified_closure() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let repository = SessionLifecycleRepository::new(pool.clone());
    let session = creation_session(63);
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(dispatched_creation(63))
        .await?;
    activate_first_turn(&pool, session, 63).await?;
    repository
        .park(
            session,
            SessionParkCause::UnknownFailure,
            SessionParkResponder::Operator,
            None,
            LifecycleActor::Operator,
        )
        .await?;

    let error = repository
        .close(
            session,
            SessionTerminalOutcome::FailedRetryable {
                cause: SessionRetryableCause::ProviderTransient,
            },
            LifecycleActor::Operator,
        )
        .await
        .expect_err("an unknown failure cannot acquire a retryable classification at closure");

    assert_eq!(
        lifecycle_rejection(error),
        SessionLifecycleRejection::StandingCauseMismatch
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// A `core` classification keeps the exact model or tool identity behind
/// it. The ownership journal is the audit that would otherwise lose it, making
/// a tool-driven release indistinguishable from ordinary daemon action.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn the_ownership_journal_keeps_the_agency_behind_a_core_flip() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let session = creation_session(26);
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(dispatched_creation(26))
        .await?;
    let acting_turn = activate_first_turn(&pool, session, 26).await?;

    SessionLifecycleRepository::new(pool.clone())
        .release(
            session,
            LifecycleActor::Core {
                agency: CoreAgency::Model { turn: acting_turn },
            },
        )
        .await?;

    let recorded: (String, Option<Uuid>, Option<Uuid>) = sqlx::query_as(
        "SELECT actor_kind, actor_turn_id, actor_tool_request_id
           FROM session_ownership_event
          WHERE session_id = $1
          ORDER BY event_ordinal DESC
          LIMIT 1",
    )
    .bind(session.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        recorded,
        (String::from("core"), Some(acting_turn.into_uuid()), None)
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// The sweep's parked exclusion is a hint filter, not an authority. A hint
/// queued before the park still reaches the activation transaction, so the
/// authoritative path reads the state under its own lock and refuses rather
/// than activating a turn in a session an operator is holding.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_parked_session_does_not_activate_its_queued_turn() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let session = creation_session(27);
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(dispatched_creation(27))
        .await?;
    queue_first_turn(&pool, session, 27).await?;
    park_by_statement(&pool, session).await?;

    let outcome = StartEligibleTurnService::new(
        FixedStartEligibleTurnIds::new(
            [SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(
                LIFECYCLE_SEED + 27 + 0x700,
            ))],
            [ContextFrontierId::from_uuid(Uuid::from_u128(
                LIFECYCLE_SEED + 27 + 0x800,
            ))],
            [TurnAttemptId::from_uuid(Uuid::from_u128(
                LIFECYCLE_SEED + 27 + 0x900,
            ))],
        ),
        StartEligibleTurnRepository::new(pool.clone()),
    )
    .execute(session)
    .await?;

    assert_eq!(outcome, StartEligibleTurnOutcome::NoEligibleTurn);

    pool.close().await;
    drop(container);
    Ok(())
}

/// A parked goal session resumes through the goal command, and the unpark
/// rides that same transaction. Without it the accepted continuation stays
/// excluded from every sweep and watchdog while its authoritative state still
/// reads parked.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn resuming_a_parked_goal_lifts_the_park() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let repository = SessionLifecycleRepository::new(pool.clone());
    let session = creation_session(28);
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(dispatched_creation(28))
        .await?;
    attach_goal(&pool, session, 28).await?;
    block_goal_by_statement(&pool, session, LIFECYCLE_SEED + 28 + 0xc00).await?;
    park_by_statement(&pool, session).await?;

    GoalRepository::new(pool.clone())
        .handle_user_command(
            GoalUserCommand::new(
                DurableCommandId::from_uuid(Uuid::from_u128(LIFECYCLE_SEED + 28 + 0xe00)),
                session,
                GoalUserAction::Resume(None),
            ),
            Some(GoalTurnCandidates::new(
                AcceptedInputId::from_uuid(Uuid::from_u128(LIFECYCLE_SEED + 28 + 0x1000)),
                TurnId::from_uuid(Uuid::from_u128(LIFECYCLE_SEED + 28 + 0x1100)),
            )),
            |_| None,
        )
        .await?;

    let resumed = repository
        .load(session)
        .await?
        .expect("the session keeps its lifecycle row")
        .state();
    assert!(!resumed.is_parked());

    pool.close().await;
    drop(container);
    Ok(())
}

/// A blocked goal resumes through its own command. Lifting the park
/// directly would expose the blocked generation to automatic resumption
/// without recording the goal's resume event or guidance.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_blocked_goal_refuses_a_direct_lifecycle_resume() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let repository = SessionLifecycleRepository::new(pool.clone());
    let session = creation_session(64);
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(dispatched_creation(64))
        .await?;
    attach_goal(&pool, session, 64).await?;
    block_goal_by_statement(&pool, session, LIFECYCLE_SEED + 64 + 0xc00).await?;
    park_by_statement(&pool, session).await?;

    let error = repository
        .resume(session)
        .await
        .expect_err("a blocked goal resumes only through its own command");

    assert_eq!(
        lifecycle_rejection(error),
        SessionLifecycleRejection::TransitionNotAdmitted
    );
    assert!(
        repository
            .load(session)
            .await?
            .expect("the session keeps its lifecycle row")
            .state()
            .is_parked()
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// Terminal is final in both directions. A projection that returned quietly on
/// a terminal session would let a later queued turn land beneath it and then
/// activate, because the deferred terminal-turn constraint fires on lifecycle
/// writes and nothing re-fires it for the turn.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_terminal_session_admits_no_later_queued_turn() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let session = creation_session(29);
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(dispatched_creation(29))
        .await?;
    SessionLifecycleRepository::new(pool.clone())
        .close(
            session,
            SessionTerminalOutcome::Retired {
                cause: SessionRetirementCause::AdmissionDeadlineExpired,
            },
            LifecycleActor::Watchdog,
        )
        .await?;

    let error = queue_first_turn(&pool, session, 29)
        .await
        .expect_err("a terminal session admits no new work");

    assert!(
        format!("{error}").contains("admits no further turn or goal work")
            || error.source().is_some_and(
                |source| format!("{source}").contains("admits no further turn or goal work")
            )
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// The ownership journal is append-only against truncation too: row triggers
/// do not fire for TRUNCATE, and metric cohort membership follows this journal.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn the_ownership_journal_cannot_be_truncated() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(dispatched_creation(32))
        .await?;

    let error = sqlx::query("TRUNCATE session_ownership_event CASCADE")
        .execute(&pool)
        .await
        .expect_err("the ownership journal cannot be truncated");

    assert_eq!(
        error
            .as_database_error()
            .and_then(DatabaseError::code)
            .as_deref(),
        Some("23514")
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// Each wait kind designates exactly one waker. A wait recorded against
/// machinery that will never end it reads as a real wait to everything
/// downstream, so the pair is constrained rather than merely both present.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_wait_cannot_name_another_kinds_waker() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let session = creation_session(33);
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(dispatched_creation(33))
        .await?;

    let error = sqlx::query(
        "UPDATE session_lifecycle
            SET state_kind = 'waiting',
                waiting_kind = 'approval',
                waiting_waker = 'scheduler_sweep'
          WHERE session_id = $1",
    )
    .bind(session.into_uuid())
    .execute(&pool)
    .await
    .expect_err("an approval wait is ended by the approval decision, not the sweep");

    assert_eq!(
        error
            .as_database_error()
            .and_then(DatabaseError::code)
            .as_deref(),
        Some("23514")
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// A live row cannot keep terminal-only payload. The discriminator comparison
/// evaluates to SQL `NULL` when the outcome is absent, which a `CHECK` accepts,
/// so the null case is stated rather than left to that accident.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_live_session_cannot_hold_terminal_payload() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let session = creation_session(34);
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(dispatched_creation(34))
        .await?;

    let error = sqlx::query(
        "UPDATE session_lifecycle SET terminal_stop_sticky = true WHERE session_id = $1",
    )
    .bind(session.into_uuid())
    .execute(&pool)
    .await
    .expect_err("terminal payload requires the outcome that gives it meaning");

    assert_eq!(
        error
            .as_database_error()
            .and_then(DatabaseError::code)
            .as_deref(),
        Some("23514")
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// A handoff settlement could never carry out is a decision recorded and then
/// stranded, so the pending shape carries the terminal shape's own rules: no
/// self-supersession, and a successor that exists.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_pending_supersession_names_a_successor_settlement_can_record()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let repository = SessionLifecycleRepository::new(pool.clone());
    let session = creation_session(35);
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(dispatched_creation(35))
        .await?;

    let self_reference = repository
        .commit_pending_terminal(
            session,
            SessionTerminalOutcome::Superseded { by: Some(session) },
            LifecycleActor::Operator,
        )
        .await
        .expect_err("a session cannot supersede itself");
    let unknown_successor = repository
        .commit_pending_terminal(
            session,
            SessionTerminalOutcome::Superseded {
                by: Some(SessionId::from_uuid(Uuid::from_u128(
                    LIFECYCLE_SEED + 0xbeef,
                ))),
            },
            LifecycleActor::Operator,
        )
        .await
        .expect_err("a handoff cannot name a session that does not exist");

    assert!(matches!(
        self_reference,
        SessionLifecycleRepositoryError::Database(_)
            | SessionLifecycleRepositoryError::CommitAmbiguous(_)
    ));
    assert!(matches!(
        unknown_successor,
        SessionLifecycleRepositoryError::Database(_)
            | SessionLifecycleRepositoryError::CommitAmbiguous(_)
    ));

    pool.close().await;
    drop(container);
    Ok(())
}

/// An automatic resume answers a recorded block; a park taken since is the
/// same "the lineage moved" case, so lifting it would undo an operator hold
/// and schedule fresh model work. An operator's own resume still lifts it.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn an_automatic_resume_does_not_lift_a_park() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let session = creation_session(36);
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(dispatched_creation(36))
        .await?;
    attach_goal(&pool, session, 36).await?;
    block_goal_by_statement(&pool, session, LIFECYCLE_SEED + 36 + 0xc00).await?;
    park_by_statement(&pool, session).await?;

    let outcome = GoalRepository::new(pool.clone())
        .handle_expected_user_command(
            GoalUserCommand::new(
                DurableCommandId::from_uuid(Uuid::from_u128(LIFECYCLE_SEED + 36 + 0xe00)),
                session,
                GoalUserAction::Resume(None),
            ),
            Some(GoalTurnCandidates::new(
                AcceptedInputId::from_uuid(Uuid::from_u128(LIFECYCLE_SEED + 36 + 0x1000)),
                TurnId::from_uuid(Uuid::from_u128(LIFECYCLE_SEED + 36 + 0x1100)),
            )),
            GoalEventOrdinal::new(NonZeroU64::new(2).expect("the blocked event is ordinal two")),
            |_| None,
        )
        .await?;

    assert_eq!(outcome, GoalCommandHandlingOutcome::LineageMoved);
    assert!(
        SessionLifecycleRepository::new(pool.clone())
            .load(session)
            .await?
            .expect("the session keeps its lifecycle row")
            .state()
            .is_parked()
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// The standing-cause gate runs before the handoff is persisted, so a decision
/// settlement would refuse is never recorded in the first place.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_pending_closure_must_carry_the_parks_standing_cause() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let repository = SessionLifecycleRepository::new(pool.clone());
    let session = creation_session(37);
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(dispatched_creation(37))
        .await?;
    activate_first_turn(&pool, session, 37).await?;
    repository
        .park(
            session,
            SessionParkCause::RetryBudgetExhausted,
            SessionParkResponder::Operator,
            Some(SessionFailureCause::Retryable(
                SessionRetryableCause::ProviderQuotaExhausted,
            )),
            LifecycleActor::Operator,
        )
        .await?;

    let error = repository
        .commit_pending_terminal(
            session,
            SessionTerminalOutcome::FailedStructural {
                cause: SessionStructuralCause::BrokenToolchain,
            },
            LifecycleActor::Operator,
        )
        .await
        .expect_err("a handoff settlement would refuse is not recorded");

    assert_eq!(
        lifecycle_rejection(error),
        SessionLifecycleRejection::StandingCauseMismatch
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// A state-specific column is null outside its own state. Checking only
/// whether the payload is complete lets a partial one survive on a state that
/// then ignores it.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_live_session_cannot_hold_another_states_payload() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let session = creation_session(38);
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(dispatched_creation(38))
        .await?;

    let error = sqlx::query(
        "UPDATE session_lifecycle SET blocked_reason = 'user_input_required'
          WHERE session_id = $1",
    )
    .bind(session.into_uuid())
    .execute(&pool)
    .await
    .expect_err("a created session holds no blocked payload");

    assert_eq!(
        error
            .as_database_error()
            .and_then(DatabaseError::code)
            .as_deref(),
        Some("23514")
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// The recorded agency is this session's own: a turn from another session
/// cannot be written as this one's actor.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn an_actor_identity_from_another_session_is_rejected() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let subject = creation_session(39);
    let stranger = creation_session(40);
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(dispatched_creation(39))
        .await?;
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(dispatched_creation(40))
        .await?;
    let foreign_turn = activate_first_turn(&pool, stranger, 40).await?;

    let error = sqlx::query(
        "UPDATE session_lifecycle
            SET actor_kind = 'core', actor_module = NULL, actor_turn_id = $2
          WHERE session_id = $1",
    )
    .bind(subject.into_uuid())
    .bind(foreign_turn.into_uuid())
    .execute(&pool)
    .await
    .expect_err("another session's turn is not this session's actor");

    assert_eq!(
        error
            .as_database_error()
            .and_then(DatabaseError::code)
            .as_deref(),
        Some("23503")
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// The armed-deadline table rejects truncation: it would silently unbound
/// every owned session at once.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn the_armed_deadline_table_cannot_be_truncated() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(dispatched_creation(41))
        .await?;

    let error = sqlx::query("TRUNCATE session_deadline")
        .execute(&pool)
        .await
        .expect_err("the armed deadlines cannot be truncated");

    assert_eq!(
        error
            .as_database_error()
            .and_then(DatabaseError::code)
            .as_deref(),
        Some("23514")
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// A committed handoff carries a cause its outcome admits, so settlement can
/// always record what the closure decided.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_pending_cause_must_belong_to_its_outcome() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let session = creation_session(42);
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(dispatched_creation(42))
        .await?;

    let error = sqlx::query(
        "UPDATE session_lifecycle
            SET pending_terminal_outcome_kind = 'failed_retryable',
                pending_terminal_cause_kind = 'broken_toolchain'
          WHERE session_id = $1",
    )
    .bind(session.into_uuid())
    .execute(&pool)
    .await
    .expect_err("a retryable outcome does not carry a structural cause");

    assert_eq!(
        error
            .as_database_error()
            .and_then(DatabaseError::code)
            .as_deref(),
        Some("23514")
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// The ownership bit and its journal are one record. A flip written to only
/// one of them would leave the cohort metric and the deadline machinery
/// disagreeing about the same session.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn the_ownership_bit_cannot_move_without_its_journal() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let session = creation_session(43);
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(dispatched_creation(43))
        .await?;

    let error = sqlx::query("UPDATE session_lifecycle SET owned = false WHERE session_id = $1")
        .bind(session.into_uuid())
        .execute(&pool)
        .await
        .expect_err("the bit and its journal move together");

    assert_eq!(
        error
            .as_database_error()
            .and_then(DatabaseError::code)
            .as_deref(),
        Some("23514")
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// A live session cannot carry a complete terminal outcome. Both sides of an
/// equivalence are false for a nonterminal row with an outcome and no
/// `ended_at`, so each column is tied to the state instead.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_live_session_cannot_hold_a_complete_terminal_outcome() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let session = creation_session(44);
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(dispatched_creation(44))
        .await?;

    let error = sqlx::query(
        "UPDATE session_lifecycle SET terminal_outcome_kind = 'failed_unknown'
          WHERE session_id = $1",
    )
    .bind(session.into_uuid())
    .execute(&pool)
    .await
    .expect_err("an outcome belongs to a terminal row");

    assert_eq!(
        error
            .as_database_error()
            .and_then(DatabaseError::code)
            .as_deref(),
        Some("23514")
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// A committed handoff survives the transitions between the decision and the
/// turn's boundary, and only the settlement it describes clears it.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_park_between_decision_and_settlement_keeps_the_handoff() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let repository = SessionLifecycleRepository::new(pool.clone());
    let session = creation_session(45);
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(dispatched_creation(45))
        .await?;
    activate_first_turn(&pool, session, 45).await?;
    let committed = SessionTerminalOutcome::Stopped {
        sticky: StopStickiness::Sticky,
    };
    repository
        .commit_pending_terminal(session, committed, LifecycleActor::Operator)
        .await?;

    repository
        .park(
            session,
            SessionParkCause::OperatorHold,
            SessionParkResponder::Operator,
            None,
            LifecycleActor::Operator,
        )
        .await?;

    assert_eq!(
        repository
            .load(session)
            .await?
            .expect("the session keeps its lifecycle row")
            .pending_terminal(),
        Some(committed)
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// A closure naming a different outcome than the one already committed is
/// refused: the committed decision is what started tearing the turn down.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_closure_cannot_override_a_committed_handoff() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let repository = SessionLifecycleRepository::new(pool.clone());
    let session = creation_session(46);
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(dispatched_creation(46))
        .await?;
    repository
        .commit_pending_terminal(
            session,
            SessionTerminalOutcome::Stopped {
                sticky: StopStickiness::Sticky,
            },
            LifecycleActor::Operator,
        )
        .await?;

    let error = repository
        .close(
            session,
            SessionTerminalOutcome::FailedUnknown,
            LifecycleActor::Operator,
        )
        .await
        .expect_err("the committed decision is the one that settles");

    assert_eq!(
        lifecycle_rejection(error),
        SessionLifecycleRejection::PendingTerminalConflict
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// A committed closure closes the steering still pending on its live turn
/// `not_delivered`, with a receipt per injection; the turn then settles with
/// no successor to reclassify into, and the session records terminal.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn terminalization_closes_pending_steering_not_delivered() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let session = creation_session(40);
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(interactive_creation(40))
        .await?;
    let turn = activate_first_turn(&pool, session, 40).await?;
    let steering = DurableCommandId::from_uuid(Uuid::from_u128(LIFECYCLE_SEED + 40 + 0xa00));
    let accepted = SubmitInputRepository::new(pool.clone())
        .handle(
            SubmitInput::new(
                steering,
                session,
                UserContent::try_text(String::from("steer before closing"))
                    .expect("fixture content is admitted"),
                DeliveryRequest::NextSafePoint {
                    expected_active_turn: turn,
                },
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(LIFECYCLE_SEED + 40 + 0xb00)),
            None,
        )
        .await?;
    assert!(matches!(
        accepted,
        SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Applied(
            SubmitInputAppliedResult::PendingSteering(_)
        ))
    ));

    let repository = SessionLifecycleRepository::new(pool.clone());
    let outcome = SessionTerminalOutcome::Stopped {
        sticky: StopStickiness::Sticky,
    };
    repository
        .commit_pending_terminal(session, outcome, LifecycleActor::Operator)
        .await?;
    let settled: (String, String, Option<Uuid>) = sqlx::query_as(
        "SELECT accepted.disposition_kind, receipt.outcome_kind, receipt.delivered_turn_id
           FROM accepted_input AS accepted
           JOIN injection_settled_outbox_event AS receipt
             ON receipt.command_id = accepted.accepting_command_id
          WHERE accepted.accepting_command_id = $1",
    )
    .bind(steering.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        settled,
        (
            String::from("closed_not_delivered"),
            String::from("not_delivered"),
            None
        )
    );

    let liveness = PostgresTurnLivenessRepository::new(
        pool.clone(),
        TurnLivenessPersistenceBounds::new(None, None, None),
    );
    let candidate = *liveness
        .quiescent_active_turns(None)
        .await?
        .candidates()
        .first()
        .expect("the live turn is quiescent");
    assert_eq!(candidate.turn(), turn);
    assert_eq!(
        liveness
            .terminalize_stale_turn(
                candidate,
                signalbox_domain::AcceptedInputTurnFailureIdentities::new(
                    SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(
                        LIFECYCLE_SEED + 40 + 0xc00
                    )),
                    ContextFrontierId::from_uuid(Uuid::from_u128(LIFECYCLE_SEED + 40 + 0xd00)),
                ),
                &mut signalbox_application::UuidV7StartupScanIdGenerator,
            )
            .await?,
        signalbox_application::StaleTurnOutcome::Terminalized
    );
    let successors: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM turn_lifecycle WHERE session_id = $1 AND turn_id <> $2",
    )
    .bind(session.into_uuid())
    .bind(turn.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(successors, 0, "closed steering is not reclassified");
    assert_eq!(
        repository.settle_pending_terminal(session).await?,
        SessionLifecycleState::Terminal { outcome }
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// The third disposition exists only for a terminal or closing session;
/// closing steering under a live one is refused at commit.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn closed_steering_requires_a_terminal_session() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let session = creation_session(41);
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(interactive_creation(41))
        .await?;
    let turn = activate_first_turn(&pool, session, 41).await?;
    let accepted_input = AcceptedInputId::from_uuid(Uuid::from_u128(LIFECYCLE_SEED + 41 + 0xb00));
    SubmitInputRepository::new(pool.clone())
        .handle(
            SubmitInput::new(
                DurableCommandId::from_uuid(Uuid::from_u128(LIFECYCLE_SEED + 41 + 0xa00)),
                session,
                UserContent::try_text(String::from("steer a live turn"))
                    .expect("fixture content is admitted"),
                DeliveryRequest::NextSafePoint {
                    expected_active_turn: turn,
                },
            ),
            accepted_input,
            None,
        )
        .await?;

    let mut transaction = pool.begin().await?;
    sqlx::query(
        "UPDATE accepted_input
            SET disposition_kind = 'closed_not_delivered'
          WHERE accepted_input_id = $1",
    )
    .bind(accepted_input.into_uuid())
    .execute(&mut *transaction)
    .await?;
    let error = transaction
        .commit()
        .await
        .expect_err("a live session cannot close its steering");
    assert_eq!(
        database_constraint(&error),
        Some("accepted_input_closed_requires_terminal_session")
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// A park cannot install standing evidence that contradicts a decision already
/// committed: settlement would then refuse the outcome the closure recorded.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_park_cannot_contradict_a_committed_closure() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let repository = SessionLifecycleRepository::new(pool.clone());
    let session = creation_session(47);
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(dispatched_creation(47))
        .await?;
    activate_first_turn(&pool, session, 47).await?;
    repository
        .commit_pending_terminal(
            session,
            SessionTerminalOutcome::FailedRetryable {
                cause: SessionRetryableCause::ProviderTransient,
            },
            LifecycleActor::Operator,
        )
        .await?;

    let error = repository
        .park(
            session,
            SessionParkCause::StructuralFailure,
            SessionParkResponder::Operator,
            Some(SessionFailureCause::Structural(
                SessionStructuralCause::BrokenToolchain,
            )),
            LifecycleActor::Operator,
        )
        .await
        .expect_err("the park would strand the committed decision");

    assert_eq!(
        lifecycle_rejection(error),
        SessionLifecycleRejection::StandingCauseMismatch
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// A closure records its acting identity in its own provenance columns, so the
/// committed model-declaration rules — which validate every non-null
/// `model_tool_request_id` as a `goal_declare` request and hold it globally
/// unique — never see it. Both core identities share these columns; the model
/// one is exercised here because a same-session tool request needs a tool loop
/// this fixture does not run.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_closure_records_its_acting_identity_in_its_own_columns() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let session = creation_session(48);
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(dispatched_creation(48))
        .await?;
    attach_goal(&pool, session, 48).await?;
    let acting_turn = TurnId::from_uuid(Uuid::from_u128(LIFECYCLE_SEED + 48 + 0xc00));

    SessionLifecycleRepository::new(pool.clone())
        .close(
            session,
            SessionTerminalOutcome::FailedUnknown,
            LifecycleActor::Core {
                agency: CoreAgency::Model { turn: acting_turn },
            },
        )
        .await?;

    let recorded: (Option<Uuid>, Option<Uuid>, Option<Uuid>) = sqlx::query_as(
        "SELECT closure_actor_turn_id, model_turn_id, model_tool_request_id
           FROM goal_event
          WHERE session_id = $1 AND event_kind = 'session_closed'",
    )
    .bind(session.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(recorded, (Some(acting_turn.into_uuid()), None, None));

    pool.close().await;
    drop(container);
    Ok(())
}

/// `abandoned` is the operator's write-off of a parked session, so
/// neither another classification nor an unparked session records one.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn only_an_operator_writes_off_a_parked_session() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let repository = SessionLifecycleRepository::new(pool.clone());
    let session = creation_session(49);
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(dispatched_creation(49))
        .await?;
    activate_first_turn(&pool, session, 49).await?;

    let unparked = repository
        .close(
            session,
            SessionTerminalOutcome::Abandoned,
            LifecycleActor::Operator,
        )
        .await
        .expect_err("nobody wrote off a session nobody parked");
    assert_eq!(
        lifecycle_rejection(unparked),
        SessionLifecycleRejection::AbandonRequiresParkedOperator
    );

    repository
        .park(
            session,
            SessionParkCause::OperatorHold,
            SessionParkResponder::Operator,
            None,
            LifecycleActor::Operator,
        )
        .await?;
    let error = repository
        .close(
            session,
            SessionTerminalOutcome::Abandoned,
            LifecycleActor::Watchdog,
        )
        .await
        .expect_err("a watchdog does not write a session off");

    assert_eq!(
        lifecycle_rejection(error),
        SessionLifecycleRejection::AbandonRequiresParkedOperator
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// The handoff carries the actor that decided, and the settlement
/// records that actor rather than whichever caller observed the turn reach its
/// boundary. The settlement takes no actor at all, which is what makes the
/// attribution unforgeable across a worker change or a restart — and the
/// abandonment gate applies to the deciding actor, at the decision.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_settlement_records_the_actor_that_decided() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let repository = SessionLifecycleRepository::new(pool.clone());
    let session = creation_session(51);
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(dispatched_creation(51))
        .await?;
    // Parked by statement: abandonment is a parked-session closure, and this
    // session has no turn to settle, so the settlement below is the handoff's
    // own write rather than a turn boundary.
    park_by_statement(&pool, session).await?;

    let refused = repository
        .commit_pending_terminal(
            session,
            SessionTerminalOutcome::Abandoned,
            LifecycleActor::Watchdog,
        )
        .await
        .expect_err("a watchdog does not write a session off");
    assert_eq!(
        lifecycle_rejection(refused),
        SessionLifecycleRejection::AbandonRequiresParkedOperator
    );

    repository
        .commit_pending_terminal(
            session,
            SessionTerminalOutcome::Abandoned,
            LifecycleActor::Operator,
        )
        .await?;
    let committed = repository
        .load(session)
        .await?
        .expect("the session keeps its lifecycle row");
    assert_eq!(
        committed.pending_terminal_actor(),
        Some(LifecycleActor::Operator)
    );

    let settled = repository.settle_pending_terminal(session).await?;
    assert_eq!(
        settled,
        SessionLifecycleState::Terminal {
            outcome: SessionTerminalOutcome::Abandoned,
        }
    );
    let terminal = repository
        .load(session)
        .await?
        .expect("the session keeps its lifecycle row");
    assert_eq!(terminal.actor(), LifecycleActor::Operator);
    assert_eq!(terminal.pending_terminal(), None);
    assert_eq!(terminal.pending_terminal_actor(), None);

    pool.close().await;
    drop(container);
    Ok(())
}

/// A turn the delegation cascade already terminated logically is not
/// live work. The parent's stop released its runtime slot and wrote the
/// terminal proof while the child turn's `state_kind` stayed put by design, so
/// reading `state_kind` alone would leave the child session unclosable.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_logically_terminated_child_turn_does_not_hold_its_session_open()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let session = creation_session(52);
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(dispatched_creation(52))
        .await?;
    activate_first_turn(&pool, session, 52).await?;

    // Stands in for the delegation cascade: the flag is admitted only on a
    // delegated turn, and building a real parent-and-child spawn is not what
    // this proves.
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

    let terminal = SessionLifecycleRepository::new(pool.clone())
        .close(
            session,
            SessionTerminalOutcome::FailedUnknown,
            LifecycleActor::Watchdog,
        )
        .await?;

    assert_eq!(
        terminal,
        SessionLifecycleState::Terminal {
            outcome: SessionTerminalOutcome::FailedUnknown,
        }
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// A handoff has to be settleable. An achievement or a stop is the goal
/// contract's own event to write, so a closure naming one over an open
/// generation is refused — and a handoff committed anyway could never settle
/// and could never be replaced, which is a session stuck by construction.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn an_unsettleable_handoff_is_never_committed() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let repository = SessionLifecycleRepository::new(pool.clone());
    let session = creation_session(53);
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(dispatched_creation(53))
        .await?;
    attach_goal(&pool, session, 53).await?;

    let error = repository
        .commit_pending_terminal(
            session,
            SessionTerminalOutcome::AchievedVerified,
            LifecycleActor::Watchdog,
        )
        .await
        .expect_err("the goal command settles an achievement, not a closure");
    assert_eq!(
        lifecycle_rejection(error),
        SessionLifecycleRejection::GoalGenerationStillOpen
    );

    // The outcomes the goal contract does admit as closures still commit.
    repository
        .commit_pending_terminal(
            session,
            SessionTerminalOutcome::FailedUnknown,
            LifecycleActor::Watchdog,
        )
        .await?;

    pool.close().await;
    drop(container);
    Ok(())
}

/// A goal event the user authored is the operator's, and the lifecycle
/// transition it projects records that. Only a lift said so before, so an
/// operator's stop or supersede read as daemon core in the durable history.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_user_authored_goal_event_projects_operator_provenance() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let session = creation_session(54);
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(dispatched_creation(54))
        .await?;
    attach_goal(&pool, session, 54).await?;
    block_goal_by_statement(&pool, session, LIFECYCLE_SEED + 54 + 0xc00).await?;
    assert!(matches!(
        SessionLifecycleRepository::new(pool.clone())
            .load(session)
            .await?
            .expect("the session keeps its lifecycle row")
            .state(),
        SessionLifecycleState::Blocked { .. }
    ));

    GoalRepository::new(pool.clone())
        .handle_user_command(
            GoalUserCommand::new(
                DurableCommandId::from_uuid(Uuid::from_u128(LIFECYCLE_SEED + 54 + 0xd00)),
                session,
                GoalUserAction::Stop {
                    descendant_scope: DescendantTerminationScope::ParentAlone,
                },
            ),
            None,
            |_| None,
        )
        .await?;

    let record = SessionLifecycleRepository::new(pool.clone())
        .load(session)
        .await?
        .expect("the session keeps its lifecycle row");
    assert_eq!(record.actor(), LifecycleActor::Operator);

    pool.close().await;
    drop(container);
    Ok(())
}

/// The standing evidence a park carries is the evidence its cause
/// names. A closure reads that evidence to classify the outcome, so a pair
/// that contradicts itself would close under a classification the park never
/// supported.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_park_carries_the_standing_evidence_its_cause_names() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let repository = SessionLifecycleRepository::new(pool.clone());
    let session = creation_session(55);
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(dispatched_creation(55))
        .await?;
    activate_first_turn(&pool, session, 55).await?;

    let bare = repository
        .park(
            session,
            SessionParkCause::RetryBudgetExhausted,
            SessionParkResponder::Operator,
            None,
            LifecycleActor::Watchdog,
        )
        .await
        .expect_err("an exhaustion cannot say what it exhausted retries on");
    assert_eq!(
        lifecycle_rejection(bare),
        SessionLifecycleRejection::ParkStandingMismatch
    );

    let contradicted = repository
        .park(
            session,
            SessionParkCause::OperatorHold,
            SessionParkResponder::Operator,
            Some(SessionFailureCause::Structural(
                SessionStructuralCause::BrokenToolchain,
            )),
            LifecycleActor::Operator,
        )
        .await
        .expect_err("an operator hold stands on no failure");
    assert_eq!(
        lifecycle_rejection(contradicted),
        SessionLifecycleRejection::ParkStandingMismatch
    );

    let crossed = repository
        .park(
            session,
            SessionParkCause::RetryBudgetExhausted,
            SessionParkResponder::Operator,
            Some(SessionFailureCause::Structural(
                SessionStructuralCause::BrokenToolchain,
            )),
            LifecycleActor::Watchdog,
        )
        .await
        .expect_err("a retry exhaustion does not stand on a structural cause");
    assert_eq!(
        lifecycle_rejection(crossed),
        SessionLifecycleRejection::ParkStandingMismatch
    );

    repository
        .park(
            session,
            SessionParkCause::RetryBudgetExhausted,
            SessionParkResponder::Operator,
            Some(SessionFailureCause::Retryable(
                SessionRetryableCause::ProviderOverloaded,
            )),
            LifecycleActor::Watchdog,
        )
        .await?;

    pool.close().await;
    drop(container);
    Ok(())
}

/// A cause-bearing park cannot omit the standing evidence its cause
/// names, including when a writer bypasses the repository.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_cause_bearing_park_requires_standing_evidence() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let session = creation_session(62);
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(dispatched_creation(62))
        .await?;

    let retry_error = sqlx::query(
        "UPDATE session_lifecycle
            SET state_kind = 'parked', state_entered_at = statement_timestamp(),
                parked_cause = 'retry_budget_exhausted',
                parked_responder = 'operator', parked_since = statement_timestamp(),
                parked_standing_cause_kind = NULL
          WHERE session_id = $1",
    )
    .bind(session.into_uuid())
    .execute(&pool)
    .await
    .expect_err("a retry-exhaustion park carries retryable standing evidence");
    assert_eq!(
        retry_error
            .as_database_error()
            .and_then(DatabaseError::code)
            .as_deref(),
        Some("23514")
    );

    let structural_error = sqlx::query(
        "UPDATE session_lifecycle
            SET state_kind = 'parked', state_entered_at = statement_timestamp(),
                parked_cause = 'structural_failure',
                parked_responder = 'operator', parked_since = statement_timestamp(),
                parked_standing_cause_kind = NULL
          WHERE session_id = $1",
    )
    .bind(session.into_uuid())
    .execute(&pool)
    .await
    .expect_err("a structural-failure park carries structural standing evidence");
    assert_eq!(
        structural_error
            .as_database_error()
            .and_then(DatabaseError::code)
            .as_deref(),
        Some("23514")
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// The active-stall deadline measures a stall, not a session's whole
/// working life. A turn terminalizing with a queued successor projects
/// `active` again, and re-arming there is what keeps a continuously
/// progressing session from tripping the deadline armed at its first turn.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn turn_progress_re_arms_the_active_stall_deadline() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let session = creation_session(56);
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(dispatched_creation(56))
        .await?;
    activate_first_turn(&pool, session, 56).await?;

    let first: OffsetDateTime =
        sqlx::query_scalar("SELECT armed_at FROM session_deadline WHERE session_id = $1")
            .bind(session.into_uuid())
            .fetch_one(&pool)
            .await?;

    // Queueing another turn is admission, not progress: it must not postpone
    // the deadline of a session the scheduler is not advancing.
    queue_successor_turn(&pool, session, 56).await?;
    assert_eq!(
        sqlx::query_scalar::<_, OffsetDateTime>(
            "SELECT armed_at FROM session_deadline WHERE session_id = $1"
        )
        .bind(session.into_uuid())
        .fetch_one(&pool)
        .await?,
        first
    );

    // Stands in for the turn-boundary write: a turn terminalizing or a
    // successor activating fires this projection, and the mapping keeps the session
    // `active` across both, so the projected shape does not move with it.
    sqlx::query("SELECT project_session_lifecycle($1, false, NULL, NULL, true)")
        .bind(session.into_uuid())
        .execute(&pool)
        .await?;

    let second: OffsetDateTime =
        sqlx::query_scalar("SELECT armed_at FROM session_deadline WHERE session_id = $1")
            .bind(session.into_uuid())
            .fetch_one(&pool)
            .await?;
    assert!(second > first, "real progress re-arms the stall deadline");
    assert_eq!(
        armed_deadline(&pool, session).await?,
        Some(String::from("active_stall"))
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// A committed closure freezes the session. Activating a successor
/// beneath a handoff is what makes its settlement impossible — the terminal
/// write would find a live turn, and the next queued turn would do it again.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_committed_handoff_takes_no_new_turn() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let repository = SessionLifecycleRepository::new(pool.clone());
    let session = creation_session(57);
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(dispatched_creation(57))
        .await?;
    queue_first_turn(&pool, session, 57).await?;
    repository
        .commit_pending_terminal(
            session,
            SessionTerminalOutcome::FailedUnknown,
            LifecycleActor::Watchdog,
        )
        .await?;

    let started = StartEligibleTurnRepository::new(pool.clone())
        .handle(
            session,
            AcceptedInputTurnActivationIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(LIFECYCLE_SEED + 57 + 0x700)),
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(LIFECYCLE_SEED + 57 + 0x710)),
                ContextFrontierId::from_uuid(Uuid::from_u128(LIFECYCLE_SEED + 57 + 0x800)),
                TurnAttemptId::from_uuid(Uuid::from_u128(LIFECYCLE_SEED + 57 + 0x900)),
            ),
        )
        .await?;
    assert!(
        matches!(started, StartEligibleTurnOutcome::NoEligibleTurn),
        "a session already committed to an outcome starts nothing new"
    );
    assert_eq!(
        repository
            .load(session)
            .await?
            .expect("the session keeps its lifecycle row")
            .state(),
        SessionLifecycleState::Dispatched
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// Input cannot create a live turn beneath a committed closure, because
/// that turn would prevent the closure from settling.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn submit_input_refuses_a_committed_terminal_handoff() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let session = creation_session(61);
    let lifecycle = SessionLifecycleRepository::new(pool.clone());
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(dispatched_creation(61))
        .await?;
    lifecycle
        .commit_pending_terminal(
            session,
            SessionTerminalOutcome::FailedUnknown,
            LifecycleActor::Watchdog,
        )
        .await?;

    let error = SubmitInputRepository::new(pool.clone())
        .handle(
            SubmitInput::new(
                DurableCommandId::from_uuid(Uuid::from_u128(LIFECYCLE_SEED + 61 + 0x500)),
                session,
                UserContent::try_text(String::from("late lifecycle fixture input"))
                    .expect("fixture content is admitted"),
                DeliveryRequest::StartWhenNoActiveTurn {
                    configuration: input_choices(1, ModelSelectionOverride::UseSessionDefault),
                },
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(LIFECYCLE_SEED + 61 + 0x600)),
            Some(TurnId::from_uuid(Uuid::from_u128(
                LIFECYCLE_SEED + 61 + 0x700,
            ))),
        )
        .await
        .expect_err("a committed terminal handoff accepts no more input");
    assert!(matches!(
        error,
        SubmitInputRepositoryError::Corruption(SubmitInputCorruption::Inconsistent(
            "session has a pending terminal handoff"
        ))
    ));

    assert_eq!(
        lifecycle.settle_pending_terminal(session).await?,
        SessionLifecycleState::Terminal {
            outcome: SessionTerminalOutcome::FailedUnknown,
        }
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// A resume cannot lift a park under a committed closure. The
/// settlement wants the park it decided on and the activation gate wants the
/// handoff gone, so lifting it strands the session between them.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_resume_cannot_lift_a_park_under_a_committed_closure() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let repository = SessionLifecycleRepository::new(pool.clone());
    let session = creation_session(58);
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(dispatched_creation(58))
        .await?;
    park_by_statement(&pool, session).await?;
    repository
        .commit_pending_terminal(
            session,
            SessionTerminalOutcome::Abandoned,
            LifecycleActor::Operator,
        )
        .await?;

    let error = repository
        .resume(session)
        .await
        .expect_err("the committed decision is the one that settles");
    assert_eq!(
        lifecycle_rejection(error),
        SessionLifecycleRejection::PendingTerminalConflict
    );

    let settled = repository.settle_pending_terminal(session).await?;
    assert_eq!(
        settled,
        SessionLifecycleState::Terminal {
            outcome: SessionTerminalOutcome::Abandoned,
        }
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// A goal command cannot contradict a committed closure. Its own terminal
/// event would make the settlement refuse, and with the handoff standing and
/// activation frozen behind it the session could not move at all, so a client
/// command takes the durable `session_closing` rejection.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_goal_command_cannot_contradict_a_committed_closure() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let repository = SessionLifecycleRepository::new(pool.clone());
    let session = creation_session(59);
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(dispatched_creation(59))
        .await?;
    attach_goal(&pool, session, 59).await?;
    repository
        .commit_pending_terminal(
            session,
            SessionTerminalOutcome::FailedUnknown,
            LifecycleActor::Watchdog,
        )
        .await?;

    let outcome = GoalRepository::new(pool.clone())
        .handle_user_command(
            GoalUserCommand::new(
                DurableCommandId::from_uuid(Uuid::from_u128(LIFECYCLE_SEED + 59 + 0xd00)),
                session,
                GoalUserAction::Stop {
                    descendant_scope: DescendantTerminationScope::ParentAlone,
                },
            ),
            None,
            |_| None,
        )
        .await?;
    assert_eq!(
        outcome,
        GoalCommandHandlingOutcome::Recorded(GoalCommandResult::Rejected(
            GoalCommandRejection::SessionClosing
        ))
    );

    let settled = repository.settle_pending_terminal(session).await?;
    assert_eq!(
        settled,
        SessionLifecycleState::Terminal {
            outcome: SessionTerminalOutcome::FailedUnknown,
        }
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// A cause-bearing outcome cannot be stored without its cause. SQL null makes
/// an unsatisfied comparison null rather than false, so a shape stated only as
/// a vocabulary test would let a causeless failure commit and then fail to
/// decode.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_causeless_failure_is_rejected() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let session = creation_session(60);
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(dispatched_creation(60))
        .await?;

    let terminal_error = sqlx::query(
        "UPDATE session_lifecycle
            SET terminal_outcome_kind = 'failed_retryable', terminal_cause_kind = NULL
          WHERE session_id = $1",
    )
    .bind(session.into_uuid())
    .execute(&pool)
    .await
    .expect_err("a retryable terminal failure names the cause it retried");
    assert_eq!(
        terminal_error
            .as_database_error()
            .and_then(DatabaseError::code)
            .as_deref(),
        Some("23514")
    );

    let pending_error = sqlx::query(
        "UPDATE session_lifecycle
            SET pending_terminal_outcome_kind = 'failed_retryable',
                pending_terminal_cause_kind = NULL,
                pending_terminal_actor_kind = 'watchdog'
          WHERE session_id = $1",
    )
    .bind(session.into_uuid())
    .execute(&pool)
    .await
    .expect_err("a retryable pending failure names the cause it retried");
    assert_eq!(
        pending_error
            .as_database_error()
            .and_then(DatabaseError::code)
            .as_deref(),
        Some("23514")
    );

    pool.close().await;
    drop(container);
    Ok(())
}
