//! Reconciliation coverage.

use super::*;

#[derive(Debug, Default)]
pub(crate) struct ReconciliationCycle {
    pub(crate) hinted_sessions: HashSet<SessionId>,
    pub(crate) processed_sessions: HashSet<SessionId>,
    pub(crate) final_batch_seen: bool,
}

impl ReconciliationWitness {
    pub(crate) fn new() -> Self {
        Self {
            completed_cycles: Arc::new(AtomicUsize::new(0)),
            cycle: Arc::new(Mutex::new(ReconciliationCycle::default())),
        }
    }

    pub(crate) fn record_batch(&self, sessions: &[SessionId], continuation: bool) {
        let mut cycle = self
            .cycle
            .lock()
            .expect("the reconciliation witness lock is available");
        cycle.hinted_sessions.extend(sessions.iter().copied());
        cycle.final_batch_seen = !continuation;
        self.complete_drained_cycle(&mut cycle);
    }

    pub(crate) fn record_processed_session(&self, session: SessionId) {
        let mut cycle = self
            .cycle
            .lock()
            .expect("the reconciliation witness lock is available");
        cycle.processed_sessions.insert(session);
        self.complete_drained_cycle(&mut cycle);
    }

    pub(crate) fn complete_drained_cycle(&self, cycle: &mut ReconciliationCycle) {
        if cycle.final_batch_seen && cycle.hinted_sessions.is_subset(&cycle.processed_sessions) {
            self.completed_cycles.fetch_add(1, Ordering::SeqCst);
            *cycle = ReconciliationCycle::default();
        }
    }

    pub(crate) fn completed_cycles(&self) -> usize {
        self.completed_cycles.load(Ordering::SeqCst)
    }
}

#[test]
fn reconciliation_witness_waits_for_final_batch_hints_to_finish() {
    let witness = ReconciliationWitness::new();
    let session = SessionId::from_uuid(Uuid::from_u128(1));

    witness.record_batch(&[session], false);
    assert_eq!(witness.completed_cycles(), 0);

    witness.record_processed_session(session);
    assert_eq!(witness.completed_cycles(), 1);
}

#[test]
fn reconciliation_witness_completes_an_empty_cycle_immediately() {
    let witness = ReconciliationWitness::new();

    witness.record_batch(&[], false);

    assert_eq!(witness.completed_cycles(), 1);
}

/// Parks the session's active turn on an ambiguous model call exactly as a
/// prior daemon incarnation would: the queued turn activates, its call is
/// authorized for send, and the next startup scan classifies the unobserved
/// issued call. The fixture writes no terminal state itself, so the parked
/// shape is the one a real restart leaves behind.
pub(crate) async fn park_turn_on_ambiguous_model_call(
    pool: &PgPool,
    session_id: CanonicalUuid,
) -> Result<(), Box<dyn Error>> {
    let session = SessionId::from_uuid(session_id.into_uuid());
    activate_turn(pool, session).await?;

    let model_configuration = support::parse_model_configuration(MODEL_CONFIGURATION)?;
    let calls = PostgresModelCallRepository::new(
        pool.clone(),
        model_configuration.target_catalog(),
        ModelCallCredentialReference::new("scripted-reconciliation-test"),
    );
    let call = ModelCallId::from_uuid(Uuid::now_v7());
    let PrepareInitialModelCallOutcome::Checkpointed(_) = calls
        .prepare_initial_call(
            session,
            call,
            FailedModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::now_v7()),
                ContextFrontierId::from_uuid(Uuid::now_v7()),
            ),
            ContextFrontierId::from_uuid(Uuid::now_v7()),
            |_| {
                (
                    SemanticTranscriptEntryId::from_uuid(Uuid::now_v7()),
                    TurnId::from_uuid(Uuid::now_v7()),
                )
            },
        )
        .await?
    else {
        return Err(io::Error::other("the fixture call must checkpoint").into());
    };
    let AuthorizeModelCallOutcome::Authorized(_) = calls.authorize_send(session, call).await?
    else {
        return Err(io::Error::other("the fixture call must authorize send").into());
    };

    let mut scan = StartupScanService::new(
        UuidV7StartupScanIdGenerator,
        PostgresStartupScanRepository::new(pool.clone()),
    );
    let recovery = scan.execute().await?;
    assert_eq!(
        recovery.recovered_turn_count(),
        0,
        "an unobserved issued call parks its turn instead of terminalizing it"
    );
    Ok(())
}

