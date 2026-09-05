//! PostgreSQL proof for the lifecycle metric views
//! (docs/spec/session-lifecycle.md).
//!
//! Every fixture here is built to be one of the cases the specification's
//! denominator and cohort rules single out: a session released mid-life, a
//! supersession that closed a failure and one that closed none, a stop, an
//! unmonitored conversation, a wall on a session that outlives its dispatch
//! week, an unbounded deadline, and a deadline that has expired. The
//! assertions are on the numbers.

use sqlx::types::time::PrimitiveDateTime;

use crate::*;
use signalbox_domain::{
    CoreAgency, DispatchingModule, LifecycleActor, ModuleDispatch, RepoWatchDispatchId,
    SessionCreationProvenance, SessionFailureCause, SessionParkCause, SessionParkResponder,
    SessionRetryableCause, SessionStructuralCause, SessionTerminalOutcome, StopStickiness,
};
use signalbox_persistence::{
    lifecycle_metrics::{
        LifecycleDeadlineViolation, LifecycleMetricsRepository, LifecycleNonTerminalState,
        LifecycleWeeklyMetrics,
    },
    operator_status::{
        ProcessOperatorStatusCounts, ProcessOperatorStatusItem, ProcessOperatorStatusRepository,
    },
    session_lifecycle::SessionLifecycleRepository,
};

const METRIC_SEED: u128 = 0x12fe_0000;

/// Builds one repository-watch dispatch, recorded as owned work.
fn dispatched_creation(seed: u128) -> PreparedCreateSession {
    CreateSession::new(
        DurableCommandId::from_uuid(Uuid::from_u128(METRIC_SEED + seed)),
        SessionCreationProvenance::module_dispatched(ModuleDispatch::RepositoryWatch {
            dispatch: RepoWatchDispatchId::from_uuid(Uuid::from_u128(METRIC_SEED + seed + 0x300)),
        }),
        SessionConfigurationDefaults::new(direct(METRIC_SEED + seed + 0x200)),
    )
    .prepare(SessionId::from_uuid(Uuid::from_u128(
        METRIC_SEED + seed + 0x100,
    )))
    .expect("a module-dispatched creation without ancestry is preparable")
}

/// Builds one interactive creation, recorded as unmonitored.
fn interactive_creation(seed: u128) -> PreparedCreateSession {
    prepared(
        METRIC_SEED + seed,
        METRIC_SEED + seed + 0x100,
        direct(METRIC_SEED + seed + 0x200),
    )
}

const fn metric_session(seed: u128) -> SessionId {
    SessionId::from_uuid(Uuid::from_u128(METRIC_SEED + seed + 0x100))
}

/// Creates one owned session with no turns and no goal.
async fn owned_session(pool: &PgPool, seed: u128) -> Result<SessionId, Box<dyn Error>> {
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(dispatched_creation(seed))
        .await?;
    Ok(metric_session(seed))
}

/// Creates one unmonitored conversation with no turns and no goal.
async fn unmonitored_session(pool: &PgPool, seed: u128) -> Result<SessionId, Box<dyn Error>> {
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(interactive_creation(seed))
        .await?;
    Ok(metric_session(seed))
}

/// Moves one closed session's `ended_at` into a chosen week.
///
/// Terminal is final by trigger, which is what makes a reported week stable
/// once it is published; a fixture that needs a session in last month's cohort
/// therefore steps around the guard rather than asking the repository to move
/// a closed row.
async fn backdate_closure(
    pool: &PgPool,
    session: SessionId,
    weeks_ago: i32,
) -> Result<(), sqlx::Error> {
    sqlx::query("ALTER TABLE session_lifecycle DISABLE TRIGGER ALL")
        .execute(pool)
        .await?;
    sqlx::query("ALTER TABLE session_ownership_event DISABLE TRIGGER ALL")
        .execute(pool)
        .await?;
    sqlx::query(
        "UPDATE session_lifecycle
            SET ended_at = ended_at - make_interval(weeks => $2)
          WHERE session_id = $1",
    )
    .bind(session.into_uuid())
    .bind(weeks_ago)
    .execute(pool)
    .await?;
    // The ownership the cohort reads has to move with the closure. A session
    // whose journal still stands at today would have ended before it was ever
    // owned, which is not a session — the cohort excludes it, correctly.
    sqlx::query(
        "UPDATE session_ownership_event
            SET recorded_at = recorded_at - make_interval(weeks => $2)
          WHERE session_id = $1",
    )
    .bind(session.into_uuid())
    .bind(weeks_ago)
    .execute(pool)
    .await?;
    sqlx::query("ALTER TABLE session_ownership_event ENABLE TRIGGER ALL")
        .execute(pool)
        .await?;
    sqlx::query("ALTER TABLE session_lifecycle ENABLE TRIGGER ALL")
        .execute(pool)
        .await?;
    Ok(())
}

