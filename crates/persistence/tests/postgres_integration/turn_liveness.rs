//! Quiescent active-turn inventory precision and stale-turn terminalization.

use crate::*;

use signalbox_application::{
    ClassifyOperatorFailure, StaleActiveTurnBound, StaleTurnCandidate, StaleTurnOutcome,
    TurnLivenessEvidence, TurnLivenessGuardKind, TurnLivenessLedger, TurnLivenessScanInterval,
    UuidV7StartupScanIdGenerator,
};
use signalbox_persistence::{
    mapping::turn_terminal_cause_to_str,
    turn_liveness::{
        PostgresTurnLivenessRepository, TurnLivenessObservationMode, TurnLivenessPersistenceBounds,
    },
};

/// Starvation allowance for an uncontended pool checkout: generous, not a
/// behavior under test. On a saturated CI node a checkout can take tens of
/// milliseconds; a starved checkout must not preempt the lock refusal these
/// tests assert on.
const POOL_ACQUIRE_ALLOWANCE: std::time::Duration = std::time::Duration::from_secs(5);

fn terminalization_bounds() -> TurnLivenessPersistenceBounds {
    // The lock budgets stay tiny: a lock_timeout only trips while genuinely
    // blocked, so they are insensitive to how loaded the host is.
    TurnLivenessPersistenceBounds::new(
        Some(std::time::Duration::from_millis(7)),
        Some(POOL_ACQUIRE_ALLOWANCE),
        Some(std::time::Duration::from_millis(13)),
    )
}

struct WatchdogFixture {
    session: SessionId,
    turn: TurnId,
    selection: DirectModelSelection,
}

/// Creates a session, accepts one input, and activates it, leaving the exact
/// shape a wedged turn holds: active, running, with a live attempt and no
/// physical operation of any kind outstanding.
async fn activated_watchdog_session(
    pool: &PgPool,
    seed: u128,
) -> Result<WatchdogFixture, Box<dyn Error>> {
    let selection = DirectModelSelection::from_uuid(Uuid::from_u128(seed + 1));
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(prepared(
            seed + 2,
            seed + 3,
            ModelSelectionRequest::Direct(selection),
        ))
        .await?;
    let session = SessionId::from_uuid(Uuid::from_u128(seed + 3));
    let turn = TurnId::from_uuid(Uuid::from_u128(seed + 4));
    SubmitInputRepository::new(pool.clone())
        .handle(
            start_input(
                seed + 5,
                seed + 3,
                "liveness fixture",
                1,
                ModelSelectionOverride::UseSessionDefault,
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(seed + 6)),
            Some(turn),
        )
        .await?;
    activate_earliest_queued_turn(
        pool,
        EarliestQueuedTurnActivation {
            session: session.into_uuid(),
            origin_entry: Uuid::from_u128(seed + 7),
            starting_frontier: Uuid::from_u128(seed + 8),
            initial_attempt: Uuid::from_u128(seed + 9),
        },
    )
    .await?;
    Ok(WatchdogFixture {
        session,
        turn,
        selection,
    })
}

/// A daemon replacement retains the durable ordinal until one interval elapses.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn restart_mid_observation_retains_staleness_evidence() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let fixture = activated_watchdog_session(&pool, 0x10_000).await?;
    let interval = TurnLivenessScanInterval::try_new(std::time::Duration::from_secs(60))?;
    let first_repository =
        PostgresTurnLivenessRepository::new(pool.clone(), terminalization_bounds());
    let candidates = first_repository
        .quiescent_active_turns(None)
        .await?
        .into_candidates();
    let first = first_repository
        .record_complete_observation(
            TurnLivenessGuardKind::Quiescent,
            interval,
            &candidates,
            TurnLivenessObservationMode::RestartBaseline,
        )
        .await?;
    drop(first_repository);
    let restarted_repository =
        PostgresTurnLivenessRepository::new(pool.clone(), terminalization_bounds());

    let second = restarted_repository
        .record_complete_observation(
            TurnLivenessGuardKind::Quiescent,
            interval,
            &candidates,
            TurnLivenessObservationMode::RestartBaseline,
        )
        .await?;
    let third = restarted_repository
        .record_complete_observation(
            TurnLivenessGuardKind::Quiescent,
            interval,
            &candidates,
            TurnLivenessObservationMode::Advance,
        )
        .await?;
    let changed_interval = TurnLivenessScanInterval::try_new(std::time::Duration::from_secs(61))?;
    let changed_cadence = restarted_repository
        .record_complete_observation(
            TurnLivenessGuardKind::Quiescent,
            changed_interval,
            &candidates,
            TurnLivenessObservationMode::Advance,
        )
        .await?;
    let bound = StaleActiveTurnBound::try_new(std::time::Duration::from_secs(60))?;
    let due = TurnLivenessLedger::new(bound, interval).reconcile(&third);

    assert_eq!(first[0].ordinal().get(), 1);
    assert_eq!(second[0].ordinal().get(), 1);
    assert_eq!(third[0].ordinal().get(), 2);
    assert_eq!(changed_cadence[0].ordinal().get(), 1);
    assert_eq!(due.as_ref(), &[third[0].candidate()]);
    assert_eq!(third[0].candidate().turn(), fixture.turn);

    pool.close().await;
    drop(container);
    Ok(())
}

