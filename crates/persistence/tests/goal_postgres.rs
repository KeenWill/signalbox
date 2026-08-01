#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    reason = "this standalone integration-test crate uses assertion panics and explicit fixture expectations; the workspace gate remains active for production targets"
)]

use std::error::Error;

use signalbox_domain::{
    CreateSession, DirectModelSelection, DurableCommandId, Goal, GoalCommandResult, GoalStatement,
    GoalUserAction, GoalUserCommand, GoalUserProvenance, ModelSelectionRequest,
    PreparedCreateSession, SessionConfigurationDefaults, SessionCreationCause,
    SessionCreationProvenance, SessionId, TranscriptAncestry,
};
use signalbox_persistence::{
    SessionCredentialPin, SessionModelCredential,
    create_session::CreateSessionRepository,
    goal::{GoalCommandHandlingOutcome, GoalRepository},
    local_test_connection_options, migrate,
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

fn latest_event(goal: &Goal) -> signalbox_domain::GoalEvent {
    goal.events()
        .last()
        .cloned()
        .expect("fixture goal has a latest event")
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
        repository.handle_user_command(attach.clone()).await?,
        attach_outcome
    );
    assert_eq!(
        repository.handle_user_command(attach.clone()).await?,
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
        repository.handle_user_command(supersede).await?,
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
        repository.handle_user_command(stop).await?,
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
        repository.handle_user_command(reattach).await?,
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
    repository.handle_user_command(attach).await?;

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