/// a turn parked on an ambiguous model call refuses ordinary input until the user reconciliation
/// decision releases the slot.
///
/// The refusal and the release are one contract: proving the release means
/// nothing unless the same session is demonstrably wedged first, against the
/// same durable state in the same execution (testing-style rule 17).
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn reconcile_turn_releases_a_wedged_ambiguous_session() -> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let session_id = create_alias_session(&mut connection).await?;
    let (_, parked_turn_id) =
        submit_first_input(&mut connection, session_id, String::from("first request")).await?;
    park_turn_on_ambiguous_model_call(&runtime.pool, session_id).await?;

    connection
        .request_version(
            ProtocolVersion::One,
            3,
            ClientRequest::SubmitInput {
                command_id: command()?,
                session_id,
                content: UserInputContent::text(String::from(
                    "work while the ambiguity is unresolved",
                )),
                expected_defaults_version: Some(CanonicalU64::new(1)),
                model_settings: ModelSettingsOverlay::inherit_all(),
                delivery: None,
            },
        )
        .await?;
    assert_eq!(
        rejected_detail(response_within(&mut connection).await?.message()),
        RejectionDetail::ActiveTurnPresent {
            session_id,
            active_turn_id: parked_turn_id,
        },
        "an ambiguity wait must keep refusing ordinary input while it holds the slot"
    );

    connection
        .request_version(
            ProtocolVersion::One,
            4,
            ClientRequest::ReconcileTurn {
                command_id: command()?,
                session_id,
                expected_active_turn_id: parked_turn_id,
                content: UserInputContent::text(String::from("continue after reconciliation")),
                expected_defaults_version: CanonicalU64::new(1),
                model_settings: ModelSettingsOverlay::inherit_all(),
            },
        )
        .await?;
    let successor_turn_id = accepted_successor_turn(&mut connection, session_id, 2).await?;
    assert_ne!(successor_turn_id, parked_turn_id);

    connection
        .request_version(
            ProtocolVersion::One,
            5,
            ClientRequest::ReadTranscript { session_id },
        )
        .await?;
    let start = response_within(&mut connection).await?;
    transcript_snapshot_start_cursor(start.message(), session_id);
    let reconciled_turn = response_within(&mut connection).await?;
    let (projected_reconciled_turn, reconciled_position, reconciled_state) =
        transcript_turn_projection(reconciled_turn.message());
    assert_eq!(projected_reconciled_turn, parked_turn_id);
    assert_eq!(reconciled_position, 1);
    assert!(matches!(
        reconciled_state,
        TurnState::ReconciliationRequired { .. }
    ));
    let successor_turn = response_within(&mut connection).await?;
    let (projected_successor_turn, successor_position, successor_state) =
        transcript_turn_projection(successor_turn.message());
    assert_eq!(projected_successor_turn, successor_turn_id);
    assert_eq!(successor_position, 2);
    assert!(matches!(successor_state, TurnState::Queued { .. }));

    drop(connection);
    runtime.stop().await
}

/// The terminal disposition the session's single model call recorded, when
/// one exists.
pub(crate) async fn sole_terminal_call_disposition(
    pool: &PgPool,
    session_id: CanonicalUuid,
    turn_id: CanonicalUuid,
) -> Result<Option<String>, Box<dyn Error>> {
    Ok(sqlx::query_scalar(
        "SELECT terminal_disposition_kind
           FROM model_call
          WHERE session_id = $1
            AND turn_id = $2",
    )
    .bind(session_id.into_uuid())
    .bind(turn_id.into_uuid())
    .fetch_one(pool)
    .await?)
}

