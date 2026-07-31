use std::{env, error::Error, ffi::OsString, fmt, io, process::ExitCode, time::Duration};

use signalbox_runner::{
    ArgumentError, RunnerConfiguration, RunnerConfigurationError, RunnerConfigurationPath,
    RunnerConnection, RunnerConnectionError, RunnerStateError, RunnerStateRoot, SocketConnectError,
    connect_verified,
};

const CONFIGURATION_ENVIRONMENT: &str = "SIGNALBOX_RUNNER_CONFIG_FILE";
const RECONNECT_DELAY: Duration = Duration::from_secs(1);

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
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .map_err(RunnerDaemonError::Signal)?;
    let mut interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
        .map_err(RunnerDaemonError::Signal)?;
    loop {
        let stream = match tokio::select! {
            connected = connect_verified(configuration.daemon_socket_path()) => connected,
            _ = terminate.recv() => return Ok(()),
            _ = interrupt.recv() => return Ok(()),
        } {
            Ok(stream) => stream,
            Err(error) if error.is_reconnectable() => {
                tokio::time::sleep(RECONNECT_DELAY).await;
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
                tokio::time::sleep(RECONNECT_DELAY).await;
                continue;
            }
            Err(error) => return Err(RunnerDaemonError::Connection(error)),
        };
        let served = tokio::select! {
            served = connection.serve(&mut state) => served,
            _ = terminate.recv() => return connection.shutdown().await.map(|_| ()).map_err(RunnerDaemonError::Connection),
            _ = interrupt.recv() => return connection.shutdown().await.map(|_| ()).map_err(RunnerDaemonError::Connection),
        };
        match served {
            Ok(_) => return Ok(()),
            Err(error) if error.is_reconnectable() => {
                tokio::time::sleep(RECONNECT_DELAY).await;
            }
            Err(error) => return Err(RunnerDaemonError::Connection(error)),
        }
    }
}

#[derive(Debug)]
enum RunnerDaemonError {
    Argument(ArgumentError),
    Configuration(RunnerConfigurationError),
    State(RunnerStateError),
    Socket(SocketConnectError),
    Connection(RunnerConnectionError),
    Signal(io::Error),
}

impl fmt::Display for RunnerDaemonError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Argument(_) => "runner arguments are invalid",
            Self::Configuration(_) => "runner configuration is invalid",
            Self::State(_) => "runner durable state is unavailable",
            Self::Socket(_) => "runner socket is unavailable",
            Self::Connection(_) => "runner connection failed",
            Self::Signal(_) => "runner signal listener failed",
        })
    }
}

impl Error for RunnerDaemonError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(match self {
            Self::Argument(error) => error,
            Self::Configuration(error) => error,
            Self::State(error) => error,
            Self::Socket(error) => error,
            Self::Connection(error) => error,
            Self::Signal(error) => error,
        })
    }
}
