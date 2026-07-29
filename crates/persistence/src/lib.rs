//! PostgreSQL migration support and connection-option helpers.
//!
//! This crate owns persistence-specific types. SQLx types do not cross into the
//! domain crate.

mod command_registry;
mod conversation_import_codec;
mod lock_inventory;

pub mod context_compaction;
pub mod conversation_import;
pub mod conversation_listing;
pub mod create_session;
pub mod create_session_from_imported_frontier;
pub mod hub_fence;
pub mod mapping;
pub mod model_execution;
pub mod outbox;
pub mod process_read;
pub mod replace_session_defaults;
pub mod review_workflow;
mod review_workflow_command;
pub mod runner_protocol;
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
use url::Url;

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

/// Environment variables that decide which roots verify the production
/// server's certificate, in alphabetical order. This crate selects SQLx's
/// `tls-rustls-ring-native-roots` feature, so SQLx seeds its root store from
/// `rustls-native-certs`, which loads roots only from the file and directories
/// these variables name whenever either is set, in place of the platform store.
/// SQLx then adds an `sslrootcert` the URL states to that store rather than
/// replacing it, so a root named by the environment stays trusted even under an
/// explicit root certificate: stating the root in the URL cannot neutralize
/// these variables, only their absence can.
const AMBIENT_TLS_TRUST_VARIABLES: [&str; 2] = ["SSL_CERT_DIR", "SSL_CERT_FILE"];

/// Reports whether the password file SQLx falls back to when `PGPASSFILE` is
/// unset exists: `~/.pgpass` under the process home directory, mirroring
/// libpq's default. SQLx consults it whenever the parsed URL carries no
/// password, so its presence is a second credential channel exactly like
/// `PGPASSFILE`. Presence alone decides; the file is never opened.
fn default_passfile_is_present() -> bool {
    std::env::home_dir().is_some_and(|home| home.join(".pgpass").exists())
}

/// Names the connection parameters SQLx would take from outside the URL
/// because this URL omits them: the user name from the process account
/// (`whoami`), and the host from a probe of the local PostgreSQL socket
/// directories that falls back to `localhost`. Each may be stated in the URL's
/// authority or in the query parameter SQLx reads for it — `user` for the user
/// name, `host` or `hostaddr` for the host. Port and database name are left to
/// SQLx: an omitted port is the fixed 5432, and an omitted database name lets
/// the server apply the user name the URL states, so neither reaches outside
/// the URL once the ambient variables are refused.
fn parameters_taken_from_outside_the_url(url: &Url) -> Vec<&'static str> {
    let mut host_is_stated = url.host_str().is_some_and(|host| !host.is_empty());
    let mut user_is_stated = !url.username().is_empty();
    for (parameter, value) in url.query_pairs() {
        if value.is_empty() {
            continue;
        }
        match &*parameter {
            "host" | "hostaddr" => host_is_stated = true,
            "user" => user_is_stated = true,
            _ => {}
        }
    }

    let mut taken = Vec::new();
    if !host_is_stated {
        taken.push("host");
    }
    if !user_is_stated {
        taken.push("user");
    }
    taken
}

/// Parses production connection options with certificate and hostname checks.
///
/// The database URL is the only supported configuration channel for the
/// production connection: when any ambient libpq-style `PG*` variable or
/// certificate-store variable is present in the process environment (even with
/// an empty value), when the default `~/.pgpass` password file exists, or when
/// the URL omits a parameter SQLx would then take from the process account or
/// the host filesystem, parsing fails closed instead of letting the environment
/// silently seed connection defaults, credentials, or trust anchors. The error
/// names the offending channel, never its contents.
pub fn production_connection_options(database_url: &str) -> Result<PgConnectOptions, Error> {
    production_connection_options_with_environment(
        database_url,
        |name| std::env::var_os(name).is_some(),
        default_passfile_is_present,
    )
}

