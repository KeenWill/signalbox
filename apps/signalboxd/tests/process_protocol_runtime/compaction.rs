//! Compaction coverage.

use super::*;

#[track_caller]
pub(crate) fn exactly_one_credential_reference(references: &[String]) -> &str {
    match references {
        [reference] => reference.as_str(),
        _ => panic!("the fixture pins exactly one credential family"),
    }
}

#[track_caller]
pub(crate) fn failed_automatic_compaction_turn(
    outcome: Result<
        (),
        ContextGuardedTurnPassError<
            RuntimeInputTokenCountError,
            signalboxd::WorkspaceInstructionPreparedExecutionError<
                signalboxd::PostgresProviderModelExecutionError<RuntimeModelCallProviderError>,
            >,
        >,
    >,
) -> TurnId {
    match outcome {
        Err(ContextGuardedTurnPassError::Compaction {
            turn,
            cause_code: "context_compaction_model",
            ..
        }) => turn,
        other => panic!("expected a failed automatic compaction, got {other:?}"),
    }
}

#[derive(Clone, Debug)]
pub(crate) struct RecordingCountedScriptedModel {
    pub(crate) inner: ScriptedModel<ModelCallId>,
    pub(crate) prepared_operations: Arc<Mutex<Vec<ModelOperation<ModelCallId>>>>,
    pub(crate) counted_operations: Arc<Mutex<Vec<ModelOperation<ModelCallId>>>>,
    pub(crate) counts: Arc<Mutex<VecDeque<u64>>>,
}

impl RecordingCountedScriptedModel {
    pub(crate) fn following(
        scripts: impl IntoIterator<Item = Script>,
        counts: impl IntoIterator<Item = u64>,
    ) -> Self {
        Self {
            inner: ScriptedModel::following(scripts),
            prepared_operations: Arc::new(Mutex::new(Vec::new())),
            counted_operations: Arc::new(Mutex::new(Vec::new())),
            counts: Arc::new(Mutex::new(counts.into_iter().collect())),
        }
    }

    pub(crate) fn prepared_operations(&self) -> Vec<ModelOperation<ModelCallId>> {
        self.prepared_operations
            .lock()
            .expect("the recording fixture lock is available")
            .clone()
    }

    pub(crate) fn counted_operations(&self) -> Vec<ModelOperation<ModelCallId>> {
        self.counted_operations
            .lock()
            .expect("the counting fixture lock is available")
            .clone()
    }
}

impl ModelRuntime<ModelCallId> for RecordingCountedScriptedModel {
    type Prepared = ScriptedPrepared<ModelCallId>;

    async fn prepare(
        &self,
        operation: ModelOperation<ModelCallId>,
        cancellation: CancellationSignal,
    ) -> PreparationOutcome<ModelCallId, Self::Prepared> {
        self.prepared_operations
            .lock()
            .expect("the recording fixture lock is available")
            .push(operation.clone());
        self.inner.prepare(operation, cancellation).await
    }

    async fn execute(
        &self,
        prepared: Self::Prepared,
        sink: &mut (dyn ObservationSink<ModelCallId> + Send),
        cancellation: CancellationSignal,
    ) -> TerminalReport<ModelCallId> {
        self.inner.execute(prepared, sink, cancellation).await
    }
}

impl ModelInputTokenCounter<ModelCallId> for RecordingCountedScriptedModel {
    async fn count_input_tokens(
        &self,
        operation: ModelOperation<ModelCallId>,
        _cancellation: CancellationSignal,
    ) -> InputTokenCountOutcome<ModelCallId> {
        let correlation = operation.correlation;
        self.counted_operations
            .lock()
            .expect("the counting fixture lock is available")
            .push(operation);
        let input_tokens = self
            .counts
            .lock()
            .expect("the count-script lock is available")
            .pop_front()
            .expect("the exact-count fixture has a scripted result");
        InputTokenCountOutcome::Counted {
            correlation,
            input_tokens,
        }
    }
}

pub(crate) async fn attach_follower_after_snapshot(
    socket: &Path,
    version: ProtocolVersion,
    request_id: u64,
    session_id: CanonicalUuid,
) -> Result<(Connection, u64), Box<dyn Error>> {
    let mut follow = Connection::connect(socket).await?;
    follow
        .request_version(
            version,
            request_id,
            ClientRequest::FollowSession { session_id },
        )
        .await?;
    let start = response_within(&mut follow).await?;
    let cursor = transcript_snapshot_start_cursor(start.message(), session_id);
    loop {
        let frame = response_within(&mut follow).await?;
        if matches!(
            frame.message(),
            ServerMessage::TranscriptSnapshotEnd {
                session_id: selected,
                ..
            } if *selected == session_id
        ) {
            return Ok((follow, cursor));
        }
    }
}

pub(crate) async fn seed_completed_compaction_session(
    runtime: &mut RunningRuntime,
) -> Result<(Connection, CanonicalUuid), Box<dyn Error>> {
    let mut connection = Connection::connect(runtime.socket()).await?;
    let session_id = create_alias_session(&mut connection).await?;
    let (_, turn) = submit_first_input(
        &mut connection,
        session_id,
        String::from("compaction transaction fixture input"),
    )
    .await?;
    let model = ScriptedModel::single(completed_script(
        "fixture-model",
        "compaction transaction fixture response",
        TokenUsage::unreported(),
    ));
    let probe = execute_streamed_turn(runtime, model, session_id, turn).await?;
    assert_eq!(probe.received_operations().len(), 1);
    Ok((connection, session_id))
}

pub(crate) fn direct_compaction_request(
    session_id: CanonicalUuid,
    command_id: DurableCommandId,
    requested_through_position: Option<u64>,
    identity_base: u128,
) -> PrepareContextCompactionRequest {
    PrepareContextCompactionRequest {
        command: command_id,
        session: SessionId::from_uuid(session_id.into_uuid()),
        requested_through_position,
        automatic_for_turn: None,
        defaults_version: SessionConfigurationDefaultsVersion::first(),
        selection: DirectModelSelection::from_uuid(Uuid::from_u128(1)),
        target: ResolvedProviderTarget::naming(ProviderModelIdentity::from_uuid(Uuid::from_u128(
            3,
        ))),
        input_includes_cache_tokens: false,
        credential_reference: String::from("synthetic-compaction-transaction-credential"),
        call: ModelCallId::from_uuid(Uuid::from_u128(identity_base)),
        compaction: ContextCompactionId::from_uuid(Uuid::from_u128(identity_base + 1)),
        summary_entry: SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(identity_base + 2)),
        result_frontier: ContextFrontierId::from_uuid(Uuid::from_u128(identity_base + 3)),
    }
}

pub(crate) async fn execute_recorded_turn(
    runtime: &mut RunningRuntime,
    scripted: RecordingCountedScriptedModel,
    model_configuration: HubModelConfiguration,
    session_id: CanonicalUuid,
    turn_id: CanonicalUuid,
) -> Result<RecordingCountedScriptedModel, Box<dyn Error>> {
    let probe = scripted.clone();
    let provider =
        RuntimeModelCallProvider::new(scripted, model_configuration.runtime_model_catalog(), None)
            .with_text_delta_sink(runtime.provider_text_delta_sink());
    let (execution, fatal_execution) =
        FatalExecutionSupervisor::new(signalboxd::WorkspaceInstructionPreparedExecution::new(
            PostgresProviderModelExecution::new(
                PostgresModelCallRepository::new(
                    runtime.pool.clone(),
                    model_configuration.target_catalog(),
                    ModelCallCredentialReference::new("recording-fixture"),
                ),
                InProcessAttemptDispatchGate::default(),
                provider,
                None,
            ),
            signalboxd::WorkspaceInstructionRuntime::new(runtime.pool.clone(), None, Vec::new()),
        ));
    let pass = ActivatedTurnPass::new(
        StartEligibleTurnService::new(
            UuidV7StartEligibleTurnIdGenerator,
            StartEligibleTurnRepository::new(runtime.pool.clone()),
        ),
        execution,
    );
    let mut scheduler = SchedulerLoop::new(runtime.take_work_source(), pass);
    let observation_pool = runtime.pool.clone();
    let session = SessionId::from_uuid(session_id.into_uuid());
    let turn = TurnId::from_uuid(turn_id.into_uuid());
    let fatal_shutdown = fatal_execution.clone();
    let shutdown = async move {
        tokio::select! {
            () = wait_for_turn_settle(&observation_pool, session, turn, TurnSettle::Terminal) => {}
            () = fatal_shutdown.wait() => {}
        }
    };
    let scheduler_outcome = timeout(RUNTIME_SETTLE_ALLOWANCE, scheduler.run_until(shutdown)).await;
    let Ok(scheduler_exit) = scheduler_outcome else {
        let lifecycle = sqlx::query_as::<_, (String, Option<String>)>(
            "SELECT state_kind, active_phase_kind
               FROM turn_lifecycle
              WHERE session_id = $1 AND turn_id = $2",
        )
        .bind(session.into_uuid())
        .bind(turn.into_uuid())
        .fetch_one(&runtime.pool)
        .await?;
        let calls: Vec<String> = sqlx::query_scalar(
            "SELECT state_kind FROM model_call
              WHERE session_id = $1 AND turn_id = $2",
        )
        .bind(session.into_uuid())
        .bind(turn.into_uuid())
        .fetch_all(&runtime.pool)
        .await?;
        let mut diagnostic_activation = StartEligibleTurnService::new(
            UuidV7StartEligibleTurnIdGenerator,
            StartEligibleTurnRepository::new(runtime.pool.clone()),
        );
        let activation = diagnostic_activation.execute(session).await;
        panic!(
            "recorded turn timed out: lifecycle={lifecycle:?}; calls={calls:?}; activation={activation:?}"
        );
    };
    assert_eq!(scheduler_exit, SchedulerLoopExit::Shutdown);
    assert!(!fatal_execution.is_triggered());
    Ok(probe)
}