/// a live streamed provider exchange that fails its stream integrity check parks the turn on an
/// unstopped ambiguous model call — exactly the wedge a mid-stream protocol violation produces —
/// and the reconciliation verb releases the session with a queued successor.
///
/// This is the process-level recovery contract for the streamed-delivery
/// path: the scripted model declares the same boundary-loss evidence the
/// Anthropic decoder emits for a protocol violation, so the park is produced
/// by the real bridge, scheduler, and persistence chain rather than by a
/// startup-scan fixture.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn streamed_protocol_violation_parks_then_reconciles() -> Result<(), Box<dyn Error>> {
    let mut runtime = RunningRuntime::start().await?;
    let mut commands = Connection::connect(runtime.socket()).await?;
    let session_id = create_alias_session(&mut commands).await?;
    let (_, parked_turn_id) = submit_first_input(
        &mut commands,
        session_id,
        String::from("provoke a mid-stream integrity failure"),
    )
    .await?;
    let script = Script::delivering(TerminalEvidence::BoundaryLoss(BoundaryLossEvidence {
        cause: LossCause::StreamProtocolViolation {
            detail: String::from("thinking block carries more than one signature"),
        },
        exchange: ExchangeFacts::default(),
        reported_model: Some(ProviderReportedModel::new("fixture-model")),
        finish_reported: None,
        tool_calls: ToolCallsAtLoss::NoneOpened,
        usage: TokenUsage::unreported(),
    }))
    .observing(ObservationFact::SendCommenced);

    let probe = execute_streamed_turn_until(
        &mut runtime,
        ScriptedModel::single(script),
        session_id,
        parked_turn_id,
        TurnSettle::ParkedOnAmbiguity,
    )
    .await?;

    let operations = probe.received_operations();
    assert_eq!(operations.len(), 1);
    assert_eq!(operations[0].delivery, DeliveryMode::Streamed);
    assert_eq!(
        sole_terminal_call_disposition(&runtime.pool, session_id, parked_turn_id).await?,
        Some(String::from("ambiguous")),
        "a mid-stream protocol violation must close the issued call as ambiguous"
    );

    connection_reconciles_the_parked_turn(&mut commands, session_id, parked_turn_id).await?;

    drop(commands);
    runtime.stop().await
}

/// Issues the reconciliation decision for one parked turn and
/// proves a distinct successor turn was queued from its content.
pub(crate) async fn connection_reconciles_the_parked_turn(
    connection: &mut Connection,
    session_id: CanonicalUuid,
    parked_turn_id: CanonicalUuid,
) -> Result<(), Box<dyn Error>> {
    connection
        .request_version(
            ProtocolVersion::One,
            3,
            ClientRequest::ReconcileTurn {
                command_id: command()?,
                session_id,
                expected_active_turn_id: parked_turn_id,
                content: UserInputContent::text(String::from("continue after the wedge")),
                expected_defaults_version: CanonicalU64::new(1),
                model_settings: ModelSettingsOverlay::inherit_all(),
            },
        )
        .await?;
    let successor_turn_id = accepted_successor_turn(connection, session_id, 2).await?;
    assert_ne!(successor_turn_id, parked_turn_id);
    Ok(())
}

