//! Tool-round authorization and recovery.

use crate::*;

#[track_caller]
pub(crate) fn assert_ambiguous_tool_recovery(outcome: StartupScanSessionOutcome) {
    match outcome {
        StartupScanSessionOutcome::RecoveredToolAttempt(outcome) => {
            assert!(matches!(*outcome, ToolAttemptCrashOutcome::Ambiguous(_)));
        }
        _ => panic!("fixture startup recovery must classify an ambiguous tool attempt"),
    }
}

#[track_caller]
pub(crate) fn process_tool_reconciliation_operation(
    state: &ProcessTurnState,
) -> (TurnAttemptId, ToolAttemptId) {
    match state {
        ProcessTurnState::ReconciliationRequired {
            terminal_attempt,
            operation: ProcessReconciliationOperation::ToolAttempt(attempt),
            ..
        } => (*terminal_attempt, *attempt),
        _ => panic!("fixture turn must require tool-attempt reconciliation"),
    }
}

#[track_caller]
pub(crate) fn assistant_tool_request(entries: &[ProcessTranscriptEntry]) -> ToolRequestId {
    entries
        .iter()
        .find_map(|entry| match entry {
            ProcessTranscriptEntry::AssistantToolUse { request, .. } => Some(*request),
            _ => None,
        })
        .expect("fixture transcript must carry assistant tool use")
}

#[track_caller]
pub(crate) fn closed_tool_request(entries: &[ProcessTranscriptEntry]) -> ToolRequestId {
    entries
        .iter()
        .find_map(|entry| match entry {
            ProcessTranscriptEntry::ToolClosed { request, .. } => Some(*request),
            _ => None,
        })
        .expect("fixture transcript must carry tool closure")
}

pub(crate) async fn dispatched_tool_reconciliation(
    pool: &PgPool,
    expected_turn: TurnId,
    expected_attempt: ToolAttemptId,
) -> Result<bool, OutboxDispatchError> {
    let mut dispatched = false;
    drain_outbox(pool, |event| {
        if matches!(
            event.kind(),
            DispatchedOutboxEventKind::TurnTerminal {
                turn,
                disposition: DispatchedTurnTerminalDisposition::ReconciliationRequired {
                    operation: DispatchedReconciliationOperation::ToolAttempt(attempt),
                    ..
                },
            } if *turn == expected_turn && *attempt == expected_attempt
        ) {
            dispatched = true;
        }
    })
    .await?;
    Ok(dispatched)
}

#[track_caller]
pub(crate) fn activated_turn(outcome: StartEligibleTurnOutcome) -> TurnId {
    match outcome {
        StartEligibleTurnOutcome::Activated(activated) => activated.turn(),
        StartEligibleTurnOutcome::NoEligibleTurn => {
            panic!("fixture successor must be eligible for activation")
        }
    }
}