/// A durable observation ordinal uses the persistence contract's whole `u64`
/// range instead of stopping at PostgreSQL's signed-bigint ceiling.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn observation_ordinal_advances_through_the_u64_range() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let _fixture = activated_watchdog_session(&pool, 0x10_100).await?;
    let interval = TurnLivenessScanInterval::try_new(std::time::Duration::from_secs(60))?;
    let repository = PostgresTurnLivenessRepository::new(pool.clone(), terminalization_bounds());
    let candidates = repository
        .quiescent_active_turns(None)
        .await?
        .into_candidates();
    repository
        .record_complete_observation(
            TurnLivenessGuardKind::Quiescent,
            interval,
            &candidates,
            TurnLivenessObservationMode::RestartBaseline,
        )
        .await?;
    sqlx::query(
        "UPDATE turn_liveness_observation
            SET observation_ordinal = $1
          WHERE guard_kind = $2",
    )
    .bind(Decimal::from(u64::MAX - 1))
    .bind(TurnLivenessGuardKind::Quiescent.as_str())
    .execute(&pool)
    .await?;

    let advanced = repository
        .record_complete_observation(
            TurnLivenessGuardKind::Quiescent,
            interval,
            &candidates,
            TurnLivenessObservationMode::Advance,
        )
        .await?;
    let saturated = repository
        .record_complete_observation(
            TurnLivenessGuardKind::Quiescent,
            interval,
            &candidates,
            TurnLivenessObservationMode::Advance,
        )
        .await?;

    assert_eq!(advanced[0].ordinal().get(), u64::MAX);
    assert_eq!(saturated[0].ordinal().get(), u64::MAX);

    pool.close().await;
    drop(container);
    Ok(())
}

/// An observation that cannot be decoded into its durable domain shape is
/// fail-closed corruption rather than a transient infrastructure failure.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn malformed_observation_is_classified_as_corruption() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let _fixture = activated_watchdog_session(&pool, 0x10_200).await?;
    let interval = TurnLivenessScanInterval::try_new(std::time::Duration::from_secs(60))?;
    let repository = PostgresTurnLivenessRepository::new(pool.clone(), terminalization_bounds());
    let candidates = repository
        .quiescent_active_turns(None)
        .await?
        .into_candidates();
    repository
        .record_complete_observation(
            TurnLivenessGuardKind::Quiescent,
            interval,
            &candidates,
            TurnLivenessObservationMode::RestartBaseline,
        )
        .await?;
    sqlx::query(
        "ALTER TABLE turn_liveness_observation
         DROP CONSTRAINT turn_liveness_observation_ordinal",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE turn_liveness_observation
            SET observation_ordinal = $1
          WHERE guard_kind = $2",
    )
    .bind(Decimal::from(u64::MAX) + Decimal::ONE)
    .bind(TurnLivenessGuardKind::Quiescent.as_str())
    .execute(&pool)
    .await?;

    let error = repository
        .record_complete_observation(
            TurnLivenessGuardKind::Quiescent,
            interval,
            &candidates,
            TurnLivenessObservationMode::RestartBaseline,
        )
        .await
        .expect_err("the malformed stored ordinal fails closed");

    assert_eq!(
        error.operator_failure_class(),
        OperatorFailureClass::FailClosedCorruption
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// Disabling either stale-turn supervision input breaks observation continuity
/// for both guards, so a later deployment starts both ledgers from their first
/// observation rather than inheriting credit earned before the disabled run.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn disabled_supervision_clears_guard_observation_history() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let _fixture = activated_watchdog_session(&pool, 0x11_000).await?;
    let interval = TurnLivenessScanInterval::try_new(std::time::Duration::from_secs(60))?;
    let repository = PostgresTurnLivenessRepository::new(pool.clone(), terminalization_bounds());
    let candidates = repository
        .quiescent_active_turns(None)
        .await?
        .into_candidates();
    let first_quiescent = repository
        .record_complete_observation(
            TurnLivenessGuardKind::Quiescent,
            interval,
            &candidates,
            TurnLivenessObservationMode::RestartBaseline,
        )
        .await?;
    let first_slot_held = repository
        .record_complete_observation(
            TurnLivenessGuardKind::SlotHeld,
            interval,
            &candidates,
            TurnLivenessObservationMode::RestartBaseline,
        )
        .await?;
    let advanced_quiescent = repository
        .record_complete_observation(
            TurnLivenessGuardKind::Quiescent,
            interval,
            &candidates,
            TurnLivenessObservationMode::Advance,
        )
        .await?;
    let advanced_slot_held = repository
        .record_complete_observation(
            TurnLivenessGuardKind::SlotHeld,
            interval,
            &candidates,
            TurnLivenessObservationMode::Advance,
        )
        .await?;

    repository.clear_guard_observations().await?;

    let reset_quiescent = repository
        .record_complete_observation(
            TurnLivenessGuardKind::Quiescent,
            interval,
            &candidates,
            TurnLivenessObservationMode::RestartBaseline,
        )
        .await?;
    let reset_slot_held = repository
        .record_complete_observation(
            TurnLivenessGuardKind::SlotHeld,
            interval,
            &candidates,
            TurnLivenessObservationMode::RestartBaseline,
        )
        .await?;
    assert_ne!(
        advanced_quiescent[0].ordinal(),
        first_quiescent[0].ordinal()
    );
    assert_ne!(
        advanced_slot_held[0].ordinal(),
        first_slot_held[0].ordinal()
    );
    assert_eq!(reset_quiescent[0].ordinal(), first_quiescent[0].ordinal());
    assert_eq!(reset_slot_held[0].ordinal(), first_slot_held[0].ordinal());

    pool.close().await;
    drop(container);
    Ok(())
}

