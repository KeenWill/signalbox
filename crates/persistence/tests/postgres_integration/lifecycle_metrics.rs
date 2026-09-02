//! PostgreSQL proof for the §12 metrics, their two companion alarms, and the
//! substrate-v0 gate.
//!
//! Every fixture here is built to be one of the cases the specification's
//! denominator and cohort rules single out: a session released mid-life, a
//! supersession that closed a failure and one that closed none, a stop, an
//! unmonitored conversation, a wall on a session that outlives its dispatch
//! week, an unbounded deadline, and an expiry inside the processing grace. The
//! assertions are on the numbers, because the numbers are what the gate turns
//! on.

use std::time::Duration;

use crate::*;
use signalbox_domain::{
    CoreAgency, DispatchingModule, LifecycleActor, ModuleDispatch, RepoWatchDispatchId,
    SessionCreationProvenance, SessionFailureCause, SessionParkCause, SessionParkResponder,
    SessionRetryableCause, SessionStructuralCause, SessionTerminalOutcome, StopStickiness,
};
use signalbox_persistence::{
    lifecycle_metrics::{
        LifecycleGateVerdict, LifecycleMetricBounds, LifecycleMetricsRepository,
        LifecycleNonTerminalState, LifecycleWeeklyMetrics,
    },
    session_lifecycle::{SessionLifecycleNumericBounds, SessionLifecycleRepository},
};

const METRIC_SEED: u128 = 0x12fe_0000;

/// One week, the width of every §12 cohort.
const WEEK: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// Builds one repository-watch dispatch, which §6 records as owned work.
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

/// Builds one interactive creation, which §6 records as unmonitored.
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
    sqlx::query(
        "UPDATE session_lifecycle
            SET ended_at = ended_at - make_interval(weeks => $2)
          WHERE session_id = $1",
    )
    .bind(session.into_uuid())
    .bind(weeks_ago)
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

/// Removes one session's armed deadline record, leaving the §1 violation.
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

/// Installs one deployment policy for the metric bounds.
async fn apply_metric_bounds(
    pool: &PgPool,
    bounds: LifecycleMetricBounds,
) -> Result<(), Box<dyn Error>> {
    LifecycleMetricsRepository::new(pool.clone())
        .apply_configured_bounds(&bounds)
        .await?;
    Ok(())
}

/// Returns the week whose members are the sessions closed most recently.
fn latest_populated_week(weeks: &[LifecycleWeeklyMetrics]) -> LifecycleWeeklyMetrics {
    *weeks
        .iter()
        .rev()
        .find(|week| week.completion_failure().denominator() > 0)
        .expect("the fixture closed at least one cohort member")
}

/// §12: the headline's cohort follows the ownership journal and its
/// denominator drops exactly the stops and the failure-free supersessions.
///
/// Six closures, one per rule the specification singles out: a release does
/// not remove a session from the gate, a stop leaves the denominator, a
/// supersession that closed a park holding a failure cause stays in both, a
/// supersession that closed no failure leaves both, an achievement stays in
/// the denominator alone, and a conversation that was never owned never
/// entered the cohort at all.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn the_headline_counts_released_and_failure_driven_closures() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let repository = SessionLifecycleRepository::new(pool.clone());
    apply_metric_bounds(&pool, LifecycleMetricBounds::default()).await?;

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
            SessionTerminalOutcome::Abandoned,
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

/// §12 and §1: an unbounded deadline satisfies the invariant explicitly and is
/// never counted; only a missing record is the violation.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn an_unbounded_deadline_is_exempt_and_a_missing_record_is_not() -> Result<(), Box<dyn Error>>
{
    let (container, pool, _database_url) = migrated_postgres().await?;
    apply_metric_bounds(&pool, LifecycleMetricBounds::default()).await?;
    let session = owned_session(&pool, 0x70).await?;

    let exempt = LifecycleMetricsRepository::new(pool.clone()).read().await?;
    assert_eq!(exempt.nonterminal_past_deadline(), 0);

    strand_deadline(&pool, session).await?;

    let violated = LifecycleMetricsRepository::new(pool.clone()).read().await?;
    assert_eq!(violated.nonterminal_past_deadline(), 1);
    let violation = violated
        .violations()
        .first()
        .expect("the alarm names the session it counted");
    assert_eq!(violation.session(), session);
    assert_eq!(violation.state(), LifecycleNonTerminalState::Created);
    assert_eq!(violation.expired_for_seconds(), None);

    pool.close().await;
    drop(container);
    Ok(())
}

