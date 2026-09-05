//! PostgreSQL proof for the lifecycle command family
//! (docs/spec/session-lifecycle.md): claim and replay, recorded rejections,
//! the finish-condition validations, closures over live and parked turns, and
//! the recorded issuer principal.

use std::error::Error;

use crate::*;
use signalbox_domain::{
    CommandPrincipal, DescendantTerminationScope, DispatchingModule, FinishCondition,
    FinishConditionStatement, GoalCommandRejection, GoalCommandResult, GoalStatement,
    GoalUserAction, GoalUserCommand, LifecycleActor, ModuleDispatch, ParentTerminationKind,
    RepoWatchDispatchId, SessionCreationProvenance, SessionFailureCause,
    SessionLifecycleApplication, SessionLifecycleCommand, SessionLifecycleCommandRejection,
    SessionLifecycleCommandResult, SessionLifecycleOperation, SessionLifecycleState,
    SessionOwnership, SessionParkCause, SessionParkResponder, SessionRetryableCause,
    SessionTerminalOutcome, StartGate, StopStickiness,
};
use signalbox_persistence::{
    create_session::CreateSessionHandlingOutcome,
    session_lifecycle::SessionLifecycleRepository,
    session_lifecycle_command::{
        SessionLifecycleCommandHandlingOutcome, SessionLifecycleCommandRepository,
    },
};

const SEED: u128 = 0x11fe_8000;

fn interactive_creation(seed: u128) -> PreparedCreateSession {
    prepared(
        SEED + seed,
        SEED + seed + 0x100,
        direct(SEED + seed + 0x200),
    )
}

fn dispatched_creation(seed: u128) -> PreparedCreateSession {
    CreateSession::new(
        DurableCommandId::from_uuid(Uuid::from_u128(SEED + seed)),
        SessionCreationProvenance::module_dispatched(ModuleDispatch::RepositoryWatch {
            dispatch: RepoWatchDispatchId::from_uuid(Uuid::from_u128(SEED + seed + 0x300)),
        }),
        SessionConfigurationDefaults::new(direct(SEED + seed + 0x200)),
    )
    .prepare(SessionId::from_uuid(Uuid::from_u128(SEED + seed + 0x100)))
    .expect("a module-dispatched creation without ancestry is preparable")
}

fn creation_session(seed: u128) -> SessionId {
    SessionId::from_uuid(Uuid::from_u128(SEED + seed + 0x100))
}

fn lifecycle_command(
    seed: u128,
    ordinal: u128,
    session: SessionId,
    operation: SessionLifecycleOperation,
) -> SessionLifecycleCommand {
    SessionLifecycleCommand::new(
        DurableCommandId::from_uuid(Uuid::from_u128(SEED + seed + 0xd00 + ordinal * 0x10)),
        session,
        operation,
    )
}

fn declared(text: &str) -> FinishCondition {
    FinishCondition::Declared(
        FinishConditionStatement::try_new(String::from(text)).expect("fixture text is admitted"),
    )
}

async fn recorded(
    pool: &PgPool,
    command: SessionLifecycleCommand,
) -> Result<SessionLifecycleCommandResult, Box<dyn Error>> {
    match SessionLifecycleCommandRepository::new(pool.clone())
        .handle(command, CommandPrincipal::Operator)
        .await?
    {
        SessionLifecycleCommandHandlingOutcome::Recorded(result) => Ok(result),
        SessionLifecycleCommandHandlingOutcome::ConflictingReuse { command_id } => {
            panic!("command {command_id:?} conflicted with an earlier claim")
        }
    }
}

/// Reads the `command_settled` receipt one command appended.
async fn settlement(
    pool: &PgPool,
    command: DurableCommandId,
) -> Result<(String, Option<String>), sqlx::Error> {
    sqlx::query_as(
        "SELECT result_kind, rejection_kind FROM command_settled_outbox_event
          WHERE command_id = $1",
    )
    .bind(command.into_uuid())
    .fetch_one(pool)
    .await
}

async fn issuer(
    pool: &PgPool,
    command: DurableCommandId,
) -> Result<(String, Option<String>), sqlx::Error> {
    sqlx::query_as("SELECT issuer_kind, issuer_module FROM durable_command WHERE command_id = $1")
        .bind(command.into_uuid())
        .fetch_one(pool)
        .await
}

async fn turn_disposition(
    pool: &PgPool,
    turn: TurnId,
) -> Result<(String, Option<String>, Option<String>), sqlx::Error> {
    sqlx::query_as(
        "SELECT state_kind, terminal_disposition_kind, terminal_cause_kind
           FROM turn_lifecycle WHERE turn_id = $1",
    )
    .bind(turn.into_uuid())
    .fetch_one(pool)
    .await
}

async fn queue_turn(
    pool: &PgPool,
    session: SessionId,
    seed: u128,
    ordinal: u128,
) -> Result<TurnId, Box<dyn Error>> {
    let turn = TurnId::from_uuid(Uuid::from_u128(SEED + seed + 0x400 + ordinal));
    SubmitInputRepository::new(pool.clone())
        .handle(
            SubmitInput::new(
                DurableCommandId::from_uuid(Uuid::from_u128(SEED + seed + 0x500 + ordinal)),
                session,
                UserContent::try_text(String::from("lifecycle command fixture input"))
                    .expect("fixture content is admitted"),
                DeliveryRequest::StartWhenNoActiveTurn {
                    configuration: input_choices(1, ModelSelectionOverride::UseSessionDefault),
                },
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(SEED + seed + 0x600 + ordinal)),
            Some(turn),
        )
        .await?;
    Ok(turn)
}

async fn activate_turn(
    pool: &PgPool,
    session: SessionId,
    seed: u128,
) -> Result<(), Box<dyn Error>> {
    activate_earliest_queued_turn(
        pool,
        EarliestQueuedTurnActivation {
            session: session.into_uuid(),
            origin_entry: Uuid::from_u128(SEED + seed + 0x700),
            starting_frontier: Uuid::from_u128(SEED + seed + 0x800),
            initial_attempt: Uuid::from_u128(SEED + seed + 0x900),
        },
    )
    .await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_core_issued_interrupt_records_the_core_actor_and_envelope_principal()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let session = creation_session(30);
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(dispatched_creation(30))
        .await?;
    let live = queue_turn(&pool, session, 30, 1).await?;
    activate_turn(&pool, session, 30).await?;
    let command_id = DurableCommandId::from_uuid(Uuid::from_u128(SEED + 0xf30));
    let interrupt = SubmitInput::new_core_interrupt(
        command_id,
        session,
        UserContent::try_text(String::from("core lifecycle closure interrupt"))
            .expect("fixture content is admitted"),
        live,
        DescendantTerminationScope::ParentAlone,
        input_choices(1, ModelSelectionOverride::UseSessionDefault),
    );
    let successor = TurnId::from_uuid(Uuid::from_u128(SEED + 0xf31));
    let _outcome = SubmitInputRepository::new(pool.clone())
        .handle_with_candidates_alias_resolver_as(
            interrupt,
            CommandPrincipal::Core,
            ParentTerminationKind::Cancelled,
            AcceptedInputId::from_uuid(Uuid::from_u128(SEED + 0xf32)),
            Some(successor),
            CancelledModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(SEED + 0xf33)),
                ContextFrontierId::from_uuid(Uuid::from_u128(SEED + 0xf34)),
            ),
            |_| successor,
            |_| {
                (
                    Vec::new(),
                    ContextFrontierId::from_uuid(Uuid::from_u128(SEED + 0xf35)),
                )
            },
            || panic!("the fixture has no committed closure"),
            || panic!("the fixture has no committed closure"),
            |_| None,
        )
        .await?;

    assert_eq!(
        issuer(&pool, command_id).await?,
        (String::from("core"), None)
    );
    let actor: String =
        sqlx::query_scalar("SELECT actor_kind FROM submit_input_command WHERE command_id = $1")
            .bind(command_id.into_uuid())
            .fetch_one(&pool)
            .await?;
    assert_eq!(actor, "core");

    pool.close().await;
    drop(container);
    Ok(())
}