/// Leaves the session's active turn holding a checkpointed, not yet observed
/// provider call: the exact shape a legitimately long provider interaction has.
async fn checkpoint_model_call(
    pool: &PgPool,
    fixture: &WatchdogFixture,
    seed: u128,
) -> Result<ModelCallId, Box<dyn Error>> {
    let targets = ModelTargetCatalog::try_from_definitions([ModelTargetDefinition::new(
        fixture.selection,
        ResolvedProviderTarget::naming(ProviderModelIdentity::from_uuid(Uuid::from_u128(
            seed + 20,
        ))),
    )])
    .expect("one exact selection forms a target catalog");
    let repository =
        PostgresModelCallRepository::new(pool.clone(), targets, model_credential_reference());
    let call = ModelCallId::from_uuid(Uuid::from_u128(seed + 21));
    let mut service = ModelCallExecutionService::new(
        FixedModelCallExecutionIds::new(
            [
                call,
                ModelCallId::from_uuid(Uuid::from_u128(seed + 22)),
                ModelCallId::from_uuid(Uuid::from_u128(seed + 23)),
            ],
            [
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 24)),
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 25)),
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 26)),
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 27)),
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 28)),
            ],
            [
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 29)),
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 30)),
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 31)),
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 32)),
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 33)),
            ],
            [
                TurnId::from_uuid(Uuid::from_u128(seed + 34)),
                TurnId::from_uuid(Uuid::from_u128(seed + 35)),
            ],
            [ToolRequestId::from_uuid(Uuid::from_u128(seed + 36))],
            [TurnAttemptId::from_uuid(Uuid::from_u128(seed + 37))],
        ),
        repository.clone(),
        repository.clone(),
        repository.clone(),
        repository,
        ScriptedModelCallProvider::new([ScriptedModelCallStep::Return(
            ModelCallTerminalObservation::Completed {
                assistant_text: vec![
                    AssistantText::try_new(String::from("unobserved"))
                        .expect("fixture assistant text is valid"),
                ],
            },
        )]),
        InProcessAttemptDispatchGate::default(),
        None,
    );
    assert_eq!(
        service.execute(fixture.session).await?,
        ModelCallExecutionOutcome::Checkpointed(call)
    );
    Ok(call)
}

