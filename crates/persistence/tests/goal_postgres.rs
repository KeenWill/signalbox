#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    reason = "this standalone integration-test crate uses assertion panics and explicit fixture expectations; the workspace gate remains active for production targets"
)]

use std::error::Error;

use signalbox_application::{
    StartEligibleTurnOutcome, StartupScanIdGenerator, StartupScanSessionOutcome,
};
use signalbox_domain::{
    AcceptedInputId, AcceptedInputTurnActivationIdentities, AcceptedInputTurnFailureIdentities,
    CancelledModelCallTurnIdentities, ContextFrontierId, CreateSession, DeliveryRequest,
    DirectModelSelection, DurableCommandId, FrozenAliasDefinition, Goal, GoalCommandRejection,
    GoalCommandResult, GoalGuidance, GoalModelBlockedReasonKind, GoalModelProvenance, GoalNeed,
    GoalSchedulerProvenance, GoalStatement, GoalUserAction, GoalUserCommand, GoalUserProvenance,
    ModelAlias, ModelSelectionRequest, PreparedCreateSession, SemanticTranscriptEntryId,
    SessionConfigurationDefaults, SessionCreationCause, SessionCreationProvenance, SessionId,
    SessionInputPosition, SubmitInput, ToolRequestId, TranscriptAncestry, TurnAttemptId, TurnId,
    UserContent,
};
use signalbox_persistence::{
    SessionCredentialPin, SessionModelCredential,
    create_session::CreateSessionRepository,
    goal::{
        GoalCommandHandlingOutcome, GoalRepository, GoalRepositoryError, GoalTransitionOutcome,
    },
    goal_turn::{GoalTurnCandidates, GoalTurnContinuationOutcome},
    local_test_connection_options, migrate,
    outbox::{
        DispatchedOutboxEventKind, OutboxDeliveryDecision, OutboxDispatchOutcome, OutboxDispatcher,
    },
    scheduler::PostgresEligibilitySweep,
    start_eligible_turn::StartEligibleTurnRepository,
    startup::PostgresStartupScanRepository,
    submit_input::SubmitInputRepository,
};
use sqlx::{PgPool, postgres::PgPoolOptions, types::Uuid};
use testcontainers_modules::{
    postgres::Postgres,
    testcontainers::{ContainerAsync, ImageExt, runners::AsyncRunner},
};

const POSTGRES_IMAGE_TAG: &str = "18.4-alpine3.23";
const DATABASE_NAME: &str = "signalbox_goal";
const DATABASE_USER: &str = "signalbox";
const DATABASE_PASSWORD: &str = "signalbox-test-only";
const SESSION: u128 = 0x701;
const CREATE_COMMAND: u128 = 0x801;
const ATTACH_COMMAND: u128 = 0x901;
const SUPERSEDE_COMMAND: u128 = 0x902;
const STOP_COMMAND: u128 = 0x903;
const REATTACH_COMMAND: u128 = 0x904;
const STEER_COMMAND: u128 = 0x905;
const RESUME_COMMAND: u128 = 0x906;

async fn migrated_postgres() -> Result<(ContainerAsync<Postgres>, PgPool), Box<dyn Error>> {
    let container = Postgres::default()
        .with_db_name(DATABASE_NAME)
        .with_user(DATABASE_USER)
        .with_password(DATABASE_PASSWORD)
        .with_fsync_enabled()
        .with_tag(POSTGRES_IMAGE_TAG)
        .start()
        .await?;
    let host = container.get_host().await?;
    let port = container.get_host_port_ipv4(5432).await?;
    let database_url =
        format!("postgres://{DATABASE_USER}:{DATABASE_PASSWORD}@{host}:{port}/{DATABASE_NAME}");
    let pool = PgPoolOptions::new()
        .max_connections(8)
        .connect_with(local_test_connection_options(&database_url)?)
        .await?;
    migrate(&pool).await?;
    Ok((container, pool))
}

fn session(value: u128) -> SessionId {
    SessionId::from_uuid(Uuid::from_u128(value))
}

fn command(value: u128) -> DurableCommandId {
    DurableCommandId::from_uuid(Uuid::from_u128(value))
}

fn tool_request(value: u128) -> ToolRequestId {
    ToolRequestId::from_uuid(Uuid::from_u128(value))
}

fn credential_pin() -> SessionCredentialPin {
    SessionCredentialPin::try_new(vec![SessionModelCredential::new(
        "test-model-family",
        "test-model-primary",
    )])
    .expect("test credential pin is valid")
}

fn creation() -> PreparedCreateSession {
    creation_with_model(ModelSelectionRequest::Direct(
        DirectModelSelection::from_uuid(Uuid::from_u128(0xa01)),
    ))
}

fn creation_with_model(model: ModelSelectionRequest) -> PreparedCreateSession {
    CreateSession::new(
        command(CREATE_COMMAND),
        SessionCreationProvenance::new(
            SessionCreationCause::UserInitiated,
            TranscriptAncestry::None,
        ),
        SessionConfigurationDefaults::new(model),
    )
    .prepare(session(SESSION))
    .expect("user-initiated creation without ancestry is preparable")
}

fn statement(value: &str) -> GoalStatement {
    GoalStatement::try_new(value.to_owned()).expect("fixture goal statement is admitted")
}

fn turn_candidates(value: u128) -> GoalTurnCandidates {
    GoalTurnCandidates::new(
        AcceptedInputId::from_uuid(Uuid::from_u128(value)),
        TurnId::from_uuid(Uuid::from_u128(value + 0x100)),
    )
}

fn latest_event(goal: &Goal) -> signalbox_domain::GoalEvent {
    goal.events()
        .last()
        .cloned()
        .expect("fixture goal has a latest event")
}

#[track_caller]
fn activated_turn(outcome: StartEligibleTurnOutcome) -> TurnId {
    match outcome {
        StartEligibleTurnOutcome::Activated(activated) => activated.turn(),
        StartEligibleTurnOutcome::NoEligibleTurn => {
            panic!("fixture goal turn must be eligible for activation")
        }
    }
}

fn activation_identities(value: u128) -> AcceptedInputTurnActivationIdentities {
    AcceptedInputTurnActivationIdentities::new(
        SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(value)),
        SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(value + 1)),
        ContextFrontierId::from_uuid(Uuid::from_u128(value + 2)),
        TurnAttemptId::from_uuid(Uuid::from_u128(value + 3)),
    )
}

async fn activate_goal_turn(pool: &PgPool, value: u128) -> Result<TurnId, Box<dyn Error>> {
    let outcome = StartEligibleTurnRepository::new(pool.clone())
        .handle(session(SESSION), activation_identities(value))
        .await?;
    Ok(activated_turn(outcome))
}

struct FixedStartupIds {
    failure_entry: SemanticTranscriptEntryId,
    terminal_frontier: ContextFrontierId,
}

impl StartupScanIdGenerator for FixedStartupIds {
    fn next_failure_entry_id(&mut self) -> SemanticTranscriptEntryId {
        self.failure_entry
    }

    fn next_terminal_frontier_id(&mut self) -> ContextFrontierId {
        self.terminal_frontier
    }

    fn next_reclassified_turn_id(&mut self, _accepted_input: AcceptedInputId) -> TurnId {
        panic!("goal failure fixture has no pending steering")
    }
}

