//! Quiescent active-turn inventory precision and stale-turn terminalization.

use crate::*;

use signalbox_application::{
    ClassifyOperatorFailure, StaleTurnCandidate, StaleTurnOutcome, TurnLivenessEvidence,
    UuidV7StartupScanIdGenerator,
};
use signalbox_persistence::turn_liveness::{
    PostgresTurnLivenessRepository, TurnLivenessPersistenceBounds,
};

fn terminalization_bounds() -> TurnLivenessPersistenceBounds {
    TurnLivenessPersistenceBounds::new(
        Some(std::time::Duration::from_millis(7)),
        Some(std::time::Duration::from_millis(11)),
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
/// shared failed-turn transition ends it without any new terminal machinery.
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
        )
        .await?;
    assert_eq!(outcome, StaleTurnOutcome::Terminalized);

    let terminal: (String, Option<String>, i64) = sqlx::query_as(
        "SELECT lifecycle.state_kind,
                lifecycle.terminal_disposition_kind,
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
        (String::from("terminal"), Some(String::from("failed")), 1)
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

/// Steering a wedged turn must not hide it. Nothing consumes a steering input
/// without a model call to consume it at a safe point, so a steered turn that
/// is otherwise quiescent stays wedged; it reaches the inventory, and
/// terminalization reports by identity that no present transition can end it.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn pending_steering_leaves_a_wedged_turn_visible_and_unreachable()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let fixture = activated_watchdog_session(&pool, 0x15_000).await?;
    let steering = SubmitInput::new(
        DurableCommandId::from_uuid(Uuid::from_u128(0x15_301)),
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
        )
        .await?;

    assert_eq!(candidate.turn(), fixture.turn);
    assert_eq!(outcome, StaleTurnOutcome::BlockedByPendingSteering);
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
