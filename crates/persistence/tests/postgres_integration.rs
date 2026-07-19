use std::{collections::VecDeque, error::Error, sync::Arc};

use rust_decimal::Decimal;
use signalbox_application::{
    CreateSessionError, CreateSessionOutcome, CreateSessionRequest, CreateSessionService,
    LoadSessionService, ReplaceSessionDefaultsOutcome, ReplaceSessionDefaultsRequest,
    ReplaceSessionDefaultsService, SessionIdGenerator, SubmitInputIdGenerator, SubmitInputOutcome,
    SubmitInputRequest, SubmitInputService,
};
use signalbox_domain::{
    AcceptedInputId, CreateSession, DeliveryRequest, DurableCommandId, ModelAlias,
    ModelSelectionOverride, ModelSelectionRequest, PerInputConfigurationChoices,
    PreparedCreateSession, ReplaceSessionDefaults, ReplaceSessionDefaultsRejectedResult,
    ReplaceSessionDefaultsResult, SessionConfigurationDefaults,
    SessionConfigurationDefaultsVersion, SessionCreationCause, SessionCreationProvenance,
    SessionId, SubmitInput, SubmitInputReconstitutionFailure, SubmitInputRejectedResult,
    SubmitInputResult, TranscriptAncestry, TurnId, UserContent,
};
use signalbox_persistence::{
    MIGRATOR,
    create_session::{
        CreateSessionCorruption, CreateSessionHandlingOutcome, CreateSessionRepository,
        CreateSessionRepositoryError,
    },
    local_test_connection_options, migrate,
    replace_session_defaults::{
        ReplaceSessionDefaultsCorruption, ReplaceSessionDefaultsHandlingOutcome,
        ReplaceSessionDefaultsRepository, ReplaceSessionDefaultsRepositoryError,
    },
    session::{SessionCorruption, SessionRepository, SessionRepositoryError},
    submit_input::{
        SubmitInputCorruption, SubmitInputHandlingOutcome, SubmitInputRepository,
        SubmitInputRepositoryError,
    },
};
use sqlx::{PgConnection, PgPool, migrate::Migrate, postgres::PgPoolOptions, types::Uuid};
use testcontainers_modules::{
    postgres::Postgres,
    testcontainers::{ContainerAsync, ImageExt, runners::AsyncRunner},
};

const POSTGRES_IMAGE_TAG: &str = "18.4-alpine3.23";
const DATABASE_NAME: &str = "signalbox_integration";
const DATABASE_USER: &str = "signalbox";
const DATABASE_PASSWORD: &str = "signalbox-test-only";

async fn migrated_postgres() -> Result<(ContainerAsync<Postgres>, PgPool, String), Box<dyn Error>> {
    let (container, pool, database_url) = unmigrated_postgres().await?;

    migrate(&pool).await?;

    Ok((container, pool, database_url))
}

async fn unmigrated_postgres() -> Result<(ContainerAsync<Postgres>, PgPool, String), Box<dyn Error>>
{
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

    Ok((container, pool, database_url))
}

async fn insert_origin_frontier(
    connection: &mut PgConnection,
    session: Uuid,
    accepted_input: Uuid,
    semantic_entry: Uuid,
    frontier: Uuid,
    member_position: Decimal,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO semantic_transcript_entry
            (source_session_id, semantic_entry_id, payload_kind,
             origin_accepted_input_id, failed_turn_id)
         VALUES ($1, $2, 'origin_accepted_input', $3, NULL)",
    )
    .bind(session)
    .bind(semantic_entry)
    .bind(accepted_input)
    .execute(&mut *connection)
    .await?;

    insert_frontier(
        connection,
        session,
        frontier,
        Decimal::ONE,
        &[(member_position, session, semantic_entry)],
    )
    .await
}

async fn insert_frontier(
    connection: &mut PgConnection,
    owning_session: Uuid,
    frontier: Uuid,
    member_count: Decimal,
    members: &[(Decimal, Uuid, Uuid)],
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO context_frontier
            (owning_session_id, context_frontier_id, member_count)
         VALUES ($1, $2, $3)",
    )
    .bind(owning_session)
    .bind(frontier)
    .bind(member_count)
    .execute(&mut *connection)
    .await?;

    for (member_position, source_session, semantic_entry) in members {
        sqlx::query(
            "INSERT INTO context_frontier_member
                (owning_session_id, context_frontier_id, member_position,
                 source_session_id, semantic_entry_id)
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(owning_session)
        .bind(frontier)
        .bind(member_position)
        .bind(source_session)
        .bind(semantic_entry)
        .execute(&mut *connection)
        .await?;
    }

    Ok(())
}

fn prepared(
    command: u128,
    session: u128,
    selection: ModelSelectionRequest,
) -> PreparedCreateSession {
    CreateSession::new(
        DurableCommandId::from_uuid(Uuid::from_u128(command)),
        SessionCreationProvenance::new(
            SessionCreationCause::OwnerInitiated,
            TranscriptAncestry::None,
        ),
        SessionConfigurationDefaults::new(selection),
    )
    .prepare(SessionId::from_uuid(Uuid::from_u128(session)))
    .expect("owner-initiated creation without ancestry is preparable")
}

fn direct(value: u128) -> ModelSelectionRequest {
    ModelSelectionRequest::Direct(signalbox_domain::DirectModelSelection::from_uuid(
        Uuid::from_u128(value),
    ))
}

fn alias(value: u128) -> ModelSelectionRequest {
    ModelSelectionRequest::Alias(ModelAlias::from_uuid(Uuid::from_u128(value)))
}

fn replacement(
    command: u128,
    session: u128,
    expected: u64,
    selection: ModelSelectionRequest,
) -> ReplaceSessionDefaults {
    ReplaceSessionDefaults::new(
        DurableCommandId::from_uuid(Uuid::from_u128(command)),
        SessionId::from_uuid(Uuid::from_u128(session)),
        SessionConfigurationDefaultsVersion::try_from_u64(expected)
            .expect("test versions are positive"),
        SessionConfigurationDefaults::new(selection),
    )
}

fn replacement_request(
    command: u128,
    session: u128,
    expected: u64,
    selection: ModelSelectionRequest,
) -> ReplaceSessionDefaultsRequest {
    ReplaceSessionDefaultsRequest::try_new(
        DurableCommandId::from_uuid(Uuid::from_u128(command)),
        SessionId::from_uuid(Uuid::from_u128(session)),
        SessionConfigurationDefaultsVersion::try_from_u64(expected)
            .expect("test versions are positive"),
        SessionConfigurationDefaults::new(selection),
    )
    .expect("ordinary test command identities are admitted")
}

fn input_choices(expected: u64, model: ModelSelectionOverride) -> PerInputConfigurationChoices {
    PerInputConfigurationChoices::new(
        SessionConfigurationDefaultsVersion::try_from_u64(expected)
            .expect("test versions are positive"),
        model,
    )
}

fn start_input(
    command: u128,
    session: u128,
    content: &str,
    expected: u64,
    model: ModelSelectionOverride,
) -> SubmitInput {
    SubmitInput::new(
        DurableCommandId::from_uuid(Uuid::from_u128(command)),
        SessionId::from_uuid(Uuid::from_u128(session)),
        UserContent::try_text(content.to_owned()).expect("test content is admitted"),
        DeliveryRequest::StartWhenNoActiveTurn {
            configuration: input_choices(expected, model),
        },
    )
}

fn input_with_delivery(
    command: u128,
    session: u128,
    content: &str,
    delivery: DeliveryRequest,
) -> SubmitInput {
    SubmitInput::new(
        DurableCommandId::from_uuid(Uuid::from_u128(command)),
        SessionId::from_uuid(Uuid::from_u128(session)),
        UserContent::try_text(content.to_owned()).expect("test content is admitted"),
        delivery,
    )
}

#[allow(clippy::too_many_arguments)]
async fn insert_malformed_submit_rejection(
    pool: &PgPool,
    command_id: Uuid,
    source_command_id: Uuid,
    rejection_kind: &str,
    result_expected_active_turn: Option<Uuid>,
    result_expected_defaults: Option<Decimal>,
    result_current_defaults: Option<Decimal>,
    result_unknown_alias: Option<Uuid>,
    result_selected_defaults: Option<Decimal>,
    result_last_position: Option<Decimal>,
) -> Result<(), sqlx::Error> {
    let mut transaction = pool.begin().await?;
    sqlx::query(
        "INSERT INTO durable_command
            (command_id, command_kind, storage_version, claimed_at)
         VALUES ($1, 'submit_input', 1, transaction_timestamp())",
    )
    .bind(command_id)
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO submit_input_command
            (command_id, command_kind, storage_version, session_id,
             actor_kind, actor_turn_id, actor_tool_request_id,
             content_kind, content_text, delivery_kind,
             expected_active_turn_id, expected_defaults_version,
             model_override_kind, replacement_model_kind,
             replacement_direct_model_selection_id, replacement_model_alias_id,
             result_kind, rejection_kind, result_session_id,
             result_accepted_input_id, result_turn_id,
             result_expected_active_turn_id, result_expected_defaults_version,
             result_current_defaults_version, result_unknown_alias_id,
             result_selected_defaults_version, result_last_position)
         SELECT
             $1, command_kind, storage_version, session_id,
             actor_kind, actor_turn_id, actor_tool_request_id,
             content_kind, content_text, delivery_kind,
             expected_active_turn_id, expected_defaults_version,
             model_override_kind, replacement_model_kind,
             replacement_direct_model_selection_id, replacement_model_alias_id,
             'rejected', $3, result_session_id,
             NULL, NULL, $4, $5, $6, $7, $8, $9
           FROM submit_input_command
          WHERE command_id = $2",
    )
    .bind(command_id)
    .bind(source_command_id)
    .bind(rejection_kind)
    .bind(result_expected_active_turn)
    .bind(result_expected_defaults)
    .bind(result_current_defaults)
    .bind(result_unknown_alias)
    .bind(result_selected_defaults)
    .bind(result_last_position)
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await
}

#[derive(Debug)]
struct FixedSessionIds {
    remaining: VecDeque<SessionId>,
}

impl FixedSessionIds {
    fn new(values: impl IntoIterator<Item = SessionId>) -> Self {
        Self {
            remaining: values.into_iter().collect(),
        }
    }
}

impl SessionIdGenerator for FixedSessionIds {
    fn next_session_id(&mut self) -> SessionId {
        self.remaining
            .pop_front()
            .expect("the integration test supplies one identity per invocation")
    }
}

#[derive(Debug)]
struct FixedSubmitInputIds {
    accepted_inputs: VecDeque<AcceptedInputId>,
    turns: VecDeque<TurnId>,
}

impl FixedSubmitInputIds {
    fn new(
        accepted_inputs: impl IntoIterator<Item = AcceptedInputId>,
        turns: impl IntoIterator<Item = TurnId>,
    ) -> Self {
        Self {
            accepted_inputs: accepted_inputs.into_iter().collect(),
            turns: turns.into_iter().collect(),
        }
    }
}

impl SubmitInputIdGenerator for FixedSubmitInputIds {
    fn next_accepted_input_id(&mut self) -> AcceptedInputId {
        self.accepted_inputs
            .pop_front()
            .expect("the integration test supplies one accepted-input candidate per invocation")
    }

    fn next_turn_id(&mut self) -> TurnId {
        self.turns
            .pop_front()
            .expect("the integration test supplies one turn candidate per invocation")
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn embedded_migrator_connects_and_is_idempotent() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    migrate(&pool).await?;
    let connected: i32 = sqlx::query_scalar("SELECT 1").fetch_one(&pool).await?;
    assert_eq!(connected, 1);

    pool.close().await;
    drop(container);

    Ok(())
}

/// INV-007 / INV-009: migration 004 gives every preexisting session its
/// scheduler serialization row and every accepted queued origin one queued
/// lifecycle row without inventing start, frontier, semantic, or attempt facts.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv007_inv009_turn_storage_migration_backfills_existing_queued_work()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = unmigrated_postgres().await?;
    let mut connection = pool.acquire().await?;
    connection
        .ensure_migrations_table("_sqlx_migrations")
        .await?;
    for migration in MIGRATOR.iter().take(3) {
        connection.apply("_sqlx_migrations", migration).await?;
    }
    drop(connection);

    let mut transaction = pool.begin().await?;
    sqlx::raw_sql(
        "INSERT INTO durable_command
            (command_id, command_kind, storage_version, claimed_at)
         VALUES
            ('10000000-0000-4000-8000-000000000401',
             'create_session', 1, transaction_timestamp());
         INSERT INTO session (session_id, creation_cause, ancestry_kind)
         VALUES
            ('70000000-0000-7000-8000-000000000401',
             'owner_initiated', 'none');
         INSERT INTO session_defaults_version
            (session_id, version, model_selection_kind,
             direct_model_selection_id, model_alias_id)
         VALUES
            ('70000000-0000-7000-8000-000000000401', 1, 'direct',
             '80000000-0000-7000-8000-000000000401', NULL);
         INSERT INTO session_current_defaults (session_id, current_version)
         VALUES ('70000000-0000-7000-8000-000000000401', 1);
         INSERT INTO create_session_command
            (command_id, command_kind, storage_version,
             creation_cause, ancestry_kind, initial_defaults_version,
             model_selection_kind, direct_model_selection_id, model_alias_id,
             result_kind, created_session_id)
         VALUES
            ('10000000-0000-4000-8000-000000000401',
             'create_session', 1, 'owner_initiated', 'none', 1,
             'direct', '80000000-0000-7000-8000-000000000401', NULL,
             'applied', '70000000-0000-7000-8000-000000000401');
         INSERT INTO durable_command
            (command_id, command_kind, storage_version, claimed_at)
         VALUES
            ('30000000-0000-4000-8000-000000000401',
             'submit_input', 1, transaction_timestamp());
         INSERT INTO submit_input_command
            (command_id, command_kind, storage_version, session_id,
             actor_kind, actor_turn_id, actor_tool_request_id,
             content_kind, content_text, delivery_kind,
             expected_active_turn_id, expected_defaults_version,
             model_override_kind, replacement_model_kind,
             replacement_direct_model_selection_id, replacement_model_alias_id,
             result_kind, rejection_kind, result_session_id,
             result_accepted_input_id, result_turn_id,
             result_expected_active_turn_id, result_expected_defaults_version,
             result_current_defaults_version, result_unknown_alias_id,
             result_selected_defaults_version, result_last_position)
         VALUES
            ('30000000-0000-4000-8000-000000000401',
             'submit_input', 1,
             '70000000-0000-7000-8000-000000000401',
             'owner', NULL, NULL, 'text', 'queued before migration',
             'start_when_no_active_turn', NULL, 1,
             'use_session_default', NULL, NULL, NULL,
             'applied', NULL,
             '70000000-0000-7000-8000-000000000401',
             '90000000-0000-7000-8000-000000000401',
             'a0000000-0000-7000-8000-000000000401',
             NULL, NULL, NULL, NULL, NULL, NULL);
         INSERT INTO accepted_input
            (accepted_input_id, accepting_command_id, session_id,
             content_kind, content_text, delivery_kind,
             expected_active_turn_id, expected_defaults_version,
             model_override_kind, replacement_model_kind,
             replacement_direct_model_selection_id, replacement_model_alias_id,
             acceptance_position, disposition_kind, origin_turn_id)
         VALUES
            ('90000000-0000-7000-8000-000000000401',
             '30000000-0000-4000-8000-000000000401',
             '70000000-0000-7000-8000-000000000401',
             'text', 'queued before migration',
             'start_when_no_active_turn', NULL, 1,
             'use_session_default', NULL, NULL, NULL,
             1, 'origin_of',
             'a0000000-0000-7000-8000-000000000401');
         INSERT INTO queued_input_origin
            (turn_id, accepted_input_id, session_id, acceptance_position,
             priority_kind, defaults_version,
             requested_model_kind, requested_direct_model_selection_id,
             requested_model_alias_id, frozen_model_kind,
             frozen_direct_model_selection_id, frozen_model_alias_id,
             frozen_alias_selected_direct_id, model_parameters,
             known_provider_failure_retry, model_fallback)
         VALUES
            ('a0000000-0000-7000-8000-000000000401',
             '90000000-0000-7000-8000-000000000401',
             '70000000-0000-7000-8000-000000000401',
             1, 'ordinary', 1,
             'direct', '80000000-0000-7000-8000-000000000401', NULL,
             'direct', '80000000-0000-7000-8000-000000000401', NULL, NULL,
             'provider_defaults', 'disabled', 'disabled');",
    )
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await?;