/// Moves one session's turn write times into a chosen week.
///
/// A turn row's write time is immutable by trigger for the same reason: it is
/// the durable instant every derived duration is measured from. The dispatch
/// cohort reads it, so a fixture that needs a session dispatched three weeks
/// ago moves it here rather than waiting three weeks.
async fn backdate_dispatch(
    pool: &PgPool,
    session: SessionId,
    weeks_ago: i32,
) -> Result<(), sqlx::Error> {
    sqlx::query("ALTER TABLE turn_lifecycle DISABLE TRIGGER ALL")
        .execute(pool)
        .await?;
    sqlx::query(
        "UPDATE turn_lifecycle
            SET recorded_at = recorded_at - make_interval(weeks => $2)
          WHERE session_id = $1",
    )
    .bind(session.into_uuid())
    .bind(weeks_ago)
    .execute(pool)
    .await?;
    sqlx::query("ALTER TABLE turn_lifecycle ENABLE TRIGGER ALL")
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
    let turn = TurnId::from_uuid(Uuid::from_u128(METRIC_SEED + seed + 0x400));
    SubmitInputRepository::new(pool.clone())
        .handle(
            SubmitInput::new(
                DurableCommandId::from_uuid(Uuid::from_u128(METRIC_SEED + seed + 0x500)),
                session,
                UserContent::try_text(String::from("metric fixture input"))
                    .expect("fixture content is admitted"),
                DeliveryRequest::StartWhenNoActiveTurn {
                    configuration: input_choices(1, ModelSelectionOverride::UseSessionDefault),
                },
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(METRIC_SEED + seed + 0x600)),
            Some(turn),
        )
        .await?;
    Ok(turn)
}

/// Settles one turn terminal under a named cause, by statement.
///
/// The turn machine's own terminalization paths each name the cause their own
/// evidence supports; a metric fixture needs the cause chosen rather than
/// derived, so it writes the settled row directly. Triggers are off for the
/// write because the projection would otherwise move a parked session out of
/// its park, which is the one thing section 1 says a park suspends.
async fn settle_turn_with_cause(
    pool: &PgPool,
    session: SessionId,
    turn: TurnId,
    seed: u128,
    cause: &str,
) -> Result<(), Box<dyn Error>> {
    sqlx::query("ALTER TABLE turn_lifecycle DISABLE TRIGGER ALL")
        .execute(pool)
        .await?;
    sqlx::query(
        "UPDATE turn_lifecycle
            SET state_kind = 'terminal', start_lineage_kind = 'first_in_session',
                immediate_predecessor_turn_id = NULL, starting_frontier_id = $3,
                terminal_frontier_id = $4, terminal_disposition_kind = 'failed',
                terminal_cause_kind = $5, active_phase_kind = NULL,
                current_attempt_id = NULL, terminal_attempt_id = NULL
          WHERE session_id = $1 AND turn_id = $2",
    )
    .bind(session.into_uuid())
    .bind(turn.into_uuid())
    .bind(Uuid::from_u128(METRIC_SEED + seed + 0xa00))
    .bind(Uuid::from_u128(METRIC_SEED + seed + 0xb00))
    .bind(cause)
    .execute(pool)
    .await?;
    sqlx::query("ALTER TABLE turn_lifecycle ENABLE TRIGGER ALL")
        .execute(pool)
        .await?;
    Ok(())
}

/// Queues one turn and settles it terminal under a named cause.
async fn settled_turn_with_cause(
    pool: &PgPool,
    session: SessionId,
    seed: u128,
    cause: &str,
) -> Result<TurnId, Box<dyn Error>> {
    let turn = queue_first_turn(pool, session, seed).await?;
    settle_turn_with_cause(pool, session, turn, seed, cause).await?;
    Ok(turn)
}

