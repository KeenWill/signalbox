//! PostgreSQL migration support and connection-option helpers.
//!
//! This crate owns persistence-specific types. SQLx types do not cross into the
//! domain crate.

mod command_registry;
mod conversation_import_codec;
mod creation_runner_placement;
mod lock_inventory;
mod model_settings_resolution;
mod user_content;

pub mod approval_judge;
pub mod attention;
pub mod automatic_reconciliation;
pub mod blob;
pub mod blob_derivation;
pub mod commissioned_dispatch;
pub mod context_compaction;
pub mod context_compaction_continuation;
pub mod convergence_sweep;
pub mod conversation_import;
pub mod conversation_import_discovery;
pub mod conversation_listing;
pub mod create_session;
pub mod create_session_from_imported_frontier;
pub mod credential_capacity;
pub mod credential_exclusions;
pub mod credential_invocations;
pub mod evaluation;
pub mod goal;
pub mod goal_turn;
pub mod hub_fence;
pub mod lifecycle_metrics;
pub mod mapping;
pub mod model_execution;
pub mod oauth_credential;
pub mod operator_status;
pub mod outbox;
pub mod plan;
pub mod process_read;
pub mod program_cancellation;
pub mod program_journal;
pub mod program_registration;
pub mod program_session;
pub mod replace_session_defaults;
pub mod repo_watch_command;
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
pub mod session_workspace;
pub mod start_eligible_turn;
pub mod startup;
pub mod submit_input;
pub mod termination_receipt;
#[cfg(feature = "test-support")]
pub mod test_support;
pub mod tool_loop;
pub mod turn_liveness;
pub mod usage;
pub mod workspace;
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

/// Environment variables ignored while building production connection options,
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

/// Ambient certificate-store variables ignored by production composition.
const AMBIENT_TLS_TRUST_VARIABLES: [&str; 2] = ["SSL_CERT_DIR", "SSL_CERT_FILE"];

/// Environment variables the daemon removes before constructing production
/// connection options.
///
/// SQLx seeds PostgreSQL options and native TLS roots from these variables.
/// Removing them before the asynchronous runtime starts keeps `DATABASE_URL`
/// authoritative without mutating the process environment while other threads
/// are running.
pub fn production_connection_environment_variables() -> impl Iterator<Item = &'static str> {
    AMBIENT_POSTGRES_VARIABLES
        .into_iter()
        .chain(AMBIENT_TLS_TRUST_VARIABLES)
}

/// Reports whether `~/.pgpass` exists so startup can warn without opening it.
fn default_passfile_is_present() -> bool {
    std::env::home_dir().is_some_and(|home| home.join(".pgpass").exists())
}

/// Names the connection parameters SQLx would take from outside the URL
/// because this URL omits them: the user name from the process account, the
/// host from a probe of local PostgreSQL socket directories, and the password
/// from a password file. Each may be stated in the URL's authority or in the
/// query parameter SQLx reads for it — `user` for the user name, `host` or
/// `hostaddr` for the TLS host, and `password` for the password. An explicitly
/// empty password still states that no password is supplied. A socket path
/// selects the transport without stating the TLS host. Port and database name
/// are left to SQLx: an omitted port is the fixed 5432, and an omitted database
/// name lets the server apply the user name the URL states.
fn parameters_taken_from_outside_the_url(url: &Url) -> Vec<&'static str> {
    let mut host_is_stated = url.host_str().is_some_and(|host| {
        !host.is_empty()
            && !host.starts_with('/')
            && !host
                .get(..3)
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case("%2f"))
    });
    let mut user_is_stated = !url.username().is_empty();
    let mut password_is_stated = url.password().is_some();
    for (parameter, value) in url.query_pairs() {
        match &*parameter {
            "host" if !value.starts_with('/') => host_is_stated = !value.is_empty(),
            "hostaddr" => host_is_stated = !value.is_empty(),
            "user" => user_is_stated = !value.is_empty(),
            "password" => password_is_stated = true,
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
    if !password_is_stated {
        taken.push("password");
    }
    taken
}

/// Parses production connection options with certificate and hostname checks.
///
/// The database URL supplies every production connection parameter. The daemon
/// removes ambient libpq and certificate-store variables before calling this
/// function, and the required explicit password prevents password-file lookup.
/// An omitted host, user, or password fails parsing before SQLx can supply it
/// from outside the URL.
pub fn production_connection_options(database_url: &str) -> Result<PgConnectOptions, Error> {
    production_connection_options_with_environment(
        database_url,
        |name| std::env::var_os(name).is_some(),
        default_passfile_is_present,
    )
}

/// Names ambient PostgreSQL channels present in this process or its default
/// password-file location.
pub fn production_connection_ambient_warnings() -> Vec<&'static str> {
    let mut warnings = production_connection_environment_variables()
        .filter(|name| std::env::var_os(name).is_some())
        .collect::<Vec<_>>();
    if default_passfile_is_present() {
        warnings.push("~/.pgpass");
    }
    warnings
}