    migrate(&pool).await?;

    let backfilled: (i64, String, i64, i64, i64) = sqlx::query_as(
        "SELECT
            (SELECT count(*) FROM session_scheduler WHERE session_id = $1),
            turn.state_kind,
            (SELECT count(*) FROM semantic_transcript_entry),
            (SELECT count(*) FROM context_frontier),
            (SELECT count(*) FROM turn_attempt)
         FROM turn_lifecycle AS turn
         WHERE turn.turn_id = $2",
    )
    .bind(Uuid::from_u128(0x70000000000070008000000000000401))
    .bind(Uuid::from_u128(0xa0000000000070008000000000000401))
    .fetch_one(&pool)
    .await?;
    assert_eq!(backfilled, (1, "queued".to_owned(), 0, 0, 0));

    pool.close().await;
    drop(container);
    Ok(())
}

/// S01 / INV-002 / INV-008 / INV-012: the Postgres adapters preserve
/// application command outcomes, return the complete current session
/// projection, and keep infrastructure failure nonterminal.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s01_inv002_inv008_inv012_application_session_services_use_postgres_adapters()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let command_id = DurableCommandId::from_uuid(Uuid::from_u128(0x601));
    let request = CreateSessionRequest::try_new(
        command_id,
        SessionConfigurationDefaults::new(direct(0x801)),
    )?;
    let conflicting_request =
        CreateSessionRequest::try_new(command_id, SessionConfigurationDefaults::new(alias(0x802)))?;
    let winner = SessionId::from_uuid(Uuid::from_u128(0x701));
    let replay_candidate = SessionId::from_uuid(Uuid::from_u128(0x702));
    let conflicting_candidate = SessionId::from_uuid(Uuid::from_u128(0x703));
    let repository = CreateSessionRepository::new(pool.clone());
    let mut service = CreateSessionService::new(
        FixedSessionIds::new([winner, replay_candidate, conflicting_candidate]),
        repository,
    );

    let first = service.execute(request).await?;
    let replay = service.execute(request).await?;
    assert_eq!(first, replay);
    let CreateSessionOutcome::Applied(recorded_receipt) = first else {
        panic!("first application must return the recorded applied receipt");
    };
    assert_eq!(recorded_receipt.session(), winner);
    assert_ne!(recorded_receipt.session(), replay_candidate);

    assert_eq!(
        service.execute(conflicting_request).await?,
        CreateSessionOutcome::ConflictingReuse { command_id }
    );
    let committed_counts: (i64, i64, i64) = sqlx::query_as(
        "SELECT
            (SELECT count(*) FROM durable_command),
            (SELECT count(*) FROM session),
            (SELECT count(*) FROM session_scheduler)",
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(committed_counts, (1, 1, 1));

    let load_service = LoadSessionService::new(SessionRepository::new(pool.clone()));
    let loaded = load_service
        .execute(winner)
        .await?
        .expect("the created session is visible through the application query");
    assert_eq!(loaded.id(), winner);
    assert_eq!(
        loaded.current_configuration_defaults().version(),
        SessionConfigurationDefaultsVersion::first()
    );
    assert_eq!(
        load_service
            .execute(SessionId::from_uuid(Uuid::from_u128(0x7ff)))
            .await?,
        None
    );

    let (_session_ids, repository) = service.into_parts();
    pool.close().await;
    let unavailable_request = CreateSessionRequest::try_new(
        DurableCommandId::from_uuid(Uuid::from_u128(0x602)),
        SessionConfigurationDefaults::new(direct(0x803)),
    )?;
    let mut unavailable_service = CreateSessionService::new(
        FixedSessionIds::new([SessionId::from_uuid(Uuid::from_u128(0x704))]),
        repository,
    );
    let error = unavailable_service
        .execute(unavailable_request)
        .await
        .expect_err("a closed pool cannot become a terminal command outcome");
    assert!(matches!(
        error,
        CreateSessionError::Transaction(CreateSessionRepositoryError::Database(_))
    ));

    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s01_inv003_inv008_inv012_create_session_schema_preserves_typed_facts()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let mut transaction = pool.begin().await?;

    sqlx::query(
        "INSERT INTO durable_command
            (command_id, command_kind, storage_version, claimed_at)
         VALUES
            ('10000000-0000-4000-8000-000000000001',
             'create_session', 1, TIMESTAMPTZ '2026-07-18 00:00:00+00')",
    )
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO session (session_id, creation_cause, ancestry_kind)
         VALUES
            ('70000000-0000-7000-8000-000000000001',
             'owner_initiated', 'none')",
    )
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO session_scheduler (session_id)
         VALUES ('70000000-0000-7000-8000-000000000001')",
    )
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO session_defaults_version
            (session_id, version, model_selection_kind,
             direct_model_selection_id, model_alias_id)
         VALUES
            ('70000000-0000-7000-8000-000000000001', 1, 'direct',
             '70000000-0000-7000-8000-000000000002', NULL)",
    )
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO session_current_defaults (session_id, current_version)
         VALUES ('70000000-0000-7000-8000-000000000001', 1)",
    )
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO create_session_command
            (command_id, command_kind, storage_version,
             creation_cause, ancestry_kind, initial_defaults_version,
             model_selection_kind, direct_model_selection_id, model_alias_id,
             result_kind, created_session_id)
         VALUES
            ('10000000-0000-4000-8000-000000000001',
             'create_session', 1, 'owner_initiated', 'none', 1,
             'direct', '70000000-0000-7000-8000-000000000002', NULL,
             'applied', '70000000-0000-7000-8000-000000000001')",
    )
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await?;

    let stored: (String, String, String, String) = sqlx::query_as(
        "SELECT s.creation_cause,
                s.ancestry_kind,
                d.model_selection_kind,
                c.result_kind
         FROM session AS s
         JOIN session_defaults_version AS d USING (session_id)
         JOIN create_session_command AS c
           ON c.created_session_id = s.session_id",
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        stored,
        (
            "owner_initiated".to_owned(),
            "none".to_owned(),
            "direct".to_owned(),
            "applied".to_owned()
        )
    );

    let generated_identity_defaults: i64 = sqlx::query_scalar(
        "SELECT count(*)
         FROM information_schema.columns
         WHERE table_schema = 'public'
           AND data_type = 'uuid'
           AND is_generated = 'NEVER'
           AND column_default IS NOT NULL",
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(generated_identity_defaults, 0);

    let duplicate_command_id = sqlx::query(
        "INSERT INTO durable_command
            (command_id, command_kind, storage_version, claimed_at)
         VALUES
            ('10000000-0000-4000-8000-000000000001',
             'create_session', 1, TIMESTAMPTZ '2026-07-18 00:00:01+00')",
    )
    .execute(&pool)
    .await
    .expect_err("the owner-global command ID must be unique");
    assert_eq!(
        duplicate_command_id
            .as_database_error()
            .and_then(|error| error.code())
            .as_deref(),
        Some("23505")
    );

    pool.close().await;
    drop(container);

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv012_registry_and_create_session_constraints_reject_torn_or_conflicting_records()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;

    let mut registry_only = pool.begin().await?;
    sqlx::query(
        "INSERT INTO durable_command
            (command_id, command_kind, storage_version, claimed_at)
         VALUES
            ('10000000-0000-4000-8000-000000000011',
             'create_session', 1, TIMESTAMPTZ '2026-07-18 00:00:00+00')",
    )
    .execute(&mut *registry_only)
    .await?;
    let missing_typed_record = registry_only
        .commit()
        .await
        .expect_err("a registry claim without its typed record must not commit");
    assert_eq!(
        missing_typed_record
            .as_database_error()
            .and_then(|error| error.code())
            .as_deref(),
        Some("23503")
    );

    let invalid_kind = sqlx::query(
        "INSERT INTO durable_command
            (command_id, command_kind, storage_version, claimed_at)
         VALUES
            ('10000000-0000-4000-8000-000000000012',
             'unsupported_command', 1, TIMESTAMPTZ '2026-07-18 00:00:00+00')",
    )
    .execute(&pool)
    .await
    .expect_err("an unadmitted command kind must be rejected");
    assert_eq!(
        invalid_kind
            .as_database_error()
            .and_then(|error| error.code())
            .as_deref(),
        Some("23514")
    );

    let mut session_without_command = pool.begin().await?;
    sqlx::query(
        "INSERT INTO session (session_id, creation_cause, ancestry_kind)
         VALUES
            ('70000000-0000-7000-8000-000000000021',
             'owner_initiated', 'none')",
    )
    .execute(&mut *session_without_command)
    .await?;
    sqlx::query(
        "INSERT INTO session_scheduler (session_id)
         VALUES ('70000000-0000-7000-8000-000000000021')",
    )
    .execute(&mut *session_without_command)
    .await?;
    sqlx::query(
        "INSERT INTO session_defaults_version
            (session_id, version, model_selection_kind,
             direct_model_selection_id, model_alias_id)
         VALUES
            ('70000000-0000-7000-8000-000000000021', 1, 'direct',
             '70000000-0000-7000-8000-000000000022', NULL)",
    )
    .execute(&mut *session_without_command)
    .await?;
    sqlx::query(
        "INSERT INTO session_current_defaults (session_id, current_version)
         VALUES ('70000000-0000-7000-8000-000000000021', 1)",
    )
    .execute(&mut *session_without_command)
    .await?;
    let missing_create_command = session_without_command
        .commit()
        .await
        .expect_err("a session without its CreateSession record must not commit");
    assert_eq!(
        missing_create_command
            .as_database_error()
            .and_then(|error| error.code())
            .as_deref(),
        Some("23503")
    );

    pool.close().await;
    drop(container);

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s01_schema_rejects_invalid_provenance_defaults_and_mutation() -> Result<(), Box<dyn Error>>
{
    let (container, pool, _database_url) = migrated_postgres().await?;

    for statement in [
        "INSERT INTO session (session_id, creation_cause, ancestry_kind)
         VALUES
            ('70000000-0000-7000-8000-000000000011',
             'delegated', 'none')",
        "INSERT INTO session (session_id, creation_cause, ancestry_kind)
         VALUES
            ('70000000-0000-7000-8000-000000000012',
             'owner_initiated', 'single_source')",
    ] {
        let error = sqlx::query(statement)
            .execute(&pool)
            .await
            .expect_err("unsupported provenance must be rejected");
        assert_eq!(
            error
                .as_database_error()
                .and_then(|database_error| database_error.code())
                .as_deref(),
            Some("23514")
        );
    }

    let mut transaction = pool.begin().await?;
    sqlx::query(
        "INSERT INTO durable_command
            (command_id, command_kind, storage_version, claimed_at)
         VALUES
            ('10000000-0000-4000-8000-000000000013',
             'create_session', 1, TIMESTAMPTZ '2026-07-18 00:00:00+00')",
    )
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO session (session_id, creation_cause, ancestry_kind)
         VALUES
            ('70000000-0000-7000-8000-000000000013',
             'owner_initiated', 'none')",
    )
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO session_scheduler (session_id)
         VALUES ('70000000-0000-7000-8000-000000000013')",
    )
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO session_defaults_version
            (session_id, version, model_selection_kind,
             direct_model_selection_id, model_alias_id)
         VALUES
            ('70000000-0000-7000-8000-000000000013', 1, 'alias',
             NULL, '70000000-0000-7000-8000-000000000014')",
    )
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO session_current_defaults (session_id, current_version)
         VALUES ('70000000-0000-7000-8000-000000000013', 1)",
    )
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO create_session_command
            (command_id, command_kind, storage_version,
             creation_cause, ancestry_kind, initial_defaults_version,
             model_selection_kind, direct_model_selection_id, model_alias_id,
             result_kind, created_session_id)
         VALUES
            ('10000000-0000-4000-8000-000000000013',
             'create_session', 1, 'owner_initiated', 'none', 1,
             'alias', NULL, '70000000-0000-7000-8000-000000000014',
             'applied', '70000000-0000-7000-8000-000000000013')",
    )
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await?;

    let zero_version = sqlx::query(
        "INSERT INTO session_defaults_version
            (session_id, version, model_selection_kind,
             direct_model_selection_id, model_alias_id)
         VALUES
            ('70000000-0000-7000-8000-000000000013', 0, 'direct',
             '70000000-0000-7000-8000-000000000015', NULL)",
    )
    .execute(&pool)
    .await
    .expect_err("zero is not a domain ordinal");
    assert_eq!(
        zero_version
            .as_database_error()
            .and_then(|error| error.code())
            .as_deref(),
        Some("23514")
    );

    let invalid_selection_shape = sqlx::query(
        "INSERT INTO session_defaults_version
            (session_id, version, model_selection_kind,
             direct_model_selection_id, model_alias_id)
         VALUES
            ('70000000-0000-7000-8000-000000000013', 2, 'direct',
             '70000000-0000-7000-8000-000000000016',
             '70000000-0000-7000-8000-000000000017')",
    )
    .execute(&pool)
    .await
    .expect_err("a typed selection must have exactly one matching UUID");
    assert_eq!(
        invalid_selection_shape
            .as_database_error()
            .and_then(|error| error.code())
            .as_deref(),
        Some("23514")
    );

    let missing_current_version = sqlx::query(
        "UPDATE session_current_defaults
         SET current_version = 2
         WHERE session_id = '70000000-0000-7000-8000-000000000013'",
    )
    .execute(&pool)
    .await
    .expect_err("the current pointer must reference an existing version");
    assert_eq!(
        missing_current_version
            .as_database_error()
            .and_then(|error| error.code())
            .as_deref(),
        Some("23503")
    );

    sqlx::query(
        "INSERT INTO session_defaults_version
            (session_id, version, model_selection_kind,
             direct_model_selection_id, model_alias_id)
         VALUES
            ('70000000-0000-7000-8000-000000000013',
             18446744073709551615, 'direct',
             '70000000-0000-7000-8000-000000000018', NULL)",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE session_current_defaults
         SET current_version = 18446744073709551615
         WHERE session_id = '70000000-0000-7000-8000-000000000013'",
    )
    .execute(&pool)
    .await?;

    let out_of_range_version = sqlx::query(
        "INSERT INTO session_defaults_version
            (session_id, version, model_selection_kind,
             direct_model_selection_id, model_alias_id)
         VALUES
            ('70000000-0000-7000-8000-000000000013',
             18446744073709551616, 'direct',
             '70000000-0000-7000-8000-000000000019', NULL)",
    )
    .execute(&pool)
    .await
    .expect_err("an ordinal above u64::MAX must be rejected");
    assert_eq!(
        out_of_range_version
            .as_database_error()
            .and_then(|error| error.code())
            .as_deref(),
        Some("23514")
    );

    let immutable_session = sqlx::query(
        "UPDATE session
         SET ancestry_kind = 'none'
         WHERE session_id = '70000000-0000-7000-8000-000000000013'",
    )
    .execute(&pool)
    .await
    .expect_err("session provenance is immutable");
    assert_eq!(
        immutable_session
            .as_database_error()
            .and_then(|error| error.code())
            .as_deref(),
        Some("23514")
    );

    pool.close().await;
    drop(container);

    Ok(())
}

/// S01 / INV-012: first handling commits the complete typed creation, equal
/// replay returns the recorded identity, and structural conflict changes
/// nothing. Direct and alias defaults round-trip through reconstitution.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s01_inv012_transaction_apply_replay_conflict_and_restart() -> Result<(), Box<dyn Error>> {
    let (container, pool, database_url) = migrated_postgres().await?;
    let repository = CreateSessionRepository::new(pool.clone());
    let first = prepared(0x101, 0x701, direct(0x801));

    assert_eq!(
        repository.handle(first).await?,
        CreateSessionHandlingOutcome::Applied(first.applied_result())
    );

    let replay_candidate = prepared(0x101, 0x702, direct(0x801));
    assert_eq!(
        repository.handle(replay_candidate).await?,
        CreateSessionHandlingOutcome::Applied(first.applied_result())
    );

    let conflicting = prepared(0x101, 0x703, alias(0x802));
    assert_eq!(
        repository.handle(conflicting).await?,
        CreateSessionHandlingOutcome::ConflictingReuse {
            command_id: first.command().command_id()
        }
    );

    let separate = prepared(0x102, 0x704, direct(0x801));
    let alias_creation = prepared(0x103, 0x705, alias(0x803));
    assert_eq!(
        repository.handle(separate).await?,
        CreateSessionHandlingOutcome::Applied(separate.applied_result())
    );
    assert_eq!(
        repository.handle(alias_creation).await?,
        CreateSessionHandlingOutcome::Applied(alias_creation.applied_result())
    );
    let loaded_alias = repository
        .load(alias_creation.command().command_id())
        .await?
        .expect("the applied alias creation must load");
    assert_eq!(loaded_alias.command(), alias_creation.command());
    assert_eq!(
        loaded_alias.applied_result(),
        alias_creation.applied_result()
    );

    let counts: (i64, i64, i64, i64, i64) = sqlx::query_as(
        "SELECT
            (SELECT count(*) FROM durable_command),
            (SELECT count(*) FROM create_session_command),
            (SELECT count(*) FROM session),
            (SELECT count(*) FROM session_defaults_version),
            (SELECT count(*) FROM session_current_defaults)",
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(counts, (3, 3, 3, 3, 3));

    pool.close().await;
    let restarted_pool = PgPoolOptions::new()
        .max_connections(2)
        .connect_with(local_test_connection_options(&database_url)?)
        .await?;
    let restarted = CreateSessionRepository::new(restarted_pool.clone());
    let reconstituted = restarted
        .load(first.command().command_id())
        .await?
        .expect("committed creation must survive a new pool");
    assert_eq!(reconstituted.command(), first.command());
    assert_eq!(reconstituted.session().id(), first.session().id());
    assert_eq!(reconstituted.applied_result(), first.applied_result());

    restarted_pool.close().await;
    drop(container);
    Ok(())
}