/// Removes one session's armed deadline record, leaving the deadline violation.
///
/// The invariant is enforced by trigger, which is why the alarm exists at all:
/// the violation it counts is the state a path that got around the enforcement
/// would leave. Reaching that state in a fixture means getting around it the
/// same way.
async fn strand_deadline(pool: &PgPool, session: SessionId) -> Result<(), sqlx::Error> {
    sqlx::query("ALTER TABLE session_deadline DISABLE TRIGGER ALL")
        .execute(pool)
        .await?;
    sqlx::query("DELETE FROM session_deadline WHERE session_id = $1")
        .bind(session.into_uuid())
        .execute(pool)
        .await?;
    sqlx::query("ALTER TABLE session_deadline ENABLE TRIGGER ALL")
        .execute(pool)
        .await?;
    Ok(())
}

/// Queues and activates one turn, moving the session to `active`.
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
            origin_entry: Uuid::from_u128(METRIC_SEED + seed + 0x700),
            starting_frontier: Uuid::from_u128(METRIC_SEED + seed + 0x800),
            initial_attempt: Uuid::from_u128(METRIC_SEED + seed + 0x900),
        },
    )
    .await?;
    Ok(turn)
}

/// Reads the one deadline violation the snapshot streams.
///
/// The report carries the alarm as a count; the rows themselves reach an
/// operator through the status snapshot's cursor, so a test that asserts on a
/// row reads it where the operator does.
async fn one_deadline_violation(
    pool: &PgPool,
) -> Result<LifecycleDeadlineViolation, Box<dyn Error>> {
    let (items, counts) = drain_operator_status(pool).await?;
    assert_eq!(counts.lifecycle_deadline_violations(), 1);
    let violation = items
        .into_iter()
        .find_map(|item| match item {
            ProcessOperatorStatusItem::LifecycleDeadlineViolation(violation) => Some(violation),
            _ => None,
        })
        .expect("the counted violation is one of the streamed rows");
    Ok(violation)
}

/// Returns the week whose members are the sessions closed most recently.
fn latest_populated_week(weeks: &[LifecycleWeeklyMetrics]) -> LifecycleWeeklyMetrics {
    *weeks
        .iter()
        .rev()
        .find(|week| week.completion_failure().denominator() > 0)
        .expect("the fixture closed at least one cohort member")
}

/// The headline's cohort follows the ownership journal and its
/// denominator drops exactly the stops and the failure-free supersessions.
///
/// Six closures, one per rule the specification singles out: a release does
/// not remove a session from the cohort, a stop leaves the denominator, a
/// supersession that closed a park holding a failure cause stays in both, a
/// supersession that closed no failure leaves both, an achievement stays in
/// the denominator alone, and a conversation that was never owned never
/// entered the cohort at all.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn the_headline_counts_released_and_failure_driven_closures() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let repository = SessionLifecycleRepository::new(pool.clone());

    let released = owned_session(&pool, 0x10).await?;
    repository
        .release(released, LifecycleActor::Operator)
        .await?;
    repository
        .close(
            released,
            SessionTerminalOutcome::FailedRetryable {
                cause: SessionRetryableCause::InfrastructureFailure,
            },
            LifecycleActor::Core {
                agency: CoreAgency::Daemon,
            },
        )
        .await?;

    let stopped = owned_session(&pool, 0x20).await?;
    repository
        .close(
            stopped,
            SessionTerminalOutcome::Stopped {
                sticky: StopStickiness::Sticky,
            },
            LifecycleActor::Operator,
        )
        .await?;

    let respawned = owned_session(&pool, 0x30).await?;
    let respawned_turn = activate_first_turn(&pool, respawned, 0x30).await?;
    repository
        .park(
            respawned,
            SessionParkCause::StructuralFailure,
            SessionParkResponder::Module {
                module: DispatchingModule::RepositoryWatch,
            },
            Some(SessionFailureCause::Structural(
                SessionStructuralCause::ContextCompactionWall,
            )),
            LifecycleActor::Core {
                agency: CoreAgency::Daemon,
            },
        )
        .await?;
    settle_turn_with_cause(
        &pool,
        respawned,
        respawned_turn,
        0x30,
        "context_compaction_wall",
    )
    .await?;
    repository
        .close(
            respawned,
            SessionTerminalOutcome::Superseded { by: None },
            LifecycleActor::Operator,
        )
        .await?;

    let withdrawn = owned_session(&pool, 0x40).await?;
    repository
        .close(
            withdrawn,
            SessionTerminalOutcome::Superseded { by: None },
            LifecycleActor::Operator,
        )
        .await?;

    let achieved = owned_session(&pool, 0x50).await?;
    repository
        .close(
            achieved,
            SessionTerminalOutcome::AchievedVerified,
            LifecycleActor::Operator,
        )
        .await?;

    let conversation = unmonitored_session(&pool, 0x60).await?;
    repository
        .close(
            conversation,
            // `abandoned` is reserved for an operator writing off a parked
            // session, and an unmonitored one cannot be parked; a stop is
            // how a conversation ends.
            SessionTerminalOutcome::Stopped {
                sticky: StopStickiness::Redispatchable,
            },
            LifecycleActor::Operator,
        )
        .await?;

    let report = LifecycleMetricsRepository::new(pool.clone()).read().await?;
    let week = latest_populated_week(report.weeks());

    // Five owned closures reach `terminal`; the conversation never does,
    // because it was never owned.
    assert_eq!(week.overflow_incidence().denominator(), 5);
    // The trim drops the stop and the withdrawal, leaving three.
    assert_eq!(week.completion_failure().denominator(), 3);
    // The retryable failure and the failure-driven supersession are the two
    // failures; the achievement is the third denominator member.
    assert_eq!(week.completion_failure().numerator(), 2);
    assert_eq!(week.completion_failure().parts_per_million(), Some(666_666));
    assert_eq!(week.failed_unknown_share().numerator(), 0);

    pool.close().await;
    drop(container);
    Ok(())
}