async fn terminalize_goal_turn_as_failed(pool: &PgPool, value: u128) -> Result<(), Box<dyn Error>> {
    let mut ids = FixedStartupIds {
        failure_entry: SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(value)),
        terminal_frontier: ContextFrontierId::from_uuid(Uuid::from_u128(value + 1)),
    };
    let outcome = PostgresStartupScanRepository::new(pool.clone())
        .recover(
            session(SESSION),
            AcceptedInputTurnFailureIdentities::new(ids.failure_entry, ids.terminal_frontier),
            &mut ids,
        )
        .await?;
    let StartupScanSessionOutcome::Recovered(_) = outcome else {
        panic!("prepared active goal turn must recover as failed");
    };
    Ok(())
}

/// INV-048: a fresh durable sweep rediscovers a pursuing goal whose current
/// turn terminalized before its scheduler disposition could commit.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv048_terminal_goal_disposition_survives_scheduler_restart() -> Result<(), Box<dyn Error>>
{
    let (container, pool) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone(), credential_pin())
        .handle(creation())
        .await?;
    let attached_turn = turn_candidates(0xb61);
    GoalRepository::new(pool.clone())
        .handle_user_command(
            GoalUserCommand::new(
                command(ATTACH_COMMAND),
                session(SESSION),
                GoalUserAction::Attach(statement("finish the commissioned task")),
            ),
            Some(attached_turn),
            |_| None,
        )
        .await?;
    assert_eq!(
        activate_goal_turn(&pool, 0xd61).await?,
        attached_turn.turn()
    );
    terminalize_goal_turn_as_failed(&pool, 0xe61).await?;

    let (sessions, continuation) = PostgresEligibilitySweep::new(pool.clone())
        .find_sessions()
        .await?
        .into_parts();

    assert_eq!(sessions, vec![session(SESSION)]);
    assert!(!continuation);

    pool.close().await;
    drop(container);
    Ok(())
}

async fn insert_goal_tool_request(
    pool: &PgPool,
    turn: TurnId,
    request: ToolRequestId,
    tool_name: &str,
    arguments: &str,
) -> Result<(), Box<dyn Error>> {
    sqlx::query("ALTER TABLE tool_request DISABLE TRIGGER ALL")
        .execute(pool)
        .await?;
    sqlx::query(
        "INSERT INTO tool_request
            (request_id, session_id, turn_id, producing_model_call_id,
             request_ordinal, tool_name, arguments_kind, arguments_text)
         VALUES ($1, $2, $3, $4, 0, $5, 'json', $6)",
    )
    .bind(request.into_uuid())
    .bind(Uuid::from_u128(SESSION))
    .bind(turn.into_uuid())
    .bind(Uuid::from_u128(request.into_uuid().as_u128() + 0x1000))
    .bind(tool_name)
    .bind(arguments)
    .execute(pool)
    .await?;
    sqlx::query("ALTER TABLE tool_request ENABLE TRIGGER ALL")
        .execute(pool)
        .await?;
    Ok(())
}

async fn set_goal_turn_acceptance_position(
    pool: &PgPool,
    turn: TurnId,
    position: SessionInputPosition,
) -> Result<(), Box<dyn Error>> {
    sqlx::query("ALTER TABLE accepted_input DISABLE TRIGGER ALL")
        .execute(pool)
        .await?;
    sqlx::query("ALTER TABLE queued_input_origin DISABLE TRIGGER ALL")
        .execute(pool)
        .await?;
    sqlx::query("ALTER TABLE turn_lifecycle DISABLE TRIGGER ALL")
        .execute(pool)
        .await?;
    sqlx::query("ALTER TABLE input_accepted_outbox_event DISABLE TRIGGER ALL")
        .execute(pool)
        .await?;
    let mut transaction = pool.begin().await?;
    sqlx::query(
        "UPDATE accepted_input
            SET acceptance_position = $3::numeric
          WHERE session_id = $1 AND origin_turn_id = $2",
    )
    .bind(Uuid::from_u128(SESSION))
    .bind(turn.into_uuid())
    .bind(position.as_u64().to_string())
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "UPDATE queued_input_origin
            SET acceptance_position = $3::numeric
          WHERE session_id = $1 AND turn_id = $2",
    )
    .bind(Uuid::from_u128(SESSION))
    .bind(turn.into_uuid())
    .bind(position.as_u64().to_string())
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "UPDATE turn_lifecycle
            SET acceptance_position = $3::numeric
          WHERE session_id = $1 AND turn_id = $2",
    )
    .bind(Uuid::from_u128(SESSION))
    .bind(turn.into_uuid())
    .bind(position.as_u64().to_string())
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "UPDATE input_accepted_outbox_event
            SET acceptance_position = $3::numeric
          WHERE session_id = $1 AND turn_id = $2",
    )
    .bind(Uuid::from_u128(SESSION))
    .bind(turn.into_uuid())
    .bind(position.as_u64().to_string())
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await?;
    sqlx::query("ALTER TABLE accepted_input ENABLE TRIGGER ALL")
        .execute(pool)
        .await?;
    sqlx::query("ALTER TABLE queued_input_origin ENABLE TRIGGER ALL")
        .execute(pool)
        .await?;
    sqlx::query("ALTER TABLE turn_lifecycle ENABLE TRIGGER ALL")
        .execute(pool)
        .await?;
    sqlx::query("ALTER TABLE input_accepted_outbox_event ENABLE TRIGGER ALL")
        .execute(pool)
        .await?;
    Ok(())
}

#[track_caller]
fn assert_model_declaration_request_rejected(error: GoalRepositoryError) {
    let GoalRepositoryError::Database(error) = error else {
        panic!("declaration-request mismatch must be a database rejection");
    };
    let database = error
        .as_database_error()
        .expect("declaration-request mismatch reports a database constraint");
    assert_eq!(database.code().as_deref(), Some("23514"));
    assert_eq!(
        database.constraint(),
        Some("goal_event_model_declaration_request")
    );
}

async fn mark_goal_turn_completed(pool: &PgPool, turn: TurnId) -> Result<(), Box<dyn Error>> {
    sqlx::query("ALTER TABLE turn_lifecycle DISABLE TRIGGER ALL")
        .execute(pool)
        .await?;
    sqlx::query(
        "UPDATE turn_lifecycle
            SET state_kind = 'terminal', terminal_disposition_kind = 'completed',
                terminal_frontier_id = $3, active_phase_kind = NULL,
                terminal_attempt_id = current_attempt_id, current_attempt_id = NULL,
                terminal_model_call_id = $4
          WHERE session_id = $1 AND turn_id = $2",
    )
    .bind(Uuid::from_u128(SESSION))
    .bind(turn.into_uuid())
    .bind(Uuid::from_u128(0xd75))
    .bind(Uuid::from_u128(0xe75))
    .execute(pool)
    .await?;
    sqlx::query("ALTER TABLE turn_lifecycle ENABLE TRIGGER ALL")
        .execute(pool)
        .await?;
    Ok(())
}

#[track_caller]
fn assert_applied_transition(outcome: GoalTransitionOutcome) {
    let GoalTransitionOutcome::Applied(_) = outcome else {
        panic!("fixture transition must apply");
    };
}