/// S01 / INV-012: the owner-global primary key is the concurrency boundary.
/// Equal duplicates return one winner; unequal duplicates retain that winner
/// and report one typed conflict.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s01_inv012_concurrent_duplicates_converge_on_the_committed_winner()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let repository = CreateSessionRepository::new(pool.clone());

    let equal_left = prepared(0x111, 0x711, direct(0x811));
    let equal_right = prepared(0x111, 0x712, direct(0x811));
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let (left, right) = tokio::join!(
        async {
            barrier.wait().await;
            repository.handle(equal_left).await
        },
        async {
            barrier.wait().await;
            repository.handle(equal_right).await
        }
    );
    let (left, right) = (left?, right?);
    let (
        CreateSessionHandlingOutcome::Applied(left_result),
        CreateSessionHandlingOutcome::Applied(right_result),
    ) = (left, right)
    else {
        panic!("equal duplicates must both return the recorded applied result");
    };
    assert_eq!(left_result, right_result);

    let conflict_left = prepared(0x112, 0x713, direct(0x812));
    let conflict_right = prepared(0x112, 0x714, alias(0x813));
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let (left, right) = tokio::join!(
        async {
            barrier.wait().await;
            repository.handle(conflict_left).await
        },
        async {
            barrier.wait().await;
            repository.handle(conflict_right).await
        }
    );
    let outcomes = [left?, right?];
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| matches!(outcome, CreateSessionHandlingOutcome::Applied(_)))
            .count(),
        1
    );
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| matches!(
                outcome,
                CreateSessionHandlingOutcome::ConflictingReuse { .. }
            ))
            .count(),
        1
    );

    let counts: (i64, i64) = sqlx::query_as(
        "SELECT
            (SELECT count(*) FROM durable_command),
            (SELECT count(*) FROM session)",
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(counts, (2, 2));

    pool.close().await;
    drop(container);
    Ok(())
}

/// S01 / INV-012: a later write failure rolls back the provisional registry
/// insert, so the same command ID remains available for a valid retry.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv012_infrastructure_failure_leaves_the_command_unclaimed() -> Result<(), Box<dyn Error>>
{
    let (container, pool, _database_url) = migrated_postgres().await?;
    let repository = CreateSessionRepository::new(pool.clone());
    let existing = prepared(0x121, 0x721, direct(0x821));
    repository.handle(existing).await?;

    let colliding = prepared(0x122, 0x721, direct(0x822));
    let error = repository
        .handle(colliding)
        .await
        .expect_err("the session identity collision must abort first handling");
    assert!(matches!(error, CreateSessionRepositoryError::Database(_)));
    assert!(
        repository
            .load(colliding.command().command_id())
            .await?
            .is_none()
    );

    let retry = prepared(0x122, 0x722, direct(0x822));
    assert_eq!(
        repository.handle(retry).await?,
        CreateSessionHandlingOutcome::Applied(retry.applied_result())
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// INV-012: an observed owner-global claim is never treated as unseen merely
/// because its typed record is missing or its storage version is unknown.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv012_incomplete_or_unknown_claims_fail_closed_as_corruption()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let defaults_repository = ReplaceSessionDefaultsRepository::new(pool.clone());
    let input_repository = SubmitInputRepository::new(pool.clone());
    let cross_wired = replacement(0x135, 0x735, 1, direct(0x835));
    defaults_repository.handle(cross_wired).await?;

    sqlx::query(
        "DROP TRIGGER durable_command_requires_typed_record
         ON durable_command",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE durable_command
         DROP CONSTRAINT durable_command_storage_version_supported",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO durable_command
            (command_id, command_kind, storage_version, claimed_at)
         VALUES
            ('10000000-0000-4000-8000-000000000131',
             'create_session', 1, transaction_timestamp()),
            ('10000000-0000-4000-8000-000000000132',
             'create_session', 99, transaction_timestamp()),
            ('10000000-0000-4000-8000-000000000133',
             'replace_session_defaults', 1, transaction_timestamp()),
            ('10000000-0000-4000-8000-000000000134',
             'replace_session_defaults', 99, transaction_timestamp()),
            ('10000000-0000-4000-8000-000000000135',
             'submit_input', 1, transaction_timestamp()),
            ('10000000-0000-4000-8000-000000000136',
             'submit_input', 99, transaction_timestamp())",
    )
    .execute(&pool)
    .await?;

    let repository = CreateSessionRepository::new(pool.clone());
    let missing_id =
        DurableCommandId::from_uuid(Uuid::parse_str("10000000-0000-4000-8000-000000000131")?);
    let missing = repository
        .load(missing_id)
        .await
        .expect_err("a claimed identifier without its typed record is corruption");
    assert!(matches!(
        missing,
        CreateSessionRepositoryError::Corruption(CreateSessionCorruption::Missing(
            "typed_command_id"
        ))
    ));

    let unknown_id =
        DurableCommandId::from_uuid(Uuid::parse_str("10000000-0000-4000-8000-000000000132")?);
    let unknown = repository
        .load(unknown_id)
        .await
        .expect_err("an unknown representation version is corruption");
    assert!(matches!(
        unknown,
        CreateSessionRepositoryError::Corruption(CreateSessionCorruption::Unsupported {
            field: "registry_version",
            ..
        })
    ));

    let missing_defaults_id =
        DurableCommandId::from_uuid(Uuid::parse_str("10000000-0000-4000-8000-000000000133")?);
    let missing_defaults = defaults_repository
        .load(missing_defaults_id)
        .await
        .expect_err("an incomplete defaults claim is corruption");
    assert!(matches!(
        missing_defaults,
        ReplaceSessionDefaultsRepositoryError::Corruption(
            ReplaceSessionDefaultsCorruption::Missing("typed_command_id")
        )
    ));

    let unknown_defaults_id =
        DurableCommandId::from_uuid(Uuid::parse_str("10000000-0000-4000-8000-000000000134")?);
    let unknown_defaults = defaults_repository
        .load(unknown_defaults_id)
        .await
        .expect_err("an unknown defaults representation is corruption");
    assert!(matches!(
        unknown_defaults,
        ReplaceSessionDefaultsRepositoryError::Corruption(
            ReplaceSessionDefaultsCorruption::Unsupported {
                field: "registry_version",
                ..
            }
        )
    ));

    let missing_input_id =
        DurableCommandId::from_uuid(Uuid::parse_str("10000000-0000-4000-8000-000000000135")?);
    assert!(matches!(
        input_repository
            .load(missing_input_id)
            .await
            .expect_err("an incomplete input claim is corruption"),
        SubmitInputRepositoryError::Corruption(SubmitInputCorruption::Missing("typed_command_id"))
    ));
    let unknown_input_id =
        DurableCommandId::from_uuid(Uuid::parse_str("10000000-0000-4000-8000-000000000136")?);
    assert!(matches!(
        input_repository
            .load(unknown_input_id)
            .await
            .expect_err("an unknown input representation is corruption"),
        SubmitInputRepositoryError::Corruption(SubmitInputCorruption::Unsupported {
            field: "registry_version",
            ..
        })
    ));

    sqlx::query(
        "ALTER TABLE replace_session_defaults_command
         DROP CONSTRAINT replace_session_defaults_command_result_session_matches,
         DISABLE TRIGGER replace_session_defaults_command_is_append_only",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE replace_session_defaults_command
         SET result_session_id = $2
         WHERE command_id = $1",
    )
    .bind(Uuid::from_u128(0x135))
    .bind(Uuid::from_u128(0x736))
    .execute(&pool)
    .await?;
    let inconsistent = defaults_repository
        .load(cross_wired.command_id())
        .await
        .expect_err("cross-wired typed result facts are corruption");
    assert!(matches!(
        inconsistent,
        ReplaceSessionDefaultsRepositoryError::Corruption(
            ReplaceSessionDefaultsCorruption::Domain(_)
        )
    ));

    pool.close().await;
    drop(container);
    Ok(())
}

/// INV-002 / INV-008 / INV-012: the second admitted command kind retains a
/// complete typed record, while the owner-global registry and append-only
/// constraints reject torn, malformed, or mutable receipts.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv002_inv008_inv012_defaults_schema_enforces_typed_receipts() -> Result<(), Box<dyn Error>>
{
    let (container, pool, _database_url) = migrated_postgres().await?;

    let mut registry_only = pool.begin().await?;
    sqlx::query(
        "INSERT INTO durable_command
            (command_id, command_kind, storage_version, claimed_at)
         VALUES
            ('10000000-0000-4000-8000-000000000201',
             'replace_session_defaults', 1, transaction_timestamp())",
    )
    .execute(&mut *registry_only)
    .await?;
    let torn = registry_only
        .commit()
        .await
        .expect_err("a defaults registry claim must have its exact typed record");
    assert_eq!(
        torn.as_database_error()
            .and_then(|error| error.code())
            .as_deref(),
        Some("23503")
    );

    let mut typed_only = pool.begin().await?;
    sqlx::query(
        "INSERT INTO replace_session_defaults_command
            (command_id, command_kind, storage_version, session_id,
             expected_current_version, model_selection_kind,
             direct_model_selection_id, model_alias_id,
             result_kind, rejection_kind, result_session_id,
             result_installed_version, result_expected_version,
             result_current_version)
         VALUES
            ('10000000-0000-4000-8000-000000000204',
             'replace_session_defaults', 1,
             '70000000-0000-7000-8000-000000000204',
             1, 'direct',
             '70000000-0000-7000-8000-000000000205', NULL,
             'rejected', 'session_not_found',
             '70000000-0000-7000-8000-000000000204',
             NULL, NULL, NULL)",
    )
    .execute(&mut *typed_only)
    .await?;
    let missing_registry = typed_only
        .commit()
        .await
        .expect_err("a typed defaults record cannot commit without its registry claim");
    assert_eq!(
        missing_registry
            .as_database_error()
            .and_then(|error| error.code())
            .as_deref(),
        Some("23503")
    );

    let mut missing_installed = pool.begin().await?;
    sqlx::query(
        "INSERT INTO durable_command
            (command_id, command_kind, storage_version, claimed_at)
         VALUES
            ('10000000-0000-4000-8000-000000000205',
             'replace_session_defaults', 1, transaction_timestamp())",
    )
    .execute(&mut *missing_installed)
    .await?;
    sqlx::query(
        "INSERT INTO replace_session_defaults_command
            (command_id, command_kind, storage_version, session_id,
             expected_current_version, model_selection_kind,
             direct_model_selection_id, model_alias_id,
             result_kind, rejection_kind, result_session_id,
             result_installed_version, result_expected_version,
             result_current_version)
         VALUES
            ('10000000-0000-4000-8000-000000000205',
             'replace_session_defaults', 1,
             '70000000-0000-7000-8000-000000000205',
             1, 'direct',
             '70000000-0000-7000-8000-000000000206', NULL,
             'applied', NULL,
             '70000000-0000-7000-8000-000000000205',
             2, NULL, NULL)",
    )
    .execute(&mut *missing_installed)
    .await?;
    let missing_exact_defaults = missing_installed
        .commit()
        .await
        .expect_err("an applied receipt requires its exact immutable installed defaults");
    assert_eq!(
        missing_exact_defaults
            .as_database_error()
            .and_then(|error| error.code())
            .as_deref(),
        Some("23503")
    );

    let malformed = sqlx::query(
        "INSERT INTO replace_session_defaults_command
            (command_id, command_kind, storage_version, session_id,
             expected_current_version, model_selection_kind,
             direct_model_selection_id, model_alias_id,
             result_kind, rejection_kind, result_session_id,
             result_installed_version, result_expected_version,
             result_current_version)
         VALUES
            ('10000000-0000-4000-8000-000000000202',
             'replace_session_defaults', 1,
             '70000000-0000-7000-8000-000000000202',
             1, 'direct',
             '70000000-0000-7000-8000-000000000203', NULL,
             'applied', NULL,
             '70000000-0000-7000-8000-000000000202',
             NULL, NULL, NULL)",
    )
    .execute(&pool)
    .await
    .expect_err("an applied result requires its typed installed version");
    assert_eq!(
        malformed
            .as_database_error()
            .and_then(|error| error.code())
            .as_deref(),
        Some("23514")
    );

    let repository = ReplaceSessionDefaultsRepository::new(pool.clone());
    let absent = replacement(0x203, 0x703, 1, direct(0x803));
    assert!(matches!(
        repository.handle(absent).await?,
        ReplaceSessionDefaultsHandlingOutcome::Rejected(
            ReplaceSessionDefaultsRejectedResult::SessionNotFound(_)
        )
    ));
    let stored: (String, String, Option<String>) = sqlx::query_as(
        "SELECT result_kind, rejection_kind, result_installed_version::text
         FROM replace_session_defaults_command
         WHERE command_id = $1",
    )
    .bind(Uuid::from_u128(0x203))
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        stored,
        ("rejected".to_owned(), "session_not_found".to_owned(), None)
    );

    let immutable = sqlx::query(
        "UPDATE replace_session_defaults_command
         SET result_kind = result_kind
         WHERE command_id = $1",
    )
    .bind(Uuid::from_u128(0x203))
    .execute(&pool)
    .await
    .expect_err("typed defaults receipts are append-only");
    assert_eq!(
        immutable
            .as_database_error()
            .and_then(|error| error.code())
            .as_deref(),
        Some("23514")
    );
    let immutable_delete = sqlx::query(
        "DELETE FROM replace_session_defaults_command
         WHERE command_id = $1",
    )
    .bind(Uuid::from_u128(0x203))
    .execute(&pool)
    .await
    .expect_err("typed defaults receipts cannot be deleted");
    assert_eq!(
        immutable_delete
            .as_database_error()
            .and_then(|error| error.code())
            .as_deref(),
        Some("23514")
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// S01 / INV-002 / INV-008 / INV-012: the application service through the
/// Postgres adapter records applied and stale outcomes, replays historical
/// receipts, and leaves creation history distinct from current Session.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s01_inv002_inv008_inv012_defaults_apply_replay_stale_and_history()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let create_repository = CreateSessionRepository::new(pool.clone());
    let mut defaults_service =
        ReplaceSessionDefaultsService::new(ReplaceSessionDefaultsRepository::new(pool.clone()));
    let load_service = LoadSessionService::new(SessionRepository::new(pool.clone()));
    let creation = prepared(0x211, 0x711, direct(0x811));
    create_repository.handle(creation).await?;

    let first = replacement_request(0x212, 0x711, 1, alias(0x812));
    let first_outcome = defaults_service.execute(first).await?;
    let ReplaceSessionDefaultsOutcome::Recorded(ReplaceSessionDefaultsResult::Applied(
        first_applied,
    )) = first_outcome
    else {
        panic!("the first replacement must apply");
    };
    assert_eq!(
        first_applied.installed().version(),
        SessionConfigurationDefaultsVersion::try_from_u64(2).expect("positive version")
    );
    assert_eq!(defaults_service.execute(first).await?, first_outcome);

    let conflict = replacement_request(0x212, 0x711, 1, direct(0x813));
    assert_eq!(
        defaults_service.execute(conflict).await?,
        ReplaceSessionDefaultsOutcome::ConflictingReuse {
            command_id: first.command_id()
        }
    );

    let stale = replacement_request(0x213, 0x711, 1, direct(0x814));
    let stale_outcome = defaults_service.execute(stale).await?;
    let ReplaceSessionDefaultsOutcome::Recorded(ReplaceSessionDefaultsResult::Rejected(
        ReplaceSessionDefaultsRejectedResult::CurrentVersionMismatch(stale_result),
    )) = stale_outcome
    else {
        panic!("the unseen stale command must record a mismatch");
    };
    assert_eq!(
        stale_result.current(),
        SessionConfigurationDefaultsVersion::try_from_u64(2).expect("positive version")
    );

    let later = replacement_request(0x214, 0x711, 2, direct(0x815));
    assert!(matches!(
        defaults_service.execute(later).await?,
        ReplaceSessionDefaultsOutcome::Recorded(ReplaceSessionDefaultsResult::Applied(_))
    ));

    assert_eq!(
        defaults_service.execute(first).await?,
        first_outcome,
        "historical applied replay must not require the mutable pointer"
    );
    assert_eq!(
        defaults_service.execute(stale).await?,
        stale_outcome,
        "recorded stale rejection must survive later state"
    );

    let current = load_service
        .execute(creation.session().id())
        .await?
        .expect("the session remains current");
    assert_eq!(
        current.current_configuration_defaults().version(),
        SessionConfigurationDefaultsVersion::try_from_u64(3).expect("positive version")
    );
    assert_eq!(
        current.current_configuration_defaults().defaults().model(),
        direct(0x815)
    );

    let receipt = create_repository
        .load(creation.command().command_id())
        .await?
        .expect("creation history remains loadable");
    assert_eq!(
        receipt.session().configuration_defaults().version(),
        SessionConfigurationDefaultsVersion::first()
    );
    assert_eq!(
        receipt
            .session()
            .configuration_defaults()
            .defaults()
            .model(),
        direct(0x811)
    );

    let counts: (i64, i64, i64) = sqlx::query_as(
        "SELECT
            (SELECT count(*) FROM replace_session_defaults_command),
            (SELECT count(*) FROM session_defaults_version
              WHERE session_id = $1),
            (SELECT current_version::bigint FROM session_current_defaults
              WHERE session_id = $1)",
    )
    .bind(Uuid::from_u128(0x711))
    .fetch_one(&pool)
    .await?;
    assert_eq!(counts, (3, 3, 3));

    pool.close().await;
    drop(container);
    Ok(())
}

/// INV-012: registry dispatch remains owner-global across command kinds while
/// purpose-specific loads distinguish a valid other-kind claim from absence.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv012_cross_kind_reuse_is_conflict_not_corruption_or_absence()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let create_repository = CreateSessionRepository::new(pool.clone());
    let defaults_repository = ReplaceSessionDefaultsRepository::new(pool.clone());
    let input_repository = SubmitInputRepository::new(pool.clone());
    let creation = prepared(0x221, 0x721, direct(0x821));
    create_repository.handle(creation).await?;

    let defaults_reuse = replacement(0x221, 0x721, 1, alias(0x822));
    assert_eq!(
        defaults_repository.handle(defaults_reuse).await?,
        ReplaceSessionDefaultsHandlingOutcome::ConflictingReuse {
            command_id: defaults_reuse.command_id()
        }
    );
    assert!(matches!(
        defaults_repository
            .load(defaults_reuse.command_id())
            .await
            .expect_err("a CreateSession ID is not an unseen defaults receipt"),
        ReplaceSessionDefaultsRepositoryError::DifferentCommandKind { .. }
    ));
    let input_reuse = start_input(
        0x221,
        0x721,
        "cross-kind",
        1,
        ModelSelectionOverride::UseSessionDefault,
    );
    assert_eq!(
        input_repository
            .handle(
                input_reuse.clone(),
                AcceptedInputId::from_uuid(Uuid::from_u128(0x921)),
                Some(TurnId::from_uuid(Uuid::from_u128(0xa21))),
            )
            .await?,
        SubmitInputHandlingOutcome::ConflictingReuse {
            command_id: input_reuse.command_id(),
        }
    );
    assert!(matches!(
        input_repository
            .load(input_reuse.command_id())
            .await
            .expect_err("a CreateSession ID is not an unseen input receipt"),
        SubmitInputRepositoryError::DifferentCommandKind { .. }
    ));

    let defaults = replacement(0x222, 0x721, 1, alias(0x823));
    defaults_repository.handle(defaults).await?;
    let create_reuse = prepared(0x222, 0x722, direct(0x824));
    assert_eq!(
        create_repository.handle(create_reuse).await?,
        CreateSessionHandlingOutcome::ConflictingReuse {
            command_id: create_reuse.command().command_id()
        }
    );
    assert!(matches!(
        create_repository
            .load(defaults.command_id())
            .await
            .expect_err("a defaults ID is not an unseen creation receipt"),
        CreateSessionRepositoryError::DifferentCommandKind { .. }
    ));

    let input = start_input(
        0x223,
        0x721,
        "input winner",
        2,
        ModelSelectionOverride::ReplaceWith(direct(0x825)),
    );
    input_repository
        .handle(
            input.clone(),
            AcceptedInputId::from_uuid(Uuid::from_u128(0x923)),
            Some(TurnId::from_uuid(Uuid::from_u128(0xa23))),
        )
        .await?;
    let defaults_reuse = replacement(0x223, 0x721, 2, direct(0x826));
    assert_eq!(
        defaults_repository.handle(defaults_reuse).await?,
        ReplaceSessionDefaultsHandlingOutcome::ConflictingReuse {
            command_id: input.command_id(),
        }
    );
    let create_reuse = prepared(0x223, 0x723, direct(0x827));
    assert_eq!(
        create_repository.handle(create_reuse).await?,
        CreateSessionHandlingOutcome::ConflictingReuse {
            command_id: input.command_id(),
        }
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// INV-008 / INV-012: two application-service calls expecting one version use
/// the adapter's pointer CAS as their linearization boundary. Exactly one
/// installs the successor and the loser records the winner's version as stale.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv008_inv012_concurrent_defaults_replacements_have_one_winner()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let create_repository = CreateSessionRepository::new(pool.clone());
    create_repository
        .handle(prepared(0x231, 0x731, direct(0x831)))
        .await?;
    let mut left_service =
        ReplaceSessionDefaultsService::new(ReplaceSessionDefaultsRepository::new(pool.clone()));
    let mut right_service =
        ReplaceSessionDefaultsService::new(ReplaceSessionDefaultsRepository::new(pool.clone()));
    let left_command = replacement_request(0x232, 0x731, 1, direct(0x832));
    let right_command = replacement_request(0x233, 0x731, 1, alias(0x833));
    let barrier = Arc::new(tokio::sync::Barrier::new(2));

    let (left, right) = tokio::join!(
        async {
            barrier.wait().await;
            left_service.execute(left_command).await
        },
        async {
            barrier.wait().await;
            right_service.execute(right_command).await
        }
    );
    let outcomes = [left?, right?];
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| matches!(
                outcome,
                ReplaceSessionDefaultsOutcome::Recorded(ReplaceSessionDefaultsResult::Applied(_))
            ))
            .count(),
        1
    );
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| matches!(
                outcome,
                ReplaceSessionDefaultsOutcome::Recorded(ReplaceSessionDefaultsResult::Rejected(
                    ReplaceSessionDefaultsRejectedResult::CurrentVersionMismatch(_)
                ))
            ))
            .count(),
        1
    );

    let counts: (i64, i64, i64) = sqlx::query_as(
        "SELECT
            (SELECT count(*) FROM replace_session_defaults_command
              WHERE session_id = $1),
            (SELECT count(*) FROM session_defaults_version
              WHERE session_id = $1 AND version = 2),
            (SELECT current_version::bigint FROM session_current_defaults
              WHERE session_id = $1)",
    )
    .bind(Uuid::from_u128(0x731))
    .fetch_one(&pool)
    .await?;
    assert_eq!(counts, (2, 1, 2));

    pool.close().await;
    drop(container);
    Ok(())
}