/// An unbounded deadline satisfies the invariant explicitly and is
/// never counted; only a missing record is the violation.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn an_unbounded_deadline_is_exempt_and_a_missing_record_is_not() -> Result<(), Box<dyn Error>>
{
    let (container, pool, _database_url) = migrated_postgres().await?;
    let session = owned_session(&pool, 0x70).await?;

    let exempt = LifecycleMetricsRepository::new(pool.clone()).read().await?;
    assert_eq!(exempt.nonterminal_past_deadline(), 0);

    strand_deadline(&pool, session).await?;

    let violated = LifecycleMetricsRepository::new(pool.clone()).read().await?;
    assert_eq!(violated.nonterminal_past_deadline(), 1);
    let violation = one_deadline_violation(&pool).await?;
    assert_eq!(violation.session(), session);
    assert_eq!(violation.state(), LifecycleNonTerminalState::Created);
    assert_eq!(violation.expired_for_seconds(), None);

    pool.close().await;
    drop(container);
    Ok(())
}

/// The first companion alarm counts an expiry, and only an expiry: a
/// deadline still ahead of the clock is the ordinary case and never counts.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_deadline_counts_once_it_expires_and_not_before() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let session = owned_session(&pool, 0x80).await?;

    sqlx::query(
        "UPDATE session_deadline
            SET expires_at = clock_timestamp() + make_interval(secs => 300)
          WHERE session_id = $1",
    )
    .bind(session.into_uuid())
    .execute(&pool)
    .await?;
    let live = LifecycleMetricsRepository::new(pool.clone()).read().await?;
    assert_eq!(live.nonterminal_past_deadline(), 0);

    sqlx::query(
        "UPDATE session_deadline
            SET expires_at = clock_timestamp() - make_interval(secs => 300)
          WHERE session_id = $1",
    )
    .bind(session.into_uuid())
    .execute(&pool)
    .await?;
    let expired = LifecycleMetricsRepository::new(pool.clone()).read().await?;
    assert_eq!(expired.nonterminal_past_deadline(), 1);
    let violation = one_deadline_violation(&pool).await?;
    assert_eq!(violation.deadline_kind(), Some("admission"));
    assert!(
        violation
            .expired_for_seconds()
            .is_some_and(|age| age >= 300)
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// A wall attributes to the week the session was dispatched in, while its
/// closure lands in the week it terminalized in — so a long-lived session
/// contributes to two different weekly cohorts, each measuring a different
/// thing.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_wall_attributes_to_its_dispatch_week_and_the_closure_to_its_own()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let walled = owned_session(&pool, 0x90).await?;
    settled_turn_with_cause(&pool, walled, 0x90, "context_compaction_wall").await?;
    backdate_dispatch(&pool, walled, 3).await?;
    SessionLifecycleRepository::new(pool.clone())
        .close(
            walled,
            SessionTerminalOutcome::FailedStructural {
                cause: SessionStructuralCause::ContextCompactionWall,
            },
            LifecycleActor::Core {
                agency: CoreAgency::Daemon,
            },
        )
        .await?;

    let report = LifecycleMetricsRepository::new(pool.clone()).read().await?;
    let dispatch_week = report
        .weeks()
        .iter()
        .find(|week| week.wall_rate().denominator() > 0)
        .copied()
        .expect("the dispatch cohort has one member");
    let closure_week = latest_populated_week(report.weeks());

    assert_ne!(dispatch_week.week_start(), closure_week.week_start());
    assert_eq!(dispatch_week.wall_rate().numerator(), 1);
    assert_eq!(dispatch_week.wall_rate().denominator(), 1);
    assert_eq!(dispatch_week.completion_failure().denominator(), 0);
    assert_eq!(dispatch_week.completion_failure().parts_per_million(), None);
    assert_eq!(closure_week.completion_failure().numerator(), 1);
    assert_eq!(closure_week.wall_rate().denominator(), 0);
    // The wall itself was recorded in the dispatch week, and the alarm reports
    // it there immediately rather than waiting for maturation.
    assert_eq!(dispatch_week.wall_occurrences(), 1);
    assert_eq!(closure_week.wall_occurrences(), 0);

    pool.close().await;
    drop(container);
    Ok(())
}

