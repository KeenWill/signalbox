//! Binary entrypoint for `signalbox-runner`: connects to the daemon socket,
//! establishes and re-establishes `RunnerConnection` with exponential
//! reconnect backoff on either the socket or the enrollment handshake, and
//! attempts a graceful exit on `SIGTERM`/`SIGINT`.

use std::{
    cmp, env, error::Error, ffi::OsString, fmt, future::Future, io, process::ExitCode,
    time::Duration,
};

use signalbox_runner::{
    AcceptedWorkspaceRelease, ArgumentError, ConnectionEnd, DispatchHttpsEndpoint,
    EnrollmentOutcome, HttpsBroker, ProtocolViolation, RunnerConfiguration,
    RunnerConfigurationError, RunnerConfigurationPath, RunnerConnection, RunnerConnectionError,
    RunnerStateError, RunnerStateRoot, ServeOutcome, SocketConnectError, connect_verified,
};
use signalbox_runner_wire::{ExecutionErrorKind, SandboxProfile, TerminalResult};
use signalbox_tools_exec::{ExecArguments, SandboxedCommandRunner, TokioProcessRunner};

const CONFIGURATION_ENVIRONMENT: &str = "SIGNALBOX_RUNNER_CONFIG_FILE";
const INITIAL_RECONNECT_DELAY: Duration = Duration::from_secs(1);
const MAXIMUM_RECONNECT_DELAY: Duration = Duration::from_secs(30);
const SHUTDOWN_WRITE_TIMEOUT: Duration = Duration::from_secs(5);

#[tokio::main]
async fn main() -> ExitCode {
    match run(
        env::args_os().skip(1),
        env::var_os(CONFIGURATION_ENVIRONMENT),
    )
    .await
    {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("signalbox-runner: {error}");
            let mut source = error.source();
            while let Some(cause) = source {
                eprintln!("caused by: {cause}");
                source = cause.source();
            }
            ExitCode::FAILURE
        }
    }
}