/// INV-008 / INV-012: exhausted versions are recorded rejections, while an
/// infrastructure failure after provisional claim rolls back both the claim
/// and the attempted pointer change.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv008_inv012_exhaustion_and_precommit_failure_are_distinct() -> Result<(), Box<dyn Error>>
{
    let (container, pool, _database_url) = migrated_postgres().await?;
    let create_repository = CreateSessionRepository::new(pool.clone());
    let defaults_repository = ReplaceSessionDefaultsRepository::new(pool.clone());
    create_repository
        .handle(prepared(0x241, 0x741, direct(0x841)))
        .await?;
    create_repository
        .handle(prepared(0x242, 0x742, direct(0x842)))
        .await?;

    sqlx::query(
        "INSERT INTO session_defaults_version
            (session_id, version, model_selection_kind,
             direct_model_selection_id, model_alias_id)
         VALUES ($1, 18446744073709551615, 'direct', $2, NULL)",
    )
    .bind(Uuid::from_u128(0x741))
    .bind(Uuid::from_u128(0x843))
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE session_current_defaults
         SET current_version = 18446744073709551615
         WHERE session_id = $1",
    )
    .bind(Uuid::from_u128(0x741))
    .execute(&pool)
    .await?;
    let exhausted = replacement(0x243, 0x741, u64::MAX, alias(0x844));
    let exhausted_outcome = defaults_repository.handle(exhausted).await?;
    assert!(matches!(
        exhausted_outcome,
        ReplaceSessionDefaultsHandlingOutcome::Rejected(
            ReplaceSessionDefaultsRejectedResult::VersionExhausted(_)
        )
    ));
    assert_eq!(
        defaults_repository.handle(exhausted).await?,
        exhausted_outcome
    );

    sqlx::query(
        "INSERT INTO session_defaults_version
            (session_id, version, model_selection_kind,
             direct_model_selection_id, model_alias_id)
         VALUES ($1, 2, 'direct', $2, NULL)",
    )
    .bind(Uuid::from_u128(0x742))
    .bind(Uuid::from_u128(0x845))
    .execute(&pool)
    .await?;
    let fails_after_claim = replacement_request(0x244, 0x742, 1, alias(0x846));
    let mut failing_service = ReplaceSessionDefaultsService::new(defaults_repository.clone());
    assert!(matches!(
        failing_service
            .execute(fails_after_claim)
            .await
            .expect_err("the colliding immutable successor aborts the transaction"),
        ReplaceSessionDefaultsRepositoryError::Database(_)
    ));
    assert!(
        defaults_repository
            .load(fails_after_claim.command_id())
            .await?
            .is_none(),
        "the failed transaction must not claim the command ID"
    );
    let pointer: i64 = sqlx::query_scalar(
        "SELECT current_version::bigint
         FROM session_current_defaults
         WHERE session_id = $1",
    )
    .bind(Uuid::from_u128(0x742))
    .fetch_one(&pool)
    .await?;
    assert_eq!(pointer, 1);

    pool.close().await;
    drop(container);
    Ok(())
}

/// S01 / INV-003 / INV-008 / INV-012: load-by-session identity returns the
/// complete version selected by the current pointer, while creation receipt
/// replay remains pinned to the immutable creation-time version.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s01_inv003_inv008_inv012_current_session_load_and_receipt_replay_remain_distinct()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let create_repository = CreateSessionRepository::new(pool.clone());
    let session_repository = SessionRepository::new(pool.clone());
    let direct_creation = prepared(0x501, 0x901, direct(0x801));
    let alias_creation = prepared(0x502, 0x902, alias(0x802));

    assert!(
        session_repository
            .load_session(SessionId::from_uuid(Uuid::from_u128(0x999)))
            .await?
            .is_none(),
        "only an absent session row is a not-found result"
    );
    assert_eq!(
        create_repository.handle(direct_creation).await?,
        CreateSessionHandlingOutcome::Applied(direct_creation.applied_result())
    );
    assert_eq!(
        create_repository.handle(alias_creation).await?,
        CreateSessionHandlingOutcome::Applied(alias_creation.applied_result())
    );

    let loaded_direct = session_repository
        .load_session(direct_creation.session().id())
        .await?
        .expect("the committed direct session must load");
    assert_eq!(loaded_direct.id(), direct_creation.session().id());
    assert_eq!(
        loaded_direct.creation_provenance(),
        direct_creation.session().provenance()
    );
    assert_eq!(
        loaded_direct.current_configuration_defaults().version(),
        SessionConfigurationDefaultsVersion::first()
    );
    assert_eq!(
        loaded_direct
            .current_configuration_defaults()
            .defaults()
            .model(),
        direct(0x801)
    );

    let loaded_alias = session_repository
        .load_session(alias_creation.session().id())
        .await?
        .expect("the committed alias session must load");
    assert_eq!(
        loaded_alias
            .current_configuration_defaults()
            .defaults()
            .model(),
        alias(0x802)
    );

    let direct_session_id = Uuid::from_u128(0x901);
    sqlx::query(
        "INSERT INTO session_defaults_version
            (session_id, version, model_selection_kind,
             direct_model_selection_id, model_alias_id)
         VALUES ($1, 2, 'alias', NULL, $2)",
    )
    .bind(direct_session_id)
    .bind(Uuid::from_u128(0x803))
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE session_current_defaults
         SET current_version = 2
         WHERE session_id = $1",
    )
    .bind(direct_session_id)
    .execute(&pool)
    .await?;

    let current = session_repository
        .load_session(direct_creation.session().id())
        .await
        .expect("the advanced current session load must succeed")
        .expect("the session row remains present");
    assert_eq!(
        current.current_configuration_defaults().version(),
        SessionConfigurationDefaultsVersion::try_from_u64(2)
            .expect("two is a positive defaults version")
    );
    assert_eq!(
        current.current_configuration_defaults().defaults().model(),
        alias(0x803)
    );

    let receipt = create_repository
        .load(direct_creation.command().command_id())
        .await?
        .expect("creation receipt remains loadable after current defaults advance");
    assert_eq!(receipt.command(), direct_creation.command());
    assert_eq!(
        receipt.session().configuration_defaults().version(),
        SessionConfigurationDefaultsVersion::first()
    );
    assert_eq!(
        receipt
            .session()
            .configuration_defaults()
            .defaults()
            .model(),
        direct(0x801)
    );

    let replay_candidate = prepared(0x501, 0x903, direct(0x801));
    assert_eq!(
        create_repository.handle(replay_candidate).await?,
        CreateSessionHandlingOutcome::Applied(direct_creation.applied_result())
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// INV-002 / INV-003 / INV-008: once the session row exists, absent,
/// malformed, unknown, undecodable, or non-unique current projection facts fail
/// closed as typed corruption rather than becoming `None` or nearby valid
/// defaults.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv002_inv003_inv008_current_session_corruption_fails_closed() -> Result<(), Box<dyn Error>>
{
    let (container, pool, _database_url) = migrated_postgres().await?;
    let create_repository = CreateSessionRepository::new(pool.clone());
    let session_repository = SessionRepository::new(pool.clone());
    let missing_pointer = prepared(0x511, 0x911, direct(0x811));
    let invalid_pointer = prepared(0x512, 0x912, direct(0x812));
    let missing_selected = prepared(0x513, 0x913, direct(0x813));
    let malformed_selected = prepared(0x514, 0x914, direct(0x814));
    let unknown_provenance = prepared(0x515, 0x915, direct(0x815));
    let duplicate_projection = prepared(0x516, 0x916, direct(0x816));
    for creation in [
        missing_pointer,
        invalid_pointer,
        missing_selected,
        malformed_selected,
        unknown_provenance,
        duplicate_projection,
    ] {
        create_repository.handle(creation).await?;
    }

    sqlx::query(
        "ALTER TABLE session
         DROP CONSTRAINT session_current_defaults_fk",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "DELETE FROM session_current_defaults
         WHERE session_id = $1",
    )
    .bind(Uuid::from_u128(0x911))
    .execute(&pool)
    .await?;

    sqlx::query(
        "ALTER TABLE session_current_defaults
         DROP CONSTRAINT session_current_defaults_version_fk,
         DROP CONSTRAINT session_current_defaults_version_positive_u64",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE session_current_defaults
         SET current_version = 0
         WHERE session_id = $1",
    )
    .bind(Uuid::from_u128(0x912))
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE session_current_defaults
         SET current_version = 2
         WHERE session_id = $1",
    )
    .bind(Uuid::from_u128(0x913))
    .execute(&pool)
    .await?;

    sqlx::query(
        "ALTER TABLE session_defaults_version
         DROP CONSTRAINT session_defaults_version_model_selection_shape",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO session_defaults_version
            (session_id, version, model_selection_kind,
             direct_model_selection_id, model_alias_id)
         VALUES ($1, 2, 'direct', NULL, NULL)",
    )
    .bind(Uuid::from_u128(0x914))
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE session_current_defaults
         SET current_version = 2
         WHERE session_id = $1",
    )
    .bind(Uuid::from_u128(0x914))
    .execute(&pool)
    .await?;

    sqlx::query(
        "ALTER TABLE create_session_command
         DROP CONSTRAINT create_session_command_provenance_fk",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE session
         DROP CONSTRAINT session_creation_cause_closed,
         DISABLE TRIGGER session_is_append_only",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE session
         SET creation_cause = 'unknown'
         WHERE session_id = $1",
    )
    .bind(Uuid::from_u128(0x915))
    .execute(&pool)
    .await?;

    sqlx::query(
        "ALTER TABLE session_current_defaults
         DROP CONSTRAINT session_current_defaults_pkey",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO session_current_defaults (session_id, current_version)
         VALUES ($1, 1)",
    )
    .bind(Uuid::from_u128(0x916))
    .execute(&pool)
    .await?;

    let missing = session_repository
        .load_session(missing_pointer.session().id())
        .await
        .expect_err("a missing pointer is corruption");
    assert!(matches!(
        missing,
        SessionRepositoryError::Corruption(SessionCorruption::Missing(
            "current_defaults_session_id"
        ))
    ));

    let invalid = session_repository
        .load_session(invalid_pointer.session().id())
        .await
        .expect_err("a non-positive pointer version is corruption");
    assert!(matches!(
        invalid,
        SessionRepositoryError::Corruption(SessionCorruption::InvalidOrdinal {
            field: "current_version",
            ..
        })
    ));

    let missing_selected_row = session_repository
        .load_session(missing_selected.session().id())
        .await
        .expect_err("a missing selected defaults row is corruption");
    assert!(matches!(
        missing_selected_row,
        SessionRepositoryError::Corruption(SessionCorruption::Missing(
            "selected_defaults_session_id"
        ))
    ));

    let malformed = session_repository
        .load_session(malformed_selected.session().id())
        .await
        .expect_err("a malformed selected defaults record is corruption");
    assert!(matches!(
        malformed,
        SessionRepositoryError::Corruption(SessionCorruption::Inconsistent("model selection"))
    ));

    let unknown = session_repository
        .load_session(unknown_provenance.session().id())
        .await
        .expect_err("an unknown creation cause is corruption");
    assert!(matches!(
        unknown,
        SessionRepositoryError::Corruption(SessionCorruption::Unsupported {
            field: "creation cause",
            ..
        })
    ));

    let duplicate = session_repository
        .load_session(duplicate_projection.session().id())
        .await
        .expect_err("more than one current projection row is corruption");
    assert!(matches!(
        duplicate,
        SessionRepositoryError::Corruption(SessionCorruption::Inconsistent(
            "current session projection cardinality"
        ))
    ));

    pool.close().await;
    drop(container);
    Ok(())
}

