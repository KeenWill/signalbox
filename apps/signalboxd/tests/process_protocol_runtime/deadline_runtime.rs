use std::{
    io::{self, Write},
    sync::{Arc, Mutex},
    time::Duration,
};

use signalbox_application::TurnLivenessScanInterval;
use signalbox_persistence::session_deadline::SessionDeadlineBounds;
use signalboxd::LifecycleDeadlineRuntime;
use tokio::sync::watch;
use tracing::instrument::WithSubscriber;

#[derive(Clone)]
struct DeadlineWarningCapture {
    bytes: Arc<Mutex<Vec<u8>>>,
    shutdown: watch::Sender<bool>,
}

impl Write for DeadlineWarningCapture {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.bytes
            .lock()
            .expect("warning capture lock")
            .extend_from_slice(bytes);
        self.shutdown.send_replace(true);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn deadline_warning_names_the_failed_operation_and_database_error() {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .connect_lazy("postgres://localhost/deadline_warning_test")
        .expect("fixture connection options");
    pool.close().await;
    let warning = capture_deadline_warning(pool).await;
    assert!(
        warning.contains("session deadline pass produced no decision"),
        "{warning}"
    );
    assert!(
        warning.contains("operation=\"select_deadline_candidate\""),
        "{warning}"
    );
    assert!(warning.contains("error_class=\"pool_closed\""), "{warning}");
}

async fn capture_deadline_warning(pool: sqlx::PgPool) -> String {
    let (shutdown, shutdown_rx) = watch::channel(false);
    let bytes = Arc::new(Mutex::new(Vec::new()));
    let capture = DeadlineWarningCapture {
        bytes: Arc::clone(&bytes),
        shutdown,
    };
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_max_level(tracing::Level::WARN)
        .with_writer(move || capture.clone())
        .finish();
    let runtime = LifecycleDeadlineRuntime::new(
        pool,
        Some(TurnLivenessScanInterval::try_new(Duration::from_secs(1)).expect("scan interval")),
        SessionDeadlineBounds::new(None, None),
    );

    tokio::time::timeout(
        Duration::from_secs(10),
        runtime.run(shutdown_rx).with_subscriber(subscriber),
    )
    .await
    .expect("the first warning shuts down the pass");

    String::from_utf8(bytes.lock().expect("warning capture lock").clone()).expect("UTF-8 warning")
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn deadline_warning_excludes_postgres_message_detail_hint_and_sql()
-> Result<(), Box<dyn std::error::Error>> {
    let runtime = super::RunningRuntime::start().await?;
    signalbox_persistence::test_support::inject_deadline_diagnostic_failure(&runtime.pool).await?;
    let warning = capture_deadline_warning(runtime.pool.clone()).await;
    assert!(
        warning.contains("operation=\"select_deadline_candidate\""),
        "{warning}"
    );
    assert!(warning.contains("error_class=\"database\""), "{warning}");
    assert!(warning.contains("sqlstate=\"P0001\""), "{warning}");
    for excluded in [
        "private-deadline",
        "private_deadline_source",
        "RAISE EXCEPTION",
        "SELECT",
        "PL/pgSQL",
        "PgDatabaseError",
    ] {
        assert!(!warning.contains(excluded), "{warning}");
    }
    runtime.stop().await
}
