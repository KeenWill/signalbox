//! PostgreSQL proof for the session lifecycle satellite: the §1 mapping, the
//! armed-deadline invariant, the parked override, §2's closures, and §6's
//! provenance and ownership journal.

use std::collections::BTreeSet;
use std::time::Duration;

use crate::*;
use signalbox_domain::{
    GoalStatement, GoalUserAction, GoalUserCommand, LifecycleActor, ModuleDispatch,
    RepoWatchDispatchId, SessionCreationProvenance, SessionFailureCause, SessionLifecycleState,
    SessionOwnership, SessionParkCause, SessionParkResponder, SessionRetirementCause,
    SessionStructuralCause, SessionTerminalOutcome, StopStickiness,
};
use signalbox_persistence::{
    session_lifecycle::{
        SessionLifecycleNumericBounds, SessionLifecycleRejection, SessionLifecycleRepository,
        SessionLifecycleRepositoryError,
    },
    turn_liveness::{PostgresTurnLivenessRepository, TurnLivenessPersistenceBounds},
};
use sqlx::error::DatabaseError;
use sqlx::types::time::OffsetDateTime;

const LIFECYCLE_SEED: u128 = 0x11fe_0000;

/// Builds one interactive creation, which §6 records as unmonitored.
fn interactive_creation(seed: u128) -> PreparedCreateSession {
    prepared(
        LIFECYCLE_SEED + seed,
        LIFECYCLE_SEED + seed + 0x100,
        direct(LIFECYCLE_SEED + seed + 0x200),
    )
}

/// Builds one repository-watch dispatch, which §6 records as owned work.
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

/// Reads the armed deadline's kind and whether it records an unbounded policy.
async fn armed_deadline(
    pool: &PgPool,
    session: SessionId,
) -> Result<Option<(String, bool)>, sqlx::Error> {
    let row: Option<(String, Option<OffsetDateTime>)> = sqlx::query_as(
        "SELECT deadline_kind, expires_at FROM session_deadline WHERE session_id = $1",
    )
    .bind(session.into_uuid())
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|(kind, expires_at)| (kind, expires_at.is_none())))
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

