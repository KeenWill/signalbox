//! PostgreSQL migration support and connection-option helpers.
//!
//! This crate owns persistence-specific types. SQLx types do not cross into the
//! domain crate.

mod command_registry;
mod conversation_import_codec;
mod lock_inventory;
mod model_settings_resolution;
mod user_content;

pub mod approval_judge;
pub mod approval_judge_eval;
pub mod attention;
pub mod automatic_reconciliation;
pub mod blob;
pub mod blob_derivation;
pub mod commissioned_dispatch;
pub mod context_compaction;
pub mod conversation_import;
pub mod conversation_import_discovery;
pub mod conversation_listing;
pub mod create_session;
pub mod create_session_from_imported_frontier;
pub mod goal;
pub mod goal_turn;
pub mod hub_fence;
pub mod lifecycle_metrics;
pub mod mapping;
pub mod model_execution;
pub mod operator_status;
pub mod outbox;
pub mod plan;
pub mod process_read;
pub mod program_journal;
pub mod replace_session_defaults;
pub mod review_orchestration;
pub mod review_workflow;
mod review_workflow_command;
pub mod runner_protocol;
pub mod scheduler;
pub mod search;
pub mod session;
pub mod session_credentials;
pub mod session_deadline;
pub mod session_delegation;
pub mod session_lifecycle;
pub mod session_lifecycle_command;
pub mod session_live;
pub mod session_metadata;
pub mod session_placement;
pub mod session_timeline;
pub mod start_eligible_turn;
pub mod startup;
pub mod submit_input;
#[cfg(feature = "test-support")]
pub mod test_support;
pub mod tool_loop;
pub mod turn_liveness;
pub mod usage;
pub mod workspace_instructions;

pub use session_credentials::{
    ModelCredentialFamilyCatalog, ModelCredentialFamilyCatalogError, SessionCredentialPin,
    SessionCredentialPinError, SessionModelCredential,
};

use std::str::FromStr;
use std::time::Duration;

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

/// The label key marking a container that exists only for one test of this
/// repository and that the test harness is itself responsible for removing.
///
/// `tooling/sweep-test-containers.sh` reclaims exactly the containers carrying
/// this label past an age bound, so this constant is the single spelling of
/// that selector; `tooling/test_sweep_test_containers.py` fails when the script
/// and this constant disagree.
pub const DISPOSABLE_TEST_CONTAINER_LABEL_KEY: &str = "org.signalbox.disposable";

/// The label value paired with [`DISPOSABLE_TEST_CONTAINER_LABEL_KEY`].
pub const DISPOSABLE_TEST_CONTAINER_LABEL_VALUE: &str = "test-container";

/// The longest a container may carry the disposable mark and still be safe.
///
/// `tooling/sweep-test-containers.sh` removes marked containers older than this
/// by default, which is what makes the mark safe to apply: a container serving a
/// test is minutes old. Anything that can be configured to hold a marked
/// container longer would be force-removed while still in use, so it checks
/// itself against this bound first — see
/// [`outlives_the_disposable_container_sweep`].
pub const DISPOSABLE_TEST_CONTAINER_LIFETIME_HOURS: u64 = 2;

/// Reports whether holding a marked container for `lifetime` would outlive the
/// sweep's default age bound, and so risk removal while it is still in use.
pub fn outlives_the_disposable_container_sweep(lifetime: Duration) -> bool {
    lifetime >= Duration::from_secs(DISPOSABLE_TEST_CONTAINER_LIFETIME_HOURS * 60 * 60)
}

/// The environment variable the testcontainers client reads to decide whether a
/// container is removed when its handle drops.
const TESTCONTAINERS_COMMAND_VARIABLE: &str = "TESTCONTAINERS_COMMAND";

/// The one `TESTCONTAINERS_COMMAND` value that stops the client removing a
/// container it started.
const TESTCONTAINERS_KEEP_COMMAND: &str = "keep";