/// An active turn with no operation outstanding reaches the inventory, and the
/// shared failed-turn transition ends it without any new terminal machinery,
/// recording the watchdog's own cause rather than the startup scan's.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_quiescent_active_turn_terminalizes_as_failed() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let fixture = activated_watchdog_session(&pool, 0x11_000).await?;
    let repository = PostgresTurnLivenessRepository::new(pool.clone(), terminalization_bounds());

    let page = repository.quiescent_active_turns(None).await?;
    let candidate = *page
        .candidates()
        .first()
        .expect("the activated turn has nothing in flight");
    assert_eq!(page.candidates().len(), 1);
    assert_eq!(
        page.resume_after(),
        None,
        "one candidate does not fill a page, so the rotation ends here"
    );
    assert_eq!(candidate.session(), fixture.session);
    assert_eq!(candidate.turn(), fixture.turn);

    let outcome = repository
        .terminalize_stale_turn(
            candidate,
            AcceptedInputTurnFailureIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0x11_100)),
                ContextFrontierId::from_uuid(Uuid::from_u128(0x11_101)),
            ),
            &mut UuidV7StartupScanIdGenerator,
        )
        .await?;
    assert_eq!(outcome, StaleTurnOutcome::Terminalized);

    let terminal: (String, Option<String>, Option<String>, i64) = sqlx::query_as(
        "SELECT lifecycle.state_kind,
                lifecycle.terminal_disposition_kind,
                lifecycle.terminal_cause_kind,
                (SELECT count(*)
                   FROM semantic_transcript_entry AS entry
                  WHERE entry.failed_turn_id = lifecycle.turn_id)
           FROM turn_lifecycle AS lifecycle
          WHERE lifecycle.turn_id = $1",
    )
    .bind(fixture.turn.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        terminal,
        (
            String::from("terminal"),
            Some(String::from("failed")),
            Some(String::from(turn_terminal_cause_to_str(
                TurnTerminalCause::WatchdogStaleTurn
            ))),
            1
        )
    );
    assert_eq!(
        repository.quiescent_active_turns(None).await?.candidates(),
        []
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// A checkpointed provider call is excluded from the quiescent inventory but
/// included in the outer slot-held inventory under the same progress evidence.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn an_outstanding_provider_call_moves_from_quiescent_to_slot_held_inventory()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let fixture = activated_watchdog_session(&pool, 0x12_000).await?;
    let repository = PostgresTurnLivenessRepository::new(pool.clone(), terminalization_bounds());
    assert_eq!(
        repository
            .quiescent_active_turns(None)
            .await?
            .candidates()
            .len(),
        1
    );

    checkpoint_model_call(&pool, &fixture, 0x12_000).await?;

    assert_eq!(
        repository.quiescent_active_turns(None).await?.candidates(),
        []
    );
    let slot_held = repository.slot_held_active_turns(None).await?;
    assert_eq!(slot_held.candidates().len(), 1);
    assert_eq!(slot_held.candidates()[0].session(), fixture.session);
    assert_eq!(slot_held.candidates()[0].turn(), fixture.turn);

    pool.close().await;
    drop(container);
    Ok(())
}

/// S10: slot-held recovery revalidates the exact turn-progress evidence under
/// the scheduler lock and declines evidence that changed after observation.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s10_slot_held_recovery_declines_changed_progress_evidence() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let fixture = activated_watchdog_session(&pool, 0x12_500).await?;
    checkpoint_model_call(&pool, &fixture, 0x12_500).await?;
    let repository = PostgresTurnLivenessRepository::new(pool.clone(), terminalization_bounds());
    let observed = repository
        .observed_slot_held_turn(fixture.session)
        .await?
        .expect("the checkpointed provider call holds the session slot");
    let stale = StaleTurnCandidate::new(
        observed.session(),
        observed.turn(),
        TurnLivenessEvidence::new(observed.evidence().current_attempt(), Some(u64::MAX)),
    );
    let mut ids = UuidV7StartupScanIdGenerator;

    let outcome = repository
        .recover_observed_slot_held_turn(
            stale,
            AcceptedInputTurnFailureIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0x12_600)),
                ContextFrontierId::from_uuid(Uuid::from_u128(0x12_601)),
            ),
            &mut ids,
        )
        .await?;
    let state: String =
        sqlx::query_scalar("SELECT state_kind FROM turn_lifecycle WHERE turn_id = $1")
            .bind(fixture.turn.into_uuid())
            .fetch_one(&pool)
            .await?;

    assert_eq!(outcome, None);
    assert_eq!(state, "active");

    pool.close().await;
    drop(container);
    Ok(())
}

