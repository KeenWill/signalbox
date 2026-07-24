//! PostgreSQL migration support and connection-option helpers.
//!
//! This crate owns persistence-specific types. SQLx types do not cross into the
//! domain crate.

mod command_registry;
#[allow(
    dead_code,
    reason = "the immediately stacked imported-conversation repository consumes this checked storage codec"
)]
mod conversation_import_codec;
mod lock_inventory;
mod outbox;

pub mod create_session;
pub mod mapping;
pub mod model_execution;
pub mod replace_session_defaults;
pub mod scheduler;
pub mod session;
pub mod start_eligible_turn;
pub mod startup;
pub mod submit_input;

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

/// Opens the shared production pool with certificate and hostname checks.
///
/// Pool sizing remains at SQLx's baseline until an operational slice selects
/// measured limits; callers receive a cheap-clone handle for composition.
pub async fn connect_production(database_url: &str) -> Result<PgPool, Error> {
    PgPoolOptions::new()
        .connect_with(production_connection_options(database_url)?)
        .await
}

/// Parses production connection options with certificate and hostname checks.
pub fn production_connection_options(database_url: &str) -> Result<PgConnectOptions, Error> {
    PgConnectOptions::from_str(database_url).map(|options| options.ssl_mode(PgSslMode::VerifyFull))
}

/// Parses ephemeral local-test options with TLS explicitly disabled.
pub fn local_test_connection_options(database_url: &str) -> Result<PgConnectOptions, Error> {
    PgConnectOptions::from_str(database_url).map(|options| options.ssl_mode(PgSslMode::Disable))
}

#[cfg(test)]
mod tests {
    use sqlx::postgres::PgSslMode;

    use super::{local_test_connection_options, production_connection_options};

    const DATABASE_URL: &str = "postgres://signalbox:secret@database.example/signalbox";

    #[test]
    fn production_options_require_full_tls_verification() {
        let options = production_connection_options(DATABASE_URL).expect("valid database URL");

        assert!(matches!(options.get_ssl_mode(), PgSslMode::VerifyFull));
    }

    #[test]
    fn local_test_options_disable_tls_explicitly() {
        let options = local_test_connection_options(DATABASE_URL).expect("valid database URL");

        assert!(matches!(options.get_ssl_mode(), PgSslMode::Disable));
    }
}