/// Overflow incidence is over the full terminal cohort before the trim,
/// and `P(finish | overflow)` is over exactly the sessions it counted.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn overflow_reads_the_untrimmed_cohort_and_its_finished_share() -> Result<(), Box<dyn Error>>
{
    let (container, pool, _database_url) = migrated_postgres().await?;
    let repository = SessionLifecycleRepository::new(pool.clone());

    let finished = owned_session(&pool, 0xb0).await?;
    settled_turn_with_cause(&pool, finished, 0xb0, "context_headroom_exhausted").await?;
    repository
        .close(
            finished,
            SessionTerminalOutcome::AchievedVerified,
            LifecycleActor::Operator,
        )
        .await?;

    let stopped = owned_session(&pool, 0xc0).await?;
    settled_turn_with_cause(&pool, stopped, 0xc0, "context_headroom_exhausted").await?;
    repository
        .close(
            stopped,
            SessionTerminalOutcome::Stopped {
                sticky: StopStickiness::Redispatchable,
            },
            LifecycleActor::Operator,
        )
        .await?;

    let untouched = owned_session(&pool, 0xd0).await?;
    repository
        .close(
            untouched,
            SessionTerminalOutcome::AchievedVerified,
            LifecycleActor::Operator,
        )
        .await?;

    let report = LifecycleMetricsRepository::new(pool.clone()).read().await?;
    let week = latest_populated_week(report.weeks());

    // The stopped session leaves the headline's denominator but stays in the
    // untrimmed cohort overflow is measured over.
    assert_eq!(week.overflow_incidence().denominator(), 3);
    assert_eq!(week.completion_failure().denominator(), 2);
    assert_eq!(week.overflow_incidence().numerator(), 2);
    assert_eq!(week.finish_given_overflow().denominator(), 2);
    assert_eq!(week.finish_given_overflow().numerator(), 1);
    assert_eq!(
        week.finish_given_overflow().parts_per_million(),
        Some(500_000)
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// Cause completeness measures two axes, each outside its own catch-all
/// set — terminal turns over their whole population, and `known_failed` model
/// calls over the calls whose disposition admits a cause.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn cause_completeness_measures_both_axes_outside_their_catch_alls()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;

    let classified = owned_session(&pool, 0xe0).await?;
    settled_turn_with_cause(&pool, classified, 0xe0, "model_call_failed").await?;
    let unclassified = owned_session(&pool, 0xf0).await?;
    settled_turn_with_cause(&pool, unclassified, 0xf0, "unclassified_failure").await?;

    let seed = METRIC_SEED + 0x1000;
    let (fixture, model_repository, authorized) =
        authorize_checkpointed_model_call(&pool, seed).await?;
    let observation = authorized
        .observation_correlation()
        .bind_terminal_observation(ModelCallTerminalObservation::KnownFailed);
    model_repository
        .apply_terminal_observation(
            fixture.session,
            observation,
            ModelCallTerminalIdentities::Failed(FailedModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 0x20)),
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 0x21)),
            )),
            |_| panic!("the fixture has no pending steering to reclassify"),
        )
        .await?;

    let report = LifecycleMetricsRepository::new(pool.clone()).read().await?;
    let week = latest_populated_turn_week(report.weeks());

    // Three terminal turns: two carry a classified cause, one carries the
    // vocabulary's sole catch-all.
    assert_eq!(week.turn_cause_completeness().denominator(), 3);
    assert_eq!(week.turn_cause_completeness().numerator(), 2);
    // The single `known_failed` call carries no provider cause at all, which
    // is the absent case the metric counts against itself.
    assert_eq!(week.model_call_cause_completeness().denominator(), 1);
    assert_eq!(week.model_call_cause_completeness().numerator(), 0);

    set_call_provider_cause(&pool, fixture.call, "quota_exhausted").await?;

    let classified_report = LifecycleMetricsRepository::new(pool.clone()).read().await?;
    let classified_week = latest_populated_turn_week(classified_report.weeks());
    assert_eq!(
        classified_week.model_call_cause_completeness().numerator(),
        1
    );

    set_call_provider_cause(&pool, fixture.call, "unrecognized").await?;

    let catch_all_report = LifecycleMetricsRepository::new(pool.clone()).read().await?;
    let catch_all_week = latest_populated_turn_week(catch_all_report.weeks());
    assert_eq!(
        catch_all_week.model_call_cause_completeness().numerator(),
        0
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// Restates one terminal call's provider cause, by statement.
///
/// A terminal model call is immutable, which is what makes the recorded cause
/// evidence; the metric's own catch-all handling still has to be shown against
/// each spelling, so the fixture writes them where the runtime cannot.
async fn set_call_provider_cause(
    pool: &PgPool,
    call: ModelCallId,
    cause: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("ALTER TABLE model_call DISABLE TRIGGER ALL")
        .execute(pool)
        .await?;
    sqlx::query(
        "UPDATE model_call
            SET terminal_provider_failure_cause = $2
          WHERE model_call_id = $1",
    )
    .bind(call.into_uuid())
    .bind(cause)
    .execute(pool)
    .await?;
    sqlx::query("ALTER TABLE model_call ENABLE TRIGGER ALL")
        .execute(pool)
        .await?;
    Ok(())
}

/// Returns the most recent week that has terminal turns to measure.
fn latest_populated_turn_week(weeks: &[LifecycleWeeklyMetrics]) -> LifecycleWeeklyMetrics {
    *weeks
        .iter()
        .rev()
        .find(|week| week.turn_cause_completeness().denominator() > 0)
        .expect("the fixture settled at least one turn")
}

/// Drains one operator-status snapshot into its rows, counts, and verdict.
async fn drain_operator_status(
    pool: &PgPool,
) -> Result<(Vec<ProcessOperatorStatusItem>, ProcessOperatorStatusCounts), Box<dyn Error>> {
    let mut reader = ProcessOperatorStatusRepository::new(pool.clone())
        .open()
        .await?;
    let mut items = Vec::new();
    while let Some(item) = reader.next_item().await? {
        items.push(item);
    }
    let counts = reader
        .counts()
        .expect("an exhausted snapshot commits its counts");
    Ok((items, counts))
}

/// The operator-status snapshot carries the metrics and the alarm as two
/// further sections of the same coherent read.
///
/// The snapshot and the telemetry pass run the same statements, which is what
/// keeps the operator's number and the exported series from disagreeing about
/// the numbers; this reads them through the snapshot's own cursors.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn the_operator_status_snapshot_carries_the_metrics_and_the_alarm()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;

    let (empty_items, empty_counts) = drain_operator_status(&pool).await?;
    assert!(empty_items.is_empty());
    assert_eq!(empty_counts.lifecycle_weeks(), 0);
    assert_eq!(empty_counts.lifecycle_deadline_violations(), 0);

    let closed = owned_session(&pool, 0x1500).await?;
    SessionLifecycleRepository::new(pool.clone())
        .close(
            closed,
            SessionTerminalOutcome::AchievedVerified,
            LifecycleActor::Operator,
        )
        .await?;
    backdate_closure(&pool, closed, 1).await?;
    let stuck = owned_session(&pool, 0x1600).await?;
    strand_deadline(&pool, stuck).await?;

    let (items, counts) = drain_operator_status(&pool).await?;

    assert_eq!(counts.lifecycle_weeks(), 1);
    assert_eq!(counts.lifecycle_deadline_violations(), 1);
    assert!(matches!(
        items.first(),
        Some(ProcessOperatorStatusItem::LifecycleWeek(_))
    ));
    assert!(matches!(
        items.last(),
        Some(ProcessOperatorStatusItem::LifecycleDeadlineViolation(_))
    ));

    pool.close().await;
    drop(container);
    Ok(())
}