/// S10: a lock refusal raised by the shared startup transition remains the
/// typed contention outcome that the detached scheduler recovery can retry.
///
/// This repository classifies no lock site of its own on this path — it sets
/// one acquisition bound and hands the observation straight to the shared
/// transition — so every refusal here reaches the caller through the
/// `StartupScanRepositoryError` conversion, and the contended row is the
/// session scheduler row that transition takes under that bound.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s10_slot_held_recovery_preserves_shared_transition_lock_refusal()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let fixture = activated_watchdog_session(&pool, 0x12_700).await?;
    checkpoint_model_call(&pool, &fixture, 0x12_700).await?;
    let repository = PostgresTurnLivenessRepository::new(pool.clone(), terminalization_bounds());
    let observed = repository
        .observed_slot_held_turn(fixture.session)
        .await?
        .expect("the checkpointed provider call holds the session slot");
    let mut blocker = pool.begin().await?;
    sqlx::query("SELECT session_id FROM session_scheduler WHERE session_id = $1 FOR UPDATE")
        .bind(fixture.session.into_uuid())
        .execute(&mut *blocker)
        .await?;
    let mut ids = UuidV7StartupScanIdGenerator;

    let error = repository
        .recover_observed_slot_held_turn(
            observed,
            AcceptedInputTurnFailureIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0x12_800)),
                ContextFrontierId::from_uuid(Uuid::from_u128(0x12_801)),
            ),
            &mut ids,
        )
        .await
        .expect_err("the scheduler row remains locked past the recovery budget");

    assert_eq!(
        error.operator_failure_cause_code(),
        "turn_liveness_terminalization_lock_unavailable"
    );

    // The refusal decided nothing, so the same observation is still the one to
    // retry: releasing the holder lets the identical call through to the shared
    // transition's own classification of this durable shape.
    blocker.rollback().await?;
    assert_eq!(
        repository
            .recover_observed_slot_held_turn(
                observed,
                AcceptedInputTurnFailureIdentities::new(
                    SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0x12_802)),
                    ContextFrontierId::from_uuid(Uuid::from_u128(0x12_803)),
                ),
                &mut ids,
            )
            .await?,
        Some(StartupScanSessionOutcome::ResumablePreparedModelCall { turn: fixture.turn })
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// Terminalization revalidates the whole observation under the session locks,
/// so evidence that moved between the scan and the decision changes nothing.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_changed_observation_is_superseded() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let fixture = activated_watchdog_session(&pool, 0x13_000).await?;
    let repository = PostgresTurnLivenessRepository::new(pool.clone(), terminalization_bounds());
    let page = repository.quiescent_active_turns(None).await?;
    let observed = *page
        .candidates()
        .first()
        .expect("the activated turn has nothing in flight");
    let stale = StaleTurnCandidate::new(
        observed.session(),
        observed.turn(),
        TurnLivenessEvidence::new(observed.evidence().current_attempt(), Some(u64::MAX)),
    );

    let outcome = repository
        .terminalize_stale_turn(
            stale,
            AcceptedInputTurnFailureIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0x13_100)),
                ContextFrontierId::from_uuid(Uuid::from_u128(0x13_101)),
            ),
            &mut UuidV7StartupScanIdGenerator,
        )
        .await?;
    assert_eq!(outcome, StaleTurnOutcome::Superseded);

    let state: (String,) =
        sqlx::query_as("SELECT state_kind FROM turn_lifecycle WHERE turn_id = $1")
            .bind(fixture.turn.into_uuid())
            .fetch_one(&pool)
            .await?;
    assert_eq!(state, (String::from("active"),));

    pool.close().await;
    drop(container);
    Ok(())
}

/// The outbox frontier is what proves a session progressed, so a session whose
/// earlier turn already completed must report one: every durable transition it
/// made appended an outbox event.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn the_outbox_frontier_resolves_over_a_worked_session() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let fixture = activated_watchdog_session(&pool, 0x14_000).await?;
    let targets = ModelTargetCatalog::try_from_definitions([ModelTargetDefinition::new(
        fixture.selection,
        ResolvedProviderTarget::naming(ProviderModelIdentity::from_uuid(Uuid::from_u128(0x14_020))),
    )])
    .expect("one exact selection forms a target catalog");
    complete_text_turn(
        &pool,
        fixture.session,
        targets,
        model_credential_reference(),
        0x14_100,
        "first reply",
    )
    .await?;
    let successor = TurnId::from_uuid(Uuid::from_u128(0x14_201));
    SubmitInputRepository::new(pool.clone())
        .handle(
            start_input(
                0x14_202,
                0x14_003,
                "second input",
                1,
                ModelSelectionOverride::UseSessionDefault,
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(0x14_203)),
            Some(successor),
        )
        .await?;
    activate_earliest_queued_turn(
        &pool,
        EarliestQueuedTurnActivation {
            session: fixture.session.into_uuid(),
            origin_entry: Uuid::from_u128(0x14_204),
            starting_frontier: Uuid::from_u128(0x14_205),
            initial_attempt: Uuid::from_u128(0x14_206),
        },
    )
    .await?;

    let page = repository_page(&pool).await?;
    let candidate = *page
        .first()
        .expect("the successor turn has nothing in flight");

    assert_eq!(candidate.turn(), successor);
    assert!(
        candidate.evidence().outbox_frontier().is_some(),
        "the completed first turn left outbox events behind for the frontier to resolve"
    );

    pool.close().await;
    drop(container);
    Ok(())
}

async fn repository_page(pool: &PgPool) -> Result<Box<[StaleTurnCandidate]>, Box<dyn Error>> {
    let page = PostgresTurnLivenessRepository::new(pool.clone(), terminalization_bounds())
        .quiescent_active_turns(None)
        .await?;
    Ok(page.candidates().to_vec().into_boxed_slice())
}