pub(crate) async fn execute_guarded_turn(
    runtime: &mut RunningRuntime,
    scripted: RecordingCountedScriptedModel,
    summary_runtime: ScriptedModel<ModelCallId>,
    model_configuration: HubModelConfiguration,
    session_id: CanonicalUuid,
    turn_id: CanonicalUuid,
) -> Result<RecordingCountedScriptedModel, Box<dyn Error>> {
    let probe = scripted.clone();
    let runtime_models = model_configuration.runtime_model_catalog();
    let provider = RuntimeModelCallProvider::new(scripted, runtime_models.clone(), None)
        .with_text_delta_sink(runtime.provider_text_delta_sink());
    let counter = provider.clone();
    let repository = PostgresModelCallRepository::new(
        runtime.pool.clone(),
        model_configuration.target_catalog(),
        ModelCallCredentialReference::new("guarded-recording-fixture"),
    )
    .with_session_credentials(model_configuration.credential_family_catalog());
    let guarded_repository = repository.clone();
    let (execution, fatal_execution) =
        FatalExecutionSupervisor::new(signalboxd::WorkspaceInstructionPreparedExecution::new(
            PostgresProviderModelExecution::new(
                repository,
                InProcessAttemptDispatchGate::default(),
                provider,
                None,
            ),
            signalboxd::WorkspaceInstructionRuntime::new(runtime.pool.clone(), None, Vec::new()),
        ));
    let compaction_model: Arc<dyn signalbox_model_provider_runtime::ContextCompactionModel> =
        Arc::new(RuntimeContextCompactionModel::new(
            summary_runtime,
            runtime_models.clone(),
        ));
    let pass = ContextGuardedTurnPass::new(
        StartEligibleTurnRepository::new(runtime.pool.clone()),
        guarded_repository,
        counter,
        NoToolCatalog,
        runtime_models,
        model_configuration,
        compaction_model,
        execution,
    )
    .with_workspace_instructions(signalboxd::WorkspaceInstructionRuntime::new(
        runtime.pool.clone(),
        None,
        Vec::new(),
    ));
    let mut scheduler = SchedulerLoop::new(runtime.take_work_source(), pass);
    let observation_pool = runtime.pool.clone();
    let session = SessionId::from_uuid(session_id.into_uuid());
    let turn = TurnId::from_uuid(turn_id.into_uuid());
    let fatal_shutdown = fatal_execution.clone();
    let shutdown = async move {
        tokio::select! {
            () = wait_for_turn_settle(&observation_pool, session, turn, TurnSettle::Terminal) => {}
            () = fatal_shutdown.wait() => {}
        }
    };
    assert_eq!(
        timeout(RUNTIME_SETTLE_ALLOWANCE, scheduler.run_until(shutdown)).await?,
        SchedulerLoopExit::Shutdown
    );
    assert!(!fatal_execution.is_triggered());
    Ok(probe)
}

pub(crate) fn rendered_text_messages(
    operation: &ModelOperation<ModelCallId>,
) -> Vec<(signalbox_model_runtime::ConversationRole, String)> {
    operation
        .messages
        .iter()
        .map(|message| {
            let [MessagePart::Text(text)] = message.parts.as_slice() else {
                panic!("the compaction fixture expects text-only runtime messages")
            };
            (message.role, text.clone())
        })
        .collect()
}

/// explicit compaction uses a dedicated scripted call, retains the complete transcript and exact
/// usage / range provenance, survives startup scan, and projects summary plus suffix into the next
/// ordinary scripted call.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn explicit_compaction_survives_restart_and_projects() -> Result<(), Box<dyn Error>> {
    let usage = TokenUsage {
        input_tokens: Some(41),
        output_tokens: Some(7),
        cache_creation_input_tokens: Some(5),
        cache_read_input_tokens: Some(29),
    };
    let summary_text = String::from("durable scripted summary");
    let summary_runtime =
        ScriptedModel::single(completed_script("fixture-model", &summary_text, usage));
    let mut runtime = RunningRuntime::start_with_compaction(summary_runtime).await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let session_id = create_alias_session(&mut connection).await?;
    let first_user = String::from("first durable request");
    let (_, first_turn) =
        submit_first_input(&mut connection, session_id, first_user.clone()).await?;
    let first_assistant = String::from("first durable reply");
    let first_model = ScriptedModel::single(completed_script(
        "fixture-model",
        &first_assistant,
        TokenUsage::unreported(),
    ));
    let first_probe =
        execute_streamed_turn(&mut runtime, first_model, session_id, first_turn).await?;
    assert_eq!(first_probe.received_operations().len(), 1);
    let (mut follow, follow_cursor) =
        attach_follower_after_snapshot(runtime.socket(), ProtocolVersion::One, 30, session_id)
            .await?;
    let before_members = sqlx::query_as::<_, (Uuid, Uuid)>(
        "SELECT member.source_session_id, member.semantic_entry_id
           FROM turn_lifecycle AS lifecycle
           JOIN context_frontier_member AS member
             ON member.owning_session_id = lifecycle.session_id
            AND member.context_frontier_id = lifecycle.terminal_frontier_id
          WHERE lifecycle.session_id = $1
            AND lifecycle.turn_id = $2
          ORDER BY member.member_position",
    )
    .bind(session_id.into_uuid())
    .bind(first_turn.into_uuid())
    .fetch_all(&runtime.pool)
    .await?;
    let compaction_command = command()?;
    connection
        .request_version(
            ProtocolVersion::One,
            3,
            ClientRequest::CompactSession {
                command_id: compaction_command,
                session_id,
                through_position: None,
            },
        )
        .await?;
    let receipt = response_within(&mut connection).await?;
    let ServerMessage::SessionCompacted {
        session_id: compacted_session,
        context_compaction_id,
        model_call_id,
        through_position,
        summary_entry_id,
        result_frontier_id,
    } = receipt.message()
    else {
        panic!(
            "the explicit compaction fixture expected a receipt, got {:?}",
            receipt.message()
        )
    };
    assert_eq!(*compacted_session, session_id);
    assert_eq!(through_position.value(), before_members.len() as u64);
    let followed = response_within(&mut follow).await?;
    let ServerMessage::SessionEvent {
        cursor: followed_cursor,
        session_id: followed_session,
        event:
            SessionEvent::ContextCompacted {
                context_compaction_id: followed_compaction,
                model_call_id: followed_call,
                through_position: followed_through,
                summary_entry_id: followed_summary,
                result_frontier_id: followed_frontier,
            },
    } = followed.message()
    else {
        panic!(
            "the established follower expected a compaction event, got {:?}",
            followed.message()
        );
    };
    assert!(followed_cursor.value() > follow_cursor);
    assert_eq!(*followed_session, session_id);
    assert_eq!(*followed_compaction, *context_compaction_id);
    assert_eq!(*followed_call, *model_call_id);
    assert_eq!(*followed_through, *through_position);
    assert_eq!(*followed_summary, *summary_entry_id);
    assert_eq!(*followed_frontier, *result_frontier_id);
    let after_members = sqlx::query_as::<_, (Uuid, Uuid)>(
        "SELECT source_session_id, semantic_entry_id
           FROM context_frontier_member
          WHERE owning_session_id = $1
            AND context_frontier_id = $2
          ORDER BY member_position",
    )
    .bind(session_id.into_uuid())
    .bind(result_frontier_id.into_uuid())
    .fetch_all(&runtime.pool)
    .await?;
    assert_eq!(
        &after_members[..before_members.len()],
        before_members.as_slice()
    );
    assert_eq!(after_members.len(), before_members.len() + 1);
    assert_eq!(
        after_members.last().map(|member| member.1),
        Some(summary_entry_id.into_uuid())
    );
    let stored_provenance = sqlx::query_as::<_, (Uuid, Uuid, Uuid, Uuid, Uuid, Uuid)>(
        "SELECT compaction.producing_call_id,
                compaction.first_source_session_id, compaction.first_entry_id,
                compaction.through_source_session_id, compaction.through_entry_id,
                summary.context_summary_producing_call_id
           FROM context_compaction AS compaction
           JOIN semantic_transcript_entry AS summary
             ON summary.source_session_id = compaction.session_id
            AND summary.semantic_entry_id = compaction.summary_entry_id
          WHERE compaction.context_compaction_id = $1",
    )
    .bind(context_compaction_id.into_uuid())
    .fetch_one(&runtime.pool)
    .await?;
    assert_eq!(stored_provenance.0, model_call_id.into_uuid());
    assert_eq!(
        (stored_provenance.1, stored_provenance.2),
        before_members[0]
    );
    assert_eq!(
        (stored_provenance.3, stored_provenance.4),
        *before_members
            .last()
            .expect("the terminal frontier is nonempty")
    );
    assert_eq!(stored_provenance.5, model_call_id.into_uuid());
    let stored_usage = sqlx::query_as::<_, (Option<i64>, Option<i64>, Option<i64>, Option<i64>)>(
        "SELECT input_tokens::bigint, output_tokens::bigint,
                cache_creation_input_tokens::bigint, cache_read_input_tokens::bigint
           FROM context_compaction_model_call
          WHERE model_call_id = $1",
    )
    .bind(model_call_id.into_uuid())
    .fetch_one(&runtime.pool)
    .await?;
    assert_eq!(stored_usage.0, usage.input_tokens.map(|value| value as i64));
    assert_eq!(
        stored_usage.1,
        usage.output_tokens.map(|value| value as i64)
    );
    assert_eq!(
        stored_usage.2,
        usage.cache_creation_input_tokens.map(|value| value as i64)
    );
    assert_eq!(
        stored_usage.3,
        usage.cache_read_input_tokens.map(|value| value as i64)
    );

    drop(connection);
    assert_eq!(runtime.restart().await?, 0);
    let mut successor = Connection::connect(runtime.socket()).await?;
    successor
        .request_version(
            ProtocolVersion::One,
            4,
            ClientRequest::CompactSession {
                command_id: compaction_command,
                session_id,
                through_position: None,
            },
        )
        .await?;
    assert_eq!(
        response_within(&mut successor).await?.message(),
        &ServerMessage::SessionCompacted {
            session_id,
            context_compaction_id: *context_compaction_id,
            model_call_id: *model_call_id,
            through_position: *through_position,
            summary_entry_id: *summary_entry_id,
            result_frontier_id: *result_frontier_id,
        }
    );
    let second_user = String::from("post-restart suffix request");
    successor
        .request_version(
            ProtocolVersion::One,
            5,
            ClientRequest::SubmitInput {
                command_id: command()?,
                session_id,
                content: UserInputContent::text(second_user.clone()),
                expected_defaults_version: Some(CanonicalU64::new(1)),
                model_settings: ModelSettingsOverlay::inherit_all(),
                delivery: None,
            },
        )
        .await?;
    let second_turn = accepted_successor_turn(&mut successor, session_id, 2).await?;
    let second_model = RecordingCountedScriptedModel::following(
        [completed_script(
            "fixture-model",
            "post-restart reply",
            TokenUsage::unreported(),
        )],
        [],
    );
    let second_probe = execute_recorded_turn(
        &mut runtime,
        second_model,
        support::parse_model_configuration(MODEL_CONFIGURATION)?,
        session_id,
        second_turn,
    )
    .await?;
    let prepared = second_probe.prepared_operations();
    assert_eq!(prepared.len(), 1);
    assert_eq!(
        rendered_text_messages(&prepared[0]),
        vec![
            (
                signalbox_model_runtime::ConversationRole::User,
                format!("Signalbox prior-conversation summary:\n{summary_text}"),
            ),
            (signalbox_model_runtime::ConversationRole::User, second_user,),
        ]
    );
    let persisted_summary: String = sqlx::query_scalar(
        "SELECT context_summary_value
           FROM semantic_transcript_entry
          WHERE source_session_id = $1
            AND semantic_entry_id = $2",
    )
    .bind(session_id.into_uuid())
    .bind(summary_entry_id.into_uuid())
    .fetch_one(&runtime.pool)
    .await?;
    assert_eq!(persisted_summary, summary_text);

    let recovery_repository = ContextCompactionRepository::new(runtime.pool.clone());
    let prepared_call = ModelCallId::from_uuid(Uuid::from_u128(0xcc20));
    let prepared_outcome = recovery_repository
        .prepare(PrepareContextCompactionRequest {
            command: DurableCommandId::from_uuid(Uuid::from_u128(0xcc21)),
            session: SessionId::from_uuid(session_id.into_uuid()),
            requested_through_position: None,
            automatic_for_turn: None,
            defaults_version: SessionConfigurationDefaultsVersion::first(),
            selection: DirectModelSelection::from_uuid(Uuid::from_u128(1)),
            target: ResolvedProviderTarget::naming(ProviderModelIdentity::from_uuid(
                Uuid::from_u128(3),
            )),
            input_includes_cache_tokens: false,
            credential_reference: String::from("synthetic-compaction-credential"),
            call: prepared_call,
            compaction: ContextCompactionId::from_uuid(Uuid::from_u128(0xcc22)),
            summary_entry: SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xcc23)),
            result_frontier: ContextFrontierId::from_uuid(Uuid::from_u128(0xcc24)),
        })
        .await?;
    let PrepareContextCompactionOutcome::Prepared(prepared) = prepared_outcome else {
        panic!("the recovery fixture must leave a Prepared compaction call");
    };
    assert_eq!(prepared.call(), prepared_call);

    drop(successor);
    assert_eq!(runtime.restart().await?, 0);
    let prepared_recovery = sqlx::query_as::<_, (String, String, String)>(
        "SELECT call.state_kind, call.terminal_disposition_kind, command.result_kind
           FROM context_compaction_model_call AS call
           JOIN compact_session_command AS command
             ON command.session_id = call.session_id
            AND command.model_call_id = call.model_call_id
          WHERE call.model_call_id = $1",
    )
    .bind(prepared_call.into_uuid())
    .fetch_one(&runtime.pool)
    .await?;
    assert_eq!(
        prepared_recovery,
        (
            String::from("terminal"),
            String::from("known_failed"),
            String::from("failed"),
        )
    );

    let in_flight_call = ModelCallId::from_uuid(Uuid::from_u128(0xcc25));
    let in_flight_outcome = recovery_repository
        .prepare(PrepareContextCompactionRequest {
            command: DurableCommandId::from_uuid(Uuid::from_u128(0xcc26)),
            session: SessionId::from_uuid(session_id.into_uuid()),
            requested_through_position: None,
            automatic_for_turn: None,
            defaults_version: SessionConfigurationDefaultsVersion::first(),
            selection: DirectModelSelection::from_uuid(Uuid::from_u128(1)),
            target: ResolvedProviderTarget::naming(ProviderModelIdentity::from_uuid(
                Uuid::from_u128(3),
            )),
            input_includes_cache_tokens: false,
            credential_reference: String::from("synthetic-compaction-credential"),
            call: in_flight_call,
            compaction: ContextCompactionId::from_uuid(Uuid::from_u128(0xcc27)),
            summary_entry: SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xcc28)),
            result_frontier: ContextFrontierId::from_uuid(Uuid::from_u128(0xcc29)),
        })
        .await?;
    let PrepareContextCompactionOutcome::Prepared(in_flight) = in_flight_outcome else {
        panic!("the recovery fixture must authorize an InFlight compaction call");
    };
    recovery_repository.authorize(&in_flight).await?;
    assert_eq!(runtime.restart().await?, 0);
    let in_flight_recovery = sqlx::query_as::<_, (String, String, String)>(
        "SELECT call.state_kind, call.terminal_disposition_kind, command.result_kind
           FROM context_compaction_model_call AS call
           JOIN compact_session_command AS command
             ON command.session_id = call.session_id
            AND command.model_call_id = call.model_call_id
          WHERE call.model_call_id = $1",
    )
    .bind(in_flight_call.into_uuid())
    .fetch_one(&runtime.pool)
    .await?;
    assert_eq!(
        in_flight_recovery,
        (
            String::from("terminal"),
            String::from("ambiguous"),
            String::from("failed"),
        )
    );
    let physical_summary_count: i64 = sqlx::query_scalar(
        "SELECT count(*)
           FROM semantic_transcript_entry
          WHERE source_session_id = $1
            AND payload_kind = 'context_summary'",
    )
    .bind(session_id.into_uuid())
    .fetch_one(&runtime.pool)
    .await?;
    assert_eq!(physical_summary_count, 1);

    runtime.stop().await
}