/// INV-002 / INV-007 / INV-008 / INV-012: the third command family is a
/// normalized closed schema whose deferred reverse and effect constraints
/// reject a claim without its typed terminal record.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv002_inv007_inv008_inv012_submit_schema_is_closed_and_normalized()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let tables: Vec<String> = sqlx::query_scalar(
        "SELECT table_name
           FROM information_schema.tables
          WHERE table_schema = 'public'
            AND table_name IN (
                'submit_input_command',
                'accepted_input',
                'queued_input_origin'
            )
          ORDER BY table_name",
    )
    .fetch_all(&pool)
    .await?;
    assert_eq!(
        tables,
        vec![
            "accepted_input".to_owned(),
            "queued_input_origin".to_owned(),
            "submit_input_command".to_owned(),
        ]
    );

    let constraints: Vec<String> = sqlx::query_scalar(
        "SELECT conname
           FROM pg_constraint
          WHERE conname IN (
                'submit_input_command_applied_effect_fk',
                'submit_input_command_last_position_fk',
                'submit_input_command_current_defaults_fk',
                'submit_input_command_selected_defaults_fk',
                'submit_input_command_actor_shape',
                'submit_input_command_delivery_shape',
                'submit_input_command_result_shape',
                'accepted_input_queued_origin_fk',
                'queued_input_origin_accepted_input_fk'
          )
          ORDER BY conname",
    )
    .fetch_all(&pool)
    .await?;
    assert_eq!(constraints.len(), 9);

    let mut transaction = pool.begin().await?;
    sqlx::query(
        "INSERT INTO durable_command
            (command_id, command_kind, storage_version, claimed_at)
         VALUES ($1, 'submit_input', 1, transaction_timestamp())",
    )
    .bind(Uuid::from_u128(0x3ff))
    .execute(&mut *transaction)
    .await?;
    let error = transaction
        .commit()
        .await
        .expect_err("a registry claim without its typed SubmitInput record must not commit");
    assert_eq!(
        error.as_database_error().and_then(|error| error.code()),
        Some("23503".into())
    );

    let command = start_input(
        0x3fe,
        0x7fe,
        "immutable",
        1,
        ModelSelectionOverride::UseSessionDefault,
    );
    SubmitInputRepository::new(pool.clone())
        .handle(
            command,
            AcceptedInputId::from_uuid(Uuid::from_u128(0x9fe)),
            Some(TurnId::from_uuid(Uuid::from_u128(0xafe))),
        )
        .await?;
    let error = sqlx::query(
        "UPDATE submit_input_command
            SET content_text = 'mutated'
          WHERE command_id = $1",
    )
    .bind(Uuid::from_u128(0x3fe))
    .execute(&pool)
    .await
    .expect_err("typed SubmitInput records are append-only");
    assert_eq!(
        error.as_database_error().and_then(|error| error.code()),
        Some("23514".into())
    );

    CreateSessionRepository::new(pool.clone())
        .handle(prepared(0x3fc, 0x7fc, direct(0x8fc)))
        .await?;
    SubmitInputRepository::new(pool.clone())
        .handle(
            start_input(
                0x3fd,
                0x7fc,
                "complete source",
                1,
                ModelSelectionOverride::UseSessionDefault,
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(0x9fd)),
            Some(TurnId::from_uuid(Uuid::from_u128(0xafd))),
        )
        .await?;

    let source_command_id = Uuid::from_u128(0x3fd);
    let malformed_rejections = [
        (
            Uuid::from_u128(0x3fa),
            "no_active_turn",
            None,
            None,
            None,
            None,
            None,
            None,
        ),
        (
            Uuid::from_u128(0x3f9),
            "session_defaults_version_mismatch",
            None,
            None,
            Some(Decimal::ONE),
            None,
            None,
            None,
        ),
        (
            Uuid::from_u128(0x3f8),
            "unknown_model_alias",
            None,
            None,
            None,
            Some(Uuid::from_u128(0x8f8)),
            None,
            None,
        ),
        (
            Uuid::from_u128(0x3f7),
            "acceptance_position_exhausted",
            None,
            None,
            None,
            None,
            None,
            None,
        ),
    ];
    for (
        command_id,
        rejection_kind,
        expected_turn,
        expected_defaults,
        current_defaults,
        unknown_alias,
        selected_defaults,
        last_position,
    ) in malformed_rejections
    {
        let error = insert_malformed_submit_rejection(
            &pool,
            command_id,
            source_command_id,
            rejection_kind,
            expected_turn,
            expected_defaults,
            current_defaults,
            unknown_alias,
            selected_defaults,
            last_position,
        )
        .await
        .expect_err("a rejection with missing required evidence must not commit");
        assert_eq!(
            error.as_database_error().and_then(|error| error.code()),
            Some("23514".into())
        );
    }

    let error = insert_malformed_submit_rejection(
        &pool,
        Uuid::from_u128(0x3f6),
        source_command_id,
        "acceptance_position_exhausted",
        None,
        None,
        None,
        None,
        None,
        Some(Decimal::from(u64::MAX)),
    )
    .await
    .expect_err("exhaustion must reference the session's actual maximum-position input");
    assert_eq!(
        error.as_database_error().and_then(|error| error.code()),
        Some("23503".into())
    );

    let mut transaction = pool.begin().await?;
    sqlx::query(
        "INSERT INTO durable_command
            (command_id, command_kind, storage_version, claimed_at)
         VALUES ($1, 'submit_input', 1, transaction_timestamp())",
    )
    .bind(Uuid::from_u128(0x3fb))
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO submit_input_command
            (command_id, command_kind, storage_version, session_id,
             actor_kind, actor_turn_id, actor_tool_request_id,
             content_kind, content_text, delivery_kind,
             expected_active_turn_id, expected_defaults_version,
             model_override_kind, replacement_model_kind,
             replacement_direct_model_selection_id, replacement_model_alias_id,
             result_kind, rejection_kind, result_session_id,
             result_accepted_input_id, result_turn_id,
             result_expected_active_turn_id, result_expected_defaults_version,
             result_current_defaults_version, result_unknown_alias_id,
             result_selected_defaults_version, result_last_position)
         SELECT
             $1, command_kind, storage_version, session_id,
             actor_kind, actor_turn_id, actor_tool_request_id,
             content_kind, content_text, delivery_kind,
             expected_active_turn_id, expected_defaults_version,
             model_override_kind, replacement_model_kind,
             replacement_direct_model_selection_id, replacement_model_alias_id,
             result_kind, rejection_kind, result_session_id,
             $2, $3,
             result_expected_active_turn_id, result_expected_defaults_version,
             result_current_defaults_version, result_unknown_alias_id,
             result_selected_defaults_version, result_last_position
           FROM submit_input_command
          WHERE command_id = $4",
    )
    .bind(Uuid::from_u128(0x3fb))
    .bind(Uuid::from_u128(0x9fb))
    .bind(Uuid::from_u128(0xafb))
    .bind(Uuid::from_u128(0x3fd))
    .execute(&mut *transaction)
    .await?;
    let error = transaction
        .commit()
        .await
        .expect_err("an applied typed receipt without its exact effects must not commit");
    assert_eq!(
        error.as_database_error().and_then(|error| error.code()),
        Some("23503".into())
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// S01 / INV-005 / INV-008 / INV-010 / INV-012 / INV-028: first acceptance
/// commits the complete exact receipt and immutable queued origin; equal
/// replay and a restarted adapter return that receipt without consulting new
/// candidates.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s01_inv005_inv008_inv010_inv012_inv028_submit_apply_replay_conflict_and_restart()
-> Result<(), Box<dyn Error>> {
    let (container, pool, database_url) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone())
        .handle(prepared(0x301, 0x701, direct(0x801)))
        .await?;
    let exact = " \tline one\r\ncafe\u{301}\n ";
    let command = start_input(
        0x302,
        0x701,
        exact,
        1,
        ModelSelectionOverride::UseSessionDefault,
    );
    let accepted = AcceptedInputId::from_uuid(Uuid::from_u128(0x901));
    let turn = TurnId::from_uuid(Uuid::from_u128(0xa01));
    let request = SubmitInputRequest::try_new(
        command.command_id(),
        command.session(),
        command.content().clone(),
        command.delivery(),
    )?;
    let mut service = SubmitInputService::new(
        FixedSubmitInputIds::new(
            [
                accepted,
                AcceptedInputId::from_uuid(Uuid::from_u128(0x902)),
                AcceptedInputId::from_uuid(Uuid::from_u128(0x903)),
            ],
            [
                turn,
                TurnId::from_uuid(Uuid::from_u128(0xa02)),
                TurnId::from_uuid(Uuid::from_u128(0xa03)),
            ],
        ),
        SubmitInputRepository::new(pool.clone()),
    );

    let first = service.execute(request.clone()).await?;
    let SubmitInputOutcome::Recorded(SubmitInputResult::Applied(applied)) = first.clone() else {
        panic!("no-active-turn start must apply");
    };
    assert_eq!(applied.accepted_input(), accepted);
    assert_eq!(applied.turn(), turn);
    assert_eq!(applied.acceptance_position().as_u64(), 1);
    assert_eq!(service.execute(request.clone()).await?, first);

    let conflicting = SubmitInputRequest::try_new(
        command.command_id(),
        command.session(),
        UserContent::try_text("different".to_owned())
            .expect("conflicting test content is admitted"),
        command.delivery(),
    )?;
    assert_eq!(
        service.execute(conflicting).await?,
        SubmitInputOutcome::ConflictingReuse {
            command_id: command.command_id(),
        }
    );

    let stored: (String, String, String, i64, String) = sqlx::query_as(
        "SELECT typed.content_text, accepted.content_text, queued.priority_kind,
                queued.acceptance_position::bigint, turn.state_kind
           FROM submit_input_command AS typed
           JOIN accepted_input AS accepted
             ON accepted.accepting_command_id = typed.command_id
           JOIN queued_input_origin AS queued
             ON queued.accepted_input_id = accepted.accepted_input_id
           JOIN turn_lifecycle AS turn
             ON turn.turn_id = queued.turn_id
          WHERE typed.command_id = $1",
    )
    .bind(Uuid::from_u128(0x302))
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        stored,
        (
            exact.to_owned(),
            exact.to_owned(),
            "ordinary".into(),
            1,
            "queued".into()
        )
    );

    drop(service);
    pool.close().await;
    let restarted_pool = PgPoolOptions::new()
        .max_connections(4)
        .connect_with(local_test_connection_options(&database_url)?)
        .await?;
    let restarted = SubmitInputRepository::new(restarted_pool.clone());
    let mut restarted_service = SubmitInputService::new(
        FixedSubmitInputIds::new(
            [AcceptedInputId::from_uuid(Uuid::from_u128(0x904))],
            [TurnId::from_uuid(Uuid::from_u128(0xa04))],
        ),
        restarted.clone(),
    );
    let loaded = restarted
        .load(command.command_id())
        .await?
        .expect("the committed receipt survives adapter restart");
    assert_eq!(loaded.command(), &command);
    assert_eq!(restarted_service.execute(request).await?, first);
    let effect_counts: (i64, i64, i64, i64) = sqlx::query_as(
        "SELECT
            (SELECT count(*) FROM submit_input_command WHERE command_id = $1),
            (SELECT count(*) FROM accepted_input WHERE accepting_command_id = $1),
            (SELECT count(*)
               FROM queued_input_origin AS queued
               JOIN accepted_input AS accepted
                 ON accepted.accepted_input_id = queued.accepted_input_id
              WHERE accepted.accepting_command_id = $1),
            (SELECT count(*)
               FROM turn_lifecycle AS turn
               JOIN accepted_input AS accepted
                 ON accepted.origin_turn_id = turn.turn_id
              WHERE accepted.accepting_command_id = $1)",
    )
    .bind(Uuid::from_u128(0x302))
    .fetch_one(&restarted_pool)
    .await?;
    assert_eq!(effect_counts, (1, 1, 1, 1));

    drop(restarted_service);
    restarted_pool.close().await;
    drop(container);
    Ok(())
}