/// Moves one parked session's park instant into an earlier week.
async fn backdate_park(
    pool: &PgPool,
    session: SessionId,
    weeks_ago: i32,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE session_lifecycle
            SET parked_since = parked_since - make_interval(weeks => $2)
          WHERE session_id = $1",
    )
    .bind(session.into_uuid())
    .bind(weeks_ago)
    .execute(pool)
    .await?;
    Ok(())
}

/// A wall is counted in the week it happened.
///
/// A wall parks the session and leaves its turn suspended, so at the
/// moment the alarm most needs to page there is no terminal turn to read. The
/// park is the durable evidence, and terminalization carries it forward so the
/// week the wall is counted in does not move when the session later closes.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_parked_wall_is_counted_in_the_week_it_happened() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let repository = SessionLifecycleRepository::new(pool.clone());

    let walled = owned_session(&pool, 0x1700).await?;
    let turn = activate_first_turn(&pool, walled, 0x1700).await?;
    // The turn began two weeks before the park, which is the ordinary
    // ordering: a turn runs, then the wall parks the session. The park instant
    // is what dates the occurrence, so the two weeks must stay distinct for
    // this test to say anything.
    backdate_dispatch(&pool, walled, 2).await?;
    repository
        .park(
            walled,
            SessionParkCause::StructuralFailure,
            SessionParkResponder::Module {
                module: DispatchingModule::RepositoryWatch,
            },
            Some(SessionFailureCause::Structural(
                SessionStructuralCause::ContextCompactionWall,
            )),
            LifecycleActor::Core {
                agency: CoreAgency::Daemon,
            },
        )
        .await?;
    backdate_park(&pool, walled, 1).await?;

    let parked = LifecycleMetricsRepository::new(pool.clone()).read().await?;
    let parked_week = one_wall_occurrence_week(parked.weeks());
    let dispatch_week = parked
        .weeks()
        .iter()
        .find(|week| week.wall_rate().denominator() > 0)
        .map(LifecycleWeeklyMetrics::week_start)
        .expect("the dispatch cohort has one member");
    assert_ne!(parked_week, dispatch_week);

    // The suspended turn has terminalized nothing, so the park is the only
    // evidence the wall happened at all.
    assert_eq!(
        parked
            .weeks()
            .iter()
            .map(LifecycleWeeklyMetrics::wall_occurrences)
            .sum::<u64>(),
        1
    );

    settle_turn_with_cause(&pool, walled, turn, 0x1700, "context_compaction_wall").await?;
    repository
        .close(
            walled,
            SessionTerminalOutcome::Superseded { by: None },
            LifecycleActor::Operator,
        )
        .await?;

    let closed = LifecycleMetricsRepository::new(pool.clone()).read().await?;
    let closed_week = one_wall_occurrence_week(closed.weeks());

    // The settled turn now also names the wall, and its row is two weeks old
    // against the park's one. The park still dates the occurrence: reading the
    // earlier of the two evidences instead would move the wall backwards into
    // the week the turn started in.
    assert_eq!(closed_week, parked_week);
    assert_eq!(
        closed
            .weeks()
            .iter()
            .map(LifecycleWeeklyMetrics::wall_occurrences)
            .sum::<u64>(),
        1
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// Returns the week the one recorded wall occurrence belongs to.
/// The wall numerator and the occurrence count read the same three evidences,
/// so a session closed on a wall its turn never named still shows one.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_wall_named_only_by_the_closure_is_still_one_occurrence() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;

    let walled = owned_session(&pool, 0x1800).await?;
    settled_turn_with_cause(&pool, walled, 0x1800, "unclassified_failure").await?;
    SessionLifecycleRepository::new(pool.clone())
        .close(
            walled,
            SessionTerminalOutcome::FailedStructural {
                cause: SessionStructuralCause::ContextCompactionWall,
            },
            LifecycleActor::Core {
                agency: CoreAgency::Daemon,
            },
        )
        .await?;

    let report = LifecycleMetricsRepository::new(pool.clone()).read().await?;
    let walled_sessions: u64 = report
        .weeks()
        .iter()
        .map(|week| week.wall_rate().numerator())
        .sum();
    let occurrences: u64 = report
        .weeks()
        .iter()
        .map(LifecycleWeeklyMetrics::wall_occurrences)
        .sum();
    assert_eq!(walled_sessions, 1);
    assert_eq!(occurrences, walled_sessions);

    pool.close().await;
    drop(container);
    Ok(())
}