/// Steering a wedged turn must not hide it, and it must not keep the turn
/// wedged: the watchdog's failed-turn transition reclassifies the pending
/// steering into a queued successor, which settles the injection `delivered`.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn pending_steering_is_reclassified_when_the_watchdog_ends_its_turn()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let fixture = activated_watchdog_session(&pool, 0x15_000).await?;
    let steering_command = DurableCommandId::from_uuid(Uuid::from_u128(0x15_301));
    let steering = SubmitInput::new(
        steering_command,
        fixture.session,
        UserContent::try_text(String::from("steer the wedged turn"))
            .expect("fixture steering content is admitted"),
        DeliveryRequest::NextSafePoint {
            expected_active_turn: fixture.turn,
        },
    );
    let recorded = SubmitInputRepository::new(pool.clone())
        .handle(
            steering,
            AcceptedInputId::from_uuid(Uuid::from_u128(0x15_302)),
            None,
        )
        .await?;
    assert!(
        matches!(
            &recorded,
            SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Applied(
                SubmitInputAppliedResult::PendingSteering(_)
            ))
        ),
        "steering a turn holding the slot is accepted as pending: {recorded:?}"
    );
    let repository = PostgresTurnLivenessRepository::new(pool.clone(), terminalization_bounds());

    let page = repository.quiescent_active_turns(None).await?;
    let candidate = *page
        .candidates()
        .first()
        .expect("pending steering is not work in flight, so the turn stays a candidate");
    let outcome = repository
        .terminalize_stale_turn(
            candidate,
            AcceptedInputTurnFailureIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0x15_400)),
                ContextFrontierId::from_uuid(Uuid::from_u128(0x15_401)),
            ),
            &mut UuidV7StartupScanIdGenerator,
        )
        .await?;

    assert_eq!(candidate.turn(), fixture.turn);
    assert_eq!(outcome, StaleTurnOutcome::Terminalized);
    let state: (String,) =
        sqlx::query_as("SELECT state_kind FROM turn_lifecycle WHERE turn_id = $1")
            .bind(fixture.turn.into_uuid())
            .fetch_one(&pool)
            .await?;
    assert_eq!(state, (String::from("terminal"),));
    let (disposition, successor, successor_state): (String, Option<Uuid>, Option<String>) =
        sqlx::query_as(
            "SELECT accepted.disposition_kind, accepted.origin_turn_id, successor.state_kind
               FROM accepted_input AS accepted
               LEFT JOIN turn_lifecycle AS successor
                 ON successor.turn_id = accepted.origin_turn_id
              WHERE accepted.accepting_command_id = $1",
        )
        .bind(steering_command.into_uuid())
        .fetch_one(&pool)
        .await?;
    assert_eq!(disposition, "reclassified_as_turn_origin");
    assert_eq!(successor_state.as_deref(), Some("queued"));
    let receipt: (String, Option<Uuid>) = sqlx::query_as(
        "SELECT outcome_kind, delivered_turn_id
           FROM injection_settled_outbox_event
          WHERE command_id = $1",
    )
    .bind(steering_command.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(receipt, (String::from("delivered"), successor));

    pool.close().await;
    drop(container);
    Ok(())
}

/// The narrower bound this recovery installs before the session locks, in the
/// shape the acquisition budget takes.
const RECOVERY_LOCK_WAIT: std::time::Duration = std::time::Duration::from_millis(50);

/// The wider bound the write phase is owed once the scheduler row is held.
const RECOVERY_WRITE_LOCK_WAIT: std::time::Duration = std::time::Duration::from_millis(1_500);

/// S10: slot-held recovery switches to its write budget once the scheduler row
/// is held, so the outbox sequence row every writer holds until it commits is
/// ordinary contention rather than a stall refused at the acquisition budget.
///
/// Without the switch every statement after the scheduler row runs at the
/// narrower acquisition bound, and a busy daemon's ordinary outbox traffic
/// refuses all four detached attempts — leaving the session occupied until the
/// thirty-minute watchdog.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s10_slot_held_recovery_spends_its_write_budget_on_the_outbox_row()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let issued = checkpoint_restart_model_call(&pool, 0x15_900, true).await?;
    let repository = PostgresTurnLivenessRepository::new(
        pool.clone(),
        TurnLivenessPersistenceBounds::new(
            Some(RECOVERY_LOCK_WAIT),
            Some(std::time::Duration::from_millis(250)),
            Some(RECOVERY_WRITE_LOCK_WAIT),
        ),
    );
    let observed = repository
        .observed_slot_held_turn(issued.session)
        .await?
        .expect("the issued provider call holds the session slot");
    let mut allocator_holder = pool.begin().await?;
    sqlx::query("SELECT singleton FROM outbox_sequence_state WHERE singleton FOR UPDATE")
        .execute(&mut *allocator_holder)
        .await?;
    let mut ids = UuidV7StartupScanIdGenerator;

    let started = std::time::Instant::now();
    let error = repository
        .recover_observed_slot_held_turn(
            observed,
            AcceptedInputTurnFailureIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0x15_910)),
                ContextFrontierId::from_uuid(Uuid::from_u128(0x15_911)),
            ),
            &mut ids,
        )
        .await
        .expect_err("the held outbox sequence row outlasts even the write budget");
    let waited = started.elapsed();
    allocator_holder.rollback().await?;

    assert_eq!(observed.turn(), issued.turn);
    assert_eq!(
        error.operator_failure_cause_code(),
        "turn_liveness_terminalization_lock_unavailable"
    );
    assert!(
        waited >= RECOVERY_WRITE_LOCK_WAIT,
        "the write budget is what expired, so {waited:?} spans it"
    );

    pool.close().await;
    drop(container);
    Ok(())
}