/// The labels every PostgreSQL container this repository's tests start carries.
///
/// The mark is what the orphan sweep selects on, and identifying disposable
/// containers positively is the point of it. A sweep keyed on the
/// testcontainers `managed-by` label, or on the image name, would also select
/// containers on a shared daemon that belong to nobody here — another project's
/// suite, a hand-run database, a long-lived instance on the same image — and no
/// list of names to skip can enumerate those in advance.
///
/// A container the operator asked the client to keep
/// (`TESTCONTAINERS_COMMAND=keep`) is not disposable: keeping it is the whole
/// request, and nothing else would remove it afterwards. Such a container is
/// left unmarked, so the sweep never selects it.
pub fn disposable_test_container_labels() -> Vec<(&'static str, &'static str)> {
    let command = std::env::var(TESTCONTAINERS_COMMAND_VARIABLE).ok();
    disposable_test_container_labels_for_command(command.as_deref())
}

/// [`disposable_test_container_labels`] with the `TESTCONTAINERS_COMMAND` value
/// supplied directly, so the keep case is decidable without a process-wide
/// environment mutation.
pub fn disposable_test_container_labels_for_command(
    command: Option<&str>,
) -> Vec<(&'static str, &'static str)> {
    if command == Some(TESTCONTAINERS_KEEP_COMMAND) {
        return Vec::new();
    }
    vec![(
        DISPOSABLE_TEST_CONTAINER_LABEL_KEY,
        DISPOSABLE_TEST_CONTAINER_LABEL_VALUE,
    )]
}

/// Where the pinned `postgres:18*` images keep every byte of database state.
///
/// The image sets `PGDATA` to `/var/lib/postgresql/<major>/docker` and declares
/// `VOLUME /var/lib/postgresql`, so a mount at this path holds the data
/// directory and its WAL, and pre-empts the anonymous volume the image would
/// otherwise create on the daemon's disk.
#[cfg(feature = "postgres-integration")]
const POSTGRES_STATE_DIRECTORY: &str = "/var/lib/postgresql";

/// The `postgres` server arguments every disposable test container starts
/// with: durability off, because every container is discarded after its test.
///
/// `fsync=off` restates the testcontainers module's own default so a caller
/// composing extra arguments through `with_cmd` — which replaces the image's
/// command wholesale — cannot silently drop it; `synchronous_commit=off` and
/// `full_page_writes=off` stop commits waiting on WAL flushes and stop
/// torn-page protection writes whose crash-recovery value is nil for a
/// database that never restarts. None of the three changes SQL semantics.
///
/// Callers needing further settings extend this list rather than restating it:
/// `disposable_postgres_server_args().into_iter().chain([...])`.
#[cfg(feature = "postgres-integration")]
pub fn disposable_postgres_server_args() -> [&'static str; 6] {
    [
        "-c",
        "fsync=off",
        "-c",
        "synchronous_commit=off",
        "-c",
        "full_page_writes=off",
    ]
}

/// The RAM-backed mount every disposable test container keeps its database
/// state on, so ephemeral `initdb`, WAL, and relation writes never reach the
/// host's disk.
///
/// A configured size makes a runaway test fail its own container with `No
/// space left on device` instead of consuming host memory without limit;
/// `None` selects the deployment's explicit unbounded policy. Tmpfs charges
/// only pages actually written. Stranded containers hold those pages until
/// removed, which is one more reason `tooling/sweep-test-containers.sh` runs on
/// a timer on shared machines.
#[cfg(feature = "postgres-integration")]
pub fn disposable_postgres_state_tmpfs(
    ceiling_bytes: Option<i64>,
) -> testcontainers_modules::testcontainers::core::Mount {
    let mount =
        testcontainers_modules::testcontainers::core::Mount::tmpfs_mount(POSTGRES_STATE_DIRECTORY);
    match ceiling_bytes {
        Some(ceiling_bytes) => mount.with_size_bytes(ceiling_bytes),
        None => mount,
    }
}