/// Configuration-owned limits reject an oversized dedicated summary while
/// retaining the adapter-reported terminal usage as durable evidence.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn explicit_compaction_over_limit_retains_usage_without_summary() -> Result<(), Box<dyn Error>>
{
    let usage = TokenUsage {
        input_tokens: Some(17),
        output_tokens: Some(257),
        cache_creation_input_tokens: Some(3),
        cache_read_input_tokens: Some(11),
    };
    let summary_runtime = ScriptedModel::single(completed_script(
        "fixture-model",
        "oversized summary must not persist",
        usage,
    ));
    let mut runtime = RunningRuntime::start_with_compaction(summary_runtime).await?;
    let (mut connection, session_id) = seed_completed_compaction_session(&mut runtime).await?;
    connection
        .request_version(
            ProtocolVersion::One,
            3,
            ClientRequest::CompactSession {
                command_id: command()?,
                session_id,
                through_position: None,
            },
        )
        .await?;

    assert_eq!(
        protocol_error_code(response_within(&mut connection).await?.message()),
        ErrorCode::Unavailable
    );
    let stored = sqlx::query_as::<
        _,
        (
            String,
            String,
            Option<i64>,
            Option<i64>,
            Option<i64>,
            Option<i64>,
        ),
    >(
        "SELECT state_kind, terminal_disposition_kind,
                input_tokens::bigint, output_tokens::bigint,
                cache_creation_input_tokens::bigint, cache_read_input_tokens::bigint
           FROM context_compaction_model_call
          WHERE session_id = $1",
    )
    .bind(session_id.into_uuid())
    .fetch_one(&runtime.pool)
    .await?;
    assert_eq!(
        stored,
        (
            String::from("terminal"),
            String::from("known_failed"),
            usage.input_tokens.map(|value| value as i64),
            usage.output_tokens.map(|value| value as i64),
            usage.cache_creation_input_tokens.map(|value| value as i64),
            usage.cache_read_input_tokens.map(|value| value as i64),
        )
    );
    let summary_count: i64 = sqlx::query_scalar(
        "SELECT count(*)
           FROM semantic_transcript_entry
          WHERE source_session_id = $1 AND payload_kind = 'context_summary'",
    )
    .bind(session_id.into_uuid())
    .fetch_one(&runtime.pool)
    .await?;
    assert_eq!(summary_count, 0);

    drop(connection);
    runtime.stop().await
}

/// exact authorization, completion, and failure
/// retries replay their durable outcomes without duplicate summary evidence.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn compaction_lifecycle_retries_are_exact() -> Result<(), Box<dyn Error>> {
    let mut runtime = RunningRuntime::start().await?;
    let (connection, session_id) = seed_completed_compaction_session(&mut runtime).await?;
    let repository = ContextCompactionRepository::new(runtime.pool.clone());
    let completed_outcome = repository
        .prepare(direct_compaction_request(
            session_id,
            DurableCommandId::from_uuid(Uuid::from_u128(0xdd01)),
            None,
            0xdd02,
        ))
        .await?;
    let PrepareContextCompactionOutcome::Prepared(completed) = completed_outcome else {
        panic!("the completion replay fixture must prepare its call");
    };
    repository.authorize(&completed).await?;
    repository.authorize(&completed).await?;
    let usage = ContextCompactionTokenUsage::unreported()
        .with_input_tokens(Some(13))
        .with_output_tokens(Some(5));
    let first = repository
        .complete(&completed, "exact retained summary", usage)
        .await?;
    let replay = repository
        .complete(&completed, "exact retained summary", usage)
        .await?;
    assert_eq!(replay, first);
    let summary_count: i64 = sqlx::query_scalar(
        "SELECT count(*)
           FROM semantic_transcript_entry
          WHERE source_session_id = $1
            AND semantic_entry_id = $2
            AND payload_kind = 'context_summary'",
    )
    .bind(session_id.into_uuid())
    .bind(first.summary_entry.into_uuid())
    .fetch_one(&runtime.pool)
    .await?;
    assert_eq!(summary_count, 1);

    let failed_outcome = repository
        .prepare(direct_compaction_request(
            session_id,
            DurableCommandId::from_uuid(Uuid::from_u128(0xdd10)),
            None,
            0xdd11,
        ))
        .await?;
    let PrepareContextCompactionOutcome::Prepared(failed) = failed_outcome else {
        panic!("the failure replay fixture must prepare its call");
    };
    repository.authorize(&failed).await?;
    let failed_usage = ContextCompactionTokenUsage::unreported()
        .with_input_tokens(Some(17))
        .with_output_tokens(Some(257));
    repository
        .fail_with_usage(
            &failed,
            FailedContextCompactionDisposition::KnownFailed,
            failed_usage,
        )
        .await?;
    repository
        .fail_with_usage(
            &failed,
            FailedContextCompactionDisposition::KnownFailed,
            failed_usage,
        )
        .await?;
    let failed_state = sqlx::query_as::<_, (String, String, String, Option<i64>, Option<i64>)>(
        "SELECT call.state_kind, call.terminal_disposition_kind, command.result_kind,
                call.input_tokens::bigint, call.output_tokens::bigint
           FROM context_compaction_model_call AS call
           JOIN compact_session_command AS command
             ON command.model_call_id = call.model_call_id
            AND command.session_id = call.session_id
          WHERE call.model_call_id = $1",
    )
    .bind(failed.call().into_uuid())
    .fetch_one(&runtime.pool)
    .await?;
    assert_eq!(
        failed_state,
        (
            String::from("terminal"),
            String::from("known_failed"),
            String::from("failed"),
            failed_usage.input_tokens().map(|value| value as i64),
            failed_usage.output_tokens().map(|value| value as i64),
        )
    );

    drop(connection);
    runtime.stop().await
}

/// concurrent reuse of one user-global command identity elects one
/// claimant and makes the loser inspect the committed winner exactly.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn concurrent_compaction_command_claim_has_one_winner() -> Result<(), Box<dyn Error>> {
    let mut runtime = RunningRuntime::start().await?;
    let (connection, session_id) = seed_completed_compaction_session(&mut runtime).await?;
    let command_id = DurableCommandId::from_uuid(Uuid::from_u128(0xde01));
    let left_repository = ContextCompactionRepository::new(runtime.pool.clone());
    let right_repository = left_repository.clone();
    let left_request = direct_compaction_request(session_id, command_id, None, 0xde10);
    let right_request = direct_compaction_request(session_id, command_id, Some(1), 0xde20);
    let (left, right) = tokio::join!(
        left_repository.prepare(left_request),
        right_repository.prepare(right_request),
    );
    let left = left?;
    let right = right?;
    assert!(
        matches!(left, PrepareContextCompactionOutcome::Prepared(_))
            && matches!(right, PrepareContextCompactionOutcome::ConflictingReuse)
            || matches!(left, PrepareContextCompactionOutcome::ConflictingReuse)
                && matches!(right, PrepareContextCompactionOutcome::Prepared(_))
    );
    let claim_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM durable_command WHERE command_id = $1")
            .bind(command_id.into_uuid())
            .fetch_one(&runtime.pool)
            .await?;
    assert_eq!(claim_count, 1);

    drop(connection);
    runtime.stop().await
}