#[track_caller]
fn assert_applied_command(outcome: GoalCommandHandlingOutcome) {
    let GoalCommandHandlingOutcome::Recorded(GoalCommandResult::Applied(_)) = outcome else {
        panic!("fixture command must apply");
    };
}

/// INV-048: a goal-owned accepted input dispatches and activates without a
/// synthetic user command, then remains a canonical active origin for steer.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s_goal_inv048_goal_owned_input_activates_without_a_user_command()
-> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone(), credential_pin())
        .handle(creation())
        .await?;
    let candidates = turn_candidates(0xb01);
    let goal_content = String::from("finish the commissioned task");
    GoalRepository::new(pool.clone())
        .handle_user_command(
            GoalUserCommand::new(
                command(ATTACH_COMMAND),
                session(SESSION),
                GoalUserAction::Attach(statement(&goal_content)),
            ),
            Some(candidates),
            |_| None,
        )
        .await?;

    let accepted_events: i64 = sqlx::query_scalar(
        "SELECT count(*)
           FROM input_accepted_outbox_event
          WHERE session_id = $1
            AND accepted_input_id = $2
            AND turn_id = $3",
    )
    .bind(Uuid::from_u128(SESSION))
    .bind(candidates.accepted_input().into_uuid())
    .bind(candidates.turn().into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(accepted_events, 1);

    let dispatcher = OutboxDispatcher::new(pool.clone());
    let mut created = None;
    assert_eq!(
        dispatcher
            .dispatch_next(|event| {
                created = Some(event.clone());
                OutboxDeliveryDecision::Delivered
            })
            .await?,
        OutboxDispatchOutcome::Delivered { sequence: 1 }
    );
    let created = created.expect("the session creation event was offered");
    assert_eq!(created.session(), session(SESSION));
    assert!(matches!(
        created.kind(),
        DispatchedOutboxEventKind::SessionCreated
    ));

    let mut accepted = None;
    assert_eq!(
        dispatcher
            .dispatch_next(|event| {
                accepted = Some(event.clone());
                OutboxDeliveryDecision::Delivered
            })
            .await?,
        OutboxDispatchOutcome::Delivered { sequence: 2 }
    );
    let accepted = accepted.expect("the goal input acceptance event was offered");
    assert_eq!(accepted.session(), session(SESSION));
    assert_eq!(
        accepted.kind(),
        &DispatchedOutboxEventKind::InputAccepted {
            accepted_input: candidates.accepted_input(),
            turn: candidates.turn(),
            acceptance_position: SessionInputPosition::first(),
            content: goal_content,
        }
    );

    let activation = StartEligibleTurnRepository::new(pool.clone())
        .handle(
            session(SESSION),
            AcceptedInputTurnActivationIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xd01)),
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xd02)),
                ContextFrontierId::from_uuid(Uuid::from_u128(0xd03)),
                TurnAttemptId::from_uuid(Uuid::from_u128(0xd04)),
            ),
        )
        .await?;

    assert_eq!(activated_turn(activation), candidates.turn());

    assert_eq!(
        GoalRepository::new(pool.clone())
            .reconcile_current_after_execution(
                session(SESSION),
                turn_candidates(0xc01),
                GoalNeed::try_new(String::from("repair execution"))
                    .expect("fixture need is admitted"),
                |_| None,
            )
            .await?,
        GoalTurnContinuationOutcome::NotTerminal
    );

    SubmitInputRepository::new(pool.clone())
        .handle_with_candidates(
            SubmitInput::new(
                command(STEER_COMMAND),
                session(SESSION),
                UserContent::try_text(String::from("keep the current scope; use this detail"))
                    .expect("fixture steering content is admitted"),
                DeliveryRequest::NextSafePoint {
                    expected_active_turn: candidates.turn(),
                },
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(0xe01)),
            None,
            CancelledModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xf01)),
                ContextFrontierId::from_uuid(Uuid::from_u128(0xf02)),
            ),
            |_| panic!("steering cannot be reclassified while its source remains active"),
            |_| {
                panic!("steering cannot cancel a tool request without a terminal model observation")
            },
        )
        .await?;
    let pending_steering: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM accepted_input
          WHERE accepting_command_id = $1 AND disposition_kind = 'pending_steering'",
    )
    .bind(Uuid::from_u128(STEER_COMMAND))
    .fetch_one(&pool)
    .await?;
    assert_eq!(pending_steering, 1);

    pool.close().await;
    drop(container);
    Ok(())
}

/// INV-048: resuming a blocked goal schedules exactly one next turn whose
/// accepted input is the exact optional user guidance.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s_goal_inv048_resume_delivers_guidance_to_the_next_turn() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone(), credential_pin())
        .handle(creation())
        .await?;
    let repository = GoalRepository::new(pool.clone());
    let attached_turn = turn_candidates(0xb10);
    repository
        .handle_user_command(
            GoalUserCommand::new(
                command(ATTACH_COMMAND),
                session(SESSION),
                GoalUserAction::Attach(statement("finish the commissioned task")),
            ),
            Some(attached_turn),
            |_| None,
        )
        .await?;
    let failure_need =
        GoalNeed::try_new(String::from("repair execution")).expect("fixture need is admitted");
    assert_eq!(
        repository
            .block_execution_failure(
                session(SESSION),
                failure_need.clone(),
                GoalSchedulerProvenance::new(attached_turn.turn()),
            )
            .await?,
        GoalTransitionOutcome::NotCurrentGoalTurn
    );
    assert_eq!(
        activate_goal_turn(&pool, 0xd20).await?,
        attached_turn.turn()
    );
    terminalize_goal_turn_as_failed(&pool, 0xe20).await?;
    let blocked = repository
        .block_execution_failure(
            session(SESSION),
            failure_need.clone(),
            GoalSchedulerProvenance::new(attached_turn.turn()),
        )
        .await?;
    assert_applied_transition(blocked.clone());

    let guidance = GoalGuidance::try_new(String::from("use the newly granted credential"))
        .expect("fixture guidance is admitted");
    let resumed_turn = turn_candidates(0xb20);
    let resumed = repository
        .handle_user_command(
            GoalUserCommand::new(
                command(RESUME_COMMAND),
                session(SESSION),
                GoalUserAction::Resume(Some(guidance.clone())),
            ),
            Some(resumed_turn),
            |_| None,
        )
        .await?;
    assert_applied_command(resumed);
    assert_eq!(
        repository
            .block_execution_failure(
                session(SESSION),
                failure_need,
                GoalSchedulerProvenance::new(attached_turn.turn()),
            )
            .await?,
        blocked
    );
    let scheduler_blocks: i64 =
        sqlx::query_scalar("SELECT count(*) FROM goal_event WHERE scheduler_turn_id = $1")
            .bind(attached_turn.turn().into_uuid())
            .fetch_one(&pool)
            .await?;

    let resumed_content: String = sqlx::query_scalar(
        "SELECT content_text
           FROM accepted_input
          WHERE accepted_input_id = $1
            AND session_id = $2",
    )
    .bind(resumed_turn.accepted_input().into_uuid())
    .bind(Uuid::from_u128(SESSION))
    .fetch_one(&pool)
    .await?;
    let resumed_goal_turns: i64 = sqlx::query_scalar(
        "SELECT count(*)
           FROM goal_turn
          WHERE turn_id = $1
            AND source_event_ordinal IS NOT NULL",
    )
    .bind(resumed_turn.turn().into_uuid())
    .fetch_one(&pool)
    .await?;

    assert_eq!(scheduler_blocks, 1);
    assert_eq!(resumed_content, guidance.as_str());
    assert_eq!(resumed_goal_turns, 1);

    pool.close().await;
    drop(container);
    Ok(())
}