/// Builds the disposable-database mount from the checked-in deployment
/// example used by repository integration tests.
#[cfg(feature = "postgres-integration")]
pub fn disposable_postgres_state_tmpfs_from_example()
-> std::io::Result<testcontainers_modules::testcontainers::core::Mount> {
    const FIELD_PREFIX: &str = "disposable_postgres_state_ceiling_bytes = ";
    let document = include_str!("../../../config/signalboxd.example.toml");
    let value = document
        .lines()
        .find_map(|line| line.strip_prefix(FIELD_PREFIX))
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "checked-in example omits disposable PostgreSQL state ceiling",
            )
        })?;
    let ceiling_bytes = value.parse::<i64>().map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "checked-in disposable PostgreSQL state ceiling is not an integer",
        )
    })?;
    Ok(disposable_postgres_state_tmpfs(Some(ceiling_bytes)))
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::process::Command;

    use expect_test::expect;
    use sqlx::postgres::PgSslMode;

    use super::{
        DISPOSABLE_TEST_CONTAINER_LABEL_KEY, DISPOSABLE_TEST_CONTAINER_LABEL_VALUE,
        DISPOSABLE_TEST_CONTAINER_LIFETIME_HOURS, Duration, TESTCONTAINERS_KEEP_COMMAND,
        commit_failure_is_ambiguous, disposable_test_container_labels_for_command,
        local_test_connection_options, outlives_the_disposable_container_sweep,
        production_connection_options, production_connection_options_with_environment,
    };

    const DATABASE_URL: &str = "postgres://signalbox:secret@database.example/signalbox";

    /// The `TESTCONTAINERS_COMMAND` value asking the client for its own default:
    /// remove the container when its handle drops.
    const TESTCONTAINERS_REMOVE_COMMAND: &str = "remove";

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

    /// The one spelling of the ambient channel the proof below plants, used
    /// both to build the child's environment and to check what the refusal
    /// names, so exercising a different variable cannot leave a stale
    /// expectation behind.
    const AMBIENT_CREDENTIAL_VARIABLE: &str = "PGPASSWORD";

    /// Synthetic only: the refusal happens before any database contact, so
    /// this value must never reach a real connection attempt.
    const AMBIENT_CREDENTIAL_VALUE: &str = "sb-fix9-synthetic-not-a-real-credential";

    /// The libtest path `--exact` needs for the fixture below. A stale path
    /// selects zero tests, which libtest still reports as success — the
    /// evidence assertion in the parent is what turns that into a failure.
    const REAL_ENVIRONMENT_FIXTURE: &str = "tests::real_ambient_environment_refusal_fixture";

    /// Prefix the fixture prints its outcome behind, so the parent can tell a
    /// fixture that ran from a filter that matched nothing.
    const FIXTURE_EVIDENCE: &str = "signalbox-fix9-child-observed:";

    /// Reports how `production_connection_options` treats the environment of
    /// the process it is running in. Deliberately assertion-free: the parent
    /// owns the expectation, and this crate's PostgreSQL suite is swept with a
    /// bare `--ignored` in CI, where no parent has planted anything.
    #[test]
    #[ignore = "subprocess fixture for the real-environment refusal proof"]
    fn real_ambient_environment_refusal_fixture() {
        println!(
            "{FIXTURE_EVIDENCE}{:?}",
            production_connection_options(DATABASE_URL).map(|_| ())
        );
    }

    #[test]
    fn production_options_refuse_a_real_ambient_pgpassword_variable() {
        // `Command::env` sets only the child's environment, so proving the
        // public, env-reading entry point needs no `std::env::set_var` — which
        // the crate's forbidden `unsafe_code` would reject anyway. Every other
        // refusal test drives the injected lookup instead of the process
        // environment `production_connection_options` actually reads.
        let executable =
            std::env::current_exe().expect("test binary path is available under `cargo test`");
        let output = Command::new(executable)
            .env(AMBIENT_CREDENTIAL_VARIABLE, AMBIENT_CREDENTIAL_VALUE)
            .args([
                "--ignored",
                "--exact",
                REAL_ENVIRONMENT_FIXTURE,
                "--nocapture",
            ])
            .output()
            .expect("spawn this test binary as the child process");
        let observed = String::from_utf8(output.stdout).expect("child stdout is UTF-8");

        assert!(
            output.status.success(),
            "the child fixture must run and pass: {observed}"
        );
        assert!(
            observed.contains(FIXTURE_EVIDENCE),
            "the child must actually execute {REAL_ENVIRONMENT_FIXTURE}: an `--exact` filter that \
             matches nothing runs zero tests and still exits zero: {observed}"
        );
        assert!(
            observed.contains(&format!("{FIXTURE_EVIDENCE}Err(")),
            "a real ambient {AMBIENT_CREDENTIAL_VARIABLE} must be refused, not silently \
             consulted: {observed}"
        );
        assert!(
            observed.contains(AMBIENT_CREDENTIAL_VARIABLE),
            "the refusal must name the ambient channel the parent planted: {observed}"
        );
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

    #[test]
    fn a_test_container_is_marked_disposable_for_the_sweep() {
        let labels = disposable_test_container_labels_for_command(None);

        assert_eq!(
            labels,
            vec![(
                DISPOSABLE_TEST_CONTAINER_LABEL_KEY,
                DISPOSABLE_TEST_CONTAINER_LABEL_VALUE
            )]
        );
    }

    #[test]
    fn a_container_the_client_will_remove_itself_is_marked_disposable() {
        let labels =
            disposable_test_container_labels_for_command(Some(TESTCONTAINERS_REMOVE_COMMAND));

        assert_eq!(
            labels,
            vec![(
                DISPOSABLE_TEST_CONTAINER_LABEL_KEY,
                DISPOSABLE_TEST_CONTAINER_LABEL_VALUE
            )]
        );
    }

    #[test]
    fn a_container_the_client_was_asked_to_keep_is_not_marked_disposable() {
        let labels =
            disposable_test_container_labels_for_command(Some(TESTCONTAINERS_KEEP_COMMAND));

        assert!(
            labels.is_empty(),
            "a kept container is nothing's to remove, so the sweep must not see a mark: {labels:?}"
        );
    }

    #[test]
    fn a_container_held_no_longer_than_a_test_is_safe_to_mark_disposable() {
        let held = Duration::from_secs(DISPOSABLE_TEST_CONTAINER_LIFETIME_HOURS * 60 * 60 - 1);

        assert!(!outlives_the_disposable_container_sweep(held));
    }

    #[test]
    fn a_container_held_to_the_sweep_bound_would_be_removed_while_in_use() {
        let held = Duration::from_secs(DISPOSABLE_TEST_CONTAINER_LIFETIME_HOURS * 60 * 60);

        assert!(outlives_the_disposable_container_sweep(held));
    }

    #[cfg(feature = "postgres-integration")]
    #[test]
    fn a_disposable_container_uses_the_supplied_tmpfs_policy() {
        use testcontainers_modules::testcontainers::core::MountType;

        let bounded = super::disposable_postgres_state_tmpfs(Some(4_096));
        let unbounded = super::disposable_postgres_state_tmpfs(None);

        assert_eq!(bounded.mount_type(), MountType::Tmpfs);
        assert_eq!(bounded.target(), Some("/var/lib/postgresql"));
        assert_eq!(
            bounded
                .tmpfs_options()
                .expect("the fixture selects a bounded tmpfs")
                .size_bytes,
            Some(4_096)
        );
        assert!(unbounded.tmpfs_options().is_none());
    }
}