struct AbandonedCompactionFixture {
    session: SessionId,
    call: ModelCallId,
    command: DurableCommandId,
}

/// Leaves the exact durable shape a pre-activation compaction abandons: its
/// dedicated call authorized but never observed, and its command still pending,
/// together holding the session's compaction boundary against every queued turn.
async fn abandoned_pre_activation_compaction(
    pool: &PgPool,
    seed: u128,
) -> Result<AbandonedCompactionFixture, Box<dyn Error>> {
    let (fixture, repository, authorized) = authorize_checkpointed_model_call(pool, seed).await?;
    let assistant = AssistantText::try_new(String::from("context before compaction"))
        .expect("fixture assistant text is admitted");
    let observation = authorized
        .observation_correlation()
        .bind_terminal_observation(ModelCallTerminalObservation::Completed {
            assistant_text: vec![assistant],
        });
    repository
        .apply_terminal_observation(
            fixture.session,
            observation,
            ModelCallTerminalIdentities::Completed(CompletedModelCallIdentities::new(
                vec![SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(
                    seed + 0x20,
                ))],
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 0x21)),
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 0x22)),
            )),
            |_| panic!("the fixture has no pending steering to reclassify"),
        )
        .await?;

    let command = DurableCommandId::from_uuid(Uuid::from_u128(seed + 0x30));
    let call = ModelCallId::from_uuid(Uuid::from_u128(seed + 0x31));
    let compaction_repository = ContextCompactionRepository::new(pool.clone());
    let prepared = compaction_repository
        .prepare(PrepareContextCompactionRequest {
            command,
            session: fixture.session,
            requested_through_position: Some(1),
            automatic_for_turn: None,
            defaults_version: SessionConfigurationDefaultsVersion::first(),
            selection: DirectModelSelection::from_uuid(Uuid::from_u128(seed + 5)),
            target: ResolvedProviderTarget::naming(ProviderModelIdentity::from_uuid(
                Uuid::from_u128(seed + 6),
            )),
            input_includes_cache_tokens: true,
            credential_reference: String::from("abandoned compaction fixture credential"),
            call,
            compaction: ContextCompactionId::from_uuid(Uuid::from_u128(seed + 0x32)),
            summary_entry: SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 0x33)),
            result_frontier: ContextFrontierId::from_uuid(Uuid::from_u128(seed + 0x34)),
        })
        .await?;
    let PrepareContextCompactionOutcome::Prepared(_) = prepared else {
        panic!("the completed turn has a compactable frontier")
    };

    Ok(AbandonedCompactionFixture {
        session: fixture.session,
        call,
        command,
    })
}

/// S11: the expiry handoff's compaction recovery acts on the abandoned
/// compaction itself, so a session that holds none is left exactly as found.
///
/// The handoff runs detached, its admission slot is released the moment the
/// pass expires, and it sleeps between attempts, so a later eligibility sweep
/// can activate a healthy successor turn before it runs. Recovery that falls
/// through to whichever turn is active now would terminalize that successor.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s11_compaction_recovery_spares_a_session_holding_no_abandoned_compaction()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let successor = checkpoint_restart_model_call(&pool, 0x15_a00, true).await?;
    let repository = PostgresTurnLivenessRepository::new(pool.clone(), terminalization_bounds());

    let recovered = repository
        .recover_abandoned_compaction(
            successor.session,
            ModelCallId::from_uuid(Uuid::from_u128(0x15_a20)),
        )
        .await?;
    let state: (String,) = sqlx::query_as(
        "SELECT state_kind FROM turn_lifecycle WHERE session_id = $1 AND turn_id = $2",
    )
    .bind(successor.session.into_uuid())
    .bind(successor.turn.into_uuid())
    .fetch_one(&pool)
    .await?;

    assert!(
        recovered.is_none(),
        "no compaction holds the boundary, so {recovered:?} must report nothing to recover"
    );
    assert_eq!(state, (String::from("active"),));

    pool.close().await;
    drop(container);
    Ok(())
}