/// S01 / INV-006 / INV-009 / INV-015: one complete future eligibility
/// transaction can bind the exact origin frontier and prepared attempt, while
/// the database independently rejects contradictory lifecycle histories.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s01_inv006_inv009_inv015_turn_storage_enforces_lifecycle_consistency()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone())
        .handle(prepared(0x401, 0x801, direct(0xc01)))
        .await?;
    let submit = SubmitInputRepository::new(pool.clone());
    submit
        .handle(
            start_input(
                0x402,
                0x801,
                "first",
                1,
                ModelSelectionOverride::UseSessionDefault,
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(0x901)),
            Some(TurnId::from_uuid(Uuid::from_u128(0xa01))),
        )
        .await?;
    submit
        .handle(
            start_input(
                0x403,
                0x801,
                "second",
                1,
                ModelSelectionOverride::UseSessionDefault,
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(0x902)),
            Some(TurnId::from_uuid(Uuid::from_u128(0xa02))),
        )
        .await?;

    let session = Uuid::from_u128(0x801);
    let first_turn = Uuid::from_u128(0xa01);
    let first_attempt = Uuid::from_u128(0xb01);
    let first_entry = Uuid::from_u128(0xd01);
    let first_frontier = Uuid::from_u128(0xe01);
    let mut activation = pool.begin().await?;
    insert_origin_frontier(
        &mut activation,
        session,
        Uuid::from_u128(0x901),
        first_entry,
        first_frontier,
        Decimal::ONE,
    )
    .await?;
    sqlx::query(
        "INSERT INTO turn_attempt
            (turn_attempt_id, turn_id, session_id, continued_from_attempt_id,
             state_kind, end_variant, end_disposition)
         VALUES ($1, $2, $3, NULL, 'prepared', NULL, NULL)",
    )
    .bind(first_attempt)
    .bind(first_turn)
    .bind(session)
    .execute(&mut *activation)
    .await?;
    sqlx::query(
        "UPDATE turn_lifecycle
            SET state_kind = 'active',
                start_lineage_kind = 'first_in_session',
                starting_frontier_id = $1,
                active_phase_kind = 'running',
                current_attempt_id = $2
          WHERE turn_id = $3
            AND state_kind = 'queued'",
    )
    .bind(first_frontier)
    .bind(first_attempt)
    .bind(first_turn)
    .execute(&mut *activation)
    .await?;
    activation.commit().await?;

    let active_shape: (String, String, String, String, i64) = sqlx::query_as(
        "SELECT turn.state_kind, turn.start_lineage_kind,
                turn.active_phase_kind, attempt.state_kind,
                frontier.member_count::bigint
           FROM turn_lifecycle AS turn
           JOIN turn_attempt AS attempt
             ON attempt.turn_attempt_id = turn.current_attempt_id
           JOIN context_frontier AS frontier
             ON frontier.owning_session_id = turn.session_id
            AND frontier.context_frontier_id = turn.starting_frontier_id
          WHERE turn.turn_id = $1",
    )
    .bind(first_turn)
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        active_shape,
        (
            "active".into(),
            "first_in_session".into(),
            "running".into(),
            "prepared".into(),
            1
        )
    );

    let born_active = sqlx::query(
        "INSERT INTO turn_lifecycle
            (turn_id, session_id, origin_accepted_input_id, acceptance_position,
             state_kind, start_lineage_kind, immediate_predecessor_turn_id,
             starting_frontier_id, terminal_frontier_id, active_phase_kind,
             current_attempt_id, terminal_disposition_kind)
         SELECT turn_id, session_id, origin_accepted_input_id, acceptance_position,
                state_kind, start_lineage_kind, immediate_predecessor_turn_id,
                starting_frontier_id, terminal_frontier_id, active_phase_kind,
                current_attempt_id, terminal_disposition_kind
           FROM turn_lifecycle
          WHERE turn_id = $1",
    )
    .bind(first_turn)
    .execute(&pool)
    .await
    .expect_err("even a complete active shape must first be inserted as queued");
    assert_eq!(
        born_active
            .as_database_error()
            .and_then(|error| error.code()),
        Some("23514".into())
    );
    assert_eq!(
        born_active
            .as_database_error()
            .and_then(|error| error.constraint()),
        Some("turn_lifecycle_inserted_queued")
    );

    for (attempt_id, state_kind, end_variant, end_disposition) in [
        (Uuid::from_u128(0xb05), "running", None, None),
        (
            Uuid::from_u128(0xb06),
            "ended",
            Some("without_stop"),
            Some("known_failure"),
        ),
    ] {
        let born_nonprepared = sqlx::query(
            "INSERT INTO turn_attempt
                (turn_attempt_id, turn_id, session_id, continued_from_attempt_id,
                 state_kind, end_variant, end_disposition)
             VALUES ($1, $2, $3, NULL, $4, $5, $6)",
        )
        .bind(attempt_id)
        .bind(Uuid::from_u128(0xa02))
        .bind(session)
        .bind(state_kind)
        .bind(end_variant)
        .bind(end_disposition)
        .execute(&pool)
        .await
        .expect_err("every attempt must first be inserted as prepared");
        assert_eq!(
            born_nonprepared
                .as_database_error()
                .and_then(|error| error.code()),
            Some("23514".into())
        );
        assert_eq!(
            born_nonprepared
                .as_database_error()
                .and_then(|error| error.constraint()),
            Some("turn_attempt_inserted_prepared"),
            "unexpected insert guard for born-{state_kind} attempt"
        );
    }

    let mut second_activation = pool.begin().await?;
    insert_origin_frontier(
        &mut second_activation,
        session,
        Uuid::from_u128(0x902),
        Uuid::from_u128(0xd02),
        Uuid::from_u128(0xe02),
        Decimal::ONE,
    )
    .await?;
    sqlx::query(
        "INSERT INTO turn_attempt
            (turn_attempt_id, turn_id, session_id, continued_from_attempt_id,
             state_kind, end_variant, end_disposition)
         VALUES ($1, $2, $3, NULL, 'prepared', NULL, NULL)",
    )
    .bind(Uuid::from_u128(0xb02))
    .bind(Uuid::from_u128(0xa02))
    .bind(session)
    .execute(&mut *second_activation)
    .await?;
    let second_active = sqlx::query(
        "UPDATE turn_lifecycle
            SET state_kind = 'active',
                start_lineage_kind = 'after',
                immediate_predecessor_turn_id = $1,
                starting_frontier_id = $2,
                active_phase_kind = 'running',
                current_attempt_id = $3
          WHERE turn_id = $4",
    )
    .bind(first_turn)
    .bind(Uuid::from_u128(0xe02))
    .bind(Uuid::from_u128(0xb02))
    .bind(Uuid::from_u128(0xa02))
    .execute(&mut *second_activation)
    .await
    .expect_err("the partial unique index must reject a second active turn");
    assert_eq!(
        second_active
            .as_database_error()
            .and_then(|error| error.constraint()),
        Some("turn_lifecycle_one_active_per_session")
    );
    second_activation.rollback().await?;

    let mut duplicate_live = pool.begin().await?;
    sqlx::query(
        "INSERT INTO turn_attempt
            (turn_attempt_id, turn_id, session_id, continued_from_attempt_id,
             state_kind, end_variant, end_disposition)
         VALUES ($1, $2, $3, NULL, 'prepared', NULL, NULL)",
    )
    .bind(Uuid::from_u128(0xb03))
    .bind(Uuid::from_u128(0xa02))
    .bind(session)
    .execute(&mut *duplicate_live)
    .await?;
    let second_live = sqlx::query(
        "INSERT INTO turn_attempt
            (turn_attempt_id, turn_id, session_id, continued_from_attempt_id,
             state_kind, end_variant, end_disposition)
         VALUES ($1, $2, $3, $4, 'prepared', NULL, NULL)",
    )
    .bind(Uuid::from_u128(0xb04))
    .bind(Uuid::from_u128(0xa02))
    .bind(session)
    .bind(Uuid::from_u128(0xb03))
    .execute(&mut *duplicate_live)
    .await
    .expect_err("the partial unique index must reject a second live attempt");
    assert_eq!(
        second_live
            .as_database_error()
            .and_then(|error| error.constraint()),
        Some("turn_attempt_one_live_per_turn")
    );
    duplicate_live.rollback().await?;

    let immutable_start = sqlx::query(
        "UPDATE turn_lifecycle
            SET starting_frontier_id = $1
          WHERE turn_id = $2",
    )
    .bind(Uuid::from_u128(0xeff))
    .bind(first_turn)
    .execute(&pool)
    .await
    .expect_err("a committed turn start must be write-once");
    assert_eq!(
        immutable_start
            .as_database_error()
            .and_then(|error| error.code()),
        Some("23514".into())
    );

    let immutable_member = sqlx::query(
        "UPDATE context_frontier_member
            SET member_position = 2
          WHERE owning_session_id = $1
            AND context_frontier_id = $2",
    )
    .bind(session)
    .bind(first_frontier)
    .execute(&pool)
    .await
    .expect_err("committed frontier membership must be immutable");
    assert_eq!(
        immutable_member
            .as_database_error()
            .and_then(|error| error.code()),
        Some("23514".into())
    );

    let duplicate_member = sqlx::query(
        "INSERT INTO context_frontier_member
            (owning_session_id, context_frontier_id, member_position,
             source_session_id, semantic_entry_id)
         VALUES ($1, $2, 2, $1, $3)",
    )
    .bind(session)
    .bind(first_frontier)
    .bind(first_entry)
    .execute(&pool)
    .await
    .expect_err("one exact source-qualified entry cannot occur twice");
    assert_eq!(
        duplicate_member
            .as_database_error()
            .and_then(|error| error.constraint()),
        Some("context_frontier_member_entry_once")
    );

    let mut unavailable_continuation = pool.begin().await?;
    sqlx::query(
        "UPDATE turn_attempt
            SET state_kind = 'ended',
                end_variant = 'without_stop',
                end_disposition = 'known_failure'
          WHERE turn_attempt_id = $1",
    )
    .bind(first_attempt)
    .execute(&mut *unavailable_continuation)
    .await?;
    let successor_attempt = Uuid::from_u128(0xb02);
    sqlx::query(
        "INSERT INTO turn_attempt
            (turn_attempt_id, turn_id, session_id, continued_from_attempt_id,
             state_kind, end_variant, end_disposition)
         VALUES ($1, $2, $3, $4, 'prepared', NULL, NULL)",
    )
    .bind(successor_attempt)
    .bind(first_turn)
    .bind(session)
    .bind(first_attempt)
    .execute(&mut *unavailable_continuation)
    .await?;
    sqlx::query(
        "UPDATE turn_lifecycle
            SET current_attempt_id = $1
          WHERE turn_id = $2",
    )
    .bind(successor_attempt)
    .bind(first_turn)
    .execute(&mut *unavailable_continuation)
    .await?;
    let unavailable_continuation_error = unavailable_continuation
        .commit()
        .await
        .expect_err("even an ended predecessor cannot admit continuation yet");
    assert_eq!(
        unavailable_continuation_error
            .as_database_error()
            .and_then(|error| error.constraint()),
        Some("turn_attempt_continuation_unavailable")
    );

    let failure_entry = Uuid::from_u128(0xd03);
    let terminal_frontier = Uuid::from_u128(0xe03);
    for contradictory_disposition in [
        "turn_completed",
        "turn_refused",
        "yielded_to_durable_wait",
        "ambiguous",
    ] {
        let mut contradictory_terminal = pool.begin().await?;
        sqlx::query(
            "INSERT INTO semantic_transcript_entry
                (source_session_id, semantic_entry_id, payload_kind,
                 origin_accepted_input_id, failed_turn_id)
             VALUES ($1, $2, 'turn_failed', NULL, $3)",
        )
        .bind(session)
        .bind(failure_entry)
        .bind(first_turn)
        .execute(&mut *contradictory_terminal)
        .await?;
        insert_frontier(
            &mut contradictory_terminal,
            session,
            terminal_frontier,
            Decimal::from(2_u64),
            &[
                (Decimal::ONE, session, first_entry),
                (Decimal::from(2_u64), session, failure_entry),
            ],
        )
        .await?;
        sqlx::query(
            "UPDATE turn_attempt
                SET state_kind = 'ended',
                    end_variant = 'without_stop',
                    end_disposition = $1
              WHERE turn_attempt_id = $2",
        )
        .bind(contradictory_disposition)
        .bind(first_attempt)
        .execute(&mut *contradictory_terminal)
        .await?;
        sqlx::query(
            "UPDATE turn_lifecycle
                SET state_kind = 'terminal',
                    active_phase_kind = NULL,
                    current_attempt_id = NULL,
                    terminal_frontier_id = $1,
                    terminal_disposition_kind = 'failed'
              WHERE turn_id = $2",
        )
        .bind(terminal_frontier)
        .bind(first_turn)
        .execute(&mut *contradictory_terminal)
        .await?;

        let contradictory_terminal_error = contradictory_terminal
            .commit()
            .await
            .expect_err("a failed turn cannot retain a contradictory ended attempt");
        let database_error = contradictory_terminal_error
            .as_database_error()
            .expect("deferred lifecycle validation must return a database error");
        assert_eq!(database_error.code(), Some("23514".into()));
        assert!(
            database_error
                .message()
                .contains("permits only known_failure or lost ended attempts"),
            "unexpected terminal consistency error for {contradictory_disposition}: {}",
            database_error.message()
        );
    }

    let mut terminalize = pool.begin().await?;
    sqlx::query(
        "INSERT INTO semantic_transcript_entry
            (source_session_id, semantic_entry_id, payload_kind,
             origin_accepted_input_id, failed_turn_id)
         VALUES ($1, $2, 'turn_failed', NULL, $3)",
    )
    .bind(session)
    .bind(failure_entry)
    .bind(first_turn)
    .execute(&mut *terminalize)
    .await?;
    insert_frontier(
        &mut terminalize,
        session,
        terminal_frontier,
        Decimal::from(2_u64),
        &[
            (Decimal::ONE, session, first_entry),
            (Decimal::from(2_u64), session, failure_entry),
        ],
    )
    .await?;
    sqlx::query(
        "UPDATE turn_attempt
            SET state_kind = 'ended',
                end_variant = 'without_stop',
                end_disposition = 'known_failure'
          WHERE turn_attempt_id = $1",
    )
    .bind(first_attempt)
    .execute(&mut *terminalize)
    .await?;
    sqlx::query(
        "UPDATE turn_lifecycle
            SET state_kind = 'terminal',
                active_phase_kind = NULL,
                current_attempt_id = NULL,
                terminal_frontier_id = $1,
                terminal_disposition_kind = 'failed'
          WHERE turn_id = $2",
    )
    .bind(terminal_frontier)
    .bind(first_turn)
    .execute(&mut *terminalize)
    .await?;
    terminalize.commit().await?;

    let immutable_attempt = sqlx::query(
        "UPDATE turn_attempt
            SET end_disposition = 'lost'
          WHERE turn_attempt_id = $1",
    )
    .bind(first_attempt)
    .execute(&pool)
    .await
    .expect_err("an ended attempt must be immutable");
    assert_eq!(
        immutable_attempt
            .as_database_error()
            .and_then(|error| error.code()),
        Some("23514".into())
    );

    let born_terminal = sqlx::query(
        "INSERT INTO turn_lifecycle
            (turn_id, session_id, origin_accepted_input_id, acceptance_position,
             state_kind, start_lineage_kind, immediate_predecessor_turn_id,
             starting_frontier_id, terminal_frontier_id, active_phase_kind,
             current_attempt_id, terminal_disposition_kind)
         SELECT turn_id, session_id, origin_accepted_input_id, acceptance_position,
                state_kind, start_lineage_kind, immediate_predecessor_turn_id,
                starting_frontier_id, terminal_frontier_id, active_phase_kind,
                current_attempt_id, terminal_disposition_kind
           FROM turn_lifecycle
          WHERE turn_id = $1",
    )
    .bind(first_turn)
    .execute(&pool)
    .await
    .expect_err("even a complete terminal shape must first be inserted as queued");
    assert_eq!(
        born_terminal
            .as_database_error()
            .and_then(|error| error.code()),
        Some("23514".into())
    );
    assert_eq!(
        born_terminal
            .as_database_error()
            .and_then(|error| error.constraint()),
        Some("turn_lifecycle_inserted_queued")
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// INV-007 / INV-009 / INV-015: an incomplete frontier cannot expose any
/// semantic entry, start binding, slot owner, or attempt after rollback.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv007_inv009_inv015_malformed_atomic_start_rolls_back_every_fact()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone())
        .handle(prepared(0x411, 0x811, direct(0xc11)))
        .await?;
    SubmitInputRepository::new(pool.clone())
        .handle(
            start_input(
                0x412,
                0x811,
                "malformed future start",
                1,
                ModelSelectionOverride::UseSessionDefault,
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(0x911)),
            Some(TurnId::from_uuid(Uuid::from_u128(0xa11))),
        )
        .await?;

    let session = Uuid::from_u128(0x811);
    let turn = Uuid::from_u128(0xa11);
    let mut malformed = pool.begin().await?;
    insert_origin_frontier(
        &mut malformed,
        session,
        Uuid::from_u128(0x911),
        Uuid::from_u128(0xd11),
        Uuid::from_u128(0xe11),
        Decimal::from(2_u64),
    )
    .await?;
    sqlx::query(
        "INSERT INTO turn_attempt
            (turn_attempt_id, turn_id, session_id, continued_from_attempt_id,
             state_kind, end_variant, end_disposition)
         VALUES ($1, $2, $3, NULL, 'prepared', NULL, NULL)",
    )
    .bind(Uuid::from_u128(0xb11))
    .bind(turn)
    .bind(session)
    .execute(&mut *malformed)
    .await?;
    sqlx::query(
        "UPDATE turn_lifecycle
            SET state_kind = 'active',
                start_lineage_kind = 'first_in_session',
                starting_frontier_id = $1,
                active_phase_kind = 'running',
                current_attempt_id = $2
          WHERE turn_id = $3",
    )
    .bind(Uuid::from_u128(0xe11))
    .bind(Uuid::from_u128(0xb11))
    .bind(turn)
    .execute(&mut *malformed)
    .await?;
    let incomplete = malformed
        .commit()
        .await
        .expect_err("a gapped one-member frontier must not commit");
    assert_eq!(
        incomplete
            .as_database_error()
            .and_then(|error| error.code()),
        Some("23514".into())
    );

    let unchanged: (String, i64, i64, i64) = sqlx::query_as(
        "SELECT
            state_kind,
            (SELECT count(*) FROM semantic_transcript_entry),
            (SELECT count(*) FROM context_frontier),
            (SELECT count(*) FROM turn_attempt)
         FROM turn_lifecycle
         WHERE turn_id = $1",
    )
    .bind(turn)
    .fetch_one(&pool)
    .await?;
    assert_eq!(unchanged, ("queued".to_owned(), 0, 0, 0));

    pool.close().await;
    drop(container);
    Ok(())
}

