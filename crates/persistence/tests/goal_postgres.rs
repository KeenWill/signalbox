#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    reason = "this standalone integration-test crate uses assertion panics and explicit fixture expectations; the workspace gate remains active for production targets"
)]

use std::error::Error;

use signalbox_application::StartEligibleTurnOutcome;
use signalbox_domain::{
    AcceptedInputId, AcceptedInputTurnActivationIdentities, CancelledModelCallTurnIdentities,
    ContextFrontierId, CreateSession, DeliveryRequest, DirectModelSelection, DurableCommandId,
    Goal, GoalCommandResult, GoalStatement, GoalUserAction, GoalUserCommand, GoalUserProvenance,
    ModelSelectionRequest, PreparedCreateSession, SemanticTranscriptEntryId,
    SessionConfigurationDefaults, SessionCreationCause, SessionCreationProvenance, SessionId,
    SubmitInput, TranscriptAncestry, TurnAttemptId, TurnId, UserContent,
};
use signalbox_persistence::{
    SessionCredentialPin, SessionModelCredential,
    create_session::CreateSessionRepository,
    goal::{GoalCommandHandlingOutcome, GoalRepository},
    goal_turn::GoalTurnCandidates,
    local_test_connection_options, migrate,
    start_eligible_turn::StartEligibleTurnRepository,
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
    CreateSession::new(
        command(CREATE_COMMAND),
        SessionCreationProvenance::new(
            SessionCreationCause::UserInitiated,
            TranscriptAncestry::None,
        ),
        SessionConfigurationDefaults::new(ModelSelectionRequest::Direct(
            DirectModelSelection::from_uuid(Uuid::from_u128(0xa01)),
        )),
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
