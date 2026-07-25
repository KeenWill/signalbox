//! PostgreSQL migration support and connection-option helpers.
//!
//! This crate owns persistence-specific types. SQLx types do not cross into the
//! domain crate.

mod command_registry;
mod conversation_import_codec;
mod lock_inventory;

pub mod conversation_import;
pub mod create_session;
pub mod create_session_from_imported_frontier;
pub mod hub_fence;
pub mod mapping;
pub mod model_execution;
pub mod outbox;
pub mod process_read;
pub mod replace_session_defaults;
pub mod scheduler;
pub mod session;
pub mod session_metadata;
pub mod start_eligible_turn;
pub mod startup;
pub mod submit_input;
pub mod tool_loop;

use std::str::FromStr;

use sqlx::{
    Error, PgPool,
    migrate::{MigrateError, Migrator},
    postgres::{PgConnectOptions, PgPoolOptions, PgSslMode},
};

/// The reviewed, forward-only migration set embedded in this crate.
pub static MIGRATOR: Migrator = sqlx::migrate!("./migrations");

/// Applies all pending embedded migrations to `pool`.
pub async fn migrate(pool: &PgPool) -> Result<(), MigrateError> {
    MIGRATOR.run(pool).await
}

fn commit_failure_is_ambiguous(error: &sqlx::Error) -> bool {
    match error {
        sqlx::Error::Database(database) => {
            matches!(database.code().as_deref(), Some("08007" | "40003"))
        }
        _ => true,
    }
}

/// Opens the shared production pool with certificate and hostname checks.
///
/// Pool sizing remains at SQLx's baseline until an operational slice selects
/// measured limits; callers receive a cheap-clone handle for composition.
pub async fn connect_production(database_url: &str) -> Result<PgPool, Error> {
    PgPoolOptions::new()
        .connect_with(production_connection_options(database_url)?)
        .await
}

/// Environment variables SQLx consults while building connection options,
/// mirroring the libpq `PG*` surface, in alphabetical order: fallback defaults
/// for anything the URL omits — including the `PGPASSWORD` credential and the
/// `PGPASSFILE` password-file override — plus `PGAPPNAME` and `PGOPTIONS`,
/// which shape the connection even when the URL is complete.
const AMBIENT_POSTGRES_VARIABLES: [&str; 13] = [
    "PGAPPNAME",
    "PGDATABASE",
    "PGHOST",
    "PGHOSTADDR",
    "PGOPTIONS",
    "PGPASSFILE",
    "PGPASSWORD",
    "PGPORT",
    "PGSSLCERT",
    "PGSSLKEY",
    "PGSSLMODE",
    "PGSSLROOTCERT",
    "PGUSER",
];

/// Parses production connection options with certificate and hostname checks.
///
/// The database URL is the only supported configuration channel for the
/// production connection: when any ambient libpq-style `PG*` variable is
/// present in the process environment (even with an empty value), parsing
/// fails closed instead of letting the environment silently seed connection
/// defaults or credentials. The error names the offending variables, never
/// their values.
pub fn production_connection_options(database_url: &str) -> Result<PgConnectOptions, Error> {
    production_connection_options_with_environment(database_url, |name| {
        std::env::var_os(name).is_some()
    })
}

/// Parses production options against an explicit variable-presence lookup.
fn production_connection_options_with_environment(
    database_url: &str,
    variable_is_present: impl Fn(&'static str) -> bool,
) -> Result<PgConnectOptions, Error> {
    let ambient: Vec<&'static str> = AMBIENT_POSTGRES_VARIABLES
        .into_iter()
        .filter(|&name| variable_is_present(name))
        .collect();
    if !ambient.is_empty() {
        return Err(Error::Configuration(
            format!(
                "ambient PostgreSQL variables would shape the production connection: {}; \
                 unset them and carry every connection parameter in the database URL",
                ambient.join(", ")
            )
            .into(),
        ));
    }
    PgConnectOptions::from_str(database_url).map(|options| options.ssl_mode(PgSslMode::VerifyFull))
}

/// Parses ephemeral local-test options with TLS explicitly disabled.
pub fn local_test_connection_options(database_url: &str) -> Result<PgConnectOptions, Error> {
    PgConnectOptions::from_str(database_url).map(|options| options.ssl_mode(PgSslMode::Disable))
}

#[cfg(test)]
mod tests {
    use std::io;

    use expect_test::expect;
    use sqlx::postgres::PgSslMode;

    use super::{
        commit_failure_is_ambiguous, local_test_connection_options,
        production_connection_options_with_environment,
    };

    const DATABASE_URL: &str = "postgres://signalbox:secret@database.example/signalbox";

    /// An environment carrying none of the ambient `PG*` variables.
    fn no_ambient_variables(_: &'static str) -> bool {
        false
    }

    #[test]
    fn production_options_require_full_tls_verification() {
        let options =
            production_connection_options_with_environment(DATABASE_URL, no_ambient_variables)
                .expect("valid database URL without ambient variables");

        assert!(matches!(options.get_ssl_mode(), PgSslMode::VerifyFull));
    }

    #[test]
    fn production_options_reject_an_ambient_credential_variable() {
        let error = production_connection_options_with_environment(DATABASE_URL, |name| {
            name == "PGPASSWORD"
        })
        .expect_err("an ambient credential channel must fail closed");

        expect!["error with configuration: ambient PostgreSQL variables would shape the production connection: PGPASSWORD; unset them and carry every connection parameter in the database URL"].assert_eq(&error.to_string());
    }

    #[test]
    fn production_options_name_every_consulted_ambient_variable() {
        let error = production_connection_options_with_environment(DATABASE_URL, |_| true)
            .expect_err("a fully ambient environment must fail closed");

        expect!["error with configuration: ambient PostgreSQL variables would shape the production connection: PGAPPNAME, PGDATABASE, PGHOST, PGHOSTADDR, PGOPTIONS, PGPASSFILE, PGPASSWORD, PGPORT, PGSSLCERT, PGSSLKEY, PGSSLMODE, PGSSLROOTCERT, PGUSER; unset them and carry every connection parameter in the database URL"].assert_eq(&error.to_string());
    }

    #[test]
    fn local_test_options_disable_tls_explicitly() {
        let options = local_test_connection_options(DATABASE_URL).expect("valid database URL");

        assert!(matches!(options.get_ssl_mode(), PgSslMode::Disable));
    }

    #[test]
    fn lost_commit_response_is_ambiguous() {
        let error = sqlx::Error::Io(io::Error::new(
            io::ErrorKind::ConnectionReset,
            "commit response was lost",
        ));

        assert!(commit_failure_is_ambiguous(&error));
    }
}