/// S12: an authorized compaction whose pass expired before it finished is the
/// evidence the handoff acts on, so recovery terminalizes it and frees the
/// session boundary that was holding every queued turn out.
///
/// This is the wedge itself: the compaction call and its pending command own
/// the boundary, and nothing else terminalizes them before a daemon restart.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s12_compaction_recovery_terminalizes_the_boundary_its_expired_pass_abandoned()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let abandoned = abandoned_pre_activation_compaction(&pool, 0x15_b00).await?;
    let repository = PostgresTurnLivenessRepository::new(pool.clone(), terminalization_bounds());

    let recovered = repository
        .recover_abandoned_compaction(abandoned.session, abandoned.call)
        .await?;
    let call: (String, Option<String>) = sqlx::query_as(
        "SELECT state_kind, terminal_disposition_kind
           FROM context_compaction_model_call
          WHERE session_id = $1 AND model_call_id = $2",
    )
    .bind(abandoned.session.into_uuid())
    .bind(abandoned.call.into_uuid())
    .fetch_one(&pool)
    .await?;
    let command: (String,) = sqlx::query_as(
        "SELECT result_kind FROM compact_session_command WHERE session_id = $1 AND command_id = $2",
    )
    .bind(abandoned.session.into_uuid())
    .bind(abandoned.command.into_uuid())
    .fetch_one(&pool)
    .await?;

    assert!(
        matches!(
            recovered,
            Some(StartupScanSessionOutcome::RecoveredContextCompaction { .. })
        ),
        "the abandoned compaction is the evidence recovery acts on, not {recovered:?}"
    );
    assert_eq!(
        call,
        (String::from("terminal"), Some(String::from("known_failed")))
    );
    assert_eq!(command, (String::from("failed"),));

    pool.close().await;
    drop(container);
    Ok(())
}

/// S13: compaction recovery installs its lock budget server-side, so a
/// contended scheduler row is refused inside the budget instead of stranding
/// the wait on a checked-out pooled connection.
///
/// The handoff drives this detached under a wall-clock deadline, and a deadline
/// cannot cancel a statement already waiting in the backend: abandoning the
/// future would leave that wait running, and repeated attempts across several
/// simultaneous expiries would exhaust the pool.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s13_compaction_recovery_refuses_a_contended_scheduler_row_inside_its_budget()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let abandoned = abandoned_pre_activation_compaction(&pool, 0x15_c00).await?;
    let repository = PostgresTurnLivenessRepository::new(
        pool.clone(),
        TurnLivenessPersistenceBounds::new(
            Some(RECOVERY_LOCK_WAIT),
            Some(RECOVERY_LOCK_WAIT),
            Some(RECOVERY_WRITE_LOCK_WAIT),
        ),
    );
    let mut scheduler_holder = pool.begin().await?;
    sqlx::query("SELECT session_id FROM session_scheduler WHERE session_id = $1 FOR UPDATE")
        .bind(abandoned.session.into_uuid())
        .execute(&mut *scheduler_holder)
        .await?;

    let error = repository
        .recover_abandoned_compaction(abandoned.session, abandoned.call)
        .await
        .expect_err("the held scheduler row outlasts the budget before that row is reached");
    scheduler_holder.rollback().await?;

    assert_eq!(
        error.operator_failure_cause_code(),
        "turn_liveness_terminalization_lock_unavailable"
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// S14: compaction recovery names the exact call its expired window made
/// durable, so a later pass's live compaction is not the one it terminalizes.
///
/// Expiry inside the read-only preflight leaves no durable call at all, and the
/// handoff waits between attempts, so by the time it reaches the database a
/// later admitted pass can be running a different compaction for the same
/// session. Selecting on the session alone would terminalize that one.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s14_compaction_recovery_spares_a_compaction_its_window_never_prepared()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let successor = abandoned_pre_activation_compaction(&pool, 0x15_d00).await?;
    let repository = PostgresTurnLivenessRepository::new(pool.clone(), terminalization_bounds());

    let recovered = repository
        .recover_abandoned_compaction(
            successor.session,
            ModelCallId::from_uuid(Uuid::from_u128(0x15_d99)),
        )
        .await?;
    let call: (String, Option<String>) = sqlx::query_as(
        "SELECT state_kind, terminal_disposition_kind
           FROM context_compaction_model_call
          WHERE session_id = $1 AND model_call_id = $2",
    )
    .bind(successor.session.into_uuid())
    .bind(successor.call.into_uuid())
    .fetch_one(&pool)
    .await?;

    assert!(
        recovered.is_none(),
        "this compaction belongs to a later pass, so {recovered:?} must report nothing to recover"
    );
    assert_eq!(call, (String::from("prepared"), None));

    pool.close().await;
    drop(container);
    Ok(())
}
