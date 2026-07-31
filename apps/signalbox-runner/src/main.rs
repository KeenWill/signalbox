use std::{env, ffi::OsString, process::ExitCode};

use signalbox_runner::{
    RunnerConfiguration, RunnerConfigurationPath, RunnerConnection, RunnerStateRoot,
    connect_verified,
};

const CONFIGURATION_ENVIRONMENT: &str = "SIGNALBOX_RUNNER_CONFIG_FILE";

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
            let mut source = error.as_ref().source();
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
) -> Result<(), Box<dyn std::error::Error>> {
    let path = RunnerConfigurationPath::resolve(arguments, environment)?;
    let configuration = RunnerConfiguration::read(path.as_path())?;
    let mut state = RunnerStateRoot::open(configuration.runner_root())?;
    let stream = connect_verified(configuration.daemon_socket_path()).await?;
    let mut connection =
        RunnerConnection::establish(stream, &mut state, configuration.advertisement()).await?;
    let _ = connection.serve(&mut state).await?;
    Ok(())
}