/// compaction preparation and turn activation share the
/// scheduler lock, so exactly one can claim the session boundary and the loser
/// reconstitutes the winner before committing any conflicting lifecycle.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn compaction_preparation_serializes_turn_activation() -> Result<(), Box<dyn Error>> {
    let mut runtime = RunningRuntime::start().await?;
    let (mut connection, session_id) = seed_completed_compaction_session(&mut runtime).await?;
    connection
        .request_version(
            ProtocolVersion::One,
            91,
            ClientRequest::SubmitInput {
                command_id: command()?,
                session_id,
                content: UserInputContent::text(String::from(
                    "scheduler race successor remains singular",
                )),
                expected_defaults_version: Some(CanonicalU64::new(1)),
                model_settings: ModelSettingsOverlay::inherit_all(),
                delivery: None,
            },
        )
        .await?;
    let queued_turn = accepted_successor_turn(&mut connection, session_id, 2).await?;
    let session = SessionId::from_uuid(session_id.into_uuid());
    let mut activation = StartEligibleTurnService::new(
        UuidV7StartEligibleTurnIdGenerator,
        StartEligibleTurnRepository::new(runtime.pool.clone()),
    );
    let repository = ContextCompactionRepository::new(runtime.pool.clone());
    let compaction_request = direct_compaction_request(
        session_id,
        DurableCommandId::from_uuid(Uuid::from_u128(0xde31)),
        None,
        0xde40,
    );

    let (activation_outcome, compaction_outcome) = tokio::join!(
        activation.execute(session),
        repository.prepare(compaction_request),
    );
    let activation_outcome = activation_outcome?;
    let compaction_outcome = compaction_outcome?;
    assert!(
        matches!(activation_outcome, StartEligibleTurnOutcome::Activated(_))
            && matches!(compaction_outcome, PrepareContextCompactionOutcome::Busy)
            || matches!(activation_outcome, StartEligibleTurnOutcome::NoEligibleTurn)
                && matches!(
                    compaction_outcome,
                    PrepareContextCompactionOutcome::Prepared(_)
                ),
        "scheduler serialization must admit exactly one owner: activation={activation_outcome:?}; compaction={compaction_outcome:?}"
    );
    let durable_state: (String, i64) = sqlx::query_as(
        "SELECT turn.state_kind,
                (SELECT count(*)
                   FROM context_compaction_model_call AS call
                  WHERE call.session_id = turn.session_id
                    AND call.state_kind <> 'terminal')
           FROM turn_lifecycle AS turn
          WHERE turn.session_id = $1 AND turn.turn_id = $2",
    )
    .bind(session_id.into_uuid())
    .bind(queued_turn.into_uuid())
    .fetch_one(&runtime.pool)
    .await?;
    assert!(
        durable_state == (String::from("active"), 0)
            || durable_state == (String::from("queued"), 1),
        "the durable boundary must have one owner: {durable_state:?}"
    );

    drop(connection);
    runtime.stop().await
}

/// an exact provider-native count above the input plus its reserved maximum output above the
/// operator-declared context window compacts before activation, recounts the projected
/// summary-plus-suffix input, and sends only that fitting operation.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn automatic_guard_repeats_compaction_until_ordinary_input_fits() -> Result<(), Box<dyn Error>>
{
    let mut runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let session_id = create_alias_session(&mut connection).await?;
    let first_user = String::from("automatic guard historical request");
    let (_, first_turn) =
        submit_first_input(&mut connection, session_id, first_user.clone()).await?;
    let first_assistant = String::from("automatic guard historical reply");
    let first_runtime = ScriptedModel::single(completed_script(
        "fixture-model",
        &first_assistant,
        TokenUsage::unreported(),
    ));
    let first_probe =
        execute_streamed_turn(&mut runtime, first_runtime, session_id, first_turn).await?;
    assert_eq!(first_probe.received_operations().len(), 1);

    drop(connection);
    assert_eq!(runtime.restart().await?, 0);
    let mut successor = Connection::connect(runtime.socket()).await?;
    let second_user = String::from("automatic guard current suffix");
    successor
        .request_version(
            ProtocolVersion::One,
            3,
            ClientRequest::SubmitInput {
                command_id: command()?,
                session_id,
                content: UserInputContent::text(second_user.clone()),
                expected_defaults_version: Some(CanonicalU64::new(1)),
                model_settings: ModelSettingsOverlay::inherit_all(),
                delivery: None,
            },
        )
        .await?;
    let second_turn = accepted_successor_turn(&mut successor, session_id, 2).await?;
    let guarded_configuration = support::parse_model_configuration(&MODEL_CONFIGURATION.replace(
        "context_window_tokens = 200000",
        "context_window_tokens = 4096",
    ))?;
    let ordinary_runtime = RecordingCountedScriptedModel::following(
        [completed_script(
            "fixture-model",
            "automatic guard current reply",
            TokenUsage::unreported(),
        )],
        [8192, 8192, 4],
    );
    let summary_text = String::from("automatic guard summary");
    let summary_runtime = ScriptedModel::following([
        completed_script(
            "fixture-model",
            &"initial summary ".repeat(32),
            TokenUsage::unreported(),
        ),
        completed_script("fixture-model", &summary_text, TokenUsage::unreported()),
    ]);
    let probe = execute_guarded_turn(
        &mut runtime,
        ordinary_runtime,
        summary_runtime,
        guarded_configuration,
        session_id,
        second_turn,
    )
    .await?;
    let counted = probe.counted_operations();
    assert_eq!(counted.len(), 3);
    let first_counted_text = rendered_text_messages(&counted[0]);
    assert!(
        first_counted_text
            .iter()
            .any(|message| message.1 == first_user)
    );
    assert!(
        first_counted_text
            .iter()
            .any(|message| message.1 == first_assistant)
    );
    assert_eq!(
        rendered_text_messages(&counted[2]),
        vec![
            (
                signalbox_model_runtime::ConversationRole::User,
                format!("Signalbox prior-conversation summary:\n{summary_text}"),
            ),
            (
                signalbox_model_runtime::ConversationRole::User,
                second_user.clone(),
            ),
        ]
    );
    let prepared = probe.prepared_operations();
    assert_eq!(prepared.len(), 1);
    assert_eq!(
        rendered_text_messages(&prepared[0]),
        rendered_text_messages(&counted[2])
    );
    let compaction_count: i64 = sqlx::query_scalar(
        "SELECT count(*)
           FROM context_compaction
          WHERE session_id = $1",
    )
    .bind(session_id.into_uuid())
    .fetch_one(&runtime.pool)
    .await?;
    assert_eq!(compaction_count, 2);
    let summary_count: i64 = sqlx::query_scalar(
        "SELECT count(*)
           FROM semantic_transcript_entry
          WHERE source_session_id = $1
            AND payload_kind = 'context_summary'
            AND context_summary_value = $2",
    )
    .bind(session_id.into_uuid())
    .bind(&summary_text)
    .fetch_one(&runtime.pool)
    .await?;
    assert_eq!(summary_count, 1);

    drop(successor);
    runtime.stop().await
}

/// Summary headroom uses admitted text independently of the compactor's billed output.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn reported_usage_rechecks_compaction_headroom() -> Result<(), Box<dyn Error>> {
    let configuration_text = reported_usage_preflight_configuration_text();
    let mut runtime = RunningRuntime::start_with_model_configuration(&configuration_text).await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let session_id = create_alias_session(&mut connection).await?;
    let (_, first_turn) = submit_first_input(
        &mut connection,
        session_id,
        String::from("reported usage historical request"),
    )
    .await?;
    let saturated_usage = TokenUsage {
        input_tokens: Some(5000),
        output_tokens: Some(0),
        cache_creation_input_tokens: None,
        cache_read_input_tokens: None,
    };
    let first_runtime = ScriptedModel::single(completed_script(
        "fixture-model",
        "reported usage historical reply",
        saturated_usage,
    ));
    let first_probe = execute_streamed_turn_until_with_configuration(
        &mut runtime,
        first_runtime,
        reported_usage_preflight_configuration()?,
        session_id,
        first_turn,
        TurnSettle::Terminal,
    )
    .await?;
    assert_eq!(first_probe.received_operations().len(), 1);

    connection
        .request_version(
            ProtocolVersion::One,
            40,
            ClientRequest::SubmitInput {
                command_id: command()?,
                session_id,
                content: UserInputContent::text(String::from("reported usage queued suffix")),
                expected_defaults_version: Some(CanonicalU64::new(1)),
                model_settings: ModelSettingsOverlay::inherit_all(),
                delivery: None,
            },
        )
        .await?;
    let queued_turn = accepted_successor_turn(&mut connection, session_id, 2).await?;
    let configuration = reported_usage_preflight_configuration()?;
    let runtime_models = configuration.runtime_model_catalog();
    let saturated_summary_usage = TokenUsage {
        input_tokens: Some(5000),
        output_tokens: Some(5000),
        cache_creation_input_tokens: None,
        cache_read_input_tokens: None,
    };
    let summary_runtime = ScriptedModel::single(completed_script(
        "fixture-model",
        "reported usage summary remains saturated",
        saturated_summary_usage,
    ));
    let summary_probe = summary_runtime.clone();
    let compaction_model: Arc<dyn signalbox_model_provider_runtime::ContextCompactionModel> =
        Arc::new(RuntimeContextCompactionModel::new(
            summary_runtime,
            runtime_models.clone(),
        ));
    let repository = PostgresModelCallRepository::new(
        runtime.pool.clone(),
        configuration.target_catalog(),
        ModelCallCredentialReference::new("reported-usage-recheck-fixture"),
    )
    .with_session_credentials(configuration.credential_family_catalog());
    let compaction = ReportedUsageCompaction::new(
        StartEligibleTurnRepository::new(runtime.pool.clone()),
        repository,
        NoToolCatalog,
        runtime_models,
        configuration,
        compaction_model,
    );

    compaction
        .compact_if_needed(SessionId::from_uuid(session_id.into_uuid()), None)
        .await?;

    assert_eq!(summary_probe.received_operations().len(), 1);
    let ordinary_call_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM model_call WHERE turn_id = $1")
            .bind(queued_turn.into_uuid())
            .fetch_one(&runtime.pool)
            .await?;
    assert_eq!(ordinary_call_count, 0);
    let lifecycle: (String, Option<String>, Option<Uuid>) = sqlx::query_as(
        "SELECT state_kind, terminal_disposition_kind, terminal_model_call_id
           FROM turn_lifecycle
          WHERE session_id = $1 AND turn_id = $2",
    )
    .bind(session_id.into_uuid())
    .bind(queued_turn.into_uuid())
    .fetch_one(&runtime.pool)
    .await?;
    assert_eq!(lifecycle, (String::from("queued"), None, None));

    drop(connection);
    runtime.stop().await
}