/// INV-001 / INV-005 / INV-009 / INV-015: the initial semantic variants
/// preserve globally unique identities and exact source correlations; eligible
/// failure records origin then failure without putting the later failure
/// marker in the starting frontier.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv005_inv009_inv015_initial_semantic_entries_are_turn_correlated()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone())
        .handle(prepared(0x421, 0x821, direct(0xc21)))
        .await?;
    let submit = SubmitInputRepository::new(pool.clone());
    submit
        .handle(
            start_input(
                0x422,
                0x821,
                "will fail eligibility",
                1,
                ModelSelectionOverride::UseSessionDefault,
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(0x921)),
            Some(TurnId::from_uuid(Uuid::from_u128(0xa21))),
        )
        .await?;

    let session = Uuid::from_u128(0x821);
    let turn = Uuid::from_u128(0xa21);
    let origin_entry = Uuid::from_u128(0xd21);
    let failure_entry = Uuid::from_u128(0xd22);
    let starting_frontier = Uuid::from_u128(0xe21);
    let terminal_frontier = Uuid::from_u128(0xe22);

    let mut missing_terminal_frontier = pool.begin().await?;
    insert_origin_frontier(
        &mut missing_terminal_frontier,
        session,
        Uuid::from_u128(0x921),
        origin_entry,
        starting_frontier,
        Decimal::ONE,
    )
    .await?;
    sqlx::query(
        "INSERT INTO semantic_transcript_entry
            (source_session_id, semantic_entry_id, payload_kind,
             origin_accepted_input_id, failed_turn_id)
         VALUES ($1, $2, 'turn_failed', NULL, $3)",
    )
    .bind(session)
    .bind(failure_entry)
    .bind(turn)
    .execute(&mut *missing_terminal_frontier)
    .await?;
    let missing_terminal_frontier_error = sqlx::query(
        "UPDATE turn_lifecycle
            SET state_kind = 'terminal',
                start_lineage_kind = 'first_in_session',
                starting_frontier_id = $1,
                terminal_disposition_kind = 'failed'
          WHERE turn_id = $2",
    )
    .bind(starting_frontier)
    .bind(turn)
    .execute(&mut *missing_terminal_frontier)
    .await
    .expect_err("a failed terminal turn must name its terminal frontier");
    assert_eq!(
        missing_terminal_frontier_error
            .as_database_error()
            .and_then(|error| error.constraint()),
        Some("turn_lifecycle_state_payload_shape")
    );
    missing_terminal_frontier.rollback().await?;

    let mut gapped_terminal_frontier = pool.begin().await?;
    insert_origin_frontier(
        &mut gapped_terminal_frontier,
        session,
        Uuid::from_u128(0x921),
        origin_entry,
        starting_frontier,
        Decimal::ONE,
    )
    .await?;
    sqlx::query(
        "INSERT INTO semantic_transcript_entry
            (source_session_id, semantic_entry_id, payload_kind,
             origin_accepted_input_id, failed_turn_id)
         VALUES ($1, $2, 'turn_failed', NULL, $3)",
    )
    .bind(session)
    .bind(failure_entry)
    .bind(turn)
    .execute(&mut *gapped_terminal_frontier)
    .await?;
    insert_frontier(
        &mut gapped_terminal_frontier,
        session,
        terminal_frontier,
        Decimal::from(2_u64),
        &[
            (Decimal::ONE, session, origin_entry),
            (Decimal::from(3_u64), session, failure_entry),
        ],
    )
    .await?;
    sqlx::query(
        "UPDATE turn_lifecycle
            SET state_kind = 'terminal',
                start_lineage_kind = 'first_in_session',
                starting_frontier_id = $1,
                terminal_frontier_id = $2,
                terminal_disposition_kind = 'failed'
          WHERE turn_id = $3",
    )
    .bind(starting_frontier)
    .bind(terminal_frontier)
    .bind(turn)
    .execute(&mut *gapped_terminal_frontier)
    .await?;
    let gapped = gapped_terminal_frontier
        .commit()
        .await
        .expect_err("a terminal frontier with a membership gap must not commit");
    assert_eq!(
        gapped.as_database_error().and_then(|error| error.code()),
        Some("23514".into())
    );

    let mut cross_wired_terminal_frontier = pool.begin().await?;
    insert_origin_frontier(
        &mut cross_wired_terminal_frontier,
        session,
        Uuid::from_u128(0x921),
        origin_entry,
        starting_frontier,
        Decimal::ONE,
    )
    .await?;
    sqlx::query(
        "INSERT INTO semantic_transcript_entry
            (source_session_id, semantic_entry_id, payload_kind,
             origin_accepted_input_id, failed_turn_id)
         VALUES ($1, $2, 'turn_failed', NULL, $3)",
    )
    .bind(session)
    .bind(failure_entry)
    .bind(turn)
    .execute(&mut *cross_wired_terminal_frontier)
    .await?;
    insert_frontier(
        &mut cross_wired_terminal_frontier,
        session,
        terminal_frontier,
        Decimal::from(2_u64),
        &[
            (Decimal::ONE, session, failure_entry),
            (Decimal::from(2_u64), session, origin_entry),
        ],
    )
    .await?;
    sqlx::query(
        "UPDATE turn_lifecycle
            SET state_kind = 'terminal',
                start_lineage_kind = 'first_in_session',
                starting_frontier_id = $1,
                terminal_frontier_id = $2,
                terminal_disposition_kind = 'failed'
          WHERE turn_id = $3",
    )
    .bind(starting_frontier)
    .bind(terminal_frontier)
    .bind(turn)
    .execute(&mut *cross_wired_terminal_frontier)
    .await?;
    let cross_wired = cross_wired_terminal_frontier
        .commit()
        .await
        .expect_err("a reordered terminal frontier must not commit");
    assert_eq!(
        cross_wired
            .as_database_error()
            .and_then(|error| error.code()),
        Some("23514".into())
    );

    let mut failure = pool.begin().await?;
    insert_origin_frontier(
        &mut failure,
        session,
        Uuid::from_u128(0x921),
        origin_entry,
        starting_frontier,
        Decimal::ONE,
    )
    .await?;
    sqlx::query(
        "INSERT INTO semantic_transcript_entry
            (source_session_id, semantic_entry_id, payload_kind,
             origin_accepted_input_id, failed_turn_id)
         VALUES ($1, $2, 'turn_failed', NULL, $3)",
    )
    .bind(session)
    .bind(failure_entry)
    .bind(turn)
    .execute(&mut *failure)
    .await?;
    insert_frontier(
        &mut failure,
        session,
        terminal_frontier,
        Decimal::from(2_u64),
        &[
            (Decimal::ONE, session, origin_entry),
            (Decimal::from(2_u64), session, failure_entry),
        ],
    )
    .await?;
    sqlx::query(
        "UPDATE turn_lifecycle
            SET state_kind = 'terminal',
                start_lineage_kind = 'first_in_session',
                starting_frontier_id = $1,
                terminal_frontier_id = $2,
                terminal_disposition_kind = 'failed'
          WHERE turn_id = $3",
    )
    .bind(starting_frontier)
    .bind(terminal_frontier)
    .bind(turn)
    .execute(&mut *failure)
    .await?;
    failure.commit().await?;

    let semantic_shape: (String, i64, i64, i64, i64) = sqlx::query_as(
        "SELECT
            turn.state_kind,
            (SELECT count(*)
               FROM semantic_transcript_entry
              WHERE source_session_id = $1),
            starting.member_count::bigint,
            terminal.member_count::bigint,
            (SELECT count(*)
               FROM context_frontier_member AS member
               JOIN semantic_transcript_entry AS entry
                 ON entry.source_session_id = member.source_session_id
                AND entry.semantic_entry_id = member.semantic_entry_id
              WHERE member.owning_session_id = $1
                AND member.context_frontier_id = $2
                AND entry.payload_kind = 'turn_failed')
         FROM turn_lifecycle AS turn
         JOIN context_frontier AS starting
           ON starting.owning_session_id = turn.session_id
          AND starting.context_frontier_id = turn.starting_frontier_id
         JOIN context_frontier AS terminal
           ON terminal.owning_session_id = turn.session_id
          AND terminal.context_frontier_id = turn.terminal_frontier_id
         WHERE turn.turn_id = $3",
    )
    .bind(session)
    .bind(starting_frontier)
    .bind(turn)
    .fetch_one(&pool)
    .await?;
    assert_eq!(semantic_shape, ("terminal".to_owned(), 2, 1, 2, 0));

    CreateSessionRepository::new(pool.clone())
        .handle(prepared(0x424, 0x822, direct(0xc24)))
        .await?;
    submit
        .handle(
            start_input(
                0x425,
                0x822,
                "cross-session identity probe",
                1,
                ModelSelectionOverride::UseSessionDefault,
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(0x924)),
            Some(TurnId::from_uuid(Uuid::from_u128(0xa24))),
        )
        .await?;
    let semantic_id_reuse = sqlx::query(
        "INSERT INTO semantic_transcript_entry
            (source_session_id, semantic_entry_id, payload_kind,
             origin_accepted_input_id, failed_turn_id)
         VALUES ($1, $2, 'origin_accepted_input', $3, NULL)",
    )
    .bind(Uuid::from_u128(0x822))
    .bind(origin_entry)
    .bind(Uuid::from_u128(0x924))
    .execute(&pool)
    .await
    .expect_err("a semantic entry identifier cannot be reused by another session");
    assert_eq!(
        semantic_id_reuse
            .as_database_error()
            .and_then(|error| error.constraint()),
        Some("semantic_transcript_entry_id_global")
    );

    let frontier_id_reuse = sqlx::query(
        "INSERT INTO context_frontier
            (owning_session_id, context_frontier_id, member_count)
         VALUES ($1, $2, 0)",
    )
    .bind(Uuid::from_u128(0x822))
    .bind(starting_frontier)
    .execute(&pool)
    .await
    .expect_err("a context frontier identifier cannot be reused by another session");
    assert_eq!(
        frontier_id_reuse
            .as_database_error()
            .and_then(|error| error.constraint()),
        Some("context_frontier_id_global")
    );

    submit
        .handle(
            start_input(
                0x423,
                0x821,
                "still queued",
                1,
                ModelSelectionOverride::UseSessionDefault,
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(0x922)),
            Some(TurnId::from_uuid(Uuid::from_u128(0xa22))),
        )
        .await?;

    let second_turn = Uuid::from_u128(0xa22);
    let second_origin = Uuid::from_u128(0xd23);
    let second_starting_frontier = Uuid::from_u128(0xe23);
    let second_attempt = Uuid::from_u128(0xb23);
    let mut omitted_predecessor_frontier = pool.begin().await?;
    insert_origin_frontier(
        &mut omitted_predecessor_frontier,
        session,
        Uuid::from_u128(0x922),
        second_origin,
        second_starting_frontier,
        Decimal::ONE,
    )
    .await?;
    sqlx::query(
        "INSERT INTO turn_attempt
            (turn_attempt_id, turn_id, session_id, continued_from_attempt_id,
             state_kind, end_variant, end_disposition)
         VALUES ($1, $2, $3, NULL, 'prepared', NULL, NULL)",
    )
    .bind(second_attempt)
    .bind(second_turn)
    .bind(session)
    .execute(&mut *omitted_predecessor_frontier)
    .await?;
    sqlx::query(
        "UPDATE turn_lifecycle
            SET state_kind = 'active',
                start_lineage_kind = 'after',
                immediate_predecessor_turn_id = $1,
                starting_frontier_id = $2,
                active_phase_kind = 'running',
                current_attempt_id = $3
          WHERE turn_id = $4",
    )
    .bind(turn)
    .bind(second_starting_frontier)
    .bind(second_attempt)
    .bind(second_turn)
    .execute(&mut *omitted_predecessor_frontier)
    .await?;
    let omitted = omitted_predecessor_frontier
        .commit()
        .await
        .expect_err("a successor start cannot omit its predecessor terminal frontier");
    assert_eq!(
        omitted.as_database_error().and_then(|error| error.code()),
        Some("23514".into())
    );

    let mut reordered_predecessor_frontier = pool.begin().await?;
    sqlx::query(
        "INSERT INTO semantic_transcript_entry
            (source_session_id, semantic_entry_id, payload_kind,
             origin_accepted_input_id, failed_turn_id)
         VALUES ($1, $2, 'origin_accepted_input', $3, NULL)",
    )
    .bind(session)
    .bind(second_origin)
    .bind(Uuid::from_u128(0x922))
    .execute(&mut *reordered_predecessor_frontier)
    .await?;
    insert_frontier(
        &mut reordered_predecessor_frontier,
        session,
        second_starting_frontier,
        Decimal::from(3_u64),
        &[
            (Decimal::ONE, session, failure_entry),
            (Decimal::from(2_u64), session, origin_entry),
            (Decimal::from(3_u64), session, second_origin),
        ],
    )
    .await?;
    sqlx::query(
        "INSERT INTO turn_attempt
            (turn_attempt_id, turn_id, session_id, continued_from_attempt_id,
             state_kind, end_variant, end_disposition)
         VALUES ($1, $2, $3, NULL, 'prepared', NULL, NULL)",
    )
    .bind(second_attempt)
    .bind(second_turn)
    .bind(session)
    .execute(&mut *reordered_predecessor_frontier)
    .await?;
    sqlx::query(
        "UPDATE turn_lifecycle
            SET state_kind = 'active',
                start_lineage_kind = 'after',
                immediate_predecessor_turn_id = $1,
                starting_frontier_id = $2,
                active_phase_kind = 'running',
                current_attempt_id = $3
          WHERE turn_id = $4",
    )
    .bind(turn)
    .bind(second_starting_frontier)
    .bind(second_attempt)
    .bind(second_turn)
    .execute(&mut *reordered_predecessor_frontier)
    .await?;
    let reordered = reordered_predecessor_frontier
        .commit()
        .await
        .expect_err("a successor start cannot reorder predecessor membership");
    assert_eq!(
        reordered.as_database_error().and_then(|error| error.code()),
        Some("23514".into())
    );

    let mut invalid_failure = pool.begin().await?;
    sqlx::query(
        "INSERT INTO semantic_transcript_entry
            (source_session_id, semantic_entry_id, payload_kind,
             origin_accepted_input_id, failed_turn_id)
         VALUES ($1, $2, 'turn_failed', NULL, $3)",
    )
    .bind(session)
    .bind(Uuid::from_u128(0xd23))
    .bind(second_turn)
    .execute(&mut *invalid_failure)
    .await?;
    let queued_failure = invalid_failure
        .commit()
        .await
        .expect_err("a queued turn cannot acquire a failure entry");
    assert_eq!(
        queued_failure
            .as_database_error()
            .and_then(|error| error.code()),
        Some("23514".into())
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// S01 / INV-008 / INV-012: all baseline authoritative rejections are typed
/// terminal records. Active-work delivery modes reject `NoActiveTurn`, stale
/// defaults and unresolved aliases retain their exact evidence, and missing
/// sessions create no aggregate or queued-work effects.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s01_inv008_inv012_submit_records_authoritative_rejections() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let create = CreateSessionRepository::new(pool.clone());
    create.handle(prepared(0x311, 0x711, direct(0x811))).await?;
    create.handle(prepared(0x312, 0x712, alias(0x812))).await?;
    let repository = SubmitInputRepository::new(pool.clone());

    let missing = start_input(
        0x313,
        0x7ff,
        "missing",
        1,
        ModelSelectionOverride::UseSessionDefault,
    );
    let missing_recorded = repository
        .handle(
            missing.clone(),
            AcceptedInputId::from_uuid(Uuid::from_u128(0x913)),
            Some(TurnId::from_uuid(Uuid::from_u128(0xa13))),
        )
        .await?;
    assert!(matches!(
        missing_recorded,
        SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Rejected(
            SubmitInputRejectedResult::SessionNotFound { .. }
        ))
    ));
    create.handle(prepared(0x31a, 0x7ff, direct(0x81a))).await?;
    assert_eq!(
        repository
            .handle(
                missing,
                AcceptedInputId::from_uuid(Uuid::from_u128(0x91a)),
                Some(TurnId::from_uuid(Uuid::from_u128(0xa1a))),
            )
            .await?,
        missing_recorded
    );

    let expected_turn = TurnId::from_uuid(Uuid::from_u128(0xb11));
    let active_modes = [
        DeliveryRequest::Interrupt {
            expected_active_turn: expected_turn,
            configuration: input_choices(1, ModelSelectionOverride::UseSessionDefault),
        },
        DeliveryRequest::NextSafePoint {
            expected_active_turn: expected_turn,
        },
        DeliveryRequest::AfterCurrentTurn {
            expected_active_turn: expected_turn,
            configuration: input_choices(1, ModelSelectionOverride::UseSessionDefault),
        },
    ];
    for (offset, delivery) in active_modes.into_iter().enumerate() {
        let turn = match delivery {
            DeliveryRequest::NextSafePoint { .. } => None,
            DeliveryRequest::Interrupt { .. } | DeliveryRequest::AfterCurrentTurn { .. } => {
                Some(TurnId::from_uuid(Uuid::from_u128(0xa14 + offset as u128)))
            }
            DeliveryRequest::StartWhenNoActiveTurn { .. } => {
                unreachable!("the table contains only active-work delivery modes")
            }
        };
        let command = input_with_delivery(0x314 + offset as u128, 0x711, "active", delivery);
        assert!(matches!(
            repository
                .handle(
                    command,
                    AcceptedInputId::from_uuid(Uuid::from_u128(0x914 + offset as u128)),
                    turn,
                )
                .await?,
            SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Rejected(
                SubmitInputRejectedResult::NoActiveTurn {
                    expected_active_turn: recorded,
                    ..
                }
            )) if recorded == expected_turn
        ));
    }

    let stale = start_input(
        0x318,
        0x711,
        "stale",
        2,
        ModelSelectionOverride::UseSessionDefault,
    );
    let stale_recorded = repository
        .handle(
            stale.clone(),
            AcceptedInputId::from_uuid(Uuid::from_u128(0x918)),
            Some(TurnId::from_uuid(Uuid::from_u128(0xa18))),
        )
        .await?;
    assert!(matches!(
        stale_recorded,
        SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Rejected(
            SubmitInputRejectedResult::SessionDefaultsVersionMismatch {
                expected,
                current,
                ..
            }
        )) if expected.as_u64() == 2 && current.as_u64() == 1
    ));
    ReplaceSessionDefaultsRepository::new(pool.clone())
        .handle(replacement(0x31b, 0x711, 1, direct(0x81b)))
        .await?;
    assert_eq!(
        repository
            .handle(
                stale,
                AcceptedInputId::from_uuid(Uuid::from_u128(0x91b)),
                Some(TurnId::from_uuid(Uuid::from_u128(0xa1b))),
            )
            .await?,
        stale_recorded
    );

    let unknown = start_input(
        0x319,
        0x712,
        "alias",
        1,
        ModelSelectionOverride::UseSessionDefault,
    );
    assert!(matches!(
        repository
            .handle(
                unknown,
                AcceptedInputId::from_uuid(Uuid::from_u128(0x919)),
                Some(TurnId::from_uuid(Uuid::from_u128(0xa19))),
            )
            .await?,
        SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Rejected(
            SubmitInputRejectedResult::UnknownModelAlias { alias, .. }
        )) if alias == ModelAlias::from_uuid(Uuid::from_u128(0x812))
    ));

    let explicit_unknown = start_input(
        0x31c,
        0x711,
        "explicit alias",
        2,
        ModelSelectionOverride::ReplaceWith(alias(0x81c)),
    );
    assert!(matches!(
        repository
            .handle(
                explicit_unknown,
                AcceptedInputId::from_uuid(Uuid::from_u128(0x91c)),
                Some(TurnId::from_uuid(Uuid::from_u128(0xa1c))),
            )
            .await?,
        SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Rejected(
            SubmitInputRejectedResult::UnknownModelAlias { alias, .. }
        )) if alias == ModelAlias::from_uuid(Uuid::from_u128(0x81c))
    ));

    let counts: (i64, i64, i64) = sqlx::query_as(
        "SELECT
            (SELECT count(*) FROM submit_input_command),
            (SELECT count(*) FROM accepted_input),
            (SELECT count(*) FROM queued_input_origin)",
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(counts, (7, 0, 0));

    pool.close().await;
    drop(container);
    Ok(())
}