/// §12 finding F8: an expiry counts only once the configured processing grace
/// has also passed, so ordinary timer and commit latency never trips the
/// zero-target alarm.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn an_expired_deadline_counts_only_past_the_processing_grace() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    apply_metric_bounds(
        &pool,
        LifecycleMetricBounds {
            deadline_processing_grace: Some(Duration::from_secs(120)),
            ..LifecycleMetricBounds::default()
        },
    )
    .await?;
    SessionLifecycleRepository::new(pool.clone())
        .apply_configured_bounds(&SessionLifecycleNumericBounds {
            first_input: Some(Duration::from_secs(60)),
            ..SessionLifecycleNumericBounds::default()
        })
        .await?;
    let session = owned_session(&pool, 0x80).await?;

    sqlx::query(
        "UPDATE session_deadline
            SET expires_at = clock_timestamp() - make_interval(secs => 30)
          WHERE session_id = $1",
    )
    .bind(session.into_uuid())
    .execute(&pool)
    .await?;
    let inside_grace = LifecycleMetricsRepository::new(pool.clone()).read().await?;
    assert_eq!(inside_grace.nonterminal_past_deadline(), 0);

    sqlx::query(
        "UPDATE session_deadline
            SET expires_at = clock_timestamp() - make_interval(secs => 300)
          WHERE session_id = $1",
    )
    .bind(session.into_uuid())
    .execute(&pool)
    .await?;
    let past_grace = LifecycleMetricsRepository::new(pool.clone()).read().await?;
    assert_eq!(past_grace.nonterminal_past_deadline(), 1);
    let violation = past_grace
        .violations()
        .first()
        .expect("the alarm names the session it counted");
    assert_eq!(violation.deadline_kind(), Some("first_input"));
    assert!(
        violation
            .expired_for_seconds()
            .is_some_and(|age| age >= 300)
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// §12: a wall attributes to the week the session was dispatched in, while its
/// closure lands in the week it terminalized in — so a long-lived session
/// contributes to two different weekly cohorts, each measuring a different
/// thing.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_wall_attributes_to_its_dispatch_week_and_the_closure_to_its_own()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    apply_metric_bounds(
        &pool,
        LifecycleMetricBounds {
            wall_cohort_maturation: Some(WEEK),
            ..LifecycleMetricBounds::default()
        },
    )
    .await?;
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
    // Its only member is terminal, so the dispatch cohort matured whatever the
    // window says.
    assert!(dispatch_week.wall_cohort_matured());
    // The wall itself was recorded in the dispatch week, and the alarm reports
    // it there immediately rather than waiting for maturation.
    assert_eq!(dispatch_week.wall_occurrences(), 1);
    assert_eq!(closure_week.wall_occurrences(), 0);

    pool.close().await;
    drop(container);
    Ok(())
}

/// §12 finding F9: a weekly dispatch cohort is gate-evaluable only once no
/// member is both non-terminal and still inside the configured maturation
/// window.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_dispatch_cohort_matures_only_past_its_configured_window() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    apply_metric_bounds(
        &pool,
        LifecycleMetricBounds {
            wall_cohort_maturation: Some(WEEK),
            ..LifecycleMetricBounds::default()
        },
    )
    .await?;
    let live = owned_session(&pool, 0xa0).await?;
    settled_turn_with_cause(&pool, live, 0xa0, "unclassified_failure").await?;

    let immature = LifecycleMetricsRepository::new(pool.clone()).read().await?;
    let immature_week = immature
        .weeks()
        .iter()
        .find(|week| week.wall_rate().denominator() > 0)
        .copied()
        .expect("the dispatch cohort has one member");
    assert!(!immature_week.wall_cohort_matured());

    backdate_dispatch(&pool, live, 3).await?;

    let matured = LifecycleMetricsRepository::new(pool.clone()).read().await?;
    let matured_week = matured
        .weeks()
        .iter()
        .find(|week| week.wall_rate().denominator() > 0)
        .copied()
        .expect("the dispatch cohort has one member");
    assert!(matured_week.wall_cohort_matured());

    pool.close().await;
    drop(container);
    Ok(())
}