/// a failed automatic compaction closes the queued candidate call-free, so a later eligibility pass
/// cannot dispatch the known-oversized ordinary request.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn failed_automatic_compaction_closes_turn_call_free() -> Result<(), Box<dyn Error>> {
    let mut runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let session_id = create_alias_session(&mut connection).await?;
    let (_, first_turn) = submit_first_input(
        &mut connection,
        session_id,
        String::from("retry guard historical request"),
    )
    .await?;
    let first_runtime = ScriptedModel::single(completed_script(
        "fixture-model",
        "retry guard historical reply",
        TokenUsage::unreported(),
    ));
    let first_probe =
        execute_streamed_turn(&mut runtime, first_runtime, session_id, first_turn).await?;
    assert_eq!(first_probe.received_operations().len(), 1);

    let oversized_suffix = String::from("oversized suffix remains above the declared window");
    connection
        .request_version(
            ProtocolVersion::One,
            40,
            ClientRequest::SubmitInput {
                command_id: command()?,
                session_id,
                content: UserInputContent::text(oversized_suffix),
                expected_defaults_version: Some(CanonicalU64::new(1)),
                model_settings: ModelSettingsOverlay::inherit_all(),
                delivery: None,
            },
        )
        .await?;
    let queued_turn = accepted_successor_turn(&mut connection, session_id, 2).await?;
    let guarded_configuration = support::parse_model_configuration(
        &MODEL_CONFIGURATION
            .replace("max_output_tokens = 256", "max_output_tokens = 1")
            .replace(
                "context_window_tokens = 200000",
                "context_window_tokens = 4096",
            ),
    )?;
    let ordinary_runtime =
        RecordingCountedScriptedModel::following(std::iter::empty::<Script>(), [8192, 8192, 8192]);
    let ordinary_probe = ordinary_runtime.clone();
    let summary_runtime = ScriptedModel::single(Script::delivering(
        TerminalEvidence::ProviderError(ProviderErrorEvidence {
            exchange: ExchangeFacts::default(),
            reported_model: None,
            kind: ProviderErrorKind::Unrecognized,
            non_acceptance_proven: true,
            native: NativeErrorFacts::default(),
            usage: TokenUsage::unreported(),
        }),
    ));
    let summary_probe = summary_runtime.clone();
    let runtime_models = guarded_configuration.runtime_model_catalog();
    let provider = RuntimeModelCallProvider::new(ordinary_runtime, runtime_models.clone(), None)
        .with_text_delta_sink(runtime.provider_text_delta_sink());
    let counter = provider.clone();
    let repository = PostgresModelCallRepository::new(
        runtime.pool.clone(),
        guarded_configuration.target_catalog(),
        ModelCallCredentialReference::new("retry-guard-recording-fixture"),
    )
    .with_session_credentials(guarded_configuration.credential_family_catalog());
    let guarded_repository = repository.clone();
    let (execution, fatal_execution) =
        FatalExecutionSupervisor::new(signalboxd::WorkspaceInstructionPreparedExecution::new(
            PostgresProviderModelExecution::new(
                repository,
                InProcessAttemptDispatchGate::default(),
                provider,
                None,
            ),
            signalboxd::WorkspaceInstructionRuntime::new(runtime.pool.clone(), None, Vec::new()),
        ));
    let compaction_model: Arc<dyn signalbox_model_provider_runtime::ContextCompactionModel> =
        Arc::new(RuntimeContextCompactionModel::new(
            summary_runtime,
            runtime_models.clone(),
        ));
    // The parsed fixture, not an independent literal, is the authority on the
    // reference automatic compaction must reuse; it pins exactly one family.
    let pinned_references: Vec<String> = guarded_configuration
        .session_credential_pin()
        .credentials()
        .map(|credential| credential.credential_reference().to_owned())
        .collect();
    let expected_compaction_credential = exactly_one_credential_reference(&pinned_references);
    let mut pass = ContextGuardedTurnPass::new(
        StartEligibleTurnRepository::new(runtime.pool.clone()),
        guarded_repository,
        counter,
        NoToolCatalog,
        runtime_models,
        guarded_configuration,
        compaction_model,
        execution,
    )
    .with_workspace_instructions(signalboxd::WorkspaceInstructionRuntime::new(
        runtime.pool.clone(),
        None,
        Vec::new(),
    ));
    let session = SessionId::from_uuid(session_id.into_uuid());
    let turn = failed_automatic_compaction_turn(pass.run(session).await);
    assert_eq!(*turn.as_uuid(), queued_turn.into_uuid());
    let second_attempt = pass.run(session).await;
    assert!(second_attempt.is_ok());
    assert!(!fatal_execution.is_triggered());
    assert_eq!(ordinary_probe.counted_operations().len(), 1);
    assert_eq!(ordinary_probe.prepared_operations().len(), 0);
    assert_eq!(summary_probe.received_operations().len(), 1);
    assert_eq!(
        summary_probe.received_operations()[0]
            .credential_reference
            .as_str(),
        expected_compaction_credential
    );
    let compaction_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM context_compaction WHERE session_id = $1")
            .bind(session_id.into_uuid())
            .fetch_one(&runtime.pool)
            .await?;
    assert_eq!(compaction_count, 0);
    let automatic_command_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM compact_session_command
          WHERE session_id = $1 AND automatic_for_turn_id = $2",
    )
    .bind(session_id.into_uuid())
    .bind(queued_turn.into_uuid())
    .fetch_one(&runtime.pool)
    .await?;
    assert_eq!(automatic_command_count, 1);
    let compaction_call: (String, Option<String>) = sqlx::query_as(
        "SELECT state_kind, terminal_disposition_kind
           FROM context_compaction_model_call
          WHERE session_id = $1",
    )
    .bind(session_id.into_uuid())
    .fetch_one(&runtime.pool)
    .await?;
    assert_eq!(
        compaction_call,
        (String::from("terminal"), Some(String::from("known_failed")))
    );
    let ordinary_call_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM model_call WHERE turn_id = $1")
            .bind(queued_turn.into_uuid())
            .fetch_one(&runtime.pool)
            .await?;
    assert_eq!(ordinary_call_count, 0);
    let lifecycle: (String, Option<String>, Option<Uuid>) = sqlx::query_as(
        "SELECT state_kind, terminal_disposition_kind, terminal_model_call_id
           FROM turn_lifecycle
          WHERE session_id = $1 AND turn_id = $2",
    )
    .bind(session_id.into_uuid())
    .bind(queued_turn.into_uuid())
    .fetch_one(&runtime.pool)
    .await?;
    assert_eq!(
        lifecycle,
        (String::from("terminal"), Some(String::from("failed")), None)
    );

    drop(connection);
    runtime.stop().await
}

/// A classified guarded-pass failure whose durable commit outcome is unknown.
///
/// Every durable stage of `ContextGuardedTurnPass` can report
/// `OperatorFailureClass::Infrastructure { commit_ambiguous: true }`; the
/// counting seam is the one a fixture can drive without a provable database
/// commit failure, and the pass owes the same reported outcome to all of them.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct CommitAmbiguousCountFailure;

#[derive(Clone, Copy, Debug)]
pub(crate) struct CommitAmbiguousCounter;

impl ModelCallInputTokenCounter for CommitAmbiguousCounter {
    type Error = CommitAmbiguousCountFailure;

    fn count_input_tokens<Cancellation>(
        &self,
        _operation: PreparedModelOperation,
        _cancellation: Cancellation,
    ) -> impl std::future::Future<Output = Result<ModelCallInputTokenCount, Self::Error>> + Send
    where
        Cancellation: std::future::Future<Output = ()> + Send + 'static,
    {
        std::future::ready(Err(CommitAmbiguousCountFailure))
    }
}

#[derive(Clone, Debug)]
pub(crate) struct CountingProbe {
    pub(crate) interactions: Arc<AtomicUsize>,
    pub(crate) outcome: ModelCallInputTokenCount,
}

impl ModelCallInputTokenCounter for CountingProbe {
    type Error = CommitAmbiguousCountFailure;

    fn count_input_tokens<Cancellation>(
        &self,
        _operation: PreparedModelOperation,
        _cancellation: Cancellation,
    ) -> impl std::future::Future<Output = Result<ModelCallInputTokenCount, Self::Error>> + Send
    where
        Cancellation: std::future::Future<Output = ()> + Send + 'static,
    {
        self.interactions.fetch_add(1, Ordering::SeqCst);
        std::future::ready(Ok(self.outcome))
    }
}

pub(crate) struct TransientUnavailableBlobStore {
    pub(crate) inner: Arc<dyn BlobStore>,
    pub(crate) reads: Arc<AtomicUsize>,
}

impl BlobStore for TransientUnavailableBlobStore {
    fn put<'a>(
        &'a self,
        expected: ExpectedBlob,
        source: BlobReader,
    ) -> BlobStoreFuture<'a, BlobPutOutcome> {
        self.inner.put(expected, source)
    }

    fn open<'a>(&'a self, key: &'a BlobObjectKey) -> BlobStoreFuture<'a, OpenedBlob> {
        if self.reads.fetch_add(1, Ordering::SeqCst) == 0 {
            Box::pin(async { Err(BlobStoreError::unavailable("transient test read")) })
        } else {
            self.inner.open(key)
        }
    }

    fn open_verified<'a>(
        &'a self,
        expected: ExpectedBlob,
        key: &'a BlobObjectKey,
    ) -> BlobStoreFuture<'a, OpenedBlob> {
        self.inner.open_verified(expected, key)
    }

    fn open_range<'a>(
        &'a self,
        expected: ExpectedBlob,
        key: &'a BlobObjectKey,
        offset: u64,
        byte_length: std::num::NonZeroU64,
    ) -> BlobStoreFuture<'a, OpenedBlob> {
        self.inner.open_range(expected, key, offset, byte_length)
    }
}