/// INV-048: PostgreSQL round-trips the complete immutable goal lineage,
/// including its user receipts and atomic statement supersession.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s_goal_inv048_complete_lineage_round_trips() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone(), credential_pin())
        .handle(creation())
        .await?;
    let repository = GoalRepository::new(pool.clone());
    let first_statement = statement("finish the commissioned task");
    let replacement_statement = statement("finish the replacement task");
    let attach = GoalUserCommand::new(
        command(ATTACH_COMMAND),
        session(SESSION),
        GoalUserAction::Attach(first_statement.clone()),
    );
    let commissioned = Goal::commission(
        session(SESSION),
        first_statement,
        GoalUserProvenance::new(command(ATTACH_COMMAND)),
    );
    let commissioned_event = latest_event(&commissioned);
    let attach_outcome =
        GoalCommandHandlingOutcome::Recorded(GoalCommandResult::Applied(commissioned_event));

    assert_eq!(
        repository
            .handle_user_command(attach.clone(), Some(turn_candidates(0xb01)), |_| None,)
            .await?,
        attach_outcome
    );
    assert_eq!(
        repository
            .handle_user_command(attach.clone(), Some(turn_candidates(0xb02)), |_| None,)
            .await?,
        attach_outcome
    );
    assert_eq!(
        repository
            .load_command(command(ATTACH_COMMAND))
            .await?
            .expect("attach receipt exists")
            .command(),
        &attach
    );

    let supersede = GoalUserCommand::new(
        command(SUPERSEDE_COMMAND),
        session(SESSION),
        GoalUserAction::Supersede(replacement_statement.clone()),
    );
    let superseded = commissioned
        .supersede(
            replacement_statement,
            GoalUserProvenance::new(command(SUPERSEDE_COMMAND)),
        )
        .expect("a pursuing goal can be superseded");
    let supersede_event = latest_event(&superseded);

    assert_eq!(
        repository
            .handle_user_command(supersede, Some(turn_candidates(0xb03)), |_| None,)
            .await?,
        GoalCommandHandlingOutcome::Recorded(GoalCommandResult::Applied(supersede_event))
    );

    let stop = GoalUserCommand::new(
        command(STOP_COMMAND),
        session(SESSION),
        GoalUserAction::Stop,
    );
    let stopped = superseded
        .stop(GoalUserProvenance::new(command(STOP_COMMAND)))
        .expect("a pursuing replacement can be stopped");
    let stop_event = latest_event(&stopped);

    assert_eq!(
        repository.handle_user_command(stop, None, |_| None).await?,
        GoalCommandHandlingOutcome::Recorded(GoalCommandResult::Applied(stop_event))
    );

    let later_statement = statement("finish a later commissioned task");
    let reattach = GoalUserCommand::new(
        command(REATTACH_COMMAND),
        session(SESSION),
        GoalUserAction::Attach(later_statement.clone()),
    );
    let recommissioned = stopped
        .commission_successor(
            later_statement,
            GoalUserProvenance::new(command(REATTACH_COMMAND)),
        )
        .expect("a user-stopped generation admits a later commission");
    let reattach_event = latest_event(&recommissioned);

    assert_eq!(
        repository
            .handle_user_command(reattach, Some(turn_candidates(0xb04)), |_| None,)
            .await?,
        GoalCommandHandlingOutcome::Recorded(GoalCommandResult::Applied(reattach_event))
    );
    assert_eq!(
        repository
            .load_goal(session(SESSION))
            .await?
            .expect("goal lineage exists"),
        recommissioned
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// INV-048: persisted goal history rejects mutation after commit.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv048_goal_event_history_is_append_only() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone(), credential_pin())
        .handle(creation())
        .await?;
    let repository = GoalRepository::new(pool.clone());
    let attach = GoalUserCommand::new(
        command(ATTACH_COMMAND),
        session(SESSION),
        GoalUserAction::Attach(statement("finish the commissioned task")),
    );
    repository
        .handle_user_command(attach, Some(turn_candidates(0xb01)), |_| None)
        .await?;

    let error =
        sqlx::query("UPDATE goal_event SET statement = 'edited in place' WHERE session_id = $1")
            .bind(Uuid::from_u128(SESSION))
            .execute(&pool)
            .await
            .expect_err("goal event mutation must be rejected");

    assert_eq!(
        error
            .as_database_error()
            .and_then(|database| database.code())
            .as_deref(),
        Some("23514")
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// INV-048: superseding before activation makes the old queued statement
/// ineligible while the replacement remains the first runnable goal turn.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv048_supersede_retires_the_obsolete_queued_turn() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone(), credential_pin())
        .handle(creation())
        .await?;
    let repository = GoalRepository::new(pool.clone());
    let obsolete = turn_candidates(0xb31);
    repository
        .handle_user_command(
            GoalUserCommand::new(
                command(ATTACH_COMMAND),
                session(SESSION),
                GoalUserAction::Attach(statement("finish the original task")),
            ),
            Some(obsolete),
            |_| None,
        )
        .await?;
    let replacement = turn_candidates(0xb32);
    repository
        .handle_user_command(
            GoalUserCommand::new(
                command(SUPERSEDE_COMMAND),
                session(SESSION),
                GoalUserAction::Supersede(statement("finish the replacement task")),
            ),
            Some(replacement),
            |_| None,
        )
        .await?;

    assert_eq!(activate_goal_turn(&pool, 0xd31).await?, replacement.turn());

    pool.close().await;
    drop(container);
    Ok(())
}

/// INV-048: stopping before activation leaves no runnable goal work and the
/// immutable stale turn cannot block a later explicit commission.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv048_stop_retires_queued_work_without_blocking_reattach() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone(), credential_pin())
        .handle(creation())
        .await?;
    let repository = GoalRepository::new(pool.clone());
    repository
        .handle_user_command(
            GoalUserCommand::new(
                command(ATTACH_COMMAND),
                session(SESSION),
                GoalUserAction::Attach(statement("finish the stopped task")),
            ),
            Some(turn_candidates(0xb41)),
            |_| None,
        )
        .await?;
    repository
        .handle_user_command(
            GoalUserCommand::new(
                command(STOP_COMMAND),
                session(SESSION),
                GoalUserAction::Stop,
            ),
            None,
            |_| None,
        )
        .await?;

    assert_eq!(
        StartEligibleTurnRepository::new(pool.clone())
            .handle(session(SESSION), activation_identities(0xd41))
            .await?,
        StartEligibleTurnOutcome::NoEligibleTurn
    );
    let (stopped_sessions, stopped_continuation) = PostgresEligibilitySweep::new(pool.clone())
        .find_sessions()
        .await?
        .into_parts();

    assert!(stopped_sessions.is_empty());
    assert!(!stopped_continuation);

    let successor = turn_candidates(0xb42);
    repository
        .handle_user_command(
            GoalUserCommand::new(
                command(REATTACH_COMMAND),
                session(SESSION),
                GoalUserAction::Attach(statement("finish the later task")),
            ),
            Some(successor),
            |_| None,
        )
        .await?;
    assert_eq!(activate_goal_turn(&pool, 0xd42).await?, successor.turn());

    pool.close().await;
    drop(container);
    Ok(())
}