/// Clones one recorded submission into a well-formed parked-approval interrupt
/// rejection naming `named_active_turn_id`, bypassing every domain guard. The
/// row satisfies each `submit_input_command` `CHECK` and foreign key, so only
/// the deferred correlation trigger can refuse it at commit.
pub(crate) async fn insert_parked_approval_interrupt_rejection(
    pool: &PgPool,
    command_id: Uuid,
    source_command_id: Uuid,
    named_active_turn_id: Uuid,
) -> Result<(), sqlx::Error> {
    let mut transaction = pool.begin().await?;
    sqlx::query(
        "INSERT INTO durable_command
            (command_id, command_kind, storage_version, claimed_at, issuer_kind)
         SELECT $1, command_kind, storage_version, transaction_timestamp(), 'operator'
           FROM durable_command
          WHERE command_id = $2",
    )
    .bind(command_id)
    .bind(source_command_id)
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO submit_input_command
            (command_id, command_kind, storage_version, session_id,
             actor_kind, actor_turn_id, actor_tool_request_id,
             delivery_kind, descendant_scope,
             expected_active_turn_id, expected_defaults_version,
             model_override_kind, replacement_model_kind,
             replacement_direct_model_selection_id, replacement_model_alias_id,
             result_kind, rejection_kind, result_session_id,
             result_accepted_input_id, result_turn_id,
             result_actual_active_turn_id, result_expected_active_turn_id,
             result_expected_defaults_version, result_current_defaults_version,
             result_unknown_alias_id, result_selected_defaults_version,
             result_last_position, result_existing_interrupt_command_id)
         SELECT
             $1, command_kind, storage_version, session_id,
             actor_kind, actor_turn_id, actor_tool_request_id,
             'interrupt', 'parent_alone',
             $3, expected_defaults_version,
             model_override_kind, replacement_model_kind,
             replacement_direct_model_selection_id, replacement_model_alias_id,
             'rejected', 'interrupt_unavailable_while_awaiting_approval',
             result_session_id,
             NULL, NULL,
             $3, NULL,
             NULL, NULL,
             NULL, NULL,
             NULL, NULL
           FROM submit_input_command
          WHERE command_id = $2",
    )
    .bind(command_id)
    .bind(source_command_id)
    .bind(named_active_turn_id)
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO submit_input_command_content_part
            (command_id, position, part_kind, text_value, blob_digest,
             attachment_kind, declared_media_type, display_filename)
         SELECT $1, position, part_kind, text_value, blob_digest,
                attachment_kind, declared_media_type, display_filename
           FROM submit_input_command_content_part
          WHERE command_id = $2",
    )
    .bind(command_id)
    .bind(source_command_id)
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await
}

pub(crate) async fn checkpoint_confirmed_tool_round_with_usage(
    pool: &PgPool,
    seed: u128,
    tool_name: &str,
    arguments: &str,
    usage: ProviderReportedTokenUsage,
) -> Result<
    (
        RestartModelCallFixture,
        PostgresModelCallRepository,
        CorrelatedModelCallTerminalObservation,
        signalbox_domain::ToolRequestId,
    ),
    Box<dyn Error>,
> {
    let (fixture, repository, observation, requests) =
        checkpoint_tool_batch_with_approval_and_usage(
            pool,
            seed,
            &[(tool_name, arguments)],
            InitialToolApproval::Confirm,
            usage,
        )
        .await?;
    let [request] = requests.as_slice() else {
        panic!("the single-proposal fixture returns one request")
    };
    Ok((fixture, repository, observation, *request))
}

pub(crate) async fn checkpoint_tool_batch_with_approval_and_usage(
    pool: &PgPool,
    seed: u128,
    proposals: &[(&str, &str)],
    initial_approval: InitialToolApproval,
    usage: ProviderReportedTokenUsage,
) -> Result<
    (
        RestartModelCallFixture,
        PostgresModelCallRepository,
        CorrelatedModelCallTerminalObservation,
        Vec<signalbox_domain::ToolRequestId>,
    ),
    Box<dyn Error>,
> {
    checkpoint_tool_batch_with_approval_and_usage_and_attachment(
        pool,
        seed,
        proposals,
        initial_approval,
        usage,
        None,
        None,
    )
    .await
}

/// The synthetic external-effect tool the ambiguity fixture proposes.
pub(crate) const AMBIGUITY_FIXTURE_TOOL: &str = "external-tool";