/// the provider-native counter is behind attachment verification, so
/// a missing replica closes the exact prospective call without provider I/O.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn attachment_verification_precedes_provider_counting() -> Result<(), Box<dyn Error>> {
    let mut fixture = CommittedBlobReadFixture::start(b"count guard attachment").await?;
    let session_id = create_alias_session(&mut fixture.connection).await?;
    fixture
        .connection
        .request(
            4,
            ClientRequest::SubmitInput {
                command_id: command()?,
                session_id,
                content: UserInputContent::from_parts(vec![UserInputPart::Attachment {
                    digest: fixture.wire_digest,
                    kind: UserAttachmentKind::File,
                    media_type: String::from("application/octet-stream"),
                    display_filename: None,
                }]),
                expected_defaults_version: Some(CanonicalU64::new(1)),
                model_settings: ModelSettingsOverlay::inherit_all(),
                delivery: None,
            },
        )
        .await?;
    let queued_turn = accepted_successor_turn(&mut fixture.connection, session_id, 1).await?;
    fs::remove_file(fixture.object_path())?;

    let model_configuration = support::parse_model_configuration(
        &fixture
            .runtime
            .blob_storage_root
            .as_ref()
            .expect("the fixture owns blob configuration")
            .model_configuration(),
    )?;
    let runtime_models = model_configuration.runtime_model_catalog();
    let provider = RuntimeModelCallProvider::new(
        ScriptedModel::<ModelCallId>::following(std::iter::empty::<Script>()),
        runtime_models.clone(),
        None,
    )
    .with_text_delta_sink(fixture.runtime.provider_text_delta_sink());
    let interactions = Arc::new(AtomicUsize::new(0));
    let counter = AttachmentPreparingModelCallProvider::for_counting(
        CountingProbe {
            interactions: Arc::clone(&interactions),
            outcome: ModelCallInputTokenCount::Counted(1),
        },
        fixture.runtime.pool.clone(),
        Some(fixture.runtime.blob_store_registry()),
        model_configuration.provider_input_count_targets(),
    );
    let repository = PostgresModelCallRepository::new(
        fixture.runtime.pool.clone(),
        model_configuration.target_catalog(),
        ModelCallCredentialReference::new("attachment-count-guard-fixture"),
    )
    .with_session_credentials(model_configuration.credential_family_catalog());
    let guarded_repository = repository.clone();
    let (execution, fatal_execution) =
        FatalExecutionSupervisor::new(signalboxd::WorkspaceInstructionPreparedExecution::new(
            PostgresProviderModelExecution::new(
                repository,
                InProcessAttemptDispatchGate::default(),
                provider,
                None,
            ),
            signalboxd::WorkspaceInstructionRuntime::new(
                fixture.runtime.pool.clone(),
                None,
                Vec::new(),
            ),
        ));
    let compaction_model: Arc<dyn signalbox_model_provider_runtime::ContextCompactionModel> =
        Arc::new(RuntimeContextCompactionModel::new(
            ScriptedModel::<ModelCallId>::following(std::iter::empty::<Script>()),
            runtime_models.clone(),
        ));
    let mut pass = ContextGuardedTurnPass::new(
        StartEligibleTurnRepository::new(fixture.runtime.pool.clone()),
        guarded_repository,
        counter,
        NoToolCatalog,
        runtime_models,
        model_configuration,
        compaction_model,
        execution,
    )
    .with_workspace_instructions(signalboxd::WorkspaceInstructionRuntime::new(
        fixture.runtime.pool.clone(),
        None,
        Vec::new(),
    ));

    pass.run(SessionId::from_uuid(session_id.into_uuid()))
        .await?;
    assert_eq!(interactions.load(Ordering::SeqCst), 0);
    assert!(!fatal_execution.is_triggered());
    let closure: (String, Option<String>, Option<String>) = sqlx::query_as(
        "SELECT state_kind, terminal_disposition_kind,
                terminal_attachment_preparation_failure_cause
           FROM model_call
          WHERE session_id = $1 AND turn_id = $2",
    )
    .bind(session_id.into_uuid())
    .bind(queued_turn.into_uuid())
    .fetch_one(&fixture.runtime.pool)
    .await?;
    assert_eq!(
        closure,
        (
            String::from("terminal"),
            Some(String::from("known_failed")),
            Some(String::from("missing")),
        )
    );

    fixture.stop().await
}

/// INV-062: transient attachment unavailability leaves the prospective call
/// uncommitted, so recovery re-verifies the attachment and performs the exact
/// provider count before activation.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn inv062_transient_attachment_unavailability_recounts_after_recovery()
-> Result<(), Box<dyn Error>> {
    let mut fixture = CommittedBlobReadFixture::start(b"transient count guard attachment").await?;
    let session_id = create_alias_session(&mut fixture.connection).await?;
    fixture
        .connection
        .request(
            4,
            ClientRequest::SubmitInput {
                command_id: command()?,
                session_id,
                content: UserInputContent::from_parts(vec![UserInputPart::Attachment {
                    digest: fixture.wire_digest,
                    kind: UserAttachmentKind::File,
                    media_type: String::from("application/octet-stream"),
                    display_filename: None,
                }]),
                expected_defaults_version: Some(CanonicalU64::new(1)),
                model_settings: ModelSettingsOverlay::inherit_all(),
                delivery: None,
            },
        )
        .await?;
    let queued_turn = accepted_successor_turn(&mut fixture.connection, session_id, 1).await?;

    let model_configuration = support::parse_model_configuration(
        &fixture
            .runtime
            .blob_storage_root
            .as_ref()
            .expect("the fixture owns blob configuration")
            .model_configuration(),
    )?;
    let reads = Arc::new(AtomicUsize::new(0));
    let mut counting_registry = BlobStoreRegistry::initialize_for_conformance(
        model_configuration.blob_storage(),
        fixture.runtime.pool.clone(),
    )
    .await?
    .expect("the fixture configures blob storage");
    let (store_name, inner) = counting_registry.routed_store(BlobStorageClass::UserAttachment);
    let store_name = store_name.clone();
    assert!(counting_registry.replace_store_for_conformance(
        &store_name,
        Arc::new(TransientUnavailableBlobStore {
            inner,
            reads: Arc::clone(&reads),
        }),
    ));
    let runtime_models = model_configuration.runtime_model_catalog();
    let provider = RuntimeModelCallProvider::new(
        ScriptedModel::<ModelCallId>::following(std::iter::empty::<Script>()),
        runtime_models.clone(),
        None,
    )
    .with_text_delta_sink(fixture.runtime.provider_text_delta_sink());
    let interactions = Arc::new(AtomicUsize::new(0));
    let counter = AttachmentPreparingModelCallProvider::for_counting(
        CountingProbe {
            interactions: Arc::clone(&interactions),
            outcome: ModelCallInputTokenCount::Cancelled,
        },
        fixture.runtime.pool.clone(),
        Some(Arc::new(counting_registry)),
        model_configuration.provider_input_count_targets(),
    );
    let repository = PostgresModelCallRepository::new(
        fixture.runtime.pool.clone(),
        model_configuration.target_catalog(),
        ModelCallCredentialReference::new("attachment-count-recovery-fixture"),
    )
    .with_session_credentials(model_configuration.credential_family_catalog());
    let guarded_repository = repository.clone();
    let (execution, fatal_execution) =
        FatalExecutionSupervisor::new(signalboxd::WorkspaceInstructionPreparedExecution::new(
            PostgresProviderModelExecution::new(
                repository,
                InProcessAttemptDispatchGate::default(),
                provider,
                None,
            ),
            signalboxd::WorkspaceInstructionRuntime::new(
                fixture.runtime.pool.clone(),
                None,
                Vec::new(),
            ),
        ));
    let compaction_model: Arc<dyn signalbox_model_provider_runtime::ContextCompactionModel> =
        Arc::new(RuntimeContextCompactionModel::new(
            ScriptedModel::<ModelCallId>::following(std::iter::empty::<Script>()),
            runtime_models.clone(),
        ));
    let mut pass = ContextGuardedTurnPass::new(
        StartEligibleTurnRepository::new(fixture.runtime.pool.clone()),
        guarded_repository,
        counter,
        NoToolCatalog,
        runtime_models,
        model_configuration,
        compaction_model,
        execution,
    )
    .with_workspace_instructions(signalboxd::WorkspaceInstructionRuntime::new(
        fixture.runtime.pool.clone(),
        None,
        Vec::new(),
    ));
    let session = SessionId::from_uuid(session_id.into_uuid());

    pass.run(session).await?;
    assert_eq!(reads.load(Ordering::SeqCst), 1);
    assert_eq!(interactions.load(Ordering::SeqCst), 0);
    let call_count: i64 = sqlx::query_scalar("SELECT count(*) FROM model_call WHERE turn_id = $1")
        .bind(queued_turn.into_uuid())
        .fetch_one(&fixture.runtime.pool)
        .await?;
    assert_eq!(call_count, 0);

    let recovered = pass.run(session).await;

    assert!(matches!(
        recovered,
        Err(ContextGuardedTurnPassError::CountCancelled(turn))
            if turn == TurnId::from_uuid(queued_turn.into_uuid())
    ));
    assert_eq!(reads.load(Ordering::SeqCst), 2);
    assert_eq!(interactions.load(Ordering::SeqCst), 1);
    assert!(!fatal_execution.is_triggered());
    let call_count: i64 = sqlx::query_scalar("SELECT count(*) FROM model_call WHERE turn_id = $1")
        .bind(queued_turn.into_uuid())
        .fetch_one(&fixture.runtime.pool)
        .await?;
    assert_eq!(call_count, 0);

    fixture.stop().await
}

/// the production guarded pass reports post-activation failure for the declared ambiguous-commit
/// class, so the daemon stops scheduling and startup recovery regains authority over durable state
/// whose outcome ordinary scheduler retry cannot decide.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn ambiguous_guarded_stage_raises_the_fatal_recovery_signal() -> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let session_id = create_alias_session(&mut connection).await?;
    submit_first_input(
        &mut connection,
        session_id,
        String::from("ambiguous guarded stage request"),
    )
    .await?;
    let model_configuration = support::parse_model_configuration(MODEL_CONFIGURATION)?;
    let runtime_models = model_configuration.runtime_model_catalog();
    let provider = RuntimeModelCallProvider::new(
        ScriptedModel::<ModelCallId>::following(std::iter::empty::<Script>()),
        runtime_models.clone(),
        None,
    )
    .with_text_delta_sink(runtime.provider_text_delta_sink());
    let repository = PostgresModelCallRepository::new(
        runtime.pool.clone(),
        model_configuration.target_catalog(),
        ModelCallCredentialReference::new("ambiguous-guard-fixture"),
    );
    let guarded_repository = repository.clone();
    let (execution, fatal_execution) =
        FatalExecutionSupervisor::new(signalboxd::WorkspaceInstructionPreparedExecution::new(
            PostgresProviderModelExecution::new(
                repository,
                InProcessAttemptDispatchGate::default(),
                provider,
                None,
            ),
            signalboxd::WorkspaceInstructionRuntime::new(runtime.pool.clone(), None, Vec::new()),
        ));
    let compaction_model: Arc<dyn signalbox_model_provider_runtime::ContextCompactionModel> =
        Arc::new(RuntimeContextCompactionModel::new(
            ScriptedModel::<ModelCallId>::following(std::iter::empty::<Script>()),
            runtime_models.clone(),
        ));
    let mut pass = ContextGuardedTurnPass::new(
        StartEligibleTurnRepository::new(runtime.pool.clone()),
        guarded_repository,
        CommitAmbiguousCounter,
        NoToolCatalog,
        runtime_models,
        model_configuration,
        compaction_model,
        execution,
    )
    .with_workspace_instructions(signalboxd::WorkspaceInstructionRuntime::new(
        runtime.pool.clone(),
        None,
        Vec::new(),
    ));
    let session = SessionId::from_uuid(session_id.into_uuid());

    let outcome = pass.run(session).await;

    assert!(matches!(
        outcome,
        Err(ContextGuardedTurnPassError::Count {
            source: CommitAmbiguousCountFailure,
            ..
        })
    ));
    assert!(fatal_execution.is_triggered());

    drop(connection);
    runtime.stop().await
}
/// a daemon-minted compaction result identity that already names a durable record is reminted
/// before the provider is called, exactly as a colliding call identity already is. Discovering it
/// in `complete` instead would cost a paid summary and admit no remint, because the in-flight
/// lifecycle pins the identities by then. The rejected claim rolls back so the reminting caller can
/// reuse its user-global command.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn taken_compaction_result_identities_remint_before_sending() -> Result<(), Box<dyn Error>> {
    let mut runtime = RunningRuntime::start().await?;
    let (connection, session_id) = seed_completed_compaction_session(&mut runtime).await?;
    let repository = ContextCompactionRepository::new(runtime.pool.clone());
    let seeded = repository
        .prepare(direct_compaction_request(
            session_id,
            DurableCommandId::from_uuid(Uuid::from_u128(0xfa01)),
            None,
            0xfa10,
        ))
        .await?;
    let PrepareContextCompactionOutcome::Prepared(seeded) = seeded else {
        panic!("the result-identity fixture must prepare its first call");
    };
    repository.authorize(&seeded).await?;
    let applied = repository
        .complete(
            &seeded,
            "result identity fixture summary",
            ContextCompactionTokenUsage::unreported(),
        )
        .await?;

    let summary_command = DurableCommandId::from_uuid(Uuid::from_u128(0xfa02));
    let mut summary_collision =
        direct_compaction_request(session_id, summary_command, None, 0xfa20);
    summary_collision.summary_entry = applied.summary_entry;
    let summary_outcome = repository.prepare(summary_collision).await;
    let mut frontier_collision = direct_compaction_request(
        session_id,
        DurableCommandId::from_uuid(Uuid::from_u128(0xfa03)),
        None,
        0xfa30,
    );
    frontier_collision.result_frontier = applied.result_frontier;
    let frontier_outcome = repository.prepare(frontier_collision).await;
    let mut compaction_collision = direct_compaction_request(
        session_id,
        DurableCommandId::from_uuid(Uuid::from_u128(0xfa04)),
        None,
        0xfa40,
    );
    compaction_collision.compaction = applied.compaction;
    let compaction_outcome = repository.prepare(compaction_collision).await;

    assert!(matches!(
        summary_outcome,
        Err(ContextCompactionRepositoryError::IdentityCollision)
    ));
    assert!(matches!(
        frontier_outcome,
        Err(ContextCompactionRepositoryError::IdentityCollision)
    ));
    assert!(matches!(
        compaction_outcome,
        Err(ContextCompactionRepositoryError::IdentityCollision)
    ));
    let claimed: i64 =
        sqlx::query_scalar("SELECT count(*) FROM durable_command WHERE command_id = $1")
            .bind(summary_command.into_uuid())
            .fetch_one(&runtime.pool)
            .await?;
    assert_eq!(claimed, 0);
    let calls: i64 = sqlx::query_scalar(
        "SELECT count(*)
           FROM context_compaction_model_call
          WHERE session_id = $1 AND state_kind <> 'terminal'",
    )
    .bind(session_id.into_uuid())
    .fetch_one(&runtime.pool)
    .await?;
    assert_eq!(calls, 0);

    drop(connection);
    runtime.stop().await
}

