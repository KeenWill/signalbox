//! The packaged daemon and runner preserve enrollment across a graceful restart.

use signalbox_domain::RunnerEnrollmentId;
use signalbox_persistence::runner_protocol::{
    NonterminalRunnerConnection, RunnerConnectionCause, RunnerConnectionSnapshot,
    RunnerConnectionState, RunnerProtocolStore,
};
use signalbox_runner::RunnerStateRoot;
use std::{error::Error, time::Duration};
use tokio::time::{sleep, timeout};

#[path = "support/runner_process.rs"]
mod runner_process;
use runner_process::{PROCESS_ALLOWANCE, RunnerProcesses, stop};

// More than the daemon's three missed five-second intervals: an unanswered
// heartbeat must have become durable loss before this observation.
const HEARTBEAT_OBSERVATION: Duration = Duration::from_secs(21);

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL, OpenSSL, and both packaged binaries"]
async fn real_runner_resumes_enrollment_after_graceful_shutdown() -> Result<(), Box<dyn Error>> {
    let processes = RunnerProcesses::start().await?;
    let store = signalboxd::runner_protocol_runtime::PostgresRunnerRegistrationService::local(
        processes.pool.clone(),
    )
    .map_err(|error| format!("local runner catalog is invalid: {error:?}"))?
    .recovery_store();
    let mut runner = processes.spawn_runner()?;
    processes
        .wait_for_runner(&mut runner, "runner enrolled")
        .await?;
    let first = connected(&store).await?;
    let enrollment = store
        .load_enrollment(first.enrollment())
        .await?
        .ok_or("enrollment was not retained")?;
    let registration = store
        .load_current_registration(&enrollment)
        .await?
        .ok_or("advertisement was not retained")?;
    assert_eq!(first.epoch().get(), 1);
    assert_eq!(
        registration
            .registration()
            .tools()
            .map(|tool| tool.name().as_str())
            .collect::<Vec<_>>(),
        ["echo"]
    );
    sleep(HEARTBEAT_OBSERVATION).await;
    let live = store
        .load_connection(first.enrollment())
        .await?
        .ok_or("connection was not retained")?;
    assert_eq!(live.epoch(), first.epoch());
    assert_eq!(
        live.state(),
        RunnerConnectionState::Connected,
        "the runner must answer heartbeat challenges"
    );
    assert!(
        runner.try_wait()?.is_none(),
        "the runner must stay alive through heartbeat exchanges"
    );
    stop(&mut runner).await?;
    let shutdown = terminal_connection(&store, first.enrollment()).await?;
    assert_eq!(shutdown.state(), RunnerConnectionState::Shutdown);
    assert_eq!(shutdown.cause(), RunnerConnectionCause::RunnerShutdown);
    let durable = RunnerStateRoot::open(&processes.runner_root)?;
    let receipt = durable
        .state()
        .receipt()
        .ok_or("runner did not journal enrollment")?
        .clone();
    assert_eq!(
        receipt.enrollment_id().into_uuid(),
        first.enrollment().into_uuid()
    );
    assert_eq!(
        receipt.runner_id().into_uuid(),
        enrollment.runner().into_uuid()
    );
    assert_eq!(
        receipt.registration_revision().get(),
        registration.revision().get()
    );
    assert_eq!(
        durable.reconnect_inventory(),
        signalbox_runner_wire::ReconnectInventory::default()
    );
    assert!(
        processes
            .runner_root
            .join("operation-journal.json")
            .is_file()
    );
    drop(durable);

    let mut resumed_runner = processes.spawn_runner()?;
    processes
        .wait_for_runner(&mut resumed_runner, "runner resumed")
        .await?;
    let resumed = connected(&store).await?;
    assert_eq!(
        resumed.enrollment(),
        first.enrollment(),
        "restart must resume the original enrollment"
    );
    assert_eq!(
        resumed.epoch().get(),
        2,
        "restart must open a new connection epoch"
    );
    let resumed_registration = store
        .load_current_registration(&enrollment)
        .await?
        .ok_or("resumed registration was not retained")?;
    assert_eq!(
        resumed_registration.revision(),
        registration.revision(),
        "equal advertisement must retain its registration"
    );
    stop(&mut resumed_runner).await?;
    let reopened = RunnerStateRoot::open(&processes.runner_root)?;
    assert_eq!(
        reopened.state().receipt(),
        Some(&receipt),
        "restart must retain the exact durable enrollment receipt"
    );
    assert_eq!(
        reopened.reconnect_inventory(),
        signalbox_runner_wire::ReconnectInventory::default()
    );
    drop(reopened);
    processes.shutdown().await?;
    Ok(())
}

async fn connected(
    store: &RunnerProtocolStore,
) -> Result<NonterminalRunnerConnection, Box<dyn Error>> {
    let mut connections = store.load_nonterminal_connection_heads().await?;
    let connection = connections
        .pop()
        .ok_or("established runner has no durable connection")?;
    assert!(connections.is_empty(), "only the spawned runner may enroll");
    Ok(connection)
}

async fn terminal_connection(
    store: &RunnerProtocolStore,
    enrollment: RunnerEnrollmentId,
) -> Result<RunnerConnectionSnapshot, Box<dyn Error>> {
    timeout(PROCESS_ALLOWANCE, async {
        loop {
            let connection = store
                .load_connection(enrollment)
                .await?
                .ok_or("connection was not retained")?;
            match connection.state() {
                RunnerConnectionState::Shutdown | RunnerConnectionState::Lost => {
                    return Ok(connection);
                }
                RunnerConnectionState::Connected | RunnerConnectionState::Suspect => {
                    sleep(Duration::from_millis(50)).await
                }
            }
        }
    })
    .await?
}