/// the reconciliation request is refused, without recording a command, for every turn that owes no
/// reconciliation decision — so the verb never becomes a general active-turn stop.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn reconcile_turn_refuses_a_turn_that_owes_no_decision() -> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let session_id = create_alias_session(&mut connection).await?;
    let (_, parked_turn_id) =
        submit_first_input(&mut connection, session_id, String::from("first request")).await?;
    park_turn_on_ambiguous_model_call(&runtime.pool, session_id).await?;

    let unparked_turn_id = CanonicalUuid::from_uuid(Uuid::from_u128(0xB1));
    connection
        .request_version(
            ProtocolVersion::One,
            3,
            ClientRequest::ReconcileTurn {
                command_id: command()?,
                session_id,
                expected_active_turn_id: unparked_turn_id,
                content: UserInputContent::text(String::from("names no parked turn")),
                expected_defaults_version: CanonicalU64::new(1),
                model_settings: ModelSettingsOverlay::inherit_all(),
            },
        )
        .await?;
    assert_eq!(
        rejected_detail(response_within(&mut connection).await?.message()),
        RejectionDetail::TurnNotAwaitingReconciliation {
            session_id,
            turn_id: unparked_turn_id,
        }
    );

    connection
        .request_version(
            ProtocolVersion::One,
            4,
            ClientRequest::ReconcileTurn {
                command_id: command()?,
                session_id,
                expected_active_turn_id: parked_turn_id,
                content: UserInputContent::text(String::from("continue after reconciliation")),
                expected_defaults_version: CanonicalU64::new(1),
                model_settings: ModelSettingsOverlay::inherit_all(),
            },
        )
        .await?;
    accepted_successor_turn(&mut connection, session_id, 2).await?;

    connection
        .request_version(
            ProtocolVersion::One,
            5,
            ClientRequest::ReconcileTurn {
                command_id: command()?,
                session_id,
                expected_active_turn_id: parked_turn_id,
                content: UserInputContent::text(String::from("the decision is already recorded")),
                expected_defaults_version: CanonicalU64::new(1),
                model_settings: ModelSettingsOverlay::inherit_all(),
            },
        )
        .await?;
    assert_eq!(
        rejected_detail(response_within(&mut connection).await?.message()),
        RejectionDetail::TurnNotAwaitingReconciliation {
            session_id,
            turn_id: parked_turn_id,
        }
    );

    drop(connection);
    runtime.stop().await
}

/// a reconciliation decision that already committed replays its exact
/// recorded successor, because a claimed command identity reaches the durable
/// replay boundary before the current-state precondition is applied.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn reconcile_turn_replays_a_committed_decision() -> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let session_id = create_alias_session(&mut connection).await?;
    let (_, parked_turn_id) =
        submit_first_input(&mut connection, session_id, String::from("first request")).await?;
    park_turn_on_ambiguous_model_call(&runtime.pool, session_id).await?;

    let decision = ClientRequest::ReconcileTurn {
        command_id: command()?,
        session_id,
        expected_active_turn_id: parked_turn_id,
        content: UserInputContent::text(String::from("continue after reconciliation")),
        expected_defaults_version: CanonicalU64::new(1),
        model_settings: ModelSettingsOverlay::inherit_all(),
    };
    connection
        .request_version(ProtocolVersion::One, 3, decision.clone())
        .await?;
    let recorded = response_within(&mut connection).await?;
    assert!(
        matches!(recorded.message(), ServerMessage::InputSubmitted {
        termination: None, session_id: recorded_session, acceptance_position, ..
    } if *recorded_session == session_id && acceptance_position.value() == 2),
        "reconciliation must omit stop-only termination metadata: {recorded:?}"
    );

    connection
        .request_version(ProtocolVersion::One, 4, decision)
        .await?;
    let replayed = response_within(&mut connection).await?;

    assert_eq!(
        replayed.message(),
        recorded.message(),
        "an equal reconciliation retry returns its recorded successor, never a refusal"
    );

    drop(connection);
    runtime.stop().await
}