/// a result identity taken after preparation fails the completion closed rather than surfacing as a
/// retryable database failure.
///
/// `complete_context_compaction_until_resolved` retries exactly the database
/// and ambiguous-commit classes, so classifying this decided uniqueness
/// violation as either would resubmit the identical rejected statement forever
/// and block the session with no error surfaced. The call stays in flight for
/// startup recovery, which is the audited path for a durable record whose
/// executor stopped.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn late_result_identity_collision_fails_completion_closed() -> Result<(), Box<dyn Error>> {
    let mut runtime = RunningRuntime::start().await?;
    let (connection, session_id) = seed_completed_compaction_session(&mut runtime).await?;
    let repository = ContextCompactionRepository::new(runtime.pool.clone());
    let outcome = repository
        .prepare(direct_compaction_request(
            session_id,
            DurableCommandId::from_uuid(Uuid::from_u128(0xfb01)),
            None,
            0xfb10,
        ))
        .await?;
    let PrepareContextCompactionOutcome::Prepared(prepared) = outcome else {
        panic!("the late-collision fixture must prepare its call");
    };
    repository.authorize(&prepared).await?;
    sqlx::query(
        "INSERT INTO context_frontier
            (owning_session_id, context_frontier_id, member_count)
         VALUES ($1, $2, 0)",
    )
    .bind(session_id.into_uuid())
    .bind(prepared.result_frontier().into_uuid())
    .execute(&runtime.pool)
    .await?;

    let outcome = repository
        .complete(
            &prepared,
            "late collision fixture summary",
            ContextCompactionTokenUsage::unreported(),
        )
        .await;

    assert!(matches!(
        outcome,
        Err(ContextCompactionRepositoryError::Corruption(
            ContextCompactionCorruption::Inconsistent("compaction result identity")
        ))
    ));
    assert!(!matches!(
        outcome,
        Err(ContextCompactionRepositoryError::Database(_)
            | ContextCompactionRepositoryError::CommitAmbiguous(_))
    ));
    let call_state: String = sqlx::query_scalar(
        "SELECT state_kind FROM context_compaction_model_call WHERE model_call_id = $1",
    )
    .bind(prepared.call().into_uuid())
    .fetch_one(&runtime.pool)
    .await?;
    assert_eq!(call_state, "in_flight");

    drop(connection);
    runtime.stop().await
}