/// INV-048: retiring a queued replacement keeps its immutable tail position
/// while excluding its turn from runtime scheduling.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv048_stopped_replacement_does_not_corrupt_the_active_acceptance_tail()
-> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone(), credential_pin())
        .handle(creation())
        .await?;
    let repository = GoalRepository::new(pool.clone());
    let active = turn_candidates(0xb61);
    repository
        .handle_user_command(
            GoalUserCommand::new(
                command(ATTACH_COMMAND),
                session(SESSION),
                GoalUserAction::Attach(statement("finish the original task")),
            ),
            Some(active),
            |_| None,
        )
        .await?;
    assert_eq!(activate_goal_turn(&pool, 0xd61).await?, active.turn());
    repository
        .handle_user_command(
            GoalUserCommand::new(
                command(SUPERSEDE_COMMAND),
                session(SESSION),
                GoalUserAction::Supersede(statement("finish the replacement task")),
            ),
            Some(turn_candidates(0xb62)),
            |_| None,
        )
        .await?;
    repository
        .handle_user_command(
            GoalUserCommand::new(
                command(STOP_COMMAND),
                session(SESSION),
                GoalUserAction::Stop,
            ),
            None,
            |_| None,
        )
        .await?;

    SubmitInputRepository::new(pool.clone())
        .handle_with_candidates(
            SubmitInput::new(
                command(STEER_COMMAND),
                session(SESSION),
                UserContent::try_text(String::from("steer the still-active original turn"))
                    .expect("fixture steering content is admitted"),
                DeliveryRequest::NextSafePoint {
                    expected_active_turn: active.turn(),
                },
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(0xe61)),
            None,
            CancelledModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xf61)),
                ContextFrontierId::from_uuid(Uuid::from_u128(0xf62)),
            ),
            |_| panic!("steering cannot be reclassified while its source remains active"),
            |_| {
                panic!("steering cannot cancel a tool request without a terminal model observation")
            },
        )
        .await?;
    let pending_position: i64 = sqlx::query_scalar(
        "SELECT acceptance_position::bigint FROM accepted_input
          WHERE accepting_command_id = $1 AND disposition_kind = 'pending_steering'",
    )
    .bind(Uuid::from_u128(STEER_COMMAND))
    .fetch_one(&pool)
    .await?;
    let replacement_runtime_relevant: bool =
        sqlx::query_scalar("SELECT goal_turn_is_runtime_relevant($1, $2)")
            .bind(Uuid::from_u128(SESSION))
            .bind(turn_candidates(0xb62).turn().into_uuid())
            .fetch_one(&pool)
            .await?;

    assert_eq!(pending_position, 3);
    assert!(!replacement_runtime_relevant);

    pool.close().await;
    drop(container);
    Ok(())
}

