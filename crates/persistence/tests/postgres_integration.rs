use std::{collections::VecDeque, error::Error, sync::Arc};

use signalbox_application::{
    CreateSessionError, CreateSessionOutcome, CreateSessionRequest, CreateSessionService,
    LoadSessionService, ReplaceSessionDefaultsOutcome, ReplaceSessionDefaultsRequest,
    ReplaceSessionDefaultsService, SessionIdGenerator,
};
use signalbox_domain::{
    CreateSession, DurableCommandId, ModelAlias, ModelSelectionRequest, PreparedCreateSession,
    ReplaceSessionDefaults, ReplaceSessionDefaultsRejectedResult, ReplaceSessionDefaultsResult,
    SessionConfigurationDefaults, SessionConfigurationDefaultsVersion, SessionCreationCause,
    SessionCreationProvenance, SessionId, TranscriptAncestry,
};
use signalbox_persistence::{
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
};
use sqlx::{PgPool, postgres::PgPoolOptions, types::Uuid};
use testcontainers_modules::{
    postgres::Postgres,
    testcontainers::{ContainerAsync, ImageExt, runners::AsyncRunner},
};

const POSTGRES_IMAGE_TAG: &str = "18.4-alpine3.23";
const DATABASE_NAME: &str = "signalbox_integration";
const DATABASE_USER: &str = "signalbox";
const DATABASE_PASSWORD: &str = "signalbox-test-only";

async fn migrated_postgres() -> Result<(ContainerAsync<Postgres>, PgPool, String), Box<dyn Error>> {
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

    Ok((container, pool, database_url))
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
    let committed_counts: (i64, i64) = sqlx::query_as(
        "SELECT
            (SELECT count(*) FROM durable_command),
            (SELECT count(*) FROM session)",
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(committed_counts, (1, 1));

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
             'submit_input', 1, TIMESTAMPTZ '2026-07-18 00:00:00+00')",
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
             'replace_session_defaults', 99, transaction_timestamp())",
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
/// malformed, unknown, or undecodable current projection facts fail closed as
/// typed corruption rather than becoming `None` or nearby valid defaults.
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
    for creation in [
        missing_pointer,
        invalid_pointer,
        missing_selected,
        malformed_selected,
        unknown_provenance,
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

    pool.close().await;
    drop(container);
    Ok(())
}