/// A lifecycle stop cancels the active approval judge and atomically denies
/// the parked delegated approval before its core interrupt settles the turn.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn lifecycle_stop_settles_an_awaiting_delegated_approval_turn() -> Result<(), Box<dyn Error>>
{
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0x11ff_9000;
    let (fixture, model_repository, _, requests) = checkpoint_tool_batch_with_approval(
        &pool,
        seed,
        &[("current_time", "{}")],
        InitialToolApproval::Delegated,
    )
    .await?;
    let [request] = requests.as_slice() else {
        panic!("the fixture has one delegated request")
    };
    let approval_repository = model_repository.approval_judge_repository();
    let judge_call = ModelCallId::from_uuid(Uuid::from_u128(seed + 0x58));
    let prepared = ready_approval_judge(
        approval_repository
            .prepare(fixture.session, fixture.turn, judge_call, None)
            .await?,
    );
    let _authorized = authorized_approval_judge(approval_repository.authorize(&prepared).await?);
    let terminal_outcome = SessionTerminalOutcome::Stopped {
        sticky: StopStickiness::Sticky,
    };
    let lifecycle_command = DurableCommandId::from_uuid(Uuid::from_u128(seed + 0x40));
    let committed = recorded(
        &pool,
        SessionLifecycleCommand::new(
            lifecycle_command,
            fixture.session,
            SessionLifecycleOperation::Stop {
                sticky: StopStickiness::Sticky,
                descendant_scope: DescendantTerminationScope::ParentAlone,
            },
        ),
    )
    .await?;
    assert_eq!(
        committed,
        SessionLifecycleCommandResult::Applied(SessionLifecycleApplication::ClosurePending {
            outcome: terminal_outcome,
            live_turn: fixture.turn,
            defaults_version: SessionConfigurationDefaultsVersion::first(),
        })
    );

    let rejected = SubmitInputRepository::new(pool.clone())
        .handle_with_candidates_alias_resolver_as(
            SubmitInput::new(
                DurableCommandId::from_uuid(Uuid::from_u128(seed + 0x50)),
                fixture.session,
                UserContent::try_text(String::from("reject before closing the approval wait"))
                    .expect("fixture content is admitted"),
                DeliveryRequest::Interrupt {
                    expected_active_turn: fixture.turn,
                    descendant_scope: DescendantTerminationScope::ParentAlone,
                    configuration: input_choices(2, ModelSelectionOverride::UseSessionDefault),
                },
            ),
            CommandPrincipal::Core,
            ParentTerminationKind::Stopped,
            AcceptedInputId::from_uuid(Uuid::from_u128(seed + 0x51)),
            Some(TurnId::from_uuid(Uuid::from_u128(seed + 0x52))),
            CancelledModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 0x53)),
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 0x54)),
            ),
            |_| TurnId::from_uuid(Uuid::from_u128(seed + 0x55)),
            |_| panic!("a rejected interrupt does not cancel the tool batch"),
            || DurableCommandId::from_uuid(Uuid::from_u128(seed + 0x56)),
            || TurnAttemptId::from_uuid(Uuid::from_u128(seed + 0x57)),
            |_| None,
        )
        .await?;
    assert_eq!(
        rejected,
        SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Rejected(
            SubmitInputRejectedResult::SessionDefaultsVersionMismatch {
                session: fixture.session,
                expected: SessionConfigurationDefaultsVersion::try_from_u64(2)
                    .expect("fixture version is positive"),
                current: SessionConfigurationDefaultsVersion::first(),
            }
        ))
    );
    let premature_decisions: i64 = sqlx::query_scalar(
        "SELECT count(*)
           FROM tool_approval_decision
          WHERE request_id = $1",
    )
    .bind(request.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(premature_decisions, 0);
    let active_judge: (String, Option<String>) = sqlx::query_as(
        "SELECT state_kind, terminal_disposition_kind
           FROM tool_approval_judge_model_call
          WHERE model_call_id = $1",
    )
    .bind(judge_call.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(active_judge, (String::from("in_flight"), None));

    let successor = TurnId::from_uuid(Uuid::from_u128(seed + 0x42));
    let closure_decision = DurableCommandId::from_uuid(Uuid::from_u128(seed + 0x48));
    let closure_attempt = TurnAttemptId::from_uuid(Uuid::from_u128(seed + 0x49));
    let settled = SubmitInputRepository::new(pool.clone())
        .handle_with_candidates_alias_resolver_as(
            SubmitInput::new(
                DurableCommandId::from_uuid(Uuid::from_u128(seed + 0x41)),
                fixture.session,
                UserContent::try_text(String::from("close the approval wait"))
                    .expect("fixture content is admitted"),
                DeliveryRequest::Interrupt {
                    expected_active_turn: fixture.turn,
                    descendant_scope: DescendantTerminationScope::ParentAlone,
                    configuration: input_choices(1, ModelSelectionOverride::UseSessionDefault),
                },
            ),
            CommandPrincipal::Core,
            ParentTerminationKind::Stopped,
            AcceptedInputId::from_uuid(Uuid::from_u128(seed + 0x43)),
            Some(successor),
            CancelledModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 0x44)),
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 0x45)),
            ),
            |_| successor,
            |requests| {
                assert_eq!(requests, [*request]);
                (
                    vec![SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(
                        seed + 0x46,
                    ))],
                    ContextFrontierId::from_uuid(Uuid::from_u128(seed + 0x47)),
                )
            },
            || closure_decision,
            || closure_attempt,
            |_| None,
        )
        .await?;
    let SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Applied(_)) = settled else {
        panic!("the closure interrupt must settle the parked turn");
    };

    let approval: (String, String, Option<String>, String) = sqlx::query_as(
        "SELECT approval.decision_kind, approval.decision_source,
                approval.denial_reason, command.issuer_kind
           FROM tool_approval_decision AS approval
           JOIN durable_command AS command
             ON command.command_id = approval.user_command_id
          WHERE request_id = $1",
    )
    .bind(request.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        approval,
        (
            String::from("deny"),
            String::from("lifecycle_closure"),
            None,
            String::from("core")
        )
    );
    let explicit_decision_events: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM tool_approval_decided_outbox_event WHERE request_id = $1",
    )
    .bind(request.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(explicit_decision_events, 0);
    let terminal_judge: (String, Option<String>) = sqlx::query_as(
        "SELECT state_kind, terminal_disposition_kind
           FROM tool_approval_judge_model_call
          WHERE model_call_id = $1",
    )
    .bind(judge_call.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        terminal_judge,
        (String::from("terminal"), Some(String::from("cancelled")))
    );
    let lifecycle = SessionLifecycleRepository::new(pool.clone())
        .load(fixture.session)
        .await?
        .expect("the session keeps its lifecycle row");
    assert_eq!(
        lifecycle.state(),
        SessionLifecycleState::Terminal {
            outcome: terminal_outcome,
        }
    );
    let mut dispatched = Vec::new();
    drain_outbox(&pool, |event| dispatched.push(event.clone())).await?;
    assert!(dispatched.iter().any(|event| {
        event.kind()
            == &DispatchedOutboxEventKind::CommandSettled {
                command: lifecycle_command,
                result: signalbox_persistence::outbox::DispatchedCommandSettlement::Applied,
            }
    }));

    pool.close().await;
    drop(container);
    Ok(())
}

