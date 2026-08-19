//! Quiescent active-turn inventory precision and stale-turn terminalization.

use crate::*;

use signalbox_application::{StaleTurnCandidate, StaleTurnOutcome, TurnLivenessEvidence};
use signalbox_persistence::turn_liveness::PostgresTurnLivenessRepository;

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
    let repository = PostgresTurnLivenessRepository::new(pool.clone());

    let quiescent = repository.quiescent_active_turns().await?;
    let candidate = *quiescent
        .first()
        .expect("the activated turn has nothing in flight");
    assert_eq!(quiescent.len(), 1);
    assert_eq!(candidate.session(), fixture.session);
    assert_eq!(candidate.turn(), fixture.turn);
    assert_eq!(candidate.evidence().model_call_count(), 0);
    assert_eq!(candidate.evidence().latest_model_call(), None);

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
    assert_eq!(repository.quiescent_active_turns().await?.len(), 0);

    pool.close().await;
    drop(container);
    Ok(())
}

/// A checkpointed provider call is work in flight, so its turn never reaches
/// the inventory however long it stays outstanding.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn an_outstanding_provider_call_keeps_its_turn_out_of_the_inventory()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let fixture = activated_watchdog_session(&pool, 0x12_000).await?;
    let repository = PostgresTurnLivenessRepository::new(pool.clone());
    assert_eq!(repository.quiescent_active_turns().await?.len(), 1);

    checkpoint_model_call(&pool, &fixture, 0x12_000).await?;

    assert_eq!(repository.quiescent_active_turns().await?.len(), 0);

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
    let repository = PostgresTurnLivenessRepository::new(pool.clone());
    let observed = *repository
        .quiescent_active_turns()
        .await?
        .first()
        .expect("the activated turn has nothing in flight");
    let stale = StaleTurnCandidate::new(
        observed.session(),
        observed.turn(),
        TurnLivenessEvidence::new(
            observed.evidence().current_attempt(),
            observed.evidence().model_call_count() + 1,
            observed.evidence().latest_model_call(),
            observed.evidence().transcript_entry_count(),
            observed.evidence().latest_transcript_entry(),
        ),
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