/// INV-048: an alias absent at acceptance is a replayable command rejection,
/// not repository corruption or a partially commissioned lineage.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv048_unknown_goal_model_alias_is_durably_rejected() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    let alias = ModelAlias::from_uuid(Uuid::from_u128(0xa11));
    CreateSessionRepository::new(pool.clone(), credential_pin())
        .handle(creation_with_model(ModelSelectionRequest::Alias(alias)))
        .await?;
    let repository = GoalRepository::new(pool.clone());
    let attach = GoalUserCommand::new(
        command(ATTACH_COMMAND),
        session(SESSION),
        GoalUserAction::Attach(statement("finish the commissioned task")),
    );
    let expected = GoalCommandHandlingOutcome::Recorded(GoalCommandResult::Rejected(
        GoalCommandRejection::UnknownModelAlias,
    ));

    assert_eq!(
        repository
            .handle_user_command(attach.clone(), Some(turn_candidates(0xb51)), |_| None)
            .await?,
        expected
    );
    assert_eq!(
        repository
            .handle_user_command(attach, Some(turn_candidates(0xb52)), |_| None)
            .await?,
        expected
    );
    assert_eq!(
        repository
            .load_command(command(ATTACH_COMMAND))
            .await?
            .expect("unknown-alias rejection receipt exists")
            .result(),
        &GoalCommandResult::Rejected(GoalCommandRejection::UnknownModelAlias)
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM goal_event WHERE session_id = $1",)
            .bind(Uuid::from_u128(SESSION))
            .fetch_one(&pool)
            .await?,
        0
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// INV-048: an applied command receipt can reference only the goal event that
/// carries that same durable command identity.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv048_applied_receipt_cannot_cross_wire_another_command_event()
-> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone(), credential_pin())
        .handle(creation())
        .await?;
    let first = command(0x921);
    let second = command(0x922);
    let mut transaction = pool.begin().await?;
    sqlx::query(
        "INSERT INTO durable_command
            (command_id, command_kind, storage_version, claimed_at)
         VALUES ($1, 'goal', 1, transaction_timestamp()),
                ($2, 'goal', 1, transaction_timestamp())",
    )
    .bind(first.into_uuid())
    .bind(second.into_uuid())
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO goal_command
            (command_id, command_kind, storage_version, session_id,
             operation_kind, statement, result_kind, result_event_ordinal)
         VALUES ($1, 'goal', 1, $2, 'attach', $3, 'applied', $4::bigint),
                ($5, 'goal', 1, $2, 'supersede', $6, 'applied', $7::bigint)",
    )
    .bind(first.into_uuid())
    .bind(Uuid::from_u128(SESSION))
    .bind("finish the first task")
    .bind(2_i64)
    .bind(second.into_uuid())
    .bind("finish the replacement task")
    .bind(1_i64)
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO goal_event
            (session_id, event_ordinal, generation, event_kind, statement, user_command_id)
         VALUES ($1, $2::bigint, $3::bigint, 'commissioned', $4, $5),
                ($1, $6::bigint, $3::bigint, 'superseded', $7, $8)",
    )
    .bind(Uuid::from_u128(SESSION))
    .bind(1_i64)
    .bind(1_i64)
    .bind("finish the first task")
    .bind(first.into_uuid())
    .bind(2_i64)
    .bind("finish the replacement task")
    .bind(second.into_uuid())
    .execute(&mut *transaction)
    .await?;
    let error = transaction
        .commit()
        .await
        .expect_err("cross-wired goal command receipts must fail at commit");

    assert_eq!(
        error
            .as_database_error()
            .and_then(|database| database.code())
            .as_deref(),
        Some("23503")
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// INV-048: an applied goal command names only the event kind corresponding
/// to its immutable operation.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv048_goal_command_operation_matches_the_applied_event_kind() -> Result<(), Box<dyn Error>>
{
    let (container, pool) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone(), credential_pin())
        .handle(creation())
        .await?;
    let mismatched = command(0x923);
    let mut transaction = pool.begin().await?;
    sqlx::query(
        "INSERT INTO durable_command
            (command_id, command_kind, storage_version, claimed_at)
         VALUES ($1, 'goal', 1, transaction_timestamp())",
    )
    .bind(mismatched.into_uuid())
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO goal_command
            (command_id, command_kind, storage_version, session_id,
             operation_kind, result_kind, result_event_ordinal)
         VALUES ($1, 'goal', 1, $2, 'stop', 'applied', 1)",
    )
    .bind(mismatched.into_uuid())
    .bind(Uuid::from_u128(SESSION))
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO goal_event
            (session_id, event_ordinal, generation, event_kind, statement, user_command_id)
         VALUES ($1, 1, 1, 'commissioned', $2, $3)",
    )
    .bind(Uuid::from_u128(SESSION))
    .bind("finish the mismatched task")
    .bind(mismatched.into_uuid())
    .execute(&mut *transaction)
    .await?;
    let error = transaction
        .commit()
        .await
        .expect_err("mismatched goal command operation and event kind must fail at commit");
    let database = error
        .as_database_error()
        .expect("deferred kind correlation reports a database constraint");

    assert_eq!(database.code().as_deref(), Some("23514"));
    assert_eq!(
        database.constraint(),
        Some("goal_command_applied_event_kind")
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// INV-048: exhausting the session acceptance ordinal yields typed scheduler
/// backpressure and a durable, replayable user-command rejection.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv048_goal_turn_acceptance_position_exhaustion_is_typed_and_durable()
-> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone(), credential_pin())
        .handle(creation())
        .await?;
    let repository = GoalRepository::new(pool.clone());
    let attached_turn = turn_candidates(0xb81);
    assert_applied_command(
        repository
            .handle_user_command(
                GoalUserCommand::new(
                    command(0x981),
                    session(SESSION),
                    GoalUserAction::Attach(statement("exhaust the acceptance position")),
                ),
                Some(attached_turn),
                |_| None,
            )
            .await?,
    );
    let maximum = SessionInputPosition::try_from_u64(u64::MAX)
        .expect("the maximum positive position is admitted");
    set_goal_turn_acceptance_position(&pool, attached_turn.turn(), maximum).await?;
    assert_eq!(
        activate_goal_turn(&pool, 0xd81).await?,
        attached_turn.turn()
    );
    mark_goal_turn_completed(&pool, attached_turn.turn()).await?;
    let history_before_rejection = repository
        .load_goal(session(SESSION))
        .await?
        .expect("the attached lineage exists");

    assert_eq!(
        repository
            .reconcile_current_after_execution(
                session(SESSION),
                turn_candidates(0xb82),
                GoalNeed::try_new(String::from("repair execution"))
                    .expect("fixture need is admitted"),
                |_| None,
            )
            .await?,
        GoalTurnContinuationOutcome::AcceptancePositionExhausted { last: maximum }
    );

    let supersede = GoalUserCommand::new(
        command(0x982),
        session(SESSION),
        GoalUserAction::Supersede(statement("replacement cannot yet be queued")),
    );
    let rejected = GoalCommandResult::Rejected(GoalCommandRejection::AcceptancePositionExhausted);
    assert_eq!(
        repository
            .handle_user_command(supersede.clone(), Some(turn_candidates(0xb83)), |_| None,)
            .await?,
        GoalCommandHandlingOutcome::Recorded(rejected.clone())
    );
    let receipt = repository
        .load_command(supersede.command_id())
        .await?
        .expect("the exhaustion receipt is durable");
    assert_eq!(receipt.command(), &supersede);
    assert_eq!(receipt.result(), &rejected);
    let history_after_rejection = repository
        .load_goal(session(SESSION))
        .await?
        .expect("the attached lineage remains");
    assert_eq!(
        history_after_rejection.events(),
        history_before_rejection.events()
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// INV-048: an unrecorded execution failure from an older turn cannot block a
/// resumed generation whose newer goal turn is now current.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv048_delayed_unrecorded_failure_does_not_block_the_resumed_turn()
-> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone(), credential_pin())
        .handle(creation())
        .await?;
    let repository = GoalRepository::new(pool.clone());
    let failed_turn = turn_candidates(0xb91);
    assert_applied_command(
        repository
            .handle_user_command(
                GoalUserCommand::new(
                    command(0x991),
                    session(SESSION),
                    GoalUserAction::Attach(statement("resume beyond the failed turn")),
                ),
                Some(failed_turn),
                |_| None,
            )
            .await?,
    );
    assert_eq!(activate_goal_turn(&pool, 0xd91).await?, failed_turn.turn());
    terminalize_goal_turn_as_failed(&pool, 0xe91).await?;
    let need_text = "wait for user input";
    let need = GoalNeed::try_new(String::from(need_text)).expect("fixture need is admitted");
    let request = tool_request(0xf91);
    let declaration_arguments = format!(
        r#"{{"need":"{need_text}","reason":"user_input_required","transition":"blocked"}}"#
    );
    insert_goal_tool_request(
        &pool,
        failed_turn.turn(),
        request,
        "goal_declare",
        &declaration_arguments,
    )
    .await?;
    assert_applied_transition(
        repository
            .declare_blocked(
                session(SESSION),
                GoalModelBlockedReasonKind::UserInputRequired,
                need,
                GoalModelProvenance::new(failed_turn.turn(), request),
            )
            .await?,
    );
    assert_applied_command(
        repository
            .handle_user_command(
                GoalUserCommand::new(
                    command(0x992),
                    session(SESSION),
                    GoalUserAction::Resume(None),
                ),
                Some(turn_candidates(0xb92)),
                |_| None,
            )
            .await?,
    );

    assert_eq!(
        repository
            .block_execution_failure(
                session(SESSION),
                GoalNeed::try_new(String::from("repair the old failed turn"))
                    .expect("fixture failure need is admitted"),
                GoalSchedulerProvenance::new(failed_turn.turn()),
            )
            .await?,
        GoalTransitionOutcome::NotCurrentGoalTurn
    );
    let scheduler_blocks: i64 =
        sqlx::query_scalar("SELECT count(*) FROM goal_event WHERE scheduler_turn_id = $1")
            .bind(failed_turn.turn().into_uuid())
            .fetch_one(&pool)
            .await?;
    assert_eq!(scheduler_blocks, 0);

    pool.close().await;
    drop(container);
    Ok(())
}