/// Parses production options against explicit ambient-channel lookups.
fn production_connection_options_with_environment(
    database_url: &str,
    variable_is_present: impl Fn(&'static str) -> bool,
    passfile_is_present: impl Fn() -> bool,
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
    let trust: Vec<&'static str> = AMBIENT_TLS_TRUST_VARIABLES
        .into_iter()
        .filter(|&name| variable_is_present(name))
        .collect();
    if !trust.is_empty() {
        return Err(Error::Configuration(
            format!(
                "ambient certificate variables would choose the roots that verify the production \
                 server: {}; unset them and leave the platform trust store to the host, which an \
                 `sslrootcert` in the database URL adds to rather than replaces",
                trust.join(", ")
            )
            .into(),
        ));
    }
    if passfile_is_present() {
        return Err(Error::Configuration(
            "the default PostgreSQL password file would supply the production credential: \
             `~/.pgpass` is present; remove it and carry every connection parameter in the \
             database URL"
                .into(),
        ));
    }
    let url = Url::parse(database_url).map_err(Error::config)?;
    let taken = parameters_taken_from_outside_the_url(&url);
    if !taken.is_empty() {
        return Err(Error::Configuration(
            format!(
                "the process account and host filesystem would supply production connection \
                 parameters the database URL omits: {}; state every connection parameter in the \
                 database URL",
                taken.join(", ")
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

    /// An environment carrying no default `~/.pgpass` password file.
    fn no_default_passfile() -> bool {
        false
    }

    #[test]
    fn production_options_require_full_tls_verification() {
        let options = production_connection_options_with_environment(
            DATABASE_URL,
            no_ambient_variables,
            no_default_passfile,
        )
        .expect("valid database URL without ambient channels");

        assert!(matches!(options.get_ssl_mode(), PgSslMode::VerifyFull));
    }

    #[test]
    fn production_options_reject_an_ambient_credential_variable() {
        let error = production_connection_options_with_environment(
            DATABASE_URL,
            |name| name == "PGPASSWORD",
            no_default_passfile,
        )
        .expect_err("an ambient credential channel must fail closed");

        expect!["error with configuration: ambient PostgreSQL variables would shape the production connection: PGPASSWORD; unset them and carry every connection parameter in the database URL"].assert_eq(&error.to_string());
    }

    #[test]
    fn production_options_name_every_consulted_ambient_variable() {
        let error = production_connection_options_with_environment(
            DATABASE_URL,
            |_| true,
            no_default_passfile,
        )
        .expect_err("a fully ambient environment must fail closed");

        expect!["error with configuration: ambient PostgreSQL variables would shape the production connection: PGAPPNAME, PGDATABASE, PGHOST, PGHOSTADDR, PGOPTIONS, PGPASSFILE, PGPASSWORD, PGPORT, PGSSLCERT, PGSSLKEY, PGSSLMODE, PGSSLROOTCERT, PGUSER; unset them and carry every connection parameter in the database URL"].assert_eq(&error.to_string());
    }

    #[test]
    fn production_options_reject_an_ambient_trust_store_variable() {
        let error = production_connection_options_with_environment(
            DATABASE_URL,
            |name| name == "SSL_CERT_FILE",
            no_default_passfile,
        )
        .expect_err("an ambient trust-anchor channel must fail closed");

        expect!["error with configuration: ambient certificate variables would choose the roots that verify the production server: SSL_CERT_FILE; unset them and leave the platform trust store to the host, which an `sslrootcert` in the database URL adds to rather than replaces"].assert_eq(&error.to_string());
    }

    #[test]
    fn production_options_name_every_consulted_trust_store_variable() {
        let error = production_connection_options_with_environment(
            DATABASE_URL,
            |name| name.starts_with("SSL_CERT_"),
            no_default_passfile,
        )
        .expect_err("a fully ambient trust store must fail closed");

        expect!["error with configuration: ambient certificate variables would choose the roots that verify the production server: SSL_CERT_DIR, SSL_CERT_FILE; unset them and leave the platform trust store to the host, which an `sslrootcert` in the database URL adds to rather than replaces"].assert_eq(&error.to_string());
    }

    #[test]
    fn production_options_reject_the_default_password_file() {
        let error = production_connection_options_with_environment(
            DATABASE_URL,
            no_ambient_variables,
            || true,
        )
        .expect_err("the default passfile is a second credential channel and must fail closed");

        expect!["error with configuration: the default PostgreSQL password file would supply the production credential: `~/.pgpass` is present; remove it and carry every connection parameter in the database URL"].assert_eq(&error.to_string());
    }

    #[test]
    fn production_options_reject_a_url_the_process_account_would_complete() {
        let error = production_connection_options_with_environment(
            "postgres:///signalbox",
            no_ambient_variables,
            no_default_passfile,
        )
        .expect_err("a URL SQLx would complete from outside must fail closed");

        expect!["error with configuration: the process account and host filesystem would supply production connection parameters the database URL omits: host, user; state every connection parameter in the database URL"].assert_eq(&error.to_string());
    }

    #[test]
    fn production_options_reject_a_url_that_states_only_the_host() {
        let error = production_connection_options_with_environment(
            "postgres://database.example/signalbox",
            no_ambient_variables,
            no_default_passfile,
        )
        .expect_err("a URL without a user name must fail closed");

        expect!["error with configuration: the process account and host filesystem would supply production connection parameters the database URL omits: user; state every connection parameter in the database URL"].assert_eq(&error.to_string());
    }

    #[test]
    fn production_options_accept_parameters_stated_in_the_query() {
        let options = production_connection_options_with_environment(
            "postgres:///signalbox?host=database.example&user=signalbox",
            no_ambient_variables,
            no_default_passfile,
        )
        .expect("SQLx reads these query parameters, so the URL states both");

        assert_eq!(options.get_host(), "database.example");
        assert_eq!(options.get_username(), "signalbox");
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