fn one_wall_occurrence_week(weeks: &[LifecycleWeeklyMetrics]) -> PrimitiveDateTime {
    weeks
        .iter()
        .find(|week| week.wall_occurrences() > 0)
        .expect("the fixture recorded one wall")
        .week_start()
}

/// The metric cohort is sessions "owned at any point in their life", so ownership
/// taken after the closure is not ownership during it.
///
/// An adoption recorded after `ended_at` would otherwise write an already-ended
/// session into a week that had already been reported without it.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn ownership_taken_after_the_closure_joins_no_cohort() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let repository = SessionLifecycleRepository::new(pool.clone());

    let conversation = unmonitored_session(&pool, 0x1800).await?;
    repository
        .close(
            conversation,
            // `abandoned` is reserved for an operator writing off a parked
            // session, and an unmonitored one cannot be parked; a stop is
            // how a conversation ends.
            SessionTerminalOutcome::Stopped {
                sticky: StopStickiness::Redispatchable,
            },
            LifecycleActor::Operator,
        )
        .await?;
    let closed = LifecycleMetricsRepository::new(pool.clone()).read().await?;
    assert_eq!(
        closed
            .weeks()
            .iter()
            .map(|week| week.overflow_incidence().denominator())
            .sum::<u64>(),
        0
    );

    adopt_by_statement(&pool, conversation).await?;

    let adopted = LifecycleMetricsRepository::new(pool.clone()).read().await?;
    assert_eq!(
        adopted
            .weeks()
            .iter()
            .map(|week| week.overflow_incidence().denominator())
            .sum::<u64>(),
        0
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// Journals an adoption for a session that has already ended.
///
/// The satellite refuses this shape — terminal is final and the journal head
/// must match the ownership bit — so the fixture writes it around those guards.
/// What is under test is the cohort: it holds even where a path around them
/// exists.
async fn adopt_by_statement(pool: &PgPool, session: SessionId) -> Result<(), sqlx::Error> {
    sqlx::query("ALTER TABLE session_ownership_event DISABLE TRIGGER ALL")
        .execute(pool)
        .await?;
    sqlx::query("ALTER TABLE session_lifecycle DISABLE TRIGGER ALL")
        .execute(pool)
        .await?;
    sqlx::query(
        "INSERT INTO session_ownership_event (
             session_id, event_ordinal, transition_kind, owned_after, actor_kind
         )
         SELECT $1,
                COALESCE(max(event_ordinal), 0) + 1,
                'adopted',
                true,
                'operator'
           FROM session_ownership_event
          WHERE session_id = $1",
    )
    .bind(session.into_uuid())
    .execute(pool)
    .await?;
    sqlx::query("UPDATE session_lifecycle SET owned = true WHERE session_id = $1")
        .bind(session.into_uuid())
        .execute(pool)
        .await?;
    sqlx::query("ALTER TABLE session_lifecycle ENABLE TRIGGER ALL")
        .execute(pool)
        .await?;
    sqlx::query("ALTER TABLE session_ownership_event ENABLE TRIGGER ALL")
        .execute(pool)
        .await?;
    Ok(())
}