async fn run(
    arguments: impl IntoIterator<Item = OsString>,
    environment: Option<OsString>,
) -> Result<(), RunnerDaemonError> {
    let path = RunnerConfigurationPath::resolve(arguments, environment)
        .map_err(RunnerDaemonError::Argument)?;
    let configuration =
        RunnerConfiguration::read(path.as_path()).map_err(RunnerDaemonError::Configuration)?;
    let mut state =
        RunnerStateRoot::open(configuration.runner_root()).map_err(RunnerDaemonError::State)?;
    // Keep both pinned executable identities live across every connection epoch.
    let execution_programs = TokioProcessRunner::try_new_with_bubblewrap(
        configuration.exec_supervisor_executable(),
        configuration.bubblewrap_path(),
    )
    .map_err(|_| RunnerDaemonError::ExecutionPrograms)?;
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .map_err(RunnerDaemonError::Signal)?;
    let mut interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
        .map_err(RunnerDaemonError::Signal)?;
    let mut backoff = ReconnectBackoff::new();
    loop {
        let stream = match tokio::select! {
            connected = connect_verified(configuration.daemon_socket_path()) => connected,
            _ = terminate.recv() => return Ok(()),
            _ = interrupt.recv() => return Ok(()),
        } {
            Ok(stream) => stream,
            Err(error) if error.is_reconnectable() => {
                let delay = backoff.next_delay();
                report_reconnect(ReconnectStage::Socket, &error, delay);
                if wait_for_retry(delay, &mut terminate, &mut interrupt).await {
                    return Ok(());
                }
                continue;
            }
            Err(error) => return Err(RunnerDaemonError::Socket(error)),
        };
        let mut connection = match tokio::select! {
            established = RunnerConnection::establish(
                stream,
                &mut state,
                configuration.advertisement(),
            ) => established,
            _ = terminate.recv() => return Ok(()),
            _ = interrupt.recv() => return Ok(()),
        } {
            Ok(connection) => connection,
            Err(error) if error.is_reconnectable() => {
                let delay = backoff.next_delay();
                report_reconnect(ReconnectStage::Establishment, &error, delay);
                if wait_for_retry(delay, &mut terminate, &mut interrupt).await {
                    return Ok(());
                }
                continue;
            }
            Err(error) => return Err(RunnerDaemonError::Connection(error)),
        };
        report_established(&connection);
        backoff.reset();
        let served = loop {
            let shutdown = async {
                tokio::select! {
                    _ = terminate.recv() => {}
                    _ = interrupt.recv() => {}
                }
            };
            match connection.serve_until_shutdown(&mut state, shutdown).await {
                Ok(ServeOutcome::DispatchReady(dispatch)) => {
                    let execution = execute_dispatch(
                        execution_programs.clone(),
                        configuration.read_only_paths().to_vec(),
                        state.duplicate_directory(),
                        configuration.allowed_network_hosts().to_vec(),
                        dispatch.correlation().clone(),
                        dispatch.normalized_arguments().clone(),
                    );
                    if let Err(error) = connection
                        .execute_while_serving(&mut state, *dispatch, execution)
                        .await
                    {
                        break Err(error);
                    }
                }
                Ok(ServeOutcome::WorkspaceReleaseReady(release)) => {
                    let cleanup = release_private_workspace(&state, release.accepted());
                    if let Err(error) = connection
                        .release_while_serving(&mut state, *release, cleanup)
                        .await
                    {
                        break Err(error);
                    }
                }
                outcome => break outcome,
            }
        };
        match served {
            Ok(ServeOutcome::ConnectionEnded(end @ ConnectionEnd::DaemonShutdown { .. })) => {
                report_graceful_shutdown(&connection, end);
                return Ok(());
            }
            Ok(ServeOutcome::ConnectionEnded(end @ ConnectionEnd::RunnerShutdown { .. })) => {
                report_graceful_shutdown(&connection, end);
                return Ok(());
            }
            Ok(ServeOutcome::ConnectionEnded(ConnectionEnd::StaleConnectionRejected {
                ..
            })) => return Err(RunnerDaemonError::StaleConnectionRejected),
            Ok(ServeOutcome::ShutdownReady) => {
                return shutdown_with_timeout(&mut connection).await;
            }
            Ok(ServeOutcome::DispatchReady(_)) => {
                return Err(RunnerDaemonError::Connection(
                    RunnerConnectionError::Violation(ProtocolViolation::DispatchMismatch),
                ));
            }
            Ok(ServeOutcome::WorkspaceReleaseReady(_)) => {
                return Err(RunnerDaemonError::Connection(
                    RunnerConnectionError::Violation(
                        ProtocolViolation::WorkspaceReleaseHandoffMismatch,
                    ),
                ));
            }
            Err(error) if error.is_reconnectable() => {
                let delay = backoff.next_delay();
                report_reconnect(ReconnectStage::Serving, &error, delay);
                if wait_for_retry(delay, &mut terminate, &mut interrupt).await {
                    return Ok(());
                }
            }
            Err(error) => return Err(RunnerDaemonError::Connection(error)),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PrivateWorkspaceCleanupFailed;

fn release_private_workspace(
    state: &RunnerStateRoot,
    accepted: &AcceptedWorkspaceRelease,
) -> impl Future<Output = Result<(), PrivateWorkspaceCleanupFailed>> + use<> {
    let store = state.workspace_store();
    let accepted = accepted.clone();
    async move {
        let store = store.map_err(|_| PrivateWorkspaceCleanupFailed)?;
        tokio::task::spawn_blocking(move || store.release_private_root(&accepted))
            .await
            .map_err(|_| PrivateWorkspaceCleanupFailed)?
            .map_err(|_| PrivateWorkspaceCleanupFailed)
    }
}

async fn execute_dispatch(
    process_runner: TokioProcessRunner,
    read_only_paths: Vec<std::path::PathBuf>,
    runner_root: io::Result<std::fs::File>,
    allowed_network_hosts: Vec<signalbox_runner::AllowedNetworkHost>,
    correlation: signalbox_runner_wire::LeaseCorrelation,
    normalized_arguments: serde_json::Value,
) -> TerminalResult {
    if correlation.sandbox_profile != SandboxProfile::WorkspaceRestricted {
        return TerminalResult::KnownFailure {
            error_kind: ExecutionErrorKind::ExecutionFailed,
            detail: None,
        };
    }
    let arguments = match serde_json::from_value::<ExecArguments>(normalized_arguments) {
        Ok(arguments) => arguments,
        Err(_) => {
            return TerminalResult::KnownFailure {
                error_kind: ExecutionErrorKind::InvalidArguments,
                detail: None,
            };
        }
    };
    let runner_root = match runner_root {
        Ok(runner_root) => runner_root,
        Err(_) => {
            return TerminalResult::KnownFailure {
                error_kind: ExecutionErrorKind::ExecutionFailed,
                detail: None,
            };
        }
    };
    let endpoint = match DispatchHttpsEndpoint::bind(runner_root, correlation.lease_id) {
        Ok(endpoint) => endpoint,
        Err(_) => {
            return TerminalResult::KnownFailure {
                error_kind: ExecutionErrorKind::ExecutionFailed,
                detail: None,
            };
        }
    };
    let mut runner = match SandboxedCommandRunner::try_new_runner_restricted_with_https_broker(
        process_runner,
        correlation.working_directory.as_str(),
        &read_only_paths,
        endpoint.socket_path(),
    ) {
        Ok(runner) => runner,
        Err(_) => {
            return TerminalResult::KnownFailure {
                error_kind: ExecutionErrorKind::ExecutionFailed,
                detail: None,
            };
        }
    };
    let Some(deadline) =
        tokio::time::Instant::now().checked_add(Duration::from_secs(arguments.timeout_seconds))
    else {
        return TerminalResult::KnownFailure {
            error_kind: ExecutionErrorKind::InvalidArguments,
            detail: None,
        };
    };
    let (broker_stop, stop_broker) = tokio::sync::oneshot::channel();
    let broker = tokio::spawn(endpoint.serve(
        HttpsBroker::production(&allowed_network_hosts),
        deadline,
        stop_broker,
    ));
    let result = runner.try_run(arguments).await;
    let _ = broker_stop.send(());
    if !matches!(broker.await, Ok(Ok(()))) {
        return TerminalResult::KnownFailure {
            error_kind: ExecutionErrorKind::ExecutionFailed,
            detail: None,
        };
    }
    let result = match result {
        Ok(result) => result,
        Err(_) => {
            return TerminalResult::KnownFailure {
                error_kind: ExecutionErrorKind::InvalidArguments,
                detail: None,
            };
        }
    };
    match serde_json::to_string(&result) {
        Ok(text) if text.len() <= signalbox_runner_wire::SUCCESS_TEXT_BYTES as usize => {
            TerminalResult::Success { text }
        }
        Ok(_) => TerminalResult::KnownFailure {
            error_kind: ExecutionErrorKind::ResultTooLarge,
            detail: None,
        },
        Err(_) => TerminalResult::KnownFailure {
            error_kind: ExecutionErrorKind::ExecutionFailed,
            detail: None,
        },
    }
}

async fn shutdown_with_timeout(
    connection: &mut RunnerConnection<tokio::net::UnixStream>,
) -> Result<(), RunnerDaemonError> {
    match tokio::time::timeout(SHUTDOWN_WRITE_TIMEOUT, connection.shutdown()).await {
        Ok(result) => {
            let end = result.map_err(RunnerDaemonError::Connection)?;
            report_graceful_shutdown(connection, end);
            Ok(())
        }
        Err(_) => Err(RunnerDaemonError::ShutdownTimeout),
    }
}

fn report_established(connection: &RunnerConnection<tokio::net::UnixStream>) {
    let receipt = connection.receipt();
    match connection.outcome() {
        outcome @ EnrollmentOutcome::Enrolled => {
            eprintln!(
                "signalbox-runner: info: runner enrolled enrollment_id={} runner_id={} registration_revision={} connection_epoch={} enrollment_outcome={outcome:?}",
                receipt.enrollment_id(),
                receipt.runner_id(),
                receipt.registration_revision().get(),
                connection.connection_epoch().get(),
            );
        }
        outcome @ EnrollmentOutcome::ReplacementPending => {
            eprintln!(
                "signalbox-runner: info: runner replacement pending enrollment_id={} runner_id={} registration_revision={} connection_epoch={} enrollment_outcome={outcome:?}",
                receipt.enrollment_id(),
                receipt.runner_id(),
                receipt.registration_revision().get(),
                connection.connection_epoch().get(),
            );
        }
        outcome @ EnrollmentOutcome::Resumed => {
            eprintln!(
                "signalbox-runner: info: runner resumed enrollment_id={} runner_id={} registration_revision={} connection_epoch={} enrollment_outcome={outcome:?}",
                receipt.enrollment_id(),
                receipt.runner_id(),
                receipt.registration_revision().get(),
                connection.connection_epoch().get(),
            );
        }
    }
}

fn report_graceful_shutdown(
    connection: &RunnerConnection<tokio::net::UnixStream>,
    end: ConnectionEnd,
) {
    match end {
        ConnectionEnd::DaemonShutdown { .. } => {
            eprintln!(
                "signalbox-runner: info: runner graceful shutdown observed enrollment_id={} runner_id={} connection_end={end:?}",
                connection.receipt().enrollment_id(),
                connection.receipt().runner_id(),
            );
        }
        ConnectionEnd::RunnerShutdown { .. } => {
            eprintln!(
                "signalbox-runner: info: runner graceful shutdown sent enrollment_id={} runner_id={} connection_end={end:?}",
                connection.receipt().enrollment_id(),
                connection.receipt().runner_id(),
            );
        }
        ConnectionEnd::StaleConnectionRejected { .. } => {}
    }
}

async fn wait_for_retry(
    delay: Duration,
    terminate: &mut tokio::signal::unix::Signal,
    interrupt: &mut tokio::signal::unix::Signal,
) -> bool {
    tokio::select! {
        _ = tokio::time::sleep(delay) => false,
        _ = terminate.recv() => true,
        _ = interrupt.recv() => true,
    }
}

fn report_reconnect(stage: ReconnectStage, error: &dyn Error, delay: Duration) {
    eprintln!(
        "signalbox-runner: {stage} failed; retrying in {} seconds: {error}",
        delay.as_secs()
    );
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ReconnectStage {
    Socket,
    Establishment,
    Serving,
}

impl fmt::Display for ReconnectStage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Socket => "socket connection",
            Self::Establishment => "protocol establishment",
            Self::Serving => "established connection",
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ReconnectBackoff {
    delay: Duration,
}

impl ReconnectBackoff {
    const fn new() -> Self {
        Self {
            delay: INITIAL_RECONNECT_DELAY,
        }
    }

    fn next_delay(&mut self) -> Duration {
        let current = self.delay;
        self.delay = cmp::min(self.delay.saturating_mul(2), MAXIMUM_RECONNECT_DELAY);
        current
    }

    fn reset(&mut self) {
        self.delay = INITIAL_RECONNECT_DELAY;
    }
}

#[derive(Debug)]
enum RunnerDaemonError {
    Argument(ArgumentError),
    Configuration(RunnerConfigurationError),
    ExecutionPrograms,
    State(RunnerStateError),
    Socket(SocketConnectError),
    Connection(RunnerConnectionError),
    Signal(io::Error),
    ShutdownTimeout,
    StaleConnectionRejected,
}

impl fmt::Display for RunnerDaemonError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Argument(_) => "runner arguments are invalid",
            Self::Configuration(_) => "runner configuration is invalid",
            Self::ExecutionPrograms => "runner execution programs are unavailable",
            Self::State(_) => "runner durable state is unavailable",
            Self::Socket(_) => "runner socket is unavailable",
            Self::Connection(_) => "runner connection failed",
            Self::Signal(_) => "runner signal listener failed",
            Self::ShutdownTimeout => "runner shutdown write timed out",
            Self::StaleConnectionRejected => "runner connection was rejected as stale",
        })
    }
}