/// INV-048: model goal events bind to the exact `goal_declare` name and
/// canonical arguments carried by their trusted request identity.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv048_model_goal_declaration_request_matches_name_and_arguments()
-> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone(), credential_pin())
        .handle(creation())
        .await?;
    let repository = GoalRepository::new(pool.clone());
    let attached_turn = turn_candidates(0xba1);
    assert_applied_command(
        repository
            .handle_user_command(
                GoalUserCommand::new(
                    command(0x9a1),
                    session(SESSION),
                    GoalUserAction::Attach(statement("bind the declaration request")),
                ),
                Some(attached_turn),
                |_| None,
            )
            .await?,
    );
    let need =
        GoalNeed::try_new(String::from("wait for user input")).expect("fixture need is admitted");
    let matching_arguments =
        r#"{"need":"wait for user input","reason":"user_input_required","transition":"blocked"}"#;
    let wrong_name = tool_request(0xfa1);
    insert_goal_tool_request(
        &pool,
        attached_turn.turn(),
        wrong_name,
        "inspect",
        matching_arguments,
    )
    .await?;
    let name_error = repository
        .declare_blocked(
            session(SESSION),
            GoalModelBlockedReasonKind::UserInputRequired,
            need.clone(),
            GoalModelProvenance::new(attached_turn.turn(), wrong_name),
        )
        .await
        .expect_err("an unrelated tool request cannot source a goal event");
    assert_model_declaration_request_rejected(name_error);

    let wrong_arguments = tool_request(0xfa2);
    insert_goal_tool_request(
        &pool,
        attached_turn.turn(),
        wrong_arguments,
        "goal_declare",
        r#"{"need":"different need","reason":"user_input_required","transition":"blocked"}"#,
    )
    .await?;
    let arguments_error = repository
        .declare_blocked(
            session(SESSION),
            GoalModelBlockedReasonKind::UserInputRequired,
            need,
            GoalModelProvenance::new(attached_turn.turn(), wrong_arguments),
        )
        .await
        .expect_err("mismatched canonical arguments cannot source a goal event");
    assert_model_declaration_request_rejected(arguments_error);

    pool.close().await;
    drop(container);
    Ok(())
}

/// INV-048: the schema admits one goal declaration event per trusted model
/// tool-request identity.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv048_model_goal_declaration_request_is_single_use() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    let unique_request: bool = sqlx::query_scalar(
        "SELECT EXISTS (
            SELECT 1
              FROM pg_constraint
             WHERE conrelid = 'goal_event'::regclass
               AND contype = 'u'
               AND pg_get_constraintdef(oid) = 'UNIQUE (model_tool_request_id)'
        )",
    )
    .fetch_one(&pool)
    .await?;

    assert!(unique_request);

    pool.close().await;
    drop(container);
    Ok(())
}

/// INV-048: a changed current alias that is unavailable at reconciliation is
/// a typed continuation outcome, not durable-state corruption.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv048_changed_unknown_alias_is_a_typed_continuation_outcome() -> Result<(), Box<dyn Error>>
{
    let (container, pool) = migrated_postgres().await?;
    let first_alias = ModelAlias::from_uuid(Uuid::from_u128(0xa21));
    let changed_alias = ModelAlias::from_uuid(Uuid::from_u128(0xa22));
    let frozen =
        FrozenAliasDefinition::selecting(DirectModelSelection::from_uuid(Uuid::from_u128(0xa23)));
    CreateSessionRepository::new(pool.clone(), credential_pin())
        .handle(creation_with_model(ModelSelectionRequest::Alias(
            first_alias,
        )))
        .await?;
    let repository = GoalRepository::new(pool.clone());
    let attached_turn = turn_candidates(0xb71);
    repository
        .handle_user_command(
            GoalUserCommand::new(
                command(0x971),
                session(SESSION),
                GoalUserAction::Attach(statement("continue after the defaults change")),
            ),
            Some(attached_turn),
            |alias| {
                assert_eq!(alias, first_alias);
                Some(frozen)
            },
        )
        .await?;
    assert_eq!(
        activate_goal_turn(&pool, 0xd71).await?,
        attached_turn.turn()
    );
    mark_goal_turn_completed(&pool, attached_turn.turn()).await?;
    sqlx::query(
        "INSERT INTO session_defaults_version
            (session_id, version, model_selection_kind, model_alias_id)
         VALUES ($1, 2, 'alias', $2)",
    )
    .bind(Uuid::from_u128(SESSION))
    .bind(changed_alias.into_uuid())
    .execute(&pool)
    .await?;
    sqlx::query("UPDATE session_current_defaults SET current_version = 2 WHERE session_id = $1")
        .bind(Uuid::from_u128(SESSION))
        .execute(&pool)
        .await?;

    assert_eq!(
        repository
            .reconcile_current_after_execution(
                session(SESSION),
                turn_candidates(0xb72),
                GoalNeed::try_new(String::from("repair execution"))
                    .expect("fixture need is admitted"),
                |_| None,
            )
            .await?,
        GoalTurnContinuationOutcome::UnknownModelAlias {
            alias: changed_alias
        }
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// INV-048: every user-provenance event names an applied receipt at that exact
/// event ordinal; rejected commands cannot source events.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv048_rejected_goal_command_cannot_source_an_event() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone(), credential_pin())
        .handle(creation())
        .await?;
    let rejected = command(0x972);
    let mut transaction = pool.begin().await?;
    sqlx::query(
        "INSERT INTO durable_command
            (command_id, command_kind, storage_version, claimed_at)
         VALUES ($1, 'goal', 1, transaction_timestamp())",
    )
    .bind(rejected.into_uuid())
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO goal_command
            (command_id, command_kind, storage_version, session_id,
             operation_kind, statement, result_kind, rejection_kind)
         VALUES ($1, 'goal', 1, $2, 'attach', $3, 'rejected',
                 'goal_already_attached')",
    )
    .bind(rejected.into_uuid())
    .bind(Uuid::from_u128(SESSION))
    .bind("rejected statement")
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO goal_event
            (session_id, event_ordinal, generation, event_kind, statement, user_command_id)
         VALUES ($1, 1, 1, 'commissioned', $2, $3)",
    )
    .bind(Uuid::from_u128(SESSION))
    .bind("rejected statement")
    .bind(rejected.into_uuid())
    .execute(&mut *transaction)
    .await?;
    let error = transaction
        .commit()
        .await
        .expect_err("a rejected command cannot source a goal event");
    let database = error
        .as_database_error()
        .expect("deferred receipt correlation reports a database constraint");

    assert_eq!(database.code().as_deref(), Some("23514"));
    assert_eq!(
        database.constraint(),
        Some("goal_event_applied_command_receipt")
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// INV-048: applied command receipts and their exact events carry the same
/// immutable statement or optional guidance payload.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv048_goal_command_payload_matches_the_applied_event() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone(), credential_pin())
        .handle(creation())
        .await?;
    let mismatched = command(0x973);
    let mut transaction = pool.begin().await?;
    sqlx::query(
        "INSERT INTO durable_command
            (command_id, command_kind, storage_version, claimed_at)
         VALUES ($1, 'goal', 1, transaction_timestamp())",
    )
    .bind(mismatched.into_uuid())
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO goal_command
            (command_id, command_kind, storage_version, session_id,
             operation_kind, statement, result_kind, result_event_ordinal)
         VALUES ($1, 'goal', 1, $2, 'attach', $3, 'applied', 1)",
    )
    .bind(mismatched.into_uuid())
    .bind(Uuid::from_u128(SESSION))
    .bind("receipt statement")
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO goal_event
            (session_id, event_ordinal, generation, event_kind, statement, user_command_id)
         VALUES ($1, 1, 1, 'commissioned', $2, $3)",
    )
    .bind(Uuid::from_u128(SESSION))
    .bind("different event statement")
    .bind(mismatched.into_uuid())
    .execute(&mut *transaction)
    .await?;
    let error = transaction
        .commit()
        .await
        .expect_err("an applied receipt cannot disagree with its event payload");
    let database = error
        .as_database_error()
        .expect("deferred payload correlation reports a database constraint");

    assert_eq!(database.code().as_deref(), Some("23514"));
    assert_eq!(
        database.constraint(),
        Some("goal_command_applied_event_kind")
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// INV-048: every pursuit-starting user event atomically creates exactly one
/// goal-owned accepted input and turn.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv048_pursuing_goal_event_requires_its_goal_turn() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone(), credential_pin())
        .handle(creation())
        .await?;
    let applied = command(0x974);
    let mut transaction = pool.begin().await?;
    sqlx::query(
        "INSERT INTO durable_command
            (command_id, command_kind, storage_version, claimed_at)
         VALUES ($1, 'goal', 1, transaction_timestamp())",
    )
    .bind(applied.into_uuid())
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO goal_command
            (command_id, command_kind, storage_version, session_id,
             operation_kind, statement, result_kind, result_event_ordinal)
         VALUES ($1, 'goal', 1, $2, 'attach', $3, 'applied', 1)",
    )
    .bind(applied.into_uuid())
    .bind(Uuid::from_u128(SESSION))
    .bind("unscheduled statement")
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO goal_event
            (session_id, event_ordinal, generation, event_kind, statement, user_command_id)
         VALUES ($1, 1, 1, 'commissioned', $2, $3)",
    )
    .bind(Uuid::from_u128(SESSION))
    .bind("unscheduled statement")
    .bind(applied.into_uuid())
    .execute(&mut *transaction)
    .await?;
    let error = transaction
        .commit()
        .await
        .expect_err("a pursuit-starting event cannot omit its goal turn");
    let database = error
        .as_database_error()
        .expect("deferred turn correlation reports a database constraint");

    assert_eq!(database.code().as_deref(), Some("23514"));
    assert_eq!(database.constraint(), Some("goal_event_pursuing_turn"));

    pool.close().await;
    drop(container);
    Ok(())
}

