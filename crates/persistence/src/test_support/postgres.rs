//! Isolated databases cloned from a migrated template in a suite-owned server.

use std::{error::Error, process::Command};

use sqlx::{Connection, PgConnection, PgPool, postgres::PgPoolOptions};
use testcontainers_modules::{
    postgres::Postgres,
    testcontainers::{ContainerAsync, ImageExt, runners::AsyncRunner},
};

#[path = "../../../../tooling/postgres_test_image.rs"]
mod postgres_test_image;
pub(crate) use postgres_test_image::POSTGRES_IMAGE_TAG;

const CONTAINER_HELPER: &str = include_str!("../../../../tooling/postgres_test_container.py");

/// Owns one test database. Dropping it disconnects its clients and removes it,
/// unless `TESTCONTAINERS_COMMAND=keep` preserves it for inspection.
#[derive(Debug)]
pub struct TestDatabase {
    admin_url: String,
    name: String,
    _container: Option<ContainerAsync<Postgres>>,
}

impl Drop for TestDatabase {
    fn drop(&mut self) {
        if std::env::var(crate::TESTCONTAINERS_COMMAND_VARIABLE).as_deref()
            == Ok(crate::TESTCONTAINERS_KEEP_COMMAND)
        {
            return;
        }
        let admin_url = self.admin_url.clone();
        let name = self.name.clone();
        // A fixture can drop inside a Tokio runtime, or after that runtime exits.
        let cleanup = std::thread::spawn(move || -> Result<(), Box<dyn Error + Send + Sync>> {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?
                .block_on(async {
                    let mut connection = PgConnection::connect_with(
                        &crate::local_test_connection_options(&admin_url)?,
                    )
                    .await?;
                    sqlx::query(sqlx::AssertSqlSafe(format!(
                        "DROP DATABASE \"{name}\" WITH (FORCE)"
                    )))
                    .execute(&mut connection)
                    .await?;
                    connection.close().await?;
                    Ok(())
                })
        });
        match cleanup.join() {
            Ok(Ok(())) => {}
            result => eprintln!("test database cleanup failed: {result:?}"),
        }
    }
}

/// Clones the migration-set template and opens an isolated pool of the requested size.
pub async fn migrated_postgres(
    max_connections: u32,
) -> Result<(TestDatabase, PgPool, String), Box<dyn Error>> {
    let (admin_url, container) =
        if cfg!(target_os = "linux") && std::env::var_os("NEXTEST_RUN_ID").is_some() {
            (tokio::task::spawn_blocking(shared_server).await??, None)
        } else {
            let (url, container) = dedicated_server().await?;
            (url, Some(container))
        };
    clone_database(admin_url, max_connections, container).await
}

async fn dedicated_server() -> Result<(String, ContainerAsync<Postgres>), Box<dyn Error>> {
    let container = Postgres::default()
        .with_user("signalbox")
        .with_password("signalbox-test-only")
        .with_db_name("postgres")
        .with_cmd(crate::disposable_postgres_server_args())
        .with_mount(crate::disposable_postgres_state_tmpfs_from_example()?)
        .with_tag(POSTGRES_IMAGE_TAG)
        .with_labels(crate::disposable_test_container_labels())
        .start()
        .await?;
    let host = container.get_host().await?;
    let port = container.get_host_port_ipv4(5432).await?;
    Ok((
        format!("postgres://signalbox:signalbox-test-only@{host}:{port}/postgres"),
        container,
    ))
}

async fn clone_database(
    admin_url: String,
    max_connections: u32,
    container: Option<ContainerAsync<Postgres>>,
) -> Result<(TestDatabase, PgPool, String), Box<dyn Error>> {
    let mut admin =
        PgConnection::connect_with(&crate::local_test_connection_options(&admin_url)?).await?;
    let mut migration_bytes = Vec::new();
    for migration in crate::MIGRATOR.iter() {
        migration_bytes.extend_from_slice(&migration.version.to_be_bytes());
        migration_bytes.extend_from_slice(&migration.checksum);
    }
    let digest = signalbox_domain::BlobDigest::digest(&migration_bytes);
    let template = format!(
        "sbx_t_{}",
        digest.as_bytes()[..28]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    );
    let lock = i64::from_be_bytes(digest.as_bytes()[..8].try_into()?);
    sqlx::query("SELECT pg_advisory_lock($1)")
        .bind(lock)
        .execute(&mut admin)
        .await?;
    let ready: Option<bool> =
        sqlx::query_scalar("SELECT NOT datallowconn FROM pg_database WHERE datname = $1")
            .bind(&template)
            .fetch_optional(&mut admin)
            .await?;
    if ready != Some(true) {
        if ready == Some(false) {
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "DROP DATABASE \"{template}\" WITH (FORCE)"
            )))
            .execute(&mut admin)
            .await?;
        }
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "CREATE DATABASE \"{template}\" TEMPLATE template0"
        )))
        .execute(&mut admin)
        .await?;
        let options = crate::local_test_connection_options(&admin_url)?.database(&template);
        let pool = PgPoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await?;
        let migration = crate::migrate(&pool).await;
        pool.close().await;
        if let Err(error) = migration {
            sqlx::query(sqlx::AssertSqlSafe(format!("DROP DATABASE \"{template}\"")))
                .execute(&mut admin)
                .await?;
            return Err(error.into());
        }
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "ALTER DATABASE \"{template}\" ALLOW_CONNECTIONS false"
        )))
        .execute(&mut admin)
        .await?;
    }
    sqlx::query("SELECT pg_advisory_unlock($1)")
        .bind(lock)
        .execute(&mut admin)
        .await?;
    let name = format!("sbx_{}", uuid::Uuid::now_v7().simple());
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "CREATE DATABASE \"{name}\" TEMPLATE \"{template}\""
    )))
    .execute(&mut admin)
    .await?;
    admin.close().await?;
    let mut database_url = url::Url::parse(&admin_url)?;
    database_url.set_path(&name);
    let database_url = database_url.to_string();
    let database = TestDatabase {
        admin_url,
        name,
        _container: container,
    };
    let pool = PgPoolOptions::new()
        .max_connections(max_connections)
        .connect_with(crate::local_test_connection_options(&database_url)?)
        .await?;
    Ok((database, pool, database_url))
}