impl Error for RunnerDaemonError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Argument(error) => Some(error),
            Self::Configuration(error) => Some(error),
            Self::State(error) => Some(error),
            Self::Socket(error) => Some(error),
            Self::Connection(error) => Some(error),
            Self::Signal(error) => Some(error),
            Self::ExecutionPrograms | Self::ShutdownTimeout | Self::StaleConnectionRejected => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use signalbox_runner::{EnrollmentAuthority, EnrollmentReceipt, PrivateWorkspaceRequest};
    use signalbox_runner_wire::{
        Advertisement, CanonicalUuid, PositiveU64, ReleaseCorrelation, SandboxProfile,
        advertisement_digest,
    };
    use tempfile::TempDir;
    use uuid::Uuid;

    use super::*;

    const SESSION: u128 = 0x018f_6f10_0000_7000_8000_0000_0000_00f1;
    const RUNNER: u128 = 0x018f_6f10_0000_7000_8000_0000_0000_00f2;
    const ENROLLMENT: u128 = 0x018f_6f10_0000_7000_8000_0000_0000_00f3;
    const AUTHENTICATION: u128 = 0x018f_6f10_0000_7000_8000_0000_0000_00f4;
    const PLACEMENT_REVISION: u64 = 3;

    struct PrivateReleaseFixture {
        _parent: TempDir,
        state: RunnerStateRoot,
        accepted: AcceptedWorkspaceRelease,
        placement: std::path::PathBuf,
    }

    fn private_release_fixture() -> PrivateReleaseFixture {
        let parent = tempfile::tempdir().expect("the release fixture parent exists");
        let runner_root = parent.path().join("runner-state");
        let mut state =
            RunnerStateRoot::open(&runner_root).expect("the runner-private state root opens");
        let advertisement = Advertisement {
            capability_classes: Vec::new(),
            tools: Vec::new(),
            workspace_capabilities: Vec::new(),
            sandbox_profiles: Vec::new(),
            credential_profiles: Vec::new(),
            repositories: Vec::new(),
        };
        let runner = CanonicalUuid::from_uuid(Uuid::from_u128(RUNNER));
        state
            .record_receipt(EnrollmentReceipt::new(
                state.state().request_id(),
                CanonicalUuid::from_uuid(Uuid::from_u128(ENROLLMENT)),
                runner,
                CanonicalUuid::from_uuid(Uuid::from_u128(AUTHENTICATION)),
                PositiveU64::try_new(1).expect("the fixture registration revision is positive"),
                advertisement_digest(&advertisement)
                    .expect("the explicit empty advertisement has a digest"),
                EnrollmentAuthority::Active,
            ))
            .expect("the fixture enrollment receipt is durable");
        let request = PrivateWorkspaceRequest::new(
            CanonicalUuid::from_uuid(Uuid::from_u128(SESSION)),
            PositiveU64::try_new(PLACEMENT_REVISION)
                .expect("the fixture placement revision is positive"),
            runner,
            SandboxProfile::WorkspaceRestricted,
        );
        let prepared = state
            .workspace_store()
            .expect("the locked root forms a workspace store")
            .prepare_private_root(&request)
            .expect("the private workspace publishes");
        let placement = runner_root
            .join("sessions")
            .join(request.session().to_string())
            .join(request.placement_revision().get().to_string());
        let accepted = state
            .accept_workspace_release(ReleaseCorrelation {
                session_id: prepared.manifest.session,
                placement_revision: prepared.manifest.placement_revision,
                runner_id: prepared.manifest.runner,
                manifest_id: prepared.manifest.manifest_id,
            })
            .expect("the exact private release is journaled");
        PrivateReleaseFixture {
            _parent: parent,
            state,
            accepted,
            placement,
        }
    }

    #[test]
    fn reconnect_backoff_caps_and_resets() {
        let mut backoff = ReconnectBackoff::new();

        assert_eq!(backoff.next_delay(), Duration::from_secs(1));
        assert_eq!(backoff.next_delay(), Duration::from_secs(2));
        assert_eq!(backoff.next_delay(), Duration::from_secs(4));
        assert_eq!(backoff.next_delay(), Duration::from_secs(8));
        assert_eq!(backoff.next_delay(), Duration::from_secs(16));
        assert_eq!(backoff.next_delay(), MAXIMUM_RECONNECT_DELAY);
        assert_eq!(backoff.next_delay(), MAXIMUM_RECONNECT_DELAY);
        backoff.reset();
        assert_eq!(backoff.next_delay(), INITIAL_RECONNECT_DELAY);
    }

    #[tokio::test]
    async fn accepted_private_release_runs_through_the_blocking_cleanup_adapter() {
        let fixture = private_release_fixture();

        release_private_workspace(&fixture.state, &fixture.accepted)
            .await
            .expect("the accepted private workspace cleanup completes");

        assert!(!fixture.placement.exists());
    }
}