/// Fails the live turn through the startup recovery scan: an authorized
/// failure transition that runs with every trigger armed.
async fn fail_live_turn(
    pool: &PgPool,
    session: SessionId,
    seed: u128,
) -> Result<(), Box<dyn Error>> {
    let mut ids = FixedStartupScanIds::new(
        [SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(
            SEED + seed + 0xa00,
        ))],
        [ContextFrontierId::from_uuid(Uuid::from_u128(
            SEED + seed + 0xa01,
        ))],
    );
    let outcome = PostgresStartupScanRepository::new(pool.clone())
        .recover(
            session,
            AcceptedInputTurnFailureIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(SEED + seed + 0xa00)),
                ContextFrontierId::from_uuid(Uuid::from_u128(SEED + seed + 0xa01)),
            ),
            &mut ids,
        )
        .await?;
    assert!(
        matches!(outcome, StartupScanSessionOutcome::Recovered(_)),
        "the live turn fails through the recovery scan"
    );
    Ok(())
}

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
                parked_cause = 'operator_hold',
                parked_responder = 'operator',
                parked_since = statement_timestamp()
          WHERE session_id = $1",
    )
    .bind(session.into_uuid())
    .execute(pool)
    .await?;
    Ok(())
}

/// A claimed stop closes the session, records its typed row and receipt
/// under the operator principal, replays for an equal retry, and refuses the
/// same identity under a different payload.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_stop_claims_records_its_receipt_and_replays() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let session = creation_session(1);
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(dispatched_creation(1))
        .await?;
    let stop = |sticky| {
        lifecycle_command(
            1,
            1,
            session,
            SessionLifecycleOperation::Stop {
                sticky,
                descendant_scope: DescendantTerminationScope::ParentAlone,
            },
        )
    };
    let outcome = SessionTerminalOutcome::Stopped {
        sticky: StopStickiness::Sticky,
    };

    let first = recorded(&pool, stop(StopStickiness::Sticky)).await?;
    let replay = recorded(&pool, stop(StopStickiness::Sticky)).await?;
    let conflict = SessionLifecycleCommandRepository::new(pool.clone())
        .handle(
            stop(StopStickiness::Redispatchable),
            CommandPrincipal::Operator,
        )
        .await?;

    assert_eq!(
        first,
        SessionLifecycleCommandResult::Applied(SessionLifecycleApplication::Closed { outcome })
    );
    assert_eq!(replay, first);
    assert!(matches!(
        conflict,
        SessionLifecycleCommandHandlingOutcome::ConflictingReuse { .. }
    ));
    let lifecycle = SessionLifecycleRepository::new(pool.clone())
        .load(session)
        .await?
        .expect("the session keeps its lifecycle row");
    assert_eq!(
        lifecycle.state(),
        SessionLifecycleState::Terminal { outcome }
    );
    assert_eq!(lifecycle.actor(), LifecycleActor::Operator);
    let command = stop(StopStickiness::Sticky).command_id();
    assert_eq!(
        settlement(&pool, command).await?,
        (String::from("applied"), None)
    );
    assert_eq!(
        issuer(&pool, command).await?,
        (String::from("operator"), None)
    );
    let typed: (String, Option<bool>) = sqlx::query_as(
        "SELECT operation_kind, stop_sticky FROM session_lifecycle_command WHERE command_id = $1",
    )
    .bind(command.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(typed, (String::from("stop"), Some(true)));

    pool.close().await;
    drop(container);
    Ok(())
}

/// An applied resume records the state its projection returned, so an
/// equal retry returns that receipt after the session moves again.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_resume_replays_its_recorded_state_after_the_session_closes() -> Result<(), Box<dyn Error>>
{
    let (container, pool, _database_url) = migrated_postgres().await?;
    let session = creation_session(0x42);
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(dispatched_creation(0x42))
        .await?;
    park_by_statement(&pool, session).await?;
    let resume = lifecycle_command(0x42, 1, session, SessionLifecycleOperation::Resume);

    let first = recorded(&pool, resume.clone()).await?;
    SessionLifecycleRepository::new(pool.clone())
        .close(
            session,
            SessionTerminalOutcome::FailedUnknown,
            LifecycleActor::Watchdog,
        )
        .await?;
    let replay = recorded(&pool, resume).await?;

    assert_eq!(
        first,
        SessionLifecycleCommandResult::Applied(SessionLifecycleApplication::Resumed {
            state: SessionLifecycleState::Created,
        })
    );
    assert_eq!(replay, first);

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_module_parked_pursuing_goal_accepts_a_lifecycle_resume() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0x43;
    let session = creation_session(seed);
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(dispatched_creation(seed))
        .await?;
    GoalRepository::new(pool.clone())
        .handle_user_command(
            GoalUserCommand::new(
                DurableCommandId::from_uuid(Uuid::from_u128(SEED + seed + 0xe00)),
                session,
                GoalUserAction::Attach(
                    GoalStatement::try_new(String::from("continue after module intervention"))
                        .expect("the fixture statement is admitted"),
                ),
            ),
            Some(GoalTurnCandidates::new(
                AcceptedInputId::from_uuid(Uuid::from_u128(SEED + seed + 0xe01)),
                TurnId::from_uuid(Uuid::from_u128(SEED + seed + 0xe02)),
            )),
            |_| None,
        )
        .await?;
    SessionLifecycleRepository::new(pool.clone())
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

    let resumed = recorded(
        &pool,
        lifecycle_command(seed, 1, session, SessionLifecycleOperation::Resume),
    )
    .await?;

    assert!(matches!(
        resumed,
        SessionLifecycleCommandResult::Applied(SessionLifecycleApplication::Resumed { state })
            if !state.is_parked()
    ));
    pool.close().await;
    drop(container);
    Ok(())
}

/// A descendant-scoped lifecycle stop with no live turn materializes the same
/// complete cascade as the deferred interrupt path.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn an_immediate_lifecycle_stop_materializes_its_descendant_cascade()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0x12fe_0000;
    let selection = DirectModelSelection::from_uuid(Uuid::from_u128(seed + 1));
    let child = SessionId::from_uuid(Uuid::from_u128(seed + 2));
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(prepared(
            seed + 3,
            seed + 2,
            ModelSelectionRequest::Direct(selection),
        ))
        .await?;
    let (parent, spawning_request) = attach_delegation_relationship_fixture(
        &pool,
        child,
        TurnId::from_uuid(Uuid::from_u128(seed + 4)),
        selection,
        seed + 0x100,
    )
    .await?;
    let stop = SessionLifecycleCommand::new(
        DurableCommandId::from_uuid(Uuid::from_u128(seed + 5)),
        parent,
        SessionLifecycleOperation::Stop {
            sticky: StopStickiness::Sticky,
            descendant_scope: DescendantTerminationScope::ParentAndDescendants,
        },
    );
    let command = stop.command_id();

    let applied = recorded(&pool, stop).await?;
    let cascade: (String, String, i64) = sqlx::query_as(
        "SELECT root_source_kind, termination_kind, disposition_count::bigint
           FROM session_delegation_termination_cascade
          WHERE root_command_id = $1",
    )
    .bind(command.into_uuid())
    .fetch_one(&pool)
    .await?;
    let disposition: (String, String, String, Uuid, i64) = sqlx::query_as(
        "SELECT outcome_kind, reason_kind, provenance_kind, provenance_command_id,
                event_ordinal::bigint
           FROM session_delegation_event
          WHERE spawning_tool_request_id = $1 AND provenance_command_id = $2",
    )
    .bind(spawning_request.into_uuid())
    .bind(command.into_uuid())
    .fetch_one(&pool)
    .await?;
    let dispatched_ordinal = u64::try_from(disposition.4)?;
    let mut dispatched = Vec::new();
    drain_outbox(&pool, |event| dispatched.push(event.clone())).await?;

    assert_eq!(
        applied,
        SessionLifecycleCommandResult::Applied(SessionLifecycleApplication::Closed {
            outcome: SessionTerminalOutcome::Stopped {
                sticky: StopStickiness::Sticky,
            },
        })
    );
    assert_eq!(
        cascade,
        (
            String::from("lifecycle_command"),
            String::from("stopped"),
            1
        )
    );
    assert_eq!(
        (
            disposition.0.as_str(),
            disposition.1.as_str(),
            disposition.2.as_str(),
            disposition.3,
        ),
        (
            "continue_running",
            "parent_stopped_parent_and_descendants",
            "parent_lifecycle_command",
            command.into_uuid(),
        )
    );
    assert!(dispatched.iter().any(|event| event.kind()
        == &DispatchedOutboxEventKind::DelegationUpdate(
            DispatchedDelegationUpdate::ChildLifecycleDisposition {
                spawning_request,
                child,
                event_ordinal: dispatched_ordinal,
                outcome: DispatchedDelegationOutcome::ContinueRunning,
                reason: DispatchedDelegationReason::ParentStoppedWithDescendants,
                provenance: DispatchedDelegationProvenance::ParentLifecycleCommand {
                    session: parent,
                    command,
                },
            }
        )));

    pool.close().await;
    drop(container);
    Ok(())
}

