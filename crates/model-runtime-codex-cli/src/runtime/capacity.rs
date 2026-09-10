//! Read-only capacity observations outside a model invocation.
use super::{
    CODEX_ENVIRONMENT, CodexCliRuntime, DISABLED_CODEX_CLI_CAPABILITY_FEATURES, OperationHome,
    VersionProbeProcessGroup, operation_home,
};
use crate::app_server::frame::{AccountRateLimitsUpdated, RateLimits, RpcError};
use serde_json::{Value, json};
use signalbox_model_runtime::{CredentialReference, RateLimitSnapshot};
use std::{
    process::Stdio,
    time::{Duration, SystemTime},
};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

/// Why a read-only Codex capacity observation produced no usable evidence.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CodexCliCapacityProbeError {
    /// The reference does not name an explicitly configured Codex home.
    UnsupportedCredential,
    /// The process or protocol did not provide a valid capacity response.
    Failed,
    /// The observation and process cleanup exceeded their shared bound.
    TimedOut,
}

impl std::fmt::Display for CodexCliCapacityProbeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::UnsupportedCredential => "capacity probe requires a configured Codex home",
            Self::Failed => "Codex capacity probe failed",
            Self::TimedOut => "Codex capacity probe timed out",
        })
    }
}
impl std::error::Error for CodexCliCapacityProbeError {}

impl CodexCliRuntime {
    /// Reads account capacity using the named subscription home without starting
    /// a thread or model turn. Failure leaves previously observed capacity intact.
    /// The process group is killed on completion, timeout, or cancellation.
    pub async fn read_credential_capacity(
        &self,
        credential: &CredentialReference,
        bound: Duration,
    ) -> Result<RateLimitSnapshot, CodexCliCapacityProbeError> {
        use CodexCliCapacityProbeError::{Failed, TimedOut, UnsupportedCredential};
        let selected = self
            .credential_homes
            .get(credential)
            .ok_or(UnsupportedCredential)?;
        let deadline = tokio::time::Instant::now()
            .checked_add(bound)
            .ok_or(Failed)?;
        if bound.is_zero() {
            return Err(TimedOut);
        }
        let OperationHome::Ready(home) =
            operation_home(Some(selected.clone())).map_err(|_| Failed)?
        else {
            return Err(Failed);
        };
        let mut command = tokio::process::Command::new(&self.executable);
        for feature in DISABLED_CODEX_CLI_CAPABILITY_FEATURES {
            command.arg("--disable").arg(feature);
        }
        command
            .args([
                "app-server",
                "--stdio",
                "--strict-config",
                "--ignore-user-config",
                "--ignore-rules",
            ])
            .current_dir(&self.working_directory)
            .env_clear()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        for variable in CODEX_ENVIRONMENT {
            if let Some(value) = std::env::var_os(variable.name()) {
                command.env(variable.name(), value);
            }
        }
        command.env(super::CODEX_CREDENTIAL_HOME, home.path());
        #[cfg(unix)]
        command.process_group(0);
        #[cfg(unix)]
        let exits = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::child())
            .map_err(|_| Failed)?;
        let mut child = command.spawn().map_err(|_| Failed)?;
        let mut group = VersionProbeProcessGroup {
            id: child.id(),
            #[cfg(unix)]
            exits,
        };
        let exchange = async {
            let mut input = child.stdin.take().ok_or(Failed)?;
            let mut output = BufReader::new(child.stdout.take().ok_or(Failed)?);
            let observed_at = SystemTime::now();
            write_frame(
                &mut input,
                json!({"id":1,"method":"initialize","params":{
                    "clientInfo":{"name":"signalbox","version":env!("CARGO_PKG_VERSION")}
                }}),
            )
            .await?;
            let (initialized, mut limits) = read_response(&mut output, 1, self.event_limit).await?;
            if !initialized.is_some_and(|result| result.is_object()) {
                return Err(Failed);
            }
            write_frame(&mut input, json!({"method":"initialized"})).await?;
            write_frame(
                &mut input,
                json!({"id":2,"method":"account/rateLimits/read"}),
            )
            .await?;
            let (result, updates) = read_response(&mut output, 2, self.event_limit).await?;
            let response = result
                .map(serde_json::from_value::<AccountRateLimitsUpdated>)
                .transpose()
                .map_err(|_| Failed)?;
            if updates.primary.is_some() || updates.secondary.is_some() {
                limits.merge(updates);
            } else if let Some(response) = response {
                limits.merge(response.rate_limits);
            }
            limits.capacity_snapshot(observed_at).ok_or(Failed)
        };
        let result = tokio::time::timeout_at(deadline, exchange)
            .await
            .map_err(|_| TimedOut)
            .and_then(|result| result);
        // Keep the leader unreaped until the group ID is relinquished.
        group.kill();
        tokio::time::timeout_at(deadline, child.wait())
            .await
            .map_err(|_| TimedOut)?
            .map_err(|_| Failed)?;
        result
    }
}

async fn write_frame(
    input: &mut tokio::process::ChildStdin,
    frame: Value,
) -> Result<(), CodexCliCapacityProbeError> {
    let mut bytes = serde_json::to_vec(&frame).map_err(|_| CodexCliCapacityProbeError::Failed)?;
    bytes.push(b'\n');
    input
        .write_all(&bytes)
        .await
        .map_err(|_| CodexCliCapacityProbeError::Failed)
}

async fn read_response(
    output: &mut BufReader<tokio::process::ChildStdout>,
    id: u64,
    limit: usize,
) -> Result<(Option<Value>, RateLimits), CodexCliCapacityProbeError> {
    use CodexCliCapacityProbeError::Failed;
    let mut updates = RateLimits::default();
    loop {
        let mut bytes = Vec::new();
        (&mut *output)
            .take(limit.saturating_add(1) as u64)
            .read_until(b'\n', &mut bytes)
            .await
            .map_err(|_| Failed)?;
        if bytes.is_empty() || bytes.len() > limit {
            return Err(Failed);
        }
        let frame: Value = serde_json::from_slice(&bytes).map_err(|_| Failed)?;
        if frame.get("method").is_some() {
            if frame.get("id").is_some() {
                return Err(Failed);
            }
            if frame.get("method").and_then(Value::as_str) == Some("account/rateLimits/updated") {
                let notification: AccountRateLimitsUpdated =
                    serde_json::from_value(frame.get("params").cloned().ok_or(Failed)?)
                        .map_err(|_| Failed)?;
                updates.merge(notification.rate_limits);
            }
            continue;
        }
        if frame.get("id").and_then(Value::as_u64) != Some(id)
            || frame.get("error").is_some() == frame.get("result").is_some()
        {
            return Err(Failed);
        }
        if let Some(error) = frame.get("error") {
            let _: RpcError = serde_json::from_value(error.clone()).map_err(|_| Failed)?;
        }
        return Ok((frame.get("result").cloned(), updates));
    }
}