/// The metrics count a supersession that closed a park holding a failure cause, so
/// the cause must outlive the committed decision waiting on the turn.
///
/// A cause cleared once the closure commits would reach settlement empty and
/// the supersession would be trimmed as a non-failure, flattering the rate.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_committed_supersession_keeps_its_cause_until_it_settles() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let repository = SessionLifecycleRepository::new(pool.clone());

    let respawned = owned_session(&pool, 0x1900).await?;
    let turn = activate_first_turn(&pool, respawned, 0x1900).await?;
    repository
        .park(
            respawned,
            SessionParkCause::StructuralFailure,
            SessionParkResponder::Module {
                module: DispatchingModule::RepositoryWatch,
            },
            Some(SessionFailureCause::Structural(
                SessionStructuralCause::ContextCompactionWall,
            )),
            LifecycleActor::Core {
                agency: CoreAgency::Daemon,
            },
        )
        .await?;
    // The closure keeps the actor that decided it, so the settlement names
    // neither an actor nor an outcome of its own.
    repository
        .commit_pending_terminal(
            respawned,
            SessionTerminalOutcome::Superseded { by: None },
            LifecycleActor::Core {
                agency: CoreAgency::Daemon,
            },
        )
        .await?;
    settle_turn_with_cause(&pool, respawned, turn, 0x1900, "context_compaction_wall").await?;
    repository.settle_pending_terminal(respawned).await?;

    let report = LifecycleMetricsRepository::new(pool.clone()).read().await?;
    let week = latest_populated_week(report.weeks());

    // The supersession closed a failure, so it stays in both halves rather
    // than being trimmed as withdrawn work.
    assert_eq!(week.completion_failure().denominator(), 1);
    assert_eq!(week.completion_failure().numerator(), 1);

    pool.close().await;
    drop(container);
    Ok(())
}