/// §12: overflow incidence is over the full terminal cohort before the trim,
/// and `P(finish | overflow)` is over exactly the sessions it counted.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn overflow_reads_the_untrimmed_cohort_and_its_finished_share() -> Result<(), Box<dyn Error>>
{
    let (container, pool, _database_url) = migrated_postgres().await?;
    let repository = SessionLifecycleRepository::new(pool.clone());
    apply_metric_bounds(&pool, LifecycleMetricBounds::default()).await?;

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

/// §12: cause completeness measures two axes, each outside its own catch-all
/// set — terminal turns over their whole population, and `known_failed` model
/// calls over the calls whose disposition admits a cause.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn cause_completeness_measures_both_axes_outside_their_catch_alls()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    apply_metric_bounds(&pool, LifecycleMetricBounds::default()).await?;

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

/// §12: the substrate-v0 gate reads the configured number of consecutive
/// weekly cohorts and the integrity alarm together — a cohort thinned by
/// sessions stuck outside `terminal` passes nothing.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn the_gate_requires_consecutive_weeks_and_a_silent_integrity_alarm()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let repository = SessionLifecycleRepository::new(pool.clone());
    apply_metric_bounds(
        &pool,
        LifecycleMetricBounds {
            gate_weeks: Some(2),
            completion_failure_rate_threshold_ppm: Some(100_000),
            ..LifecycleMetricBounds::default()
        },
    )
    .await?;

    let this_week = owned_session(&pool, 0x1100).await?;
    repository
        .close(
            this_week,
            SessionTerminalOutcome::AchievedVerified,
            LifecycleActor::Operator,
        )
        .await?;

    let one_week = LifecycleMetricsRepository::new(pool.clone()).read().await?;
    assert_eq!(one_week.gate_verdict(), LifecycleGateVerdict::Indeterminate);

    let last_week = owned_session(&pool, 0x1200).await?;
    repository
        .close(
            last_week,
            SessionTerminalOutcome::AchievedVerified,
            LifecycleActor::Operator,
        )
        .await?;
    backdate_closure(&pool, last_week, 1).await?;

    let two_weeks = LifecycleMetricsRepository::new(pool.clone()).read().await?;
    assert_eq!(two_weeks.weeks().len(), 2);
    assert_eq!(two_weeks.gate_verdict(), LifecycleGateVerdict::Met);

    let stuck = owned_session(&pool, 0x1300).await?;
    strand_deadline(&pool, stuck).await?;

    let alarmed = LifecycleMetricsRepository::new(pool.clone()).read().await?;
    assert_eq!(alarmed.nonterminal_past_deadline(), 1);
    assert_eq!(alarmed.gate_verdict(), LifecycleGateVerdict::NotMet);

    pool.close().await;
    drop(container);
    Ok(())
}

/// §12: a breached headline fails the gate even with the integrity alarm
/// silent, and the configured threshold is what decides the breach.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_breached_headline_fails_the_gate_on_its_configured_threshold()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let repository = SessionLifecycleRepository::new(pool.clone());
    apply_metric_bounds(
        &pool,
        LifecycleMetricBounds {
            gate_weeks: Some(1),
            completion_failure_rate_threshold_ppm: Some(100_000),
            ..LifecycleMetricBounds::default()
        },
    )
    .await?;

    let failed = owned_session(&pool, 0x1400).await?;
    repository
        .close(
            failed,
            SessionTerminalOutcome::FailedUnknown,
            LifecycleActor::Core {
                agency: CoreAgency::Daemon,
            },
        )
        .await?;

    let report = LifecycleMetricsRepository::new(pool.clone()).read().await?;
    let week = latest_populated_week(report.weeks());

    assert_eq!(report.nonterminal_past_deadline(), 0);
    assert_eq!(
        week.completion_failure().parts_per_million(),
        Some(1_000_000)
    );
    assert_eq!(week.failed_unknown_share().numerator(), 1);
    assert_eq!(report.gate_verdict(), LifecycleGateVerdict::NotMet);

    pool.close().await;
    drop(container);
    Ok(())
}