/// INV-048: a queued goal turn's requested and frozen configuration derive
/// from the exact defaults epoch named by its accepted input.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv048_goal_turn_configuration_matches_its_defaults_epoch() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone(), credential_pin())
        .handle(creation())
        .await?;
    let applied = command(0x975);
    let accepted = AcceptedInputId::from_uuid(Uuid::from_u128(0xa75));
    let turn = TurnId::from_uuid(Uuid::from_u128(0xb75));
    let mismatched_selection = Uuid::from_u128(0xc75);
    let mut transaction = pool.begin().await?;
    sqlx::query(
        "INSERT INTO durable_command
            (command_id, command_kind, storage_version, claimed_at)
         VALUES ($1, 'goal', 1, transaction_timestamp())",
    )
    .bind(applied.into_uuid())
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO goal_command
            (command_id, command_kind, storage_version, session_id,
             operation_kind, statement, result_kind, result_event_ordinal)
         VALUES ($1, 'goal', 1, $2, 'attach', $3, 'applied', 1)",
    )
    .bind(applied.into_uuid())
    .bind(Uuid::from_u128(SESSION))
    .bind("misconfigured turn")
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO goal_event
            (session_id, event_ordinal, generation, event_kind, statement, user_command_id)
         VALUES ($1, 1, 1, 'commissioned', $2, $3)",
    )
    .bind(Uuid::from_u128(SESSION))
    .bind("misconfigured turn")
    .bind(applied.into_uuid())
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO accepted_input
            (accepted_input_id, accepting_command_id, session_id,
             content_kind, content_text, delivery_kind,
             expected_active_turn_id, expected_defaults_version,
             model_override_kind, replacement_model_kind,
             replacement_direct_model_selection_id, replacement_model_alias_id,
             acceptance_position, disposition_kind, origin_turn_id)
         VALUES ($1, NULL, $2, 'text', $3, 'start_when_no_active_turn',
                 NULL, 1, 'use_session_default', NULL, NULL, NULL,
                 1, 'origin_of', $4)",
    )
    .bind(accepted.into_uuid())
    .bind(Uuid::from_u128(SESSION))
    .bind("misconfigured turn")
    .bind(turn.into_uuid())
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO queued_input_origin
            (turn_id, accepted_input_id, session_id, acceptance_position,
             priority_kind, defaults_version, interrupt_predecessor_turn_id,
             requested_model_kind, requested_direct_model_selection_id,
             requested_model_alias_id, frozen_model_kind,
             frozen_direct_model_selection_id, frozen_model_alias_id,
             frozen_alias_selected_direct_id, model_parameters,
             known_provider_failure_retry, model_fallback,
             dangerous_tool_auto_approval)
         VALUES ($1, $2, $3, 1, 'ordinary', 1, NULL,
                 'direct', $4, NULL, 'direct', $4, NULL, NULL,
                 'provider_defaults', 'disabled', 'disabled', 'disabled')",
    )
    .bind(turn.into_uuid())
    .bind(accepted.into_uuid())
    .bind(Uuid::from_u128(SESSION))
    .bind(mismatched_selection)
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO turn_lifecycle
            (turn_id, session_id, origin_accepted_input_id,
             acceptance_position, state_kind)
         VALUES ($1, $2, $3, 1, 'queued')",
    )
    .bind(turn.into_uuid())
    .bind(Uuid::from_u128(SESSION))
    .bind(accepted.into_uuid())
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO goal_turn
            (session_id, goal_generation, turn_id, accepted_input_id,
             source_event_ordinal, predecessor_turn_id)
         VALUES ($1, 1, $2, $3, 1, NULL)",
    )
    .bind(Uuid::from_u128(SESSION))
    .bind(turn.into_uuid())
    .bind(accepted.into_uuid())
    .execute(&mut *transaction)
    .await?;
    let error = transaction
        .commit()
        .await
        .expect_err("a goal turn cannot cross-wire another defaults selection");
    let database = error
        .as_database_error()
        .expect("deferred configuration correlation reports a database constraint");

    assert_eq!(database.code().as_deref(), Some("23514"));
    assert_eq!(database.constraint(), Some("goal_turn_runtime_shape"));

    pool.close().await;
    drop(container);
    Ok(())
}

/// INV-048: rejection reasons are closed over the operation paths that can
/// produce them.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv048_goal_command_rejection_matches_its_operation() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone(), credential_pin())
        .handle(creation())
        .await?;
    let impossible = command(0x976);
    let mut transaction = pool.begin().await?;
    sqlx::query(
        "INSERT INTO durable_command
            (command_id, command_kind, storage_version, claimed_at)
         VALUES ($1, 'goal', 1, transaction_timestamp())",
    )
    .bind(impossible.into_uuid())
    .execute(&mut *transaction)
    .await?;
    let error = sqlx::query(
        "INSERT INTO goal_command
            (command_id, command_kind, storage_version, session_id,
             operation_kind, result_kind, rejection_kind)
         VALUES ($1, 'goal', 1, $2, 'stop', 'rejected', 'unknown_model_alias')",
    )
    .bind(impossible.into_uuid())
    .bind(Uuid::from_u128(SESSION))
    .execute(&mut *transaction)
    .await
    .expect_err("stop cannot record an alias-resolution rejection");
    let database = error
        .as_database_error()
        .expect("operation rejection correlation reports a database constraint");

    assert_eq!(database.code().as_deref(), Some("23514"));
    assert_eq!(
        database.constraint(),
        Some("goal_command_rejection_operation")
    );
    drop(transaction);

    pool.close().await;
    drop(container);
    Ok(())
}
