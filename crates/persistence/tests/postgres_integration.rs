use std::error::Error;

use signalbox_persistence::{local_test_connection_options, migrate};
use sqlx::{PgPool, postgres::PgPoolOptions};
use testcontainers_modules::{
    postgres::Postgres,
    testcontainers::{ContainerAsync, ImageExt, runners::AsyncRunner},
};

const POSTGRES_IMAGE_TAG: &str = "18.4-alpine3.23";
const DATABASE_NAME: &str = "signalbox_integration";
const DATABASE_USER: &str = "signalbox";
const DATABASE_PASSWORD: &str = "signalbox-test-only";

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
        .max_connections(1)
        .connect_with(local_test_connection_options(&database_url)?)
        .await?;

    migrate(&pool).await?;

    Ok((container, pool))
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn embedded_migrator_connects_and_is_idempotent() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    migrate(&pool).await?;
    let connected: i32 = sqlx::query_scalar("SELECT 1").fetch_one(&pool).await?;
    assert_eq!(connected, 1);

    pool.close().await;
    drop(container);

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s01_inv003_inv008_inv012_create_session_schema_preserves_typed_facts()
-> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
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
    let (container, pool) = migrated_postgres().await?;

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
    let (container, pool) = migrated_postgres().await?;

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
