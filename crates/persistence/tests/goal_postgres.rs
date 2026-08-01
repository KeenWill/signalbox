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
    DirectModelSelection, DurableCommandId, Goal, GoalCommandRejection, GoalCommandResult,
    GoalGuidance, GoalNeed, GoalSchedulerProvenance, GoalStatement, GoalUserAction,
    GoalUserCommand, GoalUserProvenance, ModelAlias, ModelSelectionRequest, PreparedCreateSession,
    SemanticTranscriptEntryId, SessionConfigurationDefaults, SessionCreationCause,
    SessionCreationProvenance, SessionId, SubmitInput, TranscriptAncestry, TurnAttemptId, TurnId,
    UserContent,
};
use signalbox_persistence::{
    SessionCredentialPin, SessionModelCredential,
    create_session::CreateSessionRepository,
    goal::{GoalCommandHandlingOutcome, GoalRepository, GoalTransitionOutcome},
    goal_turn::{GoalTurnCandidates, GoalTurnContinuationOutcome},
    local_test_connection_options, migrate,
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

/// INV-048: a goal-owned accepted input activates without a synthetic user
/// command and remains a canonical active origin for the unchanged steer verb.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s_goal_inv048_goal_owned_input_activates_without_a_user_command()
-> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone(), credential_pin())
        .handle(creation())
        .await?;
    let candidates = turn_candidates(0xb01);
    GoalRepository::new(pool.clone())
        .handle_user_command(
            GoalUserCommand::new(
                command(ATTACH_COMMAND),
                session(SESSION),
                GoalUserAction::Attach(statement("finish the commissioned task")),
            ),
            Some(candidates),
            |_| None,
        )
        .await?;

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
            failure_need,
            GoalSchedulerProvenance::new(attached_turn.turn()),
        )
        .await?;
    assert_applied_transition(blocked);

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