fn shared_server() -> std::io::Result<String> {
    let executable = std::env::current_exe()?;
    let target = executable
        .parent()
        .ok_or_else(|| std::io::Error::other("test executable has no directory"))?;
    let output = Command::new("python3")
        .args(["-c", CONTAINER_HELPER])
        .arg(target.join("postgres-fixtures"))
        .arg(POSTGRES_IMAGE_TAG)
        .arg(include_str!("../../../../config/signalboxd.example.toml"))
        .output()?;
    if !output.status.success() {
        return Err(std::io::Error::other(
            String::from_utf8_lossy(&output.stderr).into_owned(),
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[ignore = "requires ephemeral PostgreSQL"]
    async fn keep_mode_preserves_the_cloned_database_after_guard_drop() -> Result<(), Box<dyn Error>>
    {
        const ADMIN_URL_VARIABLE: &str = "SIGNALBOX_TEST_KEEP_DATABASE_URL";
        const CHILD_TEST: &str = "test_support::postgres::tests::keep_mode_preserves_the_cloned_database_after_guard_drop";
        const CHILD_EVIDENCE: &str = "kept database remains queryable";
        if let Ok(admin_url) = std::env::var(ADMIN_URL_VARIABLE) {
            let (database, pool, _) = clone_database(admin_url, 1, None).await?;
            sqlx::raw_sql(
                "CREATE TABLE fixture_keep_probe (value integer); INSERT INTO fixture_keep_probe VALUES (11)",
            )
            .execute(&pool)
            .await?;
            drop(database);
            let retained: i32 = sqlx::query_scalar("SELECT value FROM fixture_keep_probe")
                .fetch_one(&pool)
                .await?;
            assert_eq!(retained, 11);
            pool.close().await;
            println!("{CHILD_EVIDENCE}");
            return Ok(());
        }

        // The parent owns the server; only the child requests database retention.
        let (admin_url, _container) = dedicated_server().await?;
        let executable = std::env::current_exe()?;
        let output = tokio::task::spawn_blocking(move || {
            Command::new(executable)
                .args(["--ignored", "--exact", CHILD_TEST, "--nocapture"])
                .env(ADMIN_URL_VARIABLE, admin_url)
                .env(
                    crate::TESTCONTAINERS_COMMAND_VARIABLE,
                    crate::TESTCONTAINERS_KEEP_COMMAND,
                )
                .output()
        })
        .await??;
        assert!(
            output.status.success(),
            "keep-mode child failed: {output:?}"
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains(CHILD_EVIDENCE));
        Ok(())
    }

    #[tokio::test]
    #[ignore = "requires ephemeral PostgreSQL"]
    async fn concurrent_template_clones_isolate_writes_and_drop_their_databases()
    -> Result<(), Box<dyn Error>> {
        let (admin_url, _container) = dedicated_server().await?;
        let (left, right) = tokio::join!(
            clone_database(admin_url.clone(), 2, None),
            clone_database(admin_url, 2, None)
        );
        let (left_database, left_pool, _) = left?;
        let (right_database, right_pool, _) = right?;
        assert_eq!(left_database.admin_url, right_database.admin_url);
        assert_ne!(left_database.name, right_database.name);
        sqlx::raw_sql(
            "CREATE TABLE fixture_probe (value integer); INSERT INTO fixture_probe VALUES (11)",
        )
        .execute(&left_pool)
        .await?;
        sqlx::raw_sql(
            "CREATE TABLE fixture_probe (value integer); INSERT INTO fixture_probe VALUES (22)",
        )
        .execute(&right_pool)
        .await?;
        let left_value: i32 = sqlx::query_scalar("SELECT value FROM fixture_probe")
            .fetch_one(&left_pool)
            .await?;
        let right_value: i32 = sqlx::query_scalar("SELECT value FROM fixture_probe")
            .fetch_one(&right_pool)
            .await?;
        assert_eq!(left_value, 11);
        assert_eq!(right_value, 22);
        let mut admin = PgConnection::connect_with(&crate::local_test_connection_options(
            &left_database.admin_url,
        )?)
        .await?;
        let names = vec![left_database.name.clone(), right_database.name.clone()];
        left_pool.close().await;
        right_pool.close().await;
        drop(left_database);
        drop(right_database);
        let remaining: i64 =
            sqlx::query_scalar("SELECT count(*) FROM pg_database WHERE datname = ANY($1)")
                .bind(names)
                .fetch_one(&mut admin)
                .await?;
        assert_eq!(remaining, 0);
        admin.close().await?;
        Ok(())
    }
}