/// A descendant-scoped stop whose live turn settles naturally materializes
/// its cascade at the same deferred settlement that closes the parent.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_deferred_lifecycle_stop_materializes_its_descendant_cascade()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0x12fe_1000;
    let selection = DirectModelSelection::from_uuid(Uuid::from_u128(seed + 1));
    let child = SessionId::from_uuid(Uuid::from_u128(seed + 2));
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(prepared(
            seed + 3,
            seed + 2,
            ModelSelectionRequest::Direct(selection),
        ))
        .await?;
    let (parent, _) = attach_delegation_relationship_fixture(
        &pool,
        child,
        TurnId::from_uuid(Uuid::from_u128(seed + 4)),
        selection,
        seed + 0x100,
    )
    .await?;
    let live = queue_turn(&pool, parent, seed, 1).await?;
    activate_turn(&pool, parent, seed).await?;
    let stop = SessionLifecycleCommand::new(
        DurableCommandId::from_uuid(Uuid::from_u128(seed + 5)),
        parent,
        SessionLifecycleOperation::Stop {
            sticky: StopStickiness::Sticky,
            descendant_scope: DescendantTerminationScope::ParentAndDescendants,
        },
    );
    let command = stop.command_id();

    let applied = recorded(&pool, stop).await?;
    fail_live_turn(&pool, parent, seed).await?;
    let cascade: (String, String, i64) = sqlx::query_as(
        "SELECT root_source_kind, termination_kind, disposition_count::bigint
           FROM session_delegation_termination_cascade
          WHERE root_command_id = $1",
    )
    .bind(command.into_uuid())
    .fetch_one(&pool)
    .await?;
    let settled = SessionLifecycleRepository::new(pool.clone())
        .load(parent)
        .await?
        .expect("the parent keeps its lifecycle row");

    assert_eq!(
        applied,
        SessionLifecycleCommandResult::Applied(SessionLifecycleApplication::ClosurePending {
            outcome: SessionTerminalOutcome::Stopped {
                sticky: StopStickiness::Sticky,
            },
            live_turn: live,
            defaults_version: SessionConfigurationDefaultsVersion::first(),
        })
    );
    assert_eq!(
        cascade,
        (
            String::from("lifecycle_command"),
            String::from("stopped"),
            1
        )
    );
    assert_eq!(
        settled.state(),
        SessionLifecycleState::Terminal {
            outcome: SessionTerminalOutcome::Stopped {
                sticky: StopStickiness::Sticky,
            },
        }
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// A synthetic closure interrupt owns the descendant cascade it materializes;
/// deferred lifecycle settlement recognizes that proof instead of rebuilding
/// the same bound-child terminal frontier under the lifecycle command.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn an_interrupt_settled_stop_does_not_rematerialize_its_descendant_cascade()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0x12fe_2000;
    let selection = DirectModelSelection::from_uuid(Uuid::from_u128(seed + 1));
    let child = SessionId::from_uuid(Uuid::from_u128(seed + 2));
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(prepared(
            seed + 3,
            seed + 2,
            ModelSelectionRequest::Direct(selection),
        ))
        .await?;
    let child_turn = queue_turn(&pool, child, seed + 0x100, 1).await?;
    sqlx::query("ALTER TABLE turn_lifecycle DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query(
        "UPDATE turn_lifecycle
            SET origin_kind = 'delegation', origin_accepted_input_id = NULL
          WHERE session_id = $1 AND turn_id = $2",
    )
    .bind(child.into_uuid())
    .bind(child_turn.into_uuid())
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE turn_lifecycle ENABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    let (parent, spawning_request) =
        attach_delegation_relationship_fixture(&pool, child, child_turn, selection, seed + 0x200)
            .await?;
    sqlx::query(
        "ALTER TABLE session_delegation
         DISABLE TRIGGER session_delegation_is_append_only",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE session_delegation
            SET policy_kind = 'bound',
                on_parent_stopped = 'stop',
                on_parent_cancelled = 'cancel'
          WHERE spawning_tool_request_id = $1",
    )
    .bind(spawning_request.into_uuid())
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE session_delegation
         ENABLE TRIGGER session_delegation_is_append_only",
    )
    .execute(&pool)
    .await?;
    let live = queue_turn(&pool, parent, seed, 1).await?;
    activate_turn(&pool, parent, seed).await?;
    let stop = lifecycle_command(
        seed,
        1,
        parent,
        SessionLifecycleOperation::Stop {
            sticky: StopStickiness::Sticky,
            descendant_scope: DescendantTerminationScope::ParentAndDescendants,
        },
    );
    let committed = recorded(&pool, stop).await?;
    let interrupt_command = DurableCommandId::from_uuid(Uuid::from_u128(seed + 0x300));
    let successor = TurnId::from_uuid(Uuid::from_u128(seed + 0x301));

    let interrupted = SubmitInputRepository::new(pool.clone())
        .handle_with_candidates_alias_resolver_as(
            SubmitInput::new_core_interrupt(
                interrupt_command,
                parent,
                UserContent::try_text(String::from("settle the descendant-scoped closure"))
                    .expect("fixture content is admitted"),
                live,
                DescendantTerminationScope::ParentAndDescendants,
                input_choices(1, ModelSelectionOverride::UseSessionDefault),
            ),
            CommandPrincipal::Core,
            ParentTerminationKind::Stopped,
            AcceptedInputId::from_uuid(Uuid::from_u128(seed + 0x302)),
            Some(successor),
            CancelledModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 0x303)),
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 0x304)),
            ),
            |_| successor,
            |_| {
                (
                    Vec::new(),
                    ContextFrontierId::from_uuid(Uuid::from_u128(seed + 0x305)),
                )
            },
            || panic!("the fixture has no approval wait"),
            || panic!("the fixture has no approval wait"),
            |_| None,
        )
        .await?;

    assert_eq!(
        committed,
        SessionLifecycleCommandResult::Applied(SessionLifecycleApplication::ClosurePending {
            outcome: SessionTerminalOutcome::Stopped {
                sticky: StopStickiness::Sticky,
            },
            live_turn: live,
            defaults_version: SessionConfigurationDefaultsVersion::first(),
        })
    );
    assert!(matches!(
        interrupted,
        SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Applied(_))
    ));
    let cascades: Vec<(Uuid, String)> = sqlx::query_as(
        "SELECT root_command_id, root_source_kind
           FROM session_delegation_termination_cascade
          WHERE root_session_id = $1 AND termination_kind = 'stopped'",
    )
    .bind(parent.into_uuid())
    .fetch_all(&pool)
    .await?;
    assert_eq!(
        cascades,
        vec![(interrupt_command.into_uuid(), String::from("turn_command"))]
    );
    let logical_terminal_root: Uuid = sqlx::query_scalar(
        "SELECT root_command_id FROM session_delegation_logical_terminal
          WHERE spawning_tool_request_id = $1",
    )
    .bind(spawning_request.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(logical_terminal_root, interrupt_command.into_uuid());
    let settled = SessionLifecycleRepository::new(pool.clone())
        .load(parent)
        .await?
        .expect("the parent keeps its lifecycle row");
    assert_eq!(
        settled.state(),
        SessionLifecycleState::Terminal {
            outcome: SessionTerminalOutcome::Stopped {
                sticky: StopStickiness::Sticky,
            },
        }
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// The typed command record admits only the effect its operation can produce.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_lifecycle_command_effect_must_match_its_operation() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let session = creation_session(32);
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(dispatched_creation(32))
        .await?;
    let command = lifecycle_command(32, 1, session, SessionLifecycleOperation::Release);
    let mut transaction = pool.begin().await?;
    sqlx::query(
        "INSERT INTO durable_command
            (command_id, command_kind, storage_version, claimed_at, issuer_kind)
         VALUES ($1, 'session_lifecycle', 1, transaction_timestamp(), 'operator')",
    )
    .bind(command.command_id().into_uuid())
    .execute(&mut *transaction)
    .await?;

    let error = sqlx::query(
        "INSERT INTO session_lifecycle_command
            (command_id, command_kind, storage_version, session_id,
             operation_kind, result_kind, applied_effect_kind)
         VALUES ($1, 'session_lifecycle', 1, $2, 'release', 'applied', 'closed')",
    )
    .bind(command.command_id().into_uuid())
    .bind(session.into_uuid())
    .execute(&mut *transaction)
    .await
    .expect_err("release cannot carry a closed effect");
    assert_eq!(
        error
            .as_database_error()
            .and_then(|error| error.constraint()),
        Some("session_lifecycle_command_result_shape")
    );

    transaction.rollback().await?;
    pool.close().await;
    drop(container);
    Ok(())
}

/// A refused command still claims its identity and records the closed
/// rejection with its receipt; a retry replays the rejection.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_refused_command_records_its_rejection() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let session = creation_session(2);
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(dispatched_creation(2))
        .await?;
    let abandon = || lifecycle_command(2, 1, session, SessionLifecycleOperation::Abandon);
    let unknown = lifecycle_command(
        2,
        2,
        SessionId::from_uuid(Uuid::from_u128(SEED + 0xbeef)),
        SessionLifecycleOperation::Release,
    );

    let refused = recorded(&pool, abandon()).await?;
    let replay = recorded(&pool, abandon()).await?;
    let missing = recorded(&pool, unknown.clone()).await?;

    assert_eq!(
        refused,
        SessionLifecycleCommandResult::Rejected(SessionLifecycleCommandRejection::RequiresParked)
    );
    assert_eq!(replay, refused);
    assert_eq!(
        missing,
        SessionLifecycleCommandResult::Rejected(SessionLifecycleCommandRejection::SessionNotFound)
    );
    assert_eq!(
        settlement(&pool, abandon().command_id()).await?,
        (
            String::from("rejected"),
            Some(String::from("requires_parked"))
        )
    );
    assert_eq!(
        settlement(&pool, unknown.command_id()).await?,
        (
            String::from("rejected"),
            Some(String::from("session_not_found"))
        )
    );
    assert!(
        !SessionLifecycleRepository::new(pool.clone())
            .load(session)
            .await?
            .expect("the session keeps its lifecycle row")
            .state()
            .is_terminal()
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// An owned creation without a finish condition, or a held gate on an
/// unmonitored one, is a recorded rejection that creates no session row and
/// replays; an owned creation declaring its condition carries it.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn creation_admits_every_lifecycle_shape() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let repository = CreateSessionRepository::new(pool.clone(), test_session_credential_pin());
    let creation = |seed: u128, gate, ownership, condition| {
        CreateSession::new(
            DurableCommandId::from_uuid(Uuid::from_u128(SEED + seed)),
            SessionCreationProvenance::new(
                SessionCreationCause::Interactive,
                TranscriptAncestry::None,
            ),
            SessionConfigurationDefaults::new(direct(SEED + seed + 0x200)),
        )
        .with_lifecycle(gate, ownership, condition)
        .prepare(creation_session(seed))
        .expect("the creation is preparable")
    };

    let unconditioned = repository
        .handle(creation(3, StartGate::Open, SessionOwnership::Owned, None))
        .await?;
    let replay = repository
        .handle(creation(3, StartGate::Open, SessionOwnership::Owned, None))
        .await?;
    let held = repository
        .handle(creation(
            4,
            StartGate::Held,
            SessionOwnership::Unmonitored,
            None,
        ))
        .await?;
    let declared_creation = repository
        .handle(creation(
            5,
            StartGate::Open,
            SessionOwnership::Owned,
            Some(declared("the fixture branch is green")),
        ))
        .await?;

    assert!(matches!(
        unconditioned,
        CreateSessionHandlingOutcome::Applied(_)
    ));
    assert_eq!(replay, unconditioned);
    assert!(matches!(held, CreateSessionHandlingOutcome::Applied(_)));
    assert!(matches!(
        declared_creation,
        CreateSessionHandlingOutcome::Applied(_)
    ));
    let held_gate: (String, bool) = sqlx::query_as(
        "SELECT state_kind, start_gate_held FROM session_lifecycle WHERE session_id = $1",
    )
    .bind(creation_session(4).into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(held_gate, (String::from("created"), true));
    let unconditioned = SessionLifecycleRepository::new(pool.clone())
        .load(creation_session(3))
        .await?
        .expect("the owned creation has its lifecycle row");
    assert_eq!(unconditioned.ownership(), SessionOwnership::Owned);
    assert_eq!(unconditioned.finish_condition(), None);
    let lifecycle = SessionLifecycleRepository::new(pool.clone())
        .load(creation_session(5))
        .await?
        .expect("the declared creation has its lifecycle row");
    assert_eq!(lifecycle.ownership(), SessionOwnership::Owned);
    assert_eq!(
        lifecycle.finish_condition(),
        Some(&declared("the fixture branch is green"))
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// A held start gate keeps an owned creation `created` through its first
/// input and arms the start-gate deadline.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_held_start_gate_keeps_the_session_created() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let session = creation_session(10);
    let creation = CreateSession::new(
        DurableCommandId::from_uuid(Uuid::from_u128(SEED + 10)),
        SessionCreationProvenance::new(SessionCreationCause::Interactive, TranscriptAncestry::None),
        SessionConfigurationDefaults::new(direct(SEED + 10 + 0x200)),
    )
    .with_lifecycle(
        StartGate::Held,
        SessionOwnership::Owned,
        Some(declared("the gate opens on release")),
    )
    .prepare(session)
    .expect("the creation is preparable");
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(creation)
        .await?;

    queue_turn(&pool, session, 10, 1).await?;
    let activation = StartEligibleTurnRepository::new(pool.clone())
        .handle(
            session,
            AcceptedInputTurnActivationIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(SEED + 10 + 0x700)),
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(SEED + 10 + 0x701)),
                ContextFrontierId::from_uuid(Uuid::from_u128(SEED + 10 + 0x800)),
                TurnAttemptId::from_uuid(Uuid::from_u128(SEED + 10 + 0x900)),
            ),
        )
        .await?;
    assert!(matches!(
        activation,
        StartEligibleTurnOutcome::NoEligibleTurn
    ));

    let lifecycle = SessionLifecycleRepository::new(pool.clone())
        .load(session)
        .await?
        .expect("the session keeps its lifecycle row");
    assert_eq!(lifecycle.state(), SessionLifecycleState::Created);

    let release = lifecycle_command(10, 2, session, SessionLifecycleOperation::ReleaseStart);
    let release_id = release.command_id();
    assert_eq!(
        recorded(&pool, release.clone()).await?,
        SessionLifecycleCommandResult::Applied(SessionLifecycleApplication::StartReleased)
    );
    assert_eq!(
        recorded(&pool, release).await?,
        SessionLifecycleCommandResult::Applied(SessionLifecycleApplication::StartReleased)
    );
    assert_eq!(
        settlement(&pool, release_id).await?,
        (String::from("applied"), None)
    );
    let released: (String, bool) = sqlx::query_as(
        "SELECT state_kind, start_gate_held FROM session_lifecycle WHERE session_id = $1",
    )
    .bind(session.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(released, (String::from("dispatched"), false));

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn releasing_ownership_also_settles_a_held_start_gate() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let session = creation_session(11);
    let creation = CreateSession::new(
        DurableCommandId::from_uuid(Uuid::from_u128(SEED + 11)),
        SessionCreationProvenance::new(SessionCreationCause::Interactive, TranscriptAncestry::None),
        SessionConfigurationDefaults::new(direct(SEED + 11 + 0x200)),
    )
    .with_lifecycle(StartGate::Held, SessionOwnership::Owned, None)
    .prepare(session)
    .expect("the creation is preparable");
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(creation)
        .await?;
    queue_turn(&pool, session, 11, 1).await?;

    assert_eq!(
        recorded(
            &pool,
            lifecycle_command(11, 2, session, SessionLifecycleOperation::Release),
        )
        .await?,
        SessionLifecycleCommandResult::Applied(SessionLifecycleApplication::OwnershipChanged)
    );
    let released: (String, bool, bool) = sqlx::query_as(
        "SELECT state_kind, start_gate_held, owned
           FROM session_lifecycle WHERE session_id = $1",
    )
    .bind(session.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(released, (String::from("dispatched"), false, false));

    pool.close().await;
    drop(container);
    Ok(())
}

/// Adopt declares the finish condition an owned session owes when it
/// carries none, and refuses to redeclare one it already carries.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn adopt_takes_an_unmonitored_session_with_or_without_a_condition()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let creation = CreateSessionRepository::new(pool.clone(), test_session_credential_pin());
    let conversation = creation_session(6);
    let dispatched = creation_session(7);
    creation.handle(interactive_creation(6)).await?;
    creation.handle(dispatched_creation(7)).await?;
    let adopt = |seed, ordinal, session, condition| {
        lifecycle_command(
            seed,
            ordinal,
            session,
            SessionLifecycleOperation::Adopt {
                finish_condition: condition,
            },
        )
    };

    let bare = recorded(&pool, adopt(6, 1, conversation, None)).await?;
    let declared_adopt = recorded(
        &pool,
        adopt(
            6,
            2,
            conversation,
            Some(declared("all fixture threads resolved")),
        ),
    )
    .await?;
    let again = recorded(&pool, adopt(6, 3, conversation, None)).await?;
    recorded(
        &pool,
        lifecycle_command(7, 1, dispatched, SessionLifecycleOperation::Release),
    )
    .await?;
    let redeclared = recorded(
        &pool,
        adopt(7, 2, dispatched, Some(declared("a second condition"))),
    )
    .await?;
    let readopted = recorded(&pool, adopt(7, 3, dispatched, None)).await?;

    assert_eq!(
        bare,
        SessionLifecycleCommandResult::Applied(SessionLifecycleApplication::OwnershipChanged)
    );
    assert_eq!(
        declared_adopt,
        SessionLifecycleCommandResult::Rejected(
            SessionLifecycleCommandRejection::OwnershipUnchanged
        )
    );
    assert_eq!(
        again,
        SessionLifecycleCommandResult::Rejected(
            SessionLifecycleCommandRejection::OwnershipUnchanged
        )
    );
    assert_eq!(
        redeclared,
        SessionLifecycleCommandResult::Rejected(
            SessionLifecycleCommandRejection::FinishConditionAlreadyDeclared
        )
    );
    assert_eq!(
        readopted,
        SessionLifecycleCommandResult::Applied(SessionLifecycleApplication::OwnershipChanged)
    );
    let lifecycle = SessionLifecycleRepository::new(pool.clone())
        .load(conversation)
        .await?
        .expect("the conversation keeps its lifecycle row");
    assert_eq!(lifecycle.ownership(), SessionOwnership::Owned);
    assert_eq!(lifecycle.finish_condition(), None);

    pool.close().await;
    drop(container);
    Ok(())
}

/// A closure over a live turn commits its outcome to the handoff and
/// reports the turn to settle; the transaction that terminalizes that turn
/// retires the queued successor and records terminal.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_closure_over_a_live_turn_settles_when_the_turn_does() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let session = creation_session(8);
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(dispatched_creation(8))
        .await?;
    let live = queue_turn(&pool, session, 8, 1).await?;
    let queued = queue_turn(&pool, session, 8, 2).await?;
    activate_turn(&pool, session, 8).await?;
    let outcome = SessionTerminalOutcome::Superseded {
        by: Some(creation_session(9)),
    };
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(dispatched_creation(9))
        .await?;
    let repository = SessionLifecycleRepository::new(pool.clone());

    let committed = recorded(
        &pool,
        lifecycle_command(
            8,
            1,
            session,
            SessionLifecycleOperation::Supersede {
                successor: creation_session(9),
            },
        ),
    )
    .await?;
    let pending = repository
        .load(session)
        .await?
        .expect("the session keeps its lifecycle row");
    let closing = GoalRepository::new(pool.clone())
        .handle_user_command(
            GoalUserCommand::new(
                DurableCommandId::from_uuid(Uuid::from_u128(SEED + 8 + 0xe00)),
                session,
                GoalUserAction::Attach(
                    GoalStatement::try_new(String::from("late goal"))
                        .expect("the fixture statement is admitted"),
                ),
            ),
            Some(GoalTurnCandidates::new(
                AcceptedInputId::from_uuid(Uuid::from_u128(SEED + 8 + 0xe01)),
                TurnId::from_uuid(Uuid::from_u128(SEED + 8 + 0xe02)),
            )),
            |_| None,
        )
        .await?;
    fail_live_turn(&pool, session, 8).await?;
    let settled = repository
        .load(session)
        .await?
        .expect("the session keeps its lifecycle row");

    assert_eq!(
        committed,
        SessionLifecycleCommandResult::Applied(SessionLifecycleApplication::ClosurePending {
            outcome,
            live_turn: live,
            defaults_version: SessionConfigurationDefaultsVersion::first(),
        })
    );
    assert_eq!(pending.pending_terminal(), Some(outcome));
    assert!(!pending.state().is_terminal());
    assert_eq!(
        closing,
        GoalCommandHandlingOutcome::Recorded(GoalCommandResult::Rejected(
            GoalCommandRejection::SessionClosing
        ))
    );
    assert_eq!(settled.state(), SessionLifecycleState::Terminal { outcome });
    assert_eq!(settled.actor(), LifecycleActor::Operator);
    assert_eq!(
        turn_disposition(&pool, queued).await?,
        (
            String::from("terminal"),
            Some(String::from("retired")),
            Some(String::from("session_closed"))
        )
    );
    let published: bool = sqlx::query_scalar(
        "SELECT EXISTS (
            SELECT 1 FROM turn_terminal_outbox_event
             WHERE turn_id = $1 AND disposition_kind = 'retired'
        )",
    )
    .bind(queued.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert!(
        published,
        "the retired successor publishes its terminal event"
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// A park closure settles the suspended turn through the committed
/// interrupt machinery; a possibly-executed call terminalizes
/// `reconciliation_required`, never `cancelled`, and deferred settlement
/// preserves the same parked failure evidence as immediate closure.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_park_closure_settles_its_turn_and_preserves_failure_evidence()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let parked =
        crate::model_call_execution_and_recovery::park_restart_ambiguity(&pool, 0x11fe_c000)
            .await?;
    let session = parked.session;
    recorded(
        &pool,
        lifecycle_command(
            0x40,
            1,
            session,
            SessionLifecycleOperation::Adopt {
                finish_condition: Some(declared("the ambiguity is resolved")),
            },
        ),
    )
    .await?;
    park_by_statement(&pool, session).await?;
    sqlx::query(
        "UPDATE session_lifecycle
            SET parked_cause = 'retry_budget_exhausted',
                parked_standing_cause_kind = 'provider_transient'
          WHERE session_id = $1",
    )
    .bind(session.into_uuid())
    .execute(&pool)
    .await?;
    let deferred_evidence_before: (String, String) = sqlx::query_as(
        "SELECT parked_since::text, parked_standing_cause_kind
           FROM session_lifecycle WHERE session_id = $1",
    )
    .bind(session.into_uuid())
    .fetch_one(&pool)
    .await?;
    let immediate_session = creation_session(0x41);
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(dispatched_creation(0x41))
        .await?;
    park_by_statement(&pool, immediate_session).await?;
    sqlx::query(
        "UPDATE session_lifecycle
            SET parked_cause = 'retry_budget_exhausted',
                parked_standing_cause_kind = 'provider_transient'
          WHERE session_id = $1",
    )
    .bind(immediate_session.into_uuid())
    .execute(&pool)
    .await?;
    let immediate_evidence_before: (String, String) = sqlx::query_as(
        "SELECT parked_since::text, parked_standing_cause_kind
           FROM session_lifecycle WHERE session_id = $1",
    )
    .bind(immediate_session.into_uuid())
    .fetch_one(&pool)
    .await?;
    recorded(
        &pool,
        lifecycle_command(
            0x41,
            1,
            immediate_session,
            SessionLifecycleOperation::CloseFailed { cause: None },
        ),
    )
    .await?;
    let repository = SessionLifecycleRepository::new(pool.clone());

    let committed = recorded(
        &pool,
        lifecycle_command(
            0x40,
            2,
            session,
            SessionLifecycleOperation::CloseFailed { cause: None },
        ),
    )
    .await?;
    let successor = TurnId::from_uuid(Uuid::from_u128(0x11fe_c222));
    let interrupt = input_with_delivery(
        0x11fe_c220,
        0x11fe_c001,
        "closure interrupt",
        DeliveryRequest::Interrupt {
            expected_active_turn: parked.turn,
            descendant_scope: DescendantTerminationScope::ParentAlone,
            configuration: input_choices(1, ModelSelectionOverride::UseSessionDefault),
        },
    );
    let operator_error = SubmitInputRepository::new(pool.clone())
        .handle(
            interrupt.clone(),
            AcceptedInputId::from_uuid(Uuid::from_u128(0x11fe_c221)),
            Some(successor),
        )
        .await
        .expect_err("an operator interrupt cannot bypass a committed closure");
    assert!(matches!(
        operator_error,
        SubmitInputRepositoryError::Corruption(SubmitInputCorruption::Inconsistent(
            "session has a pending terminal handoff"
        ))
    ));
    let interrupted = SubmitInputRepository::new(pool.clone())
        .handle_with_candidates_alias_resolver_as(
            interrupt,
            CommandPrincipal::Core,
            ParentTerminationKind::Cancelled,
            AcceptedInputId::from_uuid(Uuid::from_u128(0x11fe_c221)),
            Some(successor),
            CancelledModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0x11fe_c223)),
                ContextFrontierId::from_uuid(Uuid::from_u128(0x11fe_c224)),
            ),
            |_| successor,
            |_| {
                (
                    Vec::new(),
                    ContextFrontierId::from_uuid(Uuid::from_u128(0x11fe_c225)),
                )
            },
            || panic!("the fixture has no approval wait"),
            || panic!("the fixture has no approval wait"),
            |_| None,
        )
        .await?;
    let settled = repository
        .load(session)
        .await?
        .expect("the session keeps its lifecycle row");

    assert_eq!(
        committed,
        SessionLifecycleCommandResult::Applied(SessionLifecycleApplication::ClosurePending {
            outcome: SessionTerminalOutcome::FailedRetryable {
                cause: SessionRetryableCause::ProviderTransient,
            },
            live_turn: parked.turn,
            defaults_version: SessionConfigurationDefaultsVersion::first(),
        })
    );
    assert!(matches!(
        interrupted,
        SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Applied(_))
    ));
    assert_eq!(
        turn_disposition(&pool, parked.turn).await?,
        (
            String::from("terminal"),
            Some(String::from("reconciliation_required")),
            Some(String::from("model_call_ambiguous"))
        )
    );
    assert_eq!(
        turn_disposition(&pool, successor).await?,
        (
            String::from("terminal"),
            Some(String::from("retired")),
            Some(String::from("session_closed"))
        )
    );
    assert_eq!(
        settled.state(),
        SessionLifecycleState::Terminal {
            outcome: SessionTerminalOutcome::FailedRetryable {
                cause: SessionRetryableCause::ProviderTransient,
            },
        }
    );
    let deferred_evidence_after: (String, String) = sqlx::query_as(
        "SELECT parked_since::text, parked_standing_cause_kind
           FROM session_lifecycle WHERE session_id = $1",
    )
    .bind(session.into_uuid())
    .fetch_one(&pool)
    .await?;
    let immediate_evidence_after: (String, String) = sqlx::query_as(
        "SELECT parked_since::text, parked_standing_cause_kind
           FROM session_lifecycle WHERE session_id = $1",
    )
    .bind(immediate_session.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        (deferred_evidence_after, immediate_evidence_after),
        (deferred_evidence_before, immediate_evidence_before)
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// A supersession naming an unknown successor, or the session itself, is a
/// recorded rejection that replays: the typed row keeps the successor as named.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_supersession_naming_no_successor_records_its_rejection() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let session = creation_session(20);
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(dispatched_creation(20))
        .await?;
    let unknown = || {
        lifecycle_command(
            20,
            1,
            session,
            SessionLifecycleOperation::Supersede {
                successor: SessionId::from_uuid(Uuid::from_u128(SEED + 0xf00d)),
            },
        )
    };
    let itself = lifecycle_command(
        20,
        2,
        session,
        SessionLifecycleOperation::Supersede { successor: session },
    );

    let refused = recorded(&pool, unknown()).await?;
    let replay = recorded(&pool, unknown()).await?;
    let self_named = recorded(&pool, itself.clone()).await?;

    assert_eq!(
        refused,
        SessionLifecycleCommandResult::Rejected(
            SessionLifecycleCommandRejection::SuccessorNotFound
        )
    );
    assert_eq!(replay, refused);
    assert_eq!(
        self_named,
        SessionLifecycleCommandResult::Rejected(SessionLifecycleCommandRejection::SuccessorIsSelf)
    );
    assert_eq!(
        settlement(&pool, unknown().command_id()).await?,
        (
            String::from("rejected"),
            Some(String::from("successor_not_found"))
        )
    );
    assert_eq!(
        settlement(&pool, itself.command_id()).await?,
        (
            String::from("rejected"),
            Some(String::from("successor_is_self"))
        )
    );
    assert!(
        !SessionLifecycleRepository::new(pool.clone())
            .load(session)
            .await?
            .expect("the session keeps its lifecycle row")
            .state()
            .is_terminal()
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// Ownership cannot change on a closed session: `release` and `adopt` are
/// recorded `transition_not_admitted` rejections rather than database failures.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn ownership_commands_on_a_closed_session_are_recorded_rejections()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let session = creation_session(21);
    let successor = creation_session(22);
    let repository = CreateSessionRepository::new(pool.clone(), test_session_credential_pin());
    repository.handle(dispatched_creation(21)).await?;
    repository.handle(dispatched_creation(22)).await?;
    recorded(
        &pool,
        lifecycle_command(
            21,
            1,
            session,
            SessionLifecycleOperation::Supersede { successor },
        ),
    )
    .await?;
    assert!(
        SessionLifecycleRepository::new(pool.clone())
            .load(session)
            .await?
            .expect("the session keeps its lifecycle row")
            .state()
            .is_terminal()
    );
    let release = lifecycle_command(21, 2, session, SessionLifecycleOperation::Release);
    let adopt = lifecycle_command(
        21,
        3,
        session,
        SessionLifecycleOperation::Adopt {
            finish_condition: None,
        },
    );

    let not_admitted = SessionLifecycleCommandResult::Rejected(
        SessionLifecycleCommandRejection::TransitionNotAdmitted,
    );
    let recorded_rejection = (
        String::from("rejected"),
        Some(String::from("transition_not_admitted")),
    );
    assert_eq!(recorded(&pool, release.clone()).await?, not_admitted);
    assert_eq!(recorded(&pool, adopt.clone()).await?, not_admitted);
    assert_eq!(
        settlement(&pool, release.command_id()).await?,
        recorded_rejection
    );
    assert_eq!(
        settlement(&pool, adopt.command_id()).await?,
        recorded_rejection
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// Attaching a goal confers ownership. An unmonitored conversation becomes
/// owned by the attaching actor in the attach transaction, with its journal
/// entry; an explicit adopt afterwards finds nothing to change.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn attaching_a_goal_confers_ownership() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let session = creation_session(24);
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(interactive_creation(24))
        .await?;
    GoalRepository::new(pool.clone())
        .handle_user_command(
            GoalUserCommand::new(
                DurableCommandId::from_uuid(Uuid::from_u128(SEED + 24 + 0xa00)),
                session,
                GoalUserAction::Attach(
                    GoalStatement::try_new(String::from("converge the fixture branch"))
                        .expect("the fixture statement is admitted"),
                ),
            ),
            Some(GoalTurnCandidates::new(
                AcceptedInputId::from_uuid(Uuid::from_u128(SEED + 24 + 0xb00)),
                TurnId::from_uuid(Uuid::from_u128(SEED + 24 + 0xc00)),
            )),
            |_| None,
        )
        .await?;

    let lifecycle = SessionLifecycleRepository::new(pool.clone())
        .load(session)
        .await?
        .expect("the conversation keeps its lifecycle row");
    let journal: Vec<(String, String)> = sqlx::query_as(
        "SELECT transition_kind, actor_kind FROM session_ownership_event
          WHERE session_id = $1 ORDER BY event_ordinal",
    )
    .bind(session.into_uuid())
    .fetch_all(&pool)
    .await?;
    let adopt = recorded(
        &pool,
        lifecycle_command(
            24,
            1,
            session,
            SessionLifecycleOperation::Adopt {
                finish_condition: None,
            },
        ),
    )
    .await?;

    assert_eq!(lifecycle.ownership(), SessionOwnership::Owned);
    assert_eq!(
        journal,
        vec![
            (
                String::from("created_unmonitored"),
                String::from("operator")
            ),
            (String::from("adopted"), String::from("operator")),
        ]
    );
    assert_eq!(
        adopt,
        SessionLifecycleCommandResult::Rejected(
            SessionLifecycleCommandRejection::OwnershipUnchanged
        )
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// A `close_failed` naming a cause the park does not hold is the recorded
/// rejection `standing_cause_mismatch`; only an omitted cause closes a
/// causeless park as `failed_unknown`.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_supplied_cause_over_a_causeless_park_is_rejected() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let session = creation_session(25);
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(dispatched_creation(25))
        .await?;
    park_by_statement(&pool, session).await?;
    let invented = lifecycle_command(
        25,
        1,
        session,
        SessionLifecycleOperation::CloseFailed {
            cause: Some(SessionFailureCause::Retryable(
                SessionRetryableCause::ProviderTransient,
            )),
        },
    );

    let refused = recorded(&pool, invented.clone()).await?;

    assert_eq!(
        refused,
        SessionLifecycleCommandResult::Rejected(
            SessionLifecycleCommandRejection::StandingCauseMismatch
        )
    );
    assert_eq!(
        settlement(&pool, invented.command_id()).await?,
        (
            String::from("rejected"),
            Some(String::from("standing_cause_mismatch"))
        )
    );
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

/// A failed closure cannot replace the park's standing cause with another
/// member of the same typed cause family.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_failed_closure_rejects_a_cause_different_from_the_parks_standing_cause()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let session = creation_session(26);
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(dispatched_creation(26))
        .await?;
    park_by_statement(&pool, session).await?;
    sqlx::query(
        "UPDATE session_lifecycle
            SET parked_cause = 'retry_budget_exhausted',
                parked_standing_cause_kind = 'provider_transient'
          WHERE session_id = $1",
    )
    .bind(session.into_uuid())
    .execute(&pool)
    .await?;
    let mismatched = lifecycle_command(
        26,
        1,
        session,
        SessionLifecycleOperation::CloseFailed {
            cause: Some(SessionFailureCause::Retryable(
                SessionRetryableCause::ProviderOverloaded,
            )),
        },
    );

    let refused = recorded(&pool, mismatched.clone()).await?;

    assert_eq!(
        refused,
        SessionLifecycleCommandResult::Rejected(
            SessionLifecycleCommandRejection::StandingCauseMismatch
        )
    );
    assert_eq!(
        settlement(&pool, mismatched.command_id()).await?,
        (
            String::from("rejected"),
            Some(String::from("standing_cause_mismatch"))
        )
    );
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

/// A pending closure whose only live turn the delegation cascade terminates
/// logically settles: a runtime-terminal turn is not live.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_pending_closure_settles_when_its_live_turn_becomes_runtime_terminal()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let session = creation_session(26);
    let successor = creation_session(27);
    let creation = CreateSessionRepository::new(pool.clone(), test_session_credential_pin());
    creation.handle(dispatched_creation(26)).await?;
    creation.handle(dispatched_creation(27)).await?;
    let live = queue_turn(&pool, session, 26, 1).await?;
    activate_turn(&pool, session, 26).await?;
    let committed = recorded(
        &pool,
        lifecycle_command(
            26,
            1,
            session,
            SessionLifecycleOperation::Supersede { successor },
        ),
    )
    .await?;
    assert!(matches!(
        committed,
        SessionLifecycleCommandResult::Applied(SessionLifecycleApplication::ClosurePending { .. })
    ));

    // Stand in for a delegated child turn the parent's cascade terminates
    // logically: the flag is admitted on delegation-origin turns only.
    sqlx::query("ALTER TABLE turn_lifecycle DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query(
        "UPDATE turn_lifecycle
            SET origin_kind = 'delegation', origin_accepted_input_id = NULL
          WHERE turn_id = $1",
    )
    .bind(live.into_uuid())
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE turn_lifecycle ENABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    // The cascade's own proofs are out of scope here; only the settlement
    // trigger runs on the flag write.
    sqlx::query("ALTER TABLE turn_lifecycle DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query(
        "ALTER TABLE turn_lifecycle ENABLE TRIGGER turn_lifecycle_settles_pending_terminal",
    )
    .execute(&pool)
    .await?;
    sqlx::query("UPDATE turn_lifecycle SET delegation_runtime_terminal = true WHERE turn_id = $1")
        .bind(live.into_uuid())
        .execute(&pool)
        .await?;
    sqlx::query("ALTER TABLE turn_lifecycle ENABLE TRIGGER ALL")
        .execute(&pool)
        .await?;

    let settled = SessionLifecycleRepository::new(pool.clone())
        .load(session)
        .await?
        .expect("the session keeps its lifecycle row");
    assert_eq!(
        settled.state(),
        SessionLifecycleState::Terminal {
            outcome: SessionTerminalOutcome::Superseded {
                by: Some(successor),
            },
        }
    );

    pool.close().await;
    drop(container);
    Ok(())
}