/// reconciliation records the explicit per-call contribution with the successor origin instead of
/// dropping it at the daemon boundary.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn reconcile_turn_records_its_per_call_model_settings() -> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let session_id = create_alias_session(&mut connection).await?;
    let (_, parked_turn_id) =
        submit_first_input(&mut connection, session_id, String::from("first request")).await?;
    park_turn_on_ambiguous_model_call(&runtime.pool, session_id).await?;
    let requested = low_reasoning_override();

    connection
        .request_version(
            ProtocolVersion::One,
            3,
            ClientRequest::ReconcileTurn {
                command_id: command()?,
                session_id,
                expected_active_turn_id: parked_turn_id,
                content: UserInputContent::text(String::from("continue with deliberate reasoning")),
                expected_defaults_version: CanonicalU64::new(1),
                model_settings: requested,
            },
        )
        .await?;
    let settings = accepted_successor_model_settings(&mut connection, session_id, 2).await?;

    assert_eq!(settings.precedence.per_call, requested);
    assert_eq!(
        settings.effective.reasoning_level,
        Some(ReasoningLevel::Low)
    );
    assert_eq!(settings.reasoning_source, Some(ModelSettingSource::PerCall));
    assert_eq!(
        settings.validated_for_selection_id,
        Some(primary_direct_selection_id())
    );

    drop(connection);
    runtime.stop().await
}

/// two overlapping requests carrying one reconciliation command
/// identity both land on the committed decision.
///
/// The claim probe and the precondition read are separate statements, so the
/// loser can observe the wait already released; it must still reach the replay
/// boundary rather than the unrecorded refusal. Both halves are asserted in one
/// execution because the race is the requirement (testing-style rule 17).
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn overlapping_equal_reconciliations_both_reach_the_committed_decision()
-> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let mut setup = Connection::connect(runtime.socket()).await?;
    let session_id = create_alias_session(&mut setup).await?;
    let (_, parked_turn_id) =
        submit_first_input(&mut setup, session_id, String::from("first request")).await?;
    park_turn_on_ambiguous_model_call(&runtime.pool, session_id).await?;
    drop(setup);

    let decision = ClientRequest::ReconcileTurn {
        command_id: command()?,
        session_id,
        expected_active_turn_id: parked_turn_id,
        content: UserInputContent::text(String::from("continue after reconciliation")),
        expected_defaults_version: CanonicalU64::new(1),
        model_settings: ModelSettingsOverlay::inherit_all(),
    };
    let mut first = Connection::connect(runtime.socket()).await?;
    let mut second = Connection::connect(runtime.socket()).await?;
    first
        .request_version(ProtocolVersion::One, 1, decision.clone())
        .await?;
    second
        .request_version(ProtocolVersion::One, 1, decision)
        .await?;

    let first_turn_id = accepted_successor_turn(&mut first, session_id, 2).await?;
    let second_turn_id = accepted_successor_turn(&mut second, session_id, 2).await?;

    assert_eq!(
        second_turn_id, first_turn_id,
        "an equal identity that loses the admission race replays the committed successor"
    );
    assert_ne!(first_turn_id, parked_turn_id);

    drop(first);
    drop(second);
    runtime.stop().await
}

/// an absent session is left to the authoritative transaction's recorded `session_not_found`, not
/// collapsed into the precondition refusal.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn reconcile_turn_reports_an_absent_session_exactly() -> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let absent_session_id = CanonicalUuid::from_uuid(Uuid::from_u128(0xB2));

    connection
        .request_version(
            ProtocolVersion::One,
            1,
            ClientRequest::ReconcileTurn {
                command_id: command()?,
                session_id: absent_session_id,
                expected_active_turn_id: CanonicalUuid::from_uuid(Uuid::from_u128(0xB3)),
                content: UserInputContent::text(String::from("names no session")),
                expected_defaults_version: CanonicalU64::new(1),
                model_settings: ModelSettingsOverlay::inherit_all(),
            },
        )
        .await?;

    assert_eq!(
        rejected_detail(response_within(&mut connection).await?.message()),
        RejectionDetail::SessionNotFound {
            session_id: absent_session_id,
        }
    );

    drop(connection);
    runtime.stop().await
}