/// Terminalizes one queued goal turn by statement.
///
/// The commissioned goal's turn is queued and never activates here, and
/// `retired` — the disposition §10 gives exactly this shape — lands with the
/// event-vocabulary change. The fixture states the disposition the committed
/// vocabulary has for a turn that produced nothing, so the closure under test
/// meets a settled turn rather than a live one.
async fn settle_queued_turn_by_statement(
    pool: &PgPool,
    session: SessionId,
    turn: TurnId,
    seed: u128,
) -> Result<(), Box<dyn Error>> {
    sqlx::query("ALTER TABLE turn_lifecycle DISABLE TRIGGER ALL")
        .execute(pool)
        .await?;
    sqlx::query(
        "UPDATE turn_lifecycle
            SET state_kind = 'terminal', start_lineage_kind = 'first_in_session',
                immediate_predecessor_turn_id = NULL, starting_frontier_id = $3,
                terminal_frontier_id = $4, terminal_disposition_kind = 'failed',
                terminal_cause_kind = 'unclassified_failure'
          WHERE session_id = $1 AND turn_id = $2",
    )
    .bind(session.into_uuid())
    .bind(turn.into_uuid())
    .bind(Uuid::from_u128(seed))
    .bind(Uuid::from_u128(seed + 1))
    .execute(pool)
    .await?;
    sqlx::query("ALTER TABLE turn_lifecycle ENABLE TRIGGER ALL")
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

/// §6: an interactive creation is a conversation. It records the unmonitored
/// bit, opens its ownership journal, and carries no armed deadline, because a
/// deadline on a person's chat window is exactly what §6 forbids.
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

/// §6: a module-dispatched creation records the module and its exact dispatch,
/// is owned, and arms the first-input deadline §10 gives an owned creation.
/// The unbounded default records the deadline explicitly rather than omitting
/// it, which is what §1 requires of a `none` policy.
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
        Some((String::from("first_input"), true))
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

/// §1: the first accepted input moves the session to `dispatched`, not
/// `active` — the turn is queued, and only activation makes it active. Each
/// state re-arms the deadline that state defines, from the configured policy.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn the_mapping_follows_the_turn_from_dispatch_to_activation() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let repository = SessionLifecycleRepository::new(pool.clone());
    repository
        .apply_configured_bounds(&SessionLifecycleNumericBounds {
            dispatch: Some(Duration::from_secs(900)),
            active_stall: Some(Duration::from_secs(1800)),
            ..SessionLifecycleNumericBounds::default()
        })
        .await?;
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
        Some((String::from("dispatch"), false))
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
        Some((String::from("active_stall"), false))
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// §1: a parked session's rows are not eligibility-sweep candidates. Parking
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
    // store: §1 admits a park only from the states the turn mapping derives,
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

/// §1: a parked session's turn is not a liveness-watchdog candidate either.
/// Parking keeps the turn's phase, so a watchdog that still saw it would read
/// a deliberately held turn as a stalled one and reap the work an operator is
/// holding — the §13 safety-backfire class this conjunct exists to prevent.
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
    assert_eq!(
        armed_deadline(&pool, session).await?,
        Some((String::from("parked_renotify"), true))
    );
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

/// §1: leaving `parked` re-enters the state the mapping gives the suspended
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
            SessionParkCause::ProgressBudgetExhausted,
            SessionParkResponder::Operator,
            None,
            LifecycleActor::Operator,
        )
        .await?;

    let resumed = repository.resume(session, LifecycleActor::Operator).await?;

    assert_eq!(resumed, SessionLifecycleState::Active);
    assert_eq!(
        armed_deadline(&pool, session).await?,
        Some((String::from("active_stall"), true))
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// §1/§2: no terminal session leaves a non-terminal turn behind. A closure
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
            SessionTerminalOutcome::Abandoned,
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

/// §2 and codex finding F1: session terminalization settles the live goal
/// generation in the same closure. Goal state is the sole
/// continuation-stopping condition in the goal contract, so a pursuing goal
/// beneath a terminal session would keep scheduling work no one owns.
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

    settle_queued_turn_by_statement(
        &pool,
        session,
        TurnId::from_uuid(Uuid::from_u128(LIFECYCLE_SEED + 7 + 0xc00)),
        LIFECYCLE_SEED + 7 + 0xd00,
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

    pool.close().await;
    drop(container);
    Ok(())
}

/// §2: a verified achievement and a stop are the goal contract's own events,
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

/// §2: an operator write-off records the worktree and container cleanup its
/// closure cannot perform. Every other outcome leaves its resources to a
/// successor or to nothing at all, so only this one records an obligation.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn an_abandoned_closure_records_its_cleanup_obligation() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let repository = SessionLifecycleRepository::new(pool.clone());
    let abandoned = creation_session(9);
    let superseded = creation_session(10);
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(dispatched_creation(9))
        .await?;
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(dispatched_creation(10))
        .await?;

    repository
        .close(
            abandoned,
            SessionTerminalOutcome::Abandoned,
            LifecycleActor::Operator,
        )
        .await?;
    repository
        .close(
            superseded,
            SessionTerminalOutcome::Superseded {
                by: Some(abandoned),
            },
            LifecycleActor::Operator,
        )
        .await?;

    let obligations: Vec<(Uuid, String)> = sqlx::query_as(
        "SELECT session_id, outcome_kind FROM session_cleanup_obligation ORDER BY session_id",
    )
    .fetch_all(&pool)
    .await?;
    assert_eq!(
        obligations,
        vec![(abandoned.into_uuid(), String::from("abandoned"))]
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// §6: adopting takes the liveness obligation and arms the deadline the state
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
        Some((String::from("first_input"), true))
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

/// §6: `release` on a `parked` session is rejected. `parked` is an owned-only
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
    assert_eq!(
        armed_deadline(&pool, session).await?,
        Some((String::from("parked_renotify"), true))
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// §1's invariant, enforced rather than asserted: an owned non-terminal
/// session that holds the wrong deadline kind fails at commit. The check runs
/// deferred, so a transaction may move the state and re-arm in either order,
/// but it cannot commit having done only one.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn an_owned_session_cannot_commit_without_the_deadline_its_state_defines()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let session = creation_session(13);
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(dispatched_creation(13))
        .await?;

    let removed = sqlx::query("DELETE FROM session_deadline WHERE session_id = $1")
        .bind(session.into_uuid())
        .execute(&pool)
        .await
        .expect_err("an owned non-terminal session cannot lose its armed deadline");
    assert_eq!(
        removed
            .as_database_error()
            .and_then(DatabaseError::code)
            .as_deref(),
        Some("23514")
    );

    let wrong_kind = sqlx::query(
        "UPDATE session_deadline SET deadline_kind = 'active_stall',
                on_expiry_kind = 'park'
          WHERE session_id = $1",
    )
    .bind(session.into_uuid())
    .execute(&pool)
    .await
    .expect_err("a created session cannot hold an active-stall deadline");
    assert_eq!(
        wrong_kind
            .as_database_error()
            .and_then(DatabaseError::code)
            .as_deref(),
        Some("23514")
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// §1: an unmonitored session carries no armed deadline, and the invariant
/// says so both ways — an armed deadline on an unmonitored session is as much
/// a violation as a missing one on an owned session.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn an_unmonitored_session_cannot_hold_an_armed_deadline() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let session = creation_session(14);
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(interactive_creation(14))
        .await?;

    let error = sqlx::query(
        "INSERT INTO session_deadline
            (session_id, deadline_kind, on_expiry_kind, expires_at)
         VALUES ($1, 'first_input', 'retire', NULL)",
    )
    .bind(session.into_uuid())
    .execute(&pool)
    .await
    .expect_err("an unmonitored session carries no deadline");

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

/// Terminal is final. Without this a later transition could reopen a closed
/// session and move every §12 cohort built on `ended_at` underneath the week
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
                cause: SessionRetirementCause::FirstInputDeadlineExpired,
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

/// §2: a closure may commit to its outcome while the live turn still settles.
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

    repository.commit_pending_terminal(session, outcome).await?;
    let committed = repository
        .load(session)
        .await?
        .expect("the session keeps its lifecycle row");
    assert_eq!(committed.pending_terminal(), Some(outcome));
    assert_eq!(committed.state(), SessionLifecycleState::Created);

    let settled = repository
        .settle_pending_terminal(session, LifecycleActor::Operator)
        .await?;
    assert_eq!(settled, SessionLifecycleState::Terminal { outcome });

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

/// §2's closed terminal-outcome vocabulary, likewise read from the constraint.
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

/// §6: the eligibility sweep reports each candidate's ownership, so an
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

/// Every §2 closure releases the held dispatch slot. The release is
/// trigger-driven off a terminal goal event, and the closure's own
/// `session_closed` event joins that gate rather than growing a second release
/// path that could disagree with the first.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn the_dispatch_slot_release_gate_admits_a_session_closure() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;

    let definition: String = sqlx::query_scalar(
        "SELECT pg_get_functiondef(
                    'repo_watch_release_completed_dispatch_batches_for_goal()'::regprocedure
                )",
    )
    .fetch_one(&pool)
    .await?;

    assert!(definition.contains("'session_closed'"));
    assert!(definition.contains("'user_stopped'"));

    pool.close().await;
    drop(container);
    Ok(())
}