/// Declares the ambiguity fixture's tool the way a composed hub would.
///
/// The recovery path exercised below exists only for an external effect: the
/// application asks the catalog for the class, prepares an attempt of that
/// class, and the domain rejects an ambiguous observation against an
/// effect-free attempt outright. Naming the tool and separately handing
/// `ToolEffectClass::ExternalEffect` to the repository would freeze the class
/// on this test's own authority, leaving the fixture green for a pairing no
/// catalog declares. Declaring it once here and reading the class back through
/// the `ToolCatalog` port makes the declaration the single source of that
/// fact, so a declaration changed to `EffectFree` fails the recovery
/// assertions instead of quietly disagreeing with them.
///
/// The permission default has to agree with the fixture's durable approval
/// history for the same reason. `checkpoint_confirmed_tool_round` records an
/// `InitialToolApproval::Confirm` round closed by a user decision, and
/// `initial_tool_approval` maps an `Auto` declaration to `PolicyAuto`, so
/// declaring `Auto` here would describe a batch no application composes: the
/// recovery test would stay green while the auto-approved path it appeared to
/// cover was broken.
pub(crate) fn ambiguity_fixture_catalog() -> CompiledToolCatalog {
    let definition = ToolDefinition::new(
        ToolName::try_new(String::from(AMBIGUITY_FIXTURE_TOOL))
            .expect("the fixture tool name is admitted"),
        String::from("Synthetic external-effect tool for ambiguity fixtures"),
        ToolInputSchema::try_new(String::from(r#"{"type":"object"}"#))
            .expect("the fixture input schema is admitted"),
        ToolPermissionDefault::Confirm,
        ToolEffectClass::ExternalEffect,
    );
    CompiledToolCatalog::try_new([CompiledTool::new(
        definition,
        |_: &NormalizedToolArguments| -> Result<(), ToolExecutionErrorDetail> { Ok(()) },
    )])
    .expect("the single-tool fixture catalog is admitted")
}

/// What one dispatched event announces about the turn under test.
///
/// A named projection rather than a wildcard filter: the assertions below
/// claim an ambiguous external effect announces its proposal and its recovery
/// and *nothing definitive*, and that claim is only as good as the set of
/// kinds it considered. Adding a `DispatchedOutboxEventKind` variant makes the
/// match non-exhaustive and stops this crate compiling until the new kind is
/// classified, rather than letting a `_ => None` arm silently exclude a new
/// definitive announcement from the claim. Deliberately dependency-free, per
/// `docs/style.md`: the projection reads only the event and the two fixture
/// identities it is asked about.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum AmbiguityAnnouncement {
    /// A tool-batch presentation boundary for the batch under test.
    BatchTransition(DispatchedToolBatchState),
    /// The turn under test was announced as definitively resolved.
    DefinitiveTurnOutcome,
    /// Nothing that bears on the ambiguity claim.
    Unrelated,
}

pub(crate) fn announcement_for(
    kind: &DispatchedOutboxEventKind,
    fixture_turn: TurnId,
    fixture_call: ModelCallId,
) -> AmbiguityAnnouncement {
    match kind {
        DispatchedOutboxEventKind::ToolBatchTransition {
            turn,
            producing_call,
            state,
        } if *turn == fixture_turn && *producing_call == fixture_call => {
            AmbiguityAnnouncement::BatchTransition(*state)
        }
        // A turn announced completed, failed, refused, or cancelled has been
        // reported resolved one way or another; an effect that may or may not
        // have happened admits none of those. `TurnReconciliationRequired` is
        // the honest terminal announcement for ambiguity and is not definitive.
        DispatchedOutboxEventKind::TurnTerminal {
            turn,
            disposition:
                DispatchedTurnTerminalDisposition::Completed { .. }
                | DispatchedTurnTerminalDisposition::Failed { .. }
                | DispatchedTurnTerminalDisposition::Refused { .. }
                | DispatchedTurnTerminalDisposition::Cancelled { .. },
        } if *turn == fixture_turn => AmbiguityAnnouncement::DefinitiveTurnOutcome,
        DispatchedOutboxEventKind::ToolBatchTransition { .. }
        | DispatchedOutboxEventKind::TurnTerminal { .. }
        | DispatchedOutboxEventKind::SessionCreated(_)
        | DispatchedOutboxEventKind::SessionStateChanged(_)
        | DispatchedOutboxEventKind::SessionTerminal(_)
        | DispatchedOutboxEventKind::GoalChanged(_)
        | DispatchedOutboxEventKind::CommandSettled { .. }
        | DispatchedOutboxEventKind::InjectionSettled { .. }
        | DispatchedOutboxEventKind::SessionOwnershipChanged(_)
        | DispatchedOutboxEventKind::SessionModelSettingsChanged(_)
        | DispatchedOutboxEventKind::TurnModelSettingsResolved(_)
        | DispatchedOutboxEventKind::InputAccepted { .. }
        | DispatchedOutboxEventKind::TurnActivated { .. }
        | DispatchedOutboxEventKind::ModelCallTransition { .. }
        | DispatchedOutboxEventKind::ToolApprovalDecided { .. }
        | DispatchedOutboxEventKind::ContextCompacted { .. }
        | DispatchedOutboxEventKind::DelegationUpdate(_)
        | DispatchedOutboxEventKind::DelegationWake(_)
        | DispatchedOutboxEventKind::RunnerStateTransition { .. } => {
            AmbiguityAnnouncement::Unrelated
        }
    }
}

/// Role-aware identities for the classifier's straight-line cases.
///
/// The classifier reads only the turn and producing call it is asked about, so
/// its fixture needs distinct identities rather than particular ones. Minting
/// them by role keeps that fact visible and leaves no arbitrary hexadecimal in
/// the test body.
#[derive(Default)]
pub(crate) struct ClassifierFixtureIds {
    pub(crate) next: u128,
}

impl ClassifierFixtureIds {
    pub(crate) fn next_value(&mut self) -> Uuid {
        self.next += 1;
        Uuid::from_u128(0x9100 + self.next)
    }

    pub(crate) fn next_turn(&mut self) -> TurnId {
        TurnId::from_uuid(self.next_value())
    }

    pub(crate) fn next_call(&mut self) -> ModelCallId {
        ModelCallId::from_uuid(self.next_value())
    }

    pub(crate) fn next_tool_attempt(&mut self) -> ToolAttemptId {
        ToolAttemptId::from_uuid(self.next_value())
    }

    pub(crate) fn next_frontier(&mut self) -> ContextFrontierId {
        ContextFrontierId::from_uuid(self.next_value())
    }

    pub(crate) fn next_entry(&mut self) -> SemanticTranscriptEntryId {
        SemanticTranscriptEntryId::from_uuid(self.next_value())
    }
}

/// The batch states announced for `turn`/`call`, in dispatch order.
pub(crate) fn announced_batch_states(
    dispatched: &[DispatchedOutboxEventKind],
    turn: TurnId,
    call: ModelCallId,
) -> Vec<DispatchedToolBatchState> {
    dispatched
        .iter()
        .filter_map(|kind| match announcement_for(kind, turn, call) {
            AmbiguityAnnouncement::BatchTransition(state) => Some(state),
            AmbiguityAnnouncement::DefinitiveTurnOutcome | AmbiguityAnnouncement::Unrelated => None,
        })
        .collect()
}

/// The turns a dispatched batch announced as failed.
///
/// Matching over dispatched events is logic that `docs/agents/testing-style.md`
/// rule 2 keeps out of a test body, and reporting the turns rather than a bare
/// boolean lets a caller assert against the turn its fixture states (rule 6).
pub(crate) fn announced_failed_turns(dispatched: &[DispatchedOutboxEventKind]) -> Vec<TurnId> {
    dispatched
        .iter()
        .filter_map(|kind| match kind {
            DispatchedOutboxEventKind::TurnTerminal {
                turn,
                disposition: DispatchedTurnTerminalDisposition::Failed { .. },
            } => Some(*turn),
            _ => None,
        })
        .collect()
}

/// Whether anything announced `turn` as definitively resolved.
pub(crate) fn announces_a_definitive_turn_outcome(
    dispatched: &[DispatchedOutboxEventKind],
    turn: TurnId,
    call: ModelCallId,
) -> bool {
    dispatched.iter().any(|kind| {
        announcement_for(kind, turn, call) == AmbiguityAnnouncement::DefinitiveTurnOutcome
    })
}

/// Drives one checkpoint-confirmed tool round through approval, execution,
/// and the steering-free continuation transaction, then authorizes the
/// prepared continuation call for send, leaving it durably in flight.
pub(crate) async fn authorize_continuation_after_terminal_round(
    pool: &PgPool,
    seed: u128,
    preflight_error: Option<ToolExecutionError>,
) -> Result<
    (
        RestartModelCallFixture,
        PostgresModelCallRepository,
        ModelCallId,
        AuthorizedModelCall,
    ),
    Box<dyn Error>,
> {
    let (fixture, model_repository, _, request) =
        checkpoint_confirmed_tool_round(pool, seed, "current_time", "{}").await?;
    let tool_repository = PostgresToolLoopRepository::new(pool.clone());
    let continuation_attempt = TurnAttemptId::from_uuid(Uuid::from_u128(seed + 0x22));
    tool_repository
        .decide(
            decide_tool_request(
                DurableCommandId::from_uuid(Uuid::from_u128(seed + 0x21)),
                request,
                ToolApprovalDecision::Approve,
            ),
            || continuation_attempt,
        )
        .await?;
    let tool_attempt = ToolAttemptId::from_uuid(Uuid::from_u128(seed + 0x23));
    tool_repository
        .prepare_next_attempt(
            fixture.session,
            fixture.turn,
            tool_attempt,
            ToolEffectClass::EffectFree,
        )
        .await?;
    if let Some(error) = preflight_error {
        tool_repository
            .commit_preflight_error(fixture.session, fixture.turn, tool_attempt, error)
            .await?;
    } else {
        let authorized_attempt = tool_repository
            .authorize_attempt(fixture.session, fixture.turn, tool_attempt)
            .await?;
        tool_repository
            .commit_observation(
                authorized_attempt
                    .executor_fence()
                    .bind(ToolAttemptObservation::Completed {
                        result: ToolResultContent::Text(
                            ToolResultText::try_new(String::from("2026-07-26T12:00:00Z"))
                                .expect("bounded result"),
                        ),
                    }),
            )
            .await?;
    }
    let selection = signalbox_domain::DirectModelSelection::from_uuid(Uuid::from_u128(seed + 5));
    let provider = ProviderModelIdentity::from_uuid(Uuid::from_u128(seed + 6));
    let targets = ModelTargetCatalog::try_from_definitions([ModelTargetDefinition::new(
        selection,
        ResolvedProviderTarget::naming(provider),
    )])
    .expect("one continuation target forms a catalog");
    let continuation_call = ModelCallId::from_uuid(Uuid::from_u128(seed + 0x28));
    let continuation = PostgresToolLoopRepository::with_model_calls(
        pool.clone(),
        targets,
        model_credential_reference(),
    )
    .prepare_continuation(
        fixture.session,
        fixture.turn,
        fixture.call,
        signalbox_application::ToolContinuationIdentities::new(
            vec![SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(
                seed + 0x26,
            ))],
            ContextFrontierId::from_uuid(Uuid::from_u128(seed + 0x27)),
            continuation_call,
            FailedModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 0x29)),
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 0x2a)),
            ),
            ContextFrontierId::from_uuid(Uuid::from_u128(seed + 0x2b)),
        ),
        |_| panic!("the fixture has no pending steering"),
    )
    .await?;
    assert_eq!(
        continuation,
        signalbox_application::PrepareToolContinuationOutcome::Checkpointed(continuation_call)
    );
    let AuthorizeModelCallOutcome::Authorized(authorized) = model_repository
        .authorize_send(fixture.session, continuation_call)
        .await?
    else {
        panic!("the checkpointed continuation call authorizes for send")
    };
    Ok((fixture, model_repository, continuation_call, *authorized))
}