/// Parses production options against explicit ambient-channel lookups.
fn production_connection_options_with_environment(
    database_url: &str,
    _variable_is_present: impl Fn(&'static str) -> bool,
    _passfile_is_present: impl Fn() -> bool,
) -> Result<PgConnectOptions, Error> {
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
    fn production_options_ignore_an_ambient_credential_variable() {
        let options = production_connection_options_with_environment(
            DATABASE_URL,
            |name| name == "PGPASSWORD",
            no_default_passfile,
        )
        .expect("the complete URL is authoritative");

        assert_eq!(options.get_username(), "signalbox");
    }

    /// The one spelling of the ambient channel the proof below plants, used
    /// both to build the child's environment and to check the observed
    /// outcome, so exercising a different variable cannot leave a stale
    /// expectation behind.
    const AMBIENT_CREDENTIAL_VARIABLE: &str = "PGPASSWORD";

    /// Synthetic only: this value must never reach a real connection attempt.
    const AMBIENT_CREDENTIAL_VALUE: &str = "sb-fix9-synthetic-not-a-real-credential";

    /// The libtest path `--exact` needs for the fixture below. A stale path
    /// selects zero tests, which libtest still reports as success — the
    /// evidence assertion in the parent is what turns that into a failure.
    const REAL_ENVIRONMENT_FIXTURE: &str = "tests::real_ambient_environment_fixture";

    /// Prefix the fixture prints its outcome behind, so the parent can tell a
    /// fixture that ran from a filter that matched nothing.
    const FIXTURE_EVIDENCE: &str = "signalbox-fix9-child-observed:";

    /// Reports how `production_connection_options` treats the environment of
    /// the process it is running in. Deliberately assertion-free: the parent
    /// owns the expectation, and this crate's PostgreSQL suite is swept with a
    /// bare `--ignored` in CI, where no parent has planted anything.
    #[test]
    #[ignore = "subprocess fixture for the real-environment isolation proof"]
    fn real_ambient_environment_fixture() {
        println!(
            "{FIXTURE_EVIDENCE}{:?}",
            production_connection_options(DATABASE_URL).map(|_| ())
        );
    }

    #[test]
    fn production_options_accept_a_real_ambient_pgpassword_variable() {
        // `Command::env` sets only the child's environment, so proving the
        // public, env-reading entry point needs no `std::env::set_var` — which
        // the crate's forbidden `unsafe_code` would reject anyway. Every other
        // environment test drives the injected lookup instead of the process
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
            observed.contains(&format!("{FIXTURE_EVIDENCE}Ok(")),
            "a real ambient {AMBIENT_CREDENTIAL_VARIABLE} must not override the complete URL: \
             {observed}"
        );
    }

    #[test]
    fn production_options_ignore_every_ambient_variable() {
        let options = production_connection_options_with_environment(
            DATABASE_URL,
            |_| true,
            no_default_passfile,
        )
        .expect("the complete URL is authoritative");

        assert_eq!(options.get_host(), "database.example");
    }

    #[test]
    fn production_options_ignore_an_ambient_trust_store_variable() {
        let options = production_connection_options_with_environment(
            DATABASE_URL,
            |name| name == "SSL_CERT_FILE",
            no_default_passfile,
        )
        .expect("the complete URL is authoritative");

        assert!(matches!(options.get_ssl_mode(), PgSslMode::VerifyFull));
    }

    #[test]
    fn production_options_ignore_every_ambient_trust_store_variable() {
        let options = production_connection_options_with_environment(
            DATABASE_URL,
            |name| name.starts_with("SSL_CERT_"),
            no_default_passfile,
        )
        .expect("the complete URL is authoritative");

        assert!(matches!(options.get_ssl_mode(), PgSslMode::VerifyFull));
    }

    #[test]
    fn production_options_ignore_the_default_password_file() {
        let options = production_connection_options_with_environment(
            DATABASE_URL,
            no_ambient_variables,
            || true,
        )
        .expect("the complete URL is authoritative");

        assert_eq!(options.get_username(), "signalbox");
    }

    #[test]
    fn production_options_reject_a_url_the_process_account_would_complete() {
        let error = production_connection_options_with_environment(
            "postgres:///signalbox",
            no_ambient_variables,
            no_default_passfile,
        )
        .expect_err("a URL SQLx would complete from outside must fail closed");

        expect!["error with configuration: the process account and host filesystem would supply production connection parameters the database URL omits: host, user, password; state every connection parameter in the database URL"].assert_eq(&error.to_string());
    }

    #[test]
    fn production_options_reject_a_url_that_states_only_the_host() {
        let error = production_connection_options_with_environment(
            "postgres://database.example/signalbox",
            no_ambient_variables,
            no_default_passfile,
        )
        .expect_err("a URL without a user name must fail closed");

        expect!["error with configuration: the process account and host filesystem would supply production connection parameters the database URL omits: user, password; state every connection parameter in the database URL"].assert_eq(&error.to_string());
    }

    #[test]
    fn production_options_accept_parameters_stated_in_the_query() {
        let options = production_connection_options_with_environment(
            "postgres:///signalbox?host=database.example&user=signalbox&password=secret",
            no_ambient_variables,
            no_default_passfile,
        )
        .expect("SQLx reads these query parameters, so the URL states both");

        assert_eq!(options.get_host(), "database.example");
        assert_eq!(options.get_username(), "signalbox");
    }

    #[test]
    fn production_options_reject_socket_paths_without_a_tls_host() {
        for url in [
            "postgres:///signalbox?user=signalbox&password=secret&host=/var/run/postgresql",
            "postgres:///signalbox?user=signalbox&password=secret&host=%2Fvar%2Frun%2Fpostgresql",
            "postgres://signalbox:secret@%2Fvar%2Frun%2Fpostgresql/signalbox",
            "postgres://signalbox:secret@%2fvar%2frun%2fpostgresql/signalbox",
        ] {
            let error = production_connection_options_with_environment(
                url,
                no_ambient_variables,
                no_default_passfile,
            )
            .expect_err("a socket path does not state the TLS peer hostname");

            expect!["error with configuration: the process account and host filesystem would supply production connection parameters the database URL omits: host; state every connection parameter in the database URL"].assert_eq(&error.to_string());
        }
    }

    #[test]
    fn production_socket_options_verify_the_tls_host_stated_in_the_url() {
        for url in [
            "postgres://signalbox:secret@database.example/signalbox?host=/var/run/postgresql",
            "postgres:///signalbox?user=signalbox&password=secret&host=/var/run/postgresql&host=database.example",
            "postgres://signalbox:secret@%2Fvar%2Frun%2Fpostgresql/signalbox?host=database.example",
        ] {
            let options = production_connection_options_with_environment(
                url,
                no_ambient_variables,
                no_default_passfile,
            )
            .expect("the URL states both socket transport and TLS peer hostname");

            assert_eq!(options.get_host(), "database.example");
            assert_eq!(
                options.get_socket().map(|path| path.as_path()),
                Some(std::path::Path::new("/var/run/postgresql"))
            );
            assert!(matches!(options.get_ssl_mode(), PgSslMode::VerifyFull));
        }
    }

    #[test]
    fn production_options_reject_empty_query_overrides_of_stated_parameters() {
        for parameter in ["host", "user"] {
            let error = production_connection_options_with_environment(
                &format!("{DATABASE_URL}?{parameter}="),
                no_ambient_variables,
                no_default_passfile,
            )
            .expect_err("an empty query override erases the authority parameter");

            assert!(error.to_string().contains(&format!("omits: {parameter};")));
        }
    }

    #[test]
    fn production_options_require_an_explicit_password() {
        let error = production_connection_options_with_environment(
            "postgres://signalbox@database.example/signalbox",
            no_ambient_variables,
            no_default_passfile,
        )
        .expect_err("a URL without a password must not consult a password file");

        expect!["error with configuration: the process account and host filesystem would supply production connection parameters the database URL omits: password; state every connection parameter in the database URL"].assert_eq(&error.to_string());
    }

    #[test]
    fn production_options_accept_an_explicit_empty_password() {
        production_connection_options_with_environment(
            "postgres://signalbox@database.example/signalbox?password=",
            no_ambient_variables,
            no_default_passfile,
        )
        .expect("an explicit empty password disables password-file lookup");
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

pub mod credential_pool_exhaustion;
/// Durable configuration reload intent and receipts.
pub mod reload_configuration;
