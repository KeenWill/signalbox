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
    let (shutdown, shutdown_rx) = watch::channel(false);
    let bytes = Arc::new(Mutex::new(Vec::new()));
    let capture = DeadlineWarningCapture {
        bytes: Arc::clone(&bytes),
        shutdown,
    };
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
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

    let warning = String::from_utf8(bytes.lock().expect("warning capture lock").clone())
        .expect("UTF-8 warning");
    assert!(
        warning.contains("session deadline pass produced no decision"),
        "{warning}"
    );
    assert!(
        warning.contains("query: \"select_deadline_candidate\""),
        "{warning}"
    );
    assert!(warning.contains("PoolClosed"), "{warning}");
}
