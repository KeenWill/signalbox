//! The compiled pure echo tool executed in one ambient child process.

use std::{
    io::{self, Read as _, Write as _},
    process::Stdio,
};

use signalbox_domain::{
    NormalizedToolArguments, ToolAttemptEnd, ToolExecutionError, ToolExecutionErrorKind,
    ToolResultContent, ToolResultText,
};
use signalbox_runner_wire::{Dispatch, MAX_FRAME_BYTES};
use signalbox_tools_basic::EchoExecutor;
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    process::Command,
};

/// Internal child mode invoking only the compiled pure echo implementation.
pub const ECHO_CHILD_ARGUMENT: &str = "--execute-echo";

/// Runs the shared pure implementation with bounded stdin and exact stdout.
pub fn run_echo_child() -> io::Result<()> {
    let mut input = String::new();
    io::stdin()
        .take(MAX_FRAME_BYTES as u64 + 1)
        .read_to_string(&mut input)?;
    if input.len() > MAX_FRAME_BYTES {
        return Err(io::ErrorKind::InvalidData.into());
    }
    let arguments = NormalizedToolArguments::try_from_provider_text(input)
        .map_err(|_| io::Error::from(io::ErrorKind::InvalidData))?;
    let text = EchoExecutor::evaluate(&arguments)
        .map_err(|_| io::Error::from(io::ErrorKind::InvalidData))?;
    io::stdout().write_all(text.as_bytes())
}

pub(crate) async fn execute(dispatch: Dispatch) -> ToolAttemptEnd {
    match execute_child(dispatch).await {
        Ok(text) => match ToolResultText::try_new(text) {
            Ok(text) => ToolAttemptEnd::Completed {
                result: ToolResultContent::Text(text),
            },
            Err(error) => failure(match error.failure() {
                signalbox_domain::ToolResultTextFailure::TooLarge { .. } => {
                    ToolExecutionErrorKind::ResultTooLarge
                }
                signalbox_domain::ToolResultTextFailure::ContainsNull => {
                    ToolExecutionErrorKind::ResultContainsNull
                }
            }),
        },
        Err(_) => failure(ToolExecutionErrorKind::ExecutionFailed),
    }
}

fn failure(kind: ToolExecutionErrorKind) -> ToolAttemptEnd {
    ToolAttemptEnd::KnownFailed {
        error: ToolExecutionError::new(kind, None),
    }
}

async fn execute_child(dispatch: Dispatch) -> io::Result<String> {
    let mut child = Command::new(std::env::current_exe()?)
        .arg(ECHO_CHILD_ARGUMENT)
        .env_clear()
        .current_dir(dispatch.correlation.working_directory.as_str())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    let mut stdin = child.stdin.take().ok_or(io::ErrorKind::BrokenPipe)?;
    let stdout = child.stdout.take().ok_or(io::ErrorKind::BrokenPipe)?;
    let arguments = serde_json::to_vec(&dispatch.normalized_arguments)?;
    let write = async move {
        stdin.write_all(&arguments).await?;
        stdin.shutdown().await?;
        drop(stdin);
        Ok::<_, io::Error>(())
    };
    let read = async {
        let mut text = String::new();
        stdout
            .take(ToolResultText::MAX_UTF8_BYTES as u64 + 1)
            .read_to_string(&mut text)
            .await?;
        Ok::<_, io::Error>(text)
    };
    let ((), text) = tokio::try_join!(write, read)?;
    if text.len() > ToolResultText::MAX_UTF8_BYTES {
        child.kill().await?;
        return Ok(text);
    }
    if !child.wait().await?.success() {
        return Err(io::ErrorKind::Other.into());
    }
    Ok(text)
}