/// INV-007 / INV-008 / INV-012: the locked session row serializes concurrent
/// assignments into one gap-free position order, and a post-claim database
/// failure explicitly rolls back the claim and does not consume a position.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv007_inv008_inv012_submit_serializes_positions_and_rolls_back_failures()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone())
        .handle(prepared(0x321, 0x721, direct(0x821)))
        .await?;
    let repository = SubmitInputRepository::new(pool.clone());
    let mut tasks = Vec::new();
    for offset in 0..6_u128 {
        let repository = repository.clone();
        tasks.push(tokio::spawn(async move {
            repository
                .handle(
                    start_input(
                        0x322 + offset,
                        0x721,
                        &format!("concurrent {offset}"),
                        1,
                        ModelSelectionOverride::UseSessionDefault,
                    ),
                    AcceptedInputId::from_uuid(Uuid::from_u128(0x922 + offset)),
                    Some(TurnId::from_uuid(Uuid::from_u128(0xa22 + offset))),
                )
                .await
        }));
    }
    let mut positions = Vec::new();
    for task in tasks {
        let SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Applied(applied)) =
            task.await??
        else {
            panic!("each distinct concurrent command must apply");
        };
        positions.push(applied.acceptance_position().as_u64());
    }
    positions.sort_unstable();
    assert_eq!(positions, vec![1, 2, 3, 4, 5, 6]);

    let colliding = start_input(
        0x328,
        0x721,
        "collision",
        1,
        ModelSelectionOverride::UseSessionDefault,
    );
    let error = repository
        .handle(
            colliding.clone(),
            AcceptedInputId::from_uuid(Uuid::from_u128(0x922)),
            Some(TurnId::from_uuid(Uuid::from_u128(0xa28))),
        )
        .await
        .expect_err("an accepted-input identity collision must abort the transaction");
    assert!(matches!(error, SubmitInputRepositoryError::Database(_)));
    assert!(repository.load(colliding.command_id()).await?.is_none());

    let SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Applied(retried)) = repository
        .handle(
            colliding,
            AcceptedInputId::from_uuid(Uuid::from_u128(0x928)),
            Some(TurnId::from_uuid(Uuid::from_u128(0xa28))),
        )
        .await?
    else {
        panic!("retry after rollback must apply");
    };
    assert_eq!(retried.acceptance_position().as_u64(), 7);

    let equal = start_input(
        0x329,
        0x721,
        "equal concurrent replay",
        1,
        ModelSelectionOverride::UseSessionDefault,
    );
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let (left, right) = tokio::join!(
        {
            let repository = repository.clone();
            let command = equal.clone();
            let barrier = barrier.clone();
            async move {
                barrier.wait().await;
                repository
                    .handle(
                        command,
                        AcceptedInputId::from_uuid(Uuid::from_u128(0x929)),
                        Some(TurnId::from_uuid(Uuid::from_u128(0xa29))),
                    )
                    .await
            }
        },
        {
            let repository = repository.clone();
            let command = equal.clone();
            let barrier = barrier.clone();
            async move {
                barrier.wait().await;
                repository
                    .handle(
                        command,
                        AcceptedInputId::from_uuid(Uuid::from_u128(0x92a)),
                        Some(TurnId::from_uuid(Uuid::from_u128(0xa2a))),
                    )
                    .await
            }
        }
    );
    let left = left?;
    let right = right?;
    assert_eq!(left, right);
    let SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Applied(equal_applied)) = left
    else {
        panic!("equal concurrent first handling must converge on an application");
    };
    assert_eq!(equal_applied.acceptance_position().as_u64(), 8);
    let equal_counts: (i64, i64, i64, i64) = sqlx::query_as(
        "SELECT
            (SELECT count(*) FROM submit_input_command WHERE command_id = $1),
            (SELECT count(*) FROM accepted_input WHERE accepting_command_id = $1),
            (SELECT count(*)
               FROM queued_input_origin AS queued
               JOIN accepted_input AS accepted
                 ON accepted.accepted_input_id = queued.accepted_input_id
              WHERE accepted.accepting_command_id = $1),
            (SELECT count(*)
               FROM turn_lifecycle AS turn
               JOIN accepted_input AS accepted
                 ON accepted.origin_turn_id = turn.turn_id
              WHERE accepted.accepting_command_id = $1)",
    )
    .bind(Uuid::from_u128(0x329))
    .fetch_one(&pool)
    .await?;
    assert_eq!(equal_counts, (1, 1, 1, 1));

    pool.close().await;
    drop(container);
    Ok(())
}

/// INV-007 / INV-008 / INV-012: a defaults replacement holds the pointer-row
/// lock when its version-row insert requests `FOR KEY SHARE` on the session
/// row through the non-deferrable session foreign key, while submit orders
/// the session row before the pointer row. The forced interleaving completes
/// with typed outcomes because submit's session-row lock is
/// `FOR NO KEY UPDATE`; `FOR UPDATE` deadlocks here (Postgres 40P01).
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv007_inv008_inv012_submit_and_defaults_replacement_interleave_without_deadlock()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone())
        .handle(prepared(0x341, 0x751, direct(0x851)))
        .await?;

    // Replacement side, first half: hold the pointer-row lock exactly as the
    // defaults-replacement compare-and-set does before its version insert.
    // The pointer's version foreign key is deferred, so the successor row may
    // follow the pointer change inside the same transaction.
    let mut replacement_side = pool.begin().await?;
    let cas = sqlx::query(
        "UPDATE session_current_defaults
         SET current_version = 2
         WHERE session_id = $1
           AND current_version = 1",
    )
    .bind(Uuid::from_u128(0x751))
    .execute(&mut *replacement_side)
    .await?;
    assert_eq!(cas.rows_affected(), 1);

    // Submit side: locks the session row, then blocks on the held pointer.
    let submit = tokio::spawn({
        let repository = SubmitInputRepository::new(pool.clone());
        async move {
            repository
                .handle(
                    start_input(
                        0x342,
                        0x751,
                        "interleaved",
                        1,
                        ModelSelectionOverride::UseSessionDefault,
                    ),
                    AcceptedInputId::from_uuid(Uuid::from_u128(0x942)),
                    Some(TurnId::from_uuid(Uuid::from_u128(0xa42))),
                )
                .await
        }
    });

    // Force the interleaving: proceed only once the submit transaction holds
    // its session-row lock and waits on the pointer row.
    let mut submit_blocked_on_pointer = false;
    for _ in 0..400 {
        let waiting: i64 = sqlx::query_scalar(
            "SELECT count(*)
             FROM pg_stat_activity
             WHERE wait_event_type = 'Lock'
               AND query LIKE '%FROM session_current_defaults%'",
        )
        .fetch_one(&pool)
        .await?;
        if waiting > 0 {
            submit_blocked_on_pointer = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    assert!(
        submit_blocked_on_pointer,
        "the submit transaction must block on the held pointer row"
    );

    // Replacement side, second half: the insert's session foreign key takes
    // `FOR KEY SHARE` on the session row the submit transaction has locked.
    sqlx::query(
        "INSERT INTO session_defaults_version
            (session_id, version, model_selection_kind,
             direct_model_selection_id, model_alias_id)
         VALUES ($1, 2, 'direct', $2, NULL)",
    )
    .bind(Uuid::from_u128(0x751))
    .bind(Uuid::from_u128(0x852))
    .execute(&mut *replacement_side)
    .await?;
    replacement_side.commit().await?;

    // The unblocked submit records the advanced pointer as a typed stale
    // rejection rather than failing on infrastructure.
    assert!(matches!(
        submit.await??,
        SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Rejected(
            SubmitInputRejectedResult::SessionDefaultsVersionMismatch {
                expected,
                current,
                ..
            }
        )) if expected.as_u64() == 1 && current.as_u64() == 2
    ));

    pool.close().await;
    drop(container);
    Ok(())
}

/// INV-002 / INV-008 / INV-012: checked loads reject cross-wired immutable
/// effects even when database protections are deliberately disabled, and the
/// maximum stored position produces a durable exhaustion rejection.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv002_inv008_inv012_submit_corruption_and_position_exhaustion_fail_closed()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone())
        .handle(prepared(0x331, 0x731, direct(0x831)))
        .await?;
    let repository = SubmitInputRepository::new(pool.clone());
    let first = start_input(
        0x332,
        0x731,
        "uncorrupted",
        1,
        ModelSelectionOverride::UseSessionDefault,
    );
    repository
        .handle(
            first.clone(),
            AcceptedInputId::from_uuid(Uuid::from_u128(0x932)),
            Some(TurnId::from_uuid(Uuid::from_u128(0xa32))),
        )
        .await?;

    sqlx::query(
        "ALTER TABLE submit_input_command
            DISABLE TRIGGER submit_input_command_is_append_only",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE submit_input_command
            SET actor_kind = 'recovery'
          WHERE command_id = $1",
    )
    .bind(Uuid::from_u128(0x332))
    .execute(&pool)
    .await?;
    let non_owner = repository
        .load(first.command_id())
        .await
        .expect_err("domain reconstitution rejects a stored non-owner actor");
    assert!(matches!(
        non_owner,
        SubmitInputRepositoryError::Corruption(SubmitInputCorruption::Domain(
            SubmitInputReconstitutionFailure::StoredActorMismatch
        ))
    ));
    sqlx::query(
        "UPDATE submit_input_command
            SET actor_kind = 'owner'
          WHERE command_id = $1",
    )
    .bind(Uuid::from_u128(0x332))
    .execute(&pool)
    .await?;

    sqlx::query(
        "ALTER TABLE accepted_input
            DISABLE TRIGGER accepted_input_is_append_only",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE queued_input_origin
            DISABLE TRIGGER queued_input_origin_is_append_only",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE queued_input_origin
            DROP CONSTRAINT queued_input_origin_accepted_input_fk",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE accepted_input
            DROP CONSTRAINT accepted_input_queued_origin_fk",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE turn_lifecycle
            DROP CONSTRAINT turn_lifecycle_queued_origin_fk",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE queued_input_origin
            DROP CONSTRAINT queued_input_origin_turn_lifecycle_fk",
    )
    .execute(&pool)
    .await?;
    let mut transaction = pool.begin().await?;
    sqlx::query(
        "UPDATE accepted_input
            SET acceptance_position = 18446744073709551615
          WHERE accepting_command_id = $1",
    )
    .bind(Uuid::from_u128(0x332))
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "UPDATE queued_input_origin
            SET acceptance_position = 18446744073709551615
          WHERE accepted_input_id = $1",
    )
    .bind(Uuid::from_u128(0x932))
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await?;

    let exhausted = start_input(
        0x333,
        0x731,
        "exhausted",
        1,
        ModelSelectionOverride::UseSessionDefault,
    );
    assert!(matches!(
        repository
            .handle(
                exhausted,
                AcceptedInputId::from_uuid(Uuid::from_u128(0x933)),
                Some(TurnId::from_uuid(Uuid::from_u128(0xa33))),
            )
            .await?,
        SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Rejected(
            SubmitInputRejectedResult::AcceptancePositionExhausted { last, .. }
        )) if last.as_u64() == u64::MAX
    ));

    sqlx::query(
        "UPDATE accepted_input
            SET content_text = 'cross-wired'
          WHERE accepting_command_id = $1",
    )
    .bind(Uuid::from_u128(0x332))
    .execute(&pool)
    .await?;
    let corrupt = repository
        .load(first.command_id())
        .await
        .expect_err("domain correlation rejects altered accepted content");
    assert!(matches!(
        corrupt,
        SubmitInputRepositoryError::Corruption(SubmitInputCorruption::Domain(
            SubmitInputReconstitutionFailure::AcceptedContentMismatch
        ))
    ));

    pool.close().await;
    drop(container);
    Ok(())
}