pub(crate) fn reported_usage_compaction(
    runtime: &RunningRuntime,
    summary_runtime: ScriptedModel<ModelCallId>,
    model_configuration: HubModelConfiguration,
) -> Result<ReportedUsageCompaction, Box<dyn Error>> {
    let runtime_models = model_configuration.runtime_model_catalog();
    let model_calls = PostgresModelCallRepository::new(
        runtime.pool.clone(),
        model_configuration.target_catalog(),
        ModelCallCredentialReference::new("reported-usage-compaction-fixture"),
    )
    .with_session_credentials(model_configuration.credential_family_catalog());
    let compaction_model: Arc<dyn signalbox_model_provider_runtime::ContextCompactionModel> =
        Arc::new(RuntimeContextCompactionModel::new(
            summary_runtime,
            runtime_models.clone(),
        ));
    Ok(ReportedUsageCompaction::new(
        StartEligibleTurnRepository::new(runtime.pool.clone()),
        model_calls,
        NoToolCatalog,
        runtime_models,
        model_configuration,
        compaction_model,
    ))
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn reported_usage_activation_preview_compacts_once_forward_only() -> Result<(), Box<dyn Error>>
{
    const REPORTED_INPUT_TOKENS: u64 = 5_000; // numeric-bound: test - crosses configured context reservation threshold
    const REPORTED_OUTPUT_TOKENS: u64 = 1; // numeric-bound: test - completed output retained by the next input

    let configuration_text = reported_usage_preflight_configuration_text();
    let mut runtime = RunningRuntime::start_with_model_configuration(&configuration_text).await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let session_id = create_alias_session(&mut connection).await?;
    let (_, first_turn) = submit_first_input(
        &mut connection,
        session_id,
        String::from("reported usage historical request"),
    )
    .await?;
    let usage = TokenUsage {
        input_tokens: Some(REPORTED_INPUT_TOKENS),
        output_tokens: Some(REPORTED_OUTPUT_TOKENS),
        cache_creation_input_tokens: None,
        cache_read_input_tokens: None,
    };
    let first_runtime = ScriptedModel::single(completed_script(
        "fixture-model",
        "reported usage historical reply",
        usage,
    ));
    let first_probe = execute_streamed_turn_until_with_configuration(
        &mut runtime,
        first_runtime,
        reported_usage_preflight_configuration()?,
        session_id,
        first_turn,
        TurnSettle::Terminal,
    )
    .await?;
    connection
        .request_version(
            ProtocolVersion::One,
            3,
            ClientRequest::SubmitInput {
                command_id: command()?,
                session_id,
                content: UserInputContent::text(String::from("reported usage successor")),
                expected_defaults_version: Some(CanonicalU64::new(1)),
                model_settings: ModelSettingsOverlay::inherit_all(),
                delivery: None,
            },
        )
        .await?;
    let successor = accepted_successor_turn(&mut connection, session_id, 2).await?;
    let summary_runtime = ScriptedModel::single(completed_script(
        "fixture-model",
        "reported usage summary",
        TokenUsage::unreported(),
    ));
    let compaction = reported_usage_compaction(
        &runtime,
        summary_runtime,
        reported_usage_preflight_configuration()?,
    )?;

    compaction
        .compact_if_needed(SessionId::from_uuid(session_id.into_uuid()), None)
        .await?;
    compaction
        .compact_if_needed(SessionId::from_uuid(session_id.into_uuid()), None)
        .await?;
    #[derive(Debug, PartialEq, sqlx::FromRow)]
    struct CompactionLedger {
        compactions: i64,
        successor_compactions: i64,
        roots: i64,
    }
    let ledger: CompactionLedger = sqlx::query_as(
        "SELECT count(*) AS compactions,
                count(*) FILTER (WHERE command.automatic_for_turn_id = $2) AS successor_compactions,
                count(*) FILTER (WHERE compaction.predecessor_compaction_id IS NULL) AS roots
           FROM context_compaction AS compaction
           JOIN compact_session_command AS command
             ON command.session_id = compaction.session_id
            AND command.result_context_compaction_id = compaction.context_compaction_id
          WHERE compaction.session_id = $1",
    )
    .bind(session_id.into_uuid())
    .bind(successor.into_uuid())
    .fetch_one(&runtime.pool)
    .await?;

    assert_eq!(first_probe.received_operations().len(), 1);
    assert_eq!(
        ledger,
        CompactionLedger {
            compactions: 1,
            successor_compactions: 1,
            roots: 1
        }
    );

    drop(connection);
    runtime.stop().await
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn reported_usage_activation_preview_never_compacts_without_usage()
-> Result<(), Box<dyn Error>> {
    let configuration_text = reported_usage_preflight_configuration_text();
    let mut runtime = RunningRuntime::start_with_model_configuration(&configuration_text).await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let session_id = create_alias_session(&mut connection).await?;
    let (_, first_turn) = submit_first_input(
        &mut connection,
        session_id,
        String::from("unreported usage historical request"),
    )
    .await?;
    let first_runtime = ScriptedModel::single(completed_script(
        "fixture-model",
        "unreported usage historical reply",
        TokenUsage::unreported(),
    ));
    let first_probe = execute_streamed_turn_until_with_configuration(
        &mut runtime,
        first_runtime,
        reported_usage_preflight_configuration()?,
        session_id,
        first_turn,
        TurnSettle::Terminal,
    )
    .await?;
    connection
        .request_version(
            ProtocolVersion::One,
            3,
            ClientRequest::SubmitInput {
                command_id: command()?,
                session_id,
                content: UserInputContent::text(String::from("unreported usage successor")),
                expected_defaults_version: Some(CanonicalU64::new(1)),
                model_settings: ModelSettingsOverlay::inherit_all(),
                delivery: None,
            },
        )
        .await?;
    let _successor = accepted_successor_turn(&mut connection, session_id, 2).await?;
    let compaction = reported_usage_compaction(
        &runtime,
        ScriptedModel::following(std::iter::empty::<Script>()),
        reported_usage_preflight_configuration()?,
    )?;

    compaction
        .compact_if_needed(SessionId::from_uuid(session_id.into_uuid()), None)
        .await?;
    let compaction_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM context_compaction WHERE session_id = $1")
            .bind(session_id.into_uuid())
            .fetch_one(&runtime.pool)
            .await?;

    assert_eq!(first_probe.received_operations().len(), 1);
    assert_eq!(compaction_count, 0);

    drop(connection);
    runtime.stop().await
}

#[derive(Debug, PartialEq, sqlx::FromRow)]
struct CompactionAttachmentState {
    call_state: String,
    disposition: Option<String>,
    unsent: bool,
    command_result: String,
}

async fn completed_attachment_compaction_fixture(
    model: Option<ScriptedModel<ModelCallId>>,
) -> Result<(CommittedBlobReadFixture, CanonicalUuid), Box<dyn Error>> {
    let runtime =
        RunningRuntime::start_with_options(model, BlobStorageFixtureMode::Enabled, None, None)
            .await?;
    let mut fixture =
        CommittedBlobReadFixture::from_runtime(runtime, b"compaction attachment").await?;
    let session_id = create_alias_session(&mut fixture.connection).await?;
    fixture
        .connection
        .request(
            4,
            ClientRequest::SubmitInput {
                command_id: command()?,
                session_id,
                content: UserInputContent::from_parts(vec![UserInputPart::Attachment {
                    digest: fixture.wire_digest,
                    kind: UserAttachmentKind::File,
                    media_type: String::from("application/octet-stream"),
                    display_filename: None,
                }]),
                expected_defaults_version: Some(CanonicalU64::new(1)),
                model_settings: ModelSettingsOverlay::inherit_all(),
                delivery: None,
            },
        )
        .await?;
    let turn = accepted_successor_turn(&mut fixture.connection, session_id, 1).await?;
    let configuration = support::parse_model_configuration(
        &fixture
            .runtime
            .blob_storage_root
            .as_ref()
            .expect("blob fixture")
            .model_configuration(),
    )?;
    execute_recorded_turn(
        &mut fixture.runtime,
        RecordingCountedScriptedModel::following(
            [completed_script(
                "fixture-model",
                "attachment received",
                TokenUsage::unreported(),
            )],
            [],
        ),
        configuration,
        session_id,
        turn,
    )
    .await?;
    Ok((fixture, session_id))
}

async fn compaction_attachment_state(
    pool: &PgPool,
    command: CommandId,
) -> Result<CompactionAttachmentState, sqlx::Error> {
    sqlx::query_as(
        "SELECT call.state_kind AS call_state, call.terminal_disposition_kind AS disposition,
                call.in_flight_at IS NULL AS unsent, command.result_kind AS command_result
           FROM context_compaction_model_call AS call
           JOIN compact_session_command AS command
             ON command.session_id = call.session_id AND command.model_call_id = call.model_call_id
          WHERE command.command_id = $1",
    )
    .bind(command.into_uuid())
    .fetch_one(pool)
    .await
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn compaction_missing_attachment_never_authorizes() -> Result<(), Box<dyn Error>> {
    let (mut fixture, session_id) = completed_attachment_compaction_fixture(None).await?;
    fs::remove_file(fixture.object_path())?;
    let compaction_command = command()?;
    fixture
        .connection
        .request(
            5,
            ClientRequest::CompactSession {
                command_id: compaction_command,
                session_id,
                through_position: None,
            },
        )
        .await?;
    let response = response_within(&mut fixture.connection).await?;
    assert!(matches!(response.message(), ServerMessage::Error { .. }));
    let state = compaction_attachment_state(&fixture.runtime.pool, compaction_command).await?;
    assert_eq!(
        state,
        CompactionAttachmentState {
            call_state: String::from("terminal"),
            disposition: Some(String::from("known_failed")),
            unsent: true,
            command_result: String::from("failed"),
        }
    );
    fixture.stop().await
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn explicit_compaction_retries_its_prepared_call_after_catalog_recovery()
-> Result<(), Box<dyn Error>> {
    let model = ScriptedModel::following([completed_script(
        "fixture-model",
        "attachment summary",
        TokenUsage::unreported(),
    )]);
    let probe = model.clone();
    let (mut fixture, session_id) = completed_attachment_compaction_fixture(Some(model)).await?;
    sqlx::query("ALTER TABLE blob_replica RENAME TO fixture_unavailable_blob_replica")
        .execute(&fixture.runtime.pool)
        .await?;
    let compaction_command = command()?;
    fixture
        .connection
        .request(
            5,
            ClientRequest::CompactSession {
                command_id: compaction_command,
                session_id,
                through_position: None,
            },
        )
        .await?;
    let prepared_call: Uuid = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let call = sqlx::query_scalar::<_, Uuid>(
                "SELECT model_call_id FROM compact_session_command WHERE command_id = $1",
            )
            .bind(compaction_command.into_uuid())
            .fetch_optional(&fixture.runtime.pool)
            .await?;
            if let Some(call) = call {
                break Ok::<_, sqlx::Error>(call);
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await??;
    assert!(
        tokio::time::timeout(Duration::from_millis(150), fixture.connection.response())
            .await
            .is_err()
    );
    assert_eq!(probe.received_operations().len(), 0);
    assert_eq!(
        compaction_attachment_state(&fixture.runtime.pool, compaction_command).await?,
        CompactionAttachmentState {
            call_state: String::from("prepared"),
            disposition: None,
            unsent: true,
            command_result: String::from("pending"),
        }
    );

    sqlx::query("ALTER TABLE fixture_unavailable_blob_replica RENAME TO blob_replica")
        .execute(&fixture.runtime.pool)
        .await?;
    let response = response_within(&mut fixture.connection).await?;

    assert!(
        matches!(response.message(), ServerMessage::SessionCompacted { .. }),
        "{response:?}"
    );
    assert_eq!(probe.received_operations().len(), 1);
    let completed_call: Uuid = sqlx::query_scalar(
        "SELECT model_call_id FROM compact_session_command WHERE command_id = $1",
    )
    .bind(compaction_command.into_uuid())
    .fetch_one(&fixture.runtime.pool)
    .await?;
    assert_eq!(completed_call, prepared_call);
    assert_eq!(
        compaction_attachment_state(&fixture.runtime.pool, compaction_command)
            .await?
            .command_result,
        "applied"
    );
    fixture.stop().await
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn automatic_compaction_retries_its_prepared_call_after_attachment_recovery()
-> Result<(), Box<dyn Error>> {
    let mut fixture = CommittedBlobReadFixture::start(b"compaction attachment").await?;
    let session_id = create_alias_session(&mut fixture.connection).await?;
    fixture
        .connection
        .request(
            4,
            ClientRequest::SubmitInput {
                command_id: command()?,
                session_id,
                content: UserInputContent::from_parts(vec![UserInputPart::Attachment {
                    digest: fixture.wire_digest,
                    kind: UserAttachmentKind::File,
                    media_type: String::from("application/octet-stream"),
                    display_filename: None,
                }]),
                expected_defaults_version: Some(CanonicalU64::new(1)),
                model_settings: ModelSettingsOverlay::inherit_all(),
                delivery: None,
            },
        )
        .await?;
    let turn = accepted_successor_turn(&mut fixture.connection, session_id, 1).await?;
    let configuration = support::parse_model_configuration(
        &fixture
            .runtime
            .blob_storage_root
            .as_ref()
            .expect("blob fixture")
            .model_configuration()
            .replace("adapter = \"anthropic\"", "adapter = \"openai\""),
    )?;
    execute_recorded_turn(
        &mut fixture.runtime,
        RecordingCountedScriptedModel::following(
            [completed_script(
                "fixture-model",
                "attachment received",
                TokenUsage {
                    input_tokens: Some(500000),
                    output_tokens: Some(0),
                    cache_creation_input_tokens: None,
                    cache_read_input_tokens: None,
                },
            )],
            [],
        ),
        configuration.clone(),
        session_id,
        turn,
    )
    .await?;

    fixture
        .connection
        .request(
            6,
            ClientRequest::SubmitInput {
                command_id: command()?,
                session_id,
                content: UserInputContent::text(String::from("continue after compaction")),
                expected_defaults_version: Some(CanonicalU64::new(1)),
                model_settings: ModelSettingsOverlay::inherit_all(),
                delivery: None,
            },
        )
        .await?;
    let queued_turn = accepted_successor_turn(&mut fixture.connection, session_id, 2).await?;
    let reads = Arc::new(AtomicUsize::new(0));
    let mut registry = BlobStoreRegistry::initialize_for_conformance(
        configuration.blob_storage(),
        fixture.runtime.pool.clone(),
    )
    .await?
    .expect("configured blob storage");
    let (name, inner) = registry.routed_store(BlobStorageClass::UserAttachment);
    let name = name.clone();
    assert!(registry.replace_store_for_conformance(
        &name,
        Arc::new(TransientUnavailableBlobStore {
            inner,
            reads: Arc::clone(&reads),
        })
    ));
    let runtime_models = configuration.runtime_model_catalog();
    let summary_runtime = ScriptedModel::<ModelCallId>::following([completed_script(
        "fixture-model",
        "attachment summary",
        TokenUsage::unreported(),
    )]);
    let summary_probe = summary_runtime.clone();
    let model = Arc::new(RuntimeContextCompactionModel::new(
        summary_runtime,
        runtime_models.clone(),
    ));
    let repository = PostgresModelCallRepository::new(
        fixture.runtime.pool.clone(),
        configuration.target_catalog(),
        ModelCallCredentialReference::new("unavailable-compaction-fixture"),
    )
    .with_session_credentials(configuration.credential_family_catalog());
    let compaction = ReportedUsageCompaction::new(
        StartEligibleTurnRepository::new(fixture.runtime.pool.clone()),
        repository,
        NoToolCatalog,
        runtime_models,
        configuration,
        model,
    )
    .with_blob_store_registry(Some(Arc::new(registry)));

    let prepared_calls = Mutex::new(Vec::new());
    tokio::time::timeout(
        Duration::from_secs(5),
        compaction.compact_if_needed(
            SessionId::from_uuid(session_id.into_uuid()),
            Some(&|call| prepared_calls.lock().expect("prepared calls").push(call)),
        ),
    )
    .await??;

    assert_eq!(reads.load(Ordering::SeqCst), 2);
    assert_eq!(summary_probe.received_operations().len(), 1);
    let observed = prepared_calls.lock().expect("prepared calls").clone();
    assert_eq!(
        observed.len(),
        1,
        "retry must retain the same prepared call"
    );
    let stored_call: Uuid = sqlx::query_scalar(
        "SELECT model_call_id FROM compact_session_command WHERE automatic_for_turn_id = $1",
    )
    .bind(queued_turn.into_uuid())
    .fetch_one(&fixture.runtime.pool)
    .await?;
    assert_eq!(stored_call, observed[0].into_uuid());
    let state: (String, Option<String>, bool, String) = sqlx::query_as(
        "SELECT call.state_kind, call.terminal_disposition_kind, call.in_flight_at IS NULL,
                turn.state_kind
           FROM context_compaction_model_call AS call
           JOIN compact_session_command AS command USING (session_id, model_call_id)
           JOIN turn_lifecycle AS turn ON turn.session_id = command.session_id
            AND turn.turn_id = command.automatic_for_turn_id
          WHERE turn.turn_id = $1",
    )
    .bind(queued_turn.into_uuid())
    .fetch_one(&fixture.runtime.pool)
    .await?;
    assert_eq!(
        state,
        (
            String::from("terminal"),
            Some(String::from("completed")),
            false,
            String::from("queued")
        )
    );
    fixture.stop().await
}
