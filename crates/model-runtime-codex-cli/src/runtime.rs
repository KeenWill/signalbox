//! One operation, one Codex CLI process spawn.

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::process::Stdio;
use std::task::Context;
use std::time::Duration;

use signalbox_model_runtime::{
    CancellationSignal, DeliveryMode, LossCause, ModelOperation, ModelRuntime, Observation,
    ObservationFact, ObservationSink, PreparationDefect, PreparationFailure, PreparationOutcome,
    ProvenUnsentEvidence, TerminalEvidence, TerminalReport, TransportFacts, UnsentCause,
};
use tempfile::NamedTempFile;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};

use crate::config::CodexCliConfig;
use crate::event::EventDecoder;
use crate::translate::{TranslationError, translate};
use crate::wire::OUTPUT_SCHEMA;

/// Codex CLI protocol snapshot covered by this adapter's offline fixtures.
///
/// Composition must select this executable version before wiring the adapter;
/// the runtime does not add a version-probe process to a model dispatch.
pub const SUPPORTED_CODEX_CLI_VERSION: &str = "0.145.0";

/// Stateless subscription-backed Codex CLI adapter.
pub struct CodexCliRuntime {
    executable: PathBuf,
    working_directory: PathBuf,
    credential_reference: signalbox_model_runtime::CredentialReference,
    exchange_timeout: Duration,
    interrupt_grace: Duration,
    event_limit: usize,
    stderr_limit: usize,
}

/// Opaque one-shot capability for one Codex CLI spawn.
///
/// It owns the rendered full context and the temporary output-schema file.
/// It deliberately implements neither `Clone`, serialization, nor diagnostic
/// formatting.
#[must_use]
pub struct CodexCliPreparedRequest<C> {
    executable: PathBuf,
    working_directory: PathBuf,
    prompt: Vec<u8>,
    output_schema: NamedTempFile,
    correlation: C,
    resolved_target: String,
    delivery: DeliveryMode,
    translated: crate::translate::TranslatedOperation,
    exchange_timeout: Duration,
    interrupt_grace: Duration,
    event_limit: usize,
    stderr_limit: usize,
}

/// Why a [`CodexCliRuntime`] could not be constructed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodexCliConstructionError {
    /// No executable path was configured.
    EmptyExecutable,
    /// The working directory does not exist or is not a directory.
    InvalidWorkingDirectory,
    /// Whole-process timeout is zero.
    InvalidExchangeTimeout,
    /// Interrupt grace is zero.
    InvalidInterruptGrace,
    /// One of the process-output evidence bounds is zero.
    InvalidOutputLimit,
}

impl std::fmt::Display for CodexCliConstructionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyExecutable => formatter.write_str("Codex executable path is empty"),
            Self::InvalidWorkingDirectory => {
                formatter.write_str("Codex working directory is not an existing directory")
            }
            Self::InvalidExchangeTimeout => {
                formatter.write_str("exchange timeout must be greater than zero")
            }
            Self::InvalidInterruptGrace => {
                formatter.write_str("interrupt grace must be greater than zero")
            }
            Self::InvalidOutputLimit => {
                formatter.write_str("event and stderr limits must be greater than zero")
            }
        }
    }
}

impl std::error::Error for CodexCliConstructionError {}

impl std::fmt::Debug for CodexCliRuntime {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CodexCliRuntime")
            .field("executable", &self.executable)
            .field("working_directory", &self.working_directory)
            .field("credential_reference", &self.credential_reference)
            .field("exchange_timeout", &self.exchange_timeout)
            .field("interrupt_grace", &self.interrupt_grace)
            .field("event_limit", &self.event_limit)
            .field("stderr_limit", &self.stderr_limit)
            .finish()
    }
}

impl CodexCliRuntime {
    /// Validates adapter configuration without invoking Codex or inspecting
    /// its login store.
    pub fn new(config: CodexCliConfig) -> Result<Self, CodexCliConstructionError> {
        if config.executable.as_os_str().is_empty() {
            return Err(CodexCliConstructionError::EmptyExecutable);
        }
        if !config.working_directory.is_dir() {
            return Err(CodexCliConstructionError::InvalidWorkingDirectory);
        }
        if config.exchange_timeout.is_zero() {
            return Err(CodexCliConstructionError::InvalidExchangeTimeout);
        }
        if config.interrupt_grace.is_zero() {
            return Err(CodexCliConstructionError::InvalidInterruptGrace);
        }
        if config.event_limit == 0 || config.stderr_limit == 0 {
            return Err(CodexCliConstructionError::InvalidOutputLimit);
        }
        Ok(Self {
            executable: config.executable,
            working_directory: config.working_directory,
            credential_reference: config.credential_reference,
            exchange_timeout: config.exchange_timeout,
            interrupt_grace: config.interrupt_grace,
            event_limit: config.event_limit,
            stderr_limit: config.stderr_limit,
        })
    }

    fn prepare_request<C>(
        &self,
        operation: ModelOperation<C>,
    ) -> PreparationOutcome<C, CodexCliPreparedRequest<C>> {
        let correlation = operation.correlation;
        if operation.credential_reference != self.credential_reference {
            return PreparationOutcome::Failed {
                correlation,
                failure: PreparationFailure::CredentialUnavailable {
                    error: signalbox_model_runtime::CredentialAccessError::new(
                        operation.credential_reference,
                        signalbox_model_runtime::CredentialAccessFailure::Unmapped,
                    ),
                },
            };
        }
        let operation = ModelOperation {
            correlation: (),
            credential_reference: operation.credential_reference,
            requested_target: operation.requested_target,
            resolved_target: operation.resolved_target,
            system: operation.system,
            messages: operation.messages,
            settings: operation.settings,
            tools: operation.tools,
            tool_choice: operation.tool_choice,
            output_contract: operation.output_contract,
            delivery: operation.delivery,
        };
        let translated = match translate(&operation) {
            Ok(translated) => translated,
            Err(TranslationError::Failure(failure)) => {
                return PreparationOutcome::Failed {
                    correlation,
                    failure,
                };
            }
            Err(TranslationError::Defect(defect)) => {
                return PreparationOutcome::Defect {
                    correlation,
                    defect,
                };
            }
        };
        let mut output_schema = match tempfile::Builder::new()
            .prefix("signalbox-codex-output-")
            .suffix(".json")
            .tempfile()
        {
            Ok(file) => file,
            Err(error) => {
                return PreparationOutcome::Defect {
                    correlation,
                    defect: PreparationDefect::RequestConstructionFailed {
                        detail: format!("could not create output-schema file: {error}"),
                    },
                };
            }
        };
        if let Err(error) = std::io::Write::write_all(&mut output_schema, OUTPUT_SCHEMA.as_bytes())
        {
            return PreparationOutcome::Defect {
                correlation,
                defect: PreparationDefect::RequestConstructionFailed {
                    detail: format!("could not write output-schema file: {error}"),
                },
            };
        }
        let prompt = translated.prompt.clone();
        PreparationOutcome::Prepared(CodexCliPreparedRequest {
            executable: self.executable.clone(),
            working_directory: self.working_directory.clone(),
            prompt,
            output_schema,
            correlation,
            resolved_target: operation.resolved_target.as_str().to_string(),
            delivery: operation.delivery,
            translated,
            exchange_timeout: self.exchange_timeout,
            interrupt_grace: self.interrupt_grace,
            event_limit: self.event_limit,
            stderr_limit: self.stderr_limit,
        })
    }
}

impl<C: Clone + Send + Sync> ModelRuntime<C> for CodexCliRuntime {
    type Prepared = CodexCliPreparedRequest<C>;

    async fn prepare(
        &self,
        operation: ModelOperation<C>,
        mut cancellation: CancellationSignal,
    ) -> PreparationOutcome<C, Self::Prepared> {
        if already_fired(&mut cancellation) {
            return PreparationOutcome::Cancelled {
                correlation: operation.correlation,
            };
        }
        self.prepare_request(operation)
    }

    async fn execute(
        &self,
        prepared: Self::Prepared,
        sink: &mut (dyn ObservationSink<C> + Send),
        mut cancellation: CancellationSignal,
    ) -> TerminalReport<C> {
        let correlation = prepared.correlation.clone();
        if already_fired(&mut cancellation) {
            return TerminalReport {
                correlation,
                evidence: TerminalEvidence::ProvenUnsent(ProvenUnsentEvidence {
                    cause: UnsentCause::CancelledBeforeSend,
                }),
            };
        }
        let evidence = execute_process(prepared, sink, &mut cancellation).await;
        TerminalReport {
            correlation,
            evidence,
        }
    }
}

async fn execute_process<C: Clone + Send + Sync>(
    prepared: CodexCliPreparedRequest<C>,
    sink: &mut (dyn ObservationSink<C> + Send),
    cancellation: &mut CancellationSignal,
) -> TerminalEvidence {
    let mut command = Command::new(&prepared.executable);
    command
        .arg("exec")
        .arg("--json")
        .arg("--ephemeral")
        .arg("--ignore-user-config")
        .arg("--ignore-rules")
        .arg("--strict-config")
        .arg("--sandbox")
        .arg("read-only")
        .arg("--skip-git-repo-check")
        .arg("--cd")
        .arg(&prepared.working_directory)
        .arg("--model")
        .arg(&prepared.resolved_target)
        .arg("--output-schema")
        .arg(prepared.output_schema.path())
        .arg("-")
        .current_dir(&prepared.working_directory)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);

    sink.observe(Observation {
        correlation: prepared.correlation.clone(),
        fact: ObservationFact::SendCommenced,
    });
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            return TerminalEvidence::ProvenUnsent(ProvenUnsentEvidence {
                cause: UnsentCause::ConnectFailed(TransportFacts::new(error.to_string())),
            });
        }
    };
    let deadline = tokio::time::Instant::now() + prepared.exchange_timeout;
    let Some(mut stdin) = child.stdin.take() else {
        let _ = child.start_kill();
        let _ = child.wait().await;
        return pre_exchange_transport_loss("spawned Codex process has no stdin");
    };
    let Some(stdout) = child.stdout.take() else {
        let _ = child.start_kill();
        let _ = child.wait().await;
        return pre_exchange_transport_loss("spawned Codex process has no stdout");
    };
    let Some(stderr) = child.stderr.take() else {
        let _ = child.start_kill();
        let _ = child.wait().await;
        return pre_exchange_transport_loss("spawned Codex process has no stderr");
    };
    let stderr_limit = prepared.stderr_limit;
    let stderr_task = tokio::spawn(async move { read_bounded_output(stderr, stderr_limit).await });
    let input_step = {
        let send_prompt = async {
            stdin.write_all(&prepared.prompt).await?;
            stdin.shutdown().await
        };
        tokio::pin!(send_prompt);
        tokio::select! {
            biased;
            () = &mut *cancellation => InputStep::Cancelled,
            () = tokio::time::sleep_until(deadline) => InputStep::TimedOut,
            result = &mut send_prompt => InputStep::Written(result),
        }
    };
    match input_step {
        InputStep::Written(Ok(())) => {}
        InputStep::Written(Err(error)) => {
            force_kill(&mut child).await;
            let _ = stderr_task.await;
            return pre_exchange_transport_loss(format!("Codex stdin write failed: {error}"));
        }
        InputStep::Cancelled => {
            interrupt_then_kill(&mut child, prepared.interrupt_grace).await;
            let _ = stderr_task.await;
            return pre_exchange_boundary_loss(LossCause::CancellationRequested);
        }
        InputStep::TimedOut => {
            force_kill(&mut child).await;
            let _ = stderr_task.await;
            return pre_exchange_boundary_loss(LossCause::TimedOut(TransportFacts::new(
                "Codex CLI process exceeded its exchange timeout",
            )));
        }
    }
    drop(stdin);

    let mut stdout = BufReader::new(stdout);
    let mut decoder = EventDecoder::new(
        prepared.correlation,
        prepared.delivery,
        &prepared.translated,
    );

    loop {
        let next = tokio::select! {
            biased;
            () = &mut *cancellation => ProcessStep::Cancelled,
            () = tokio::time::sleep_until(deadline) => ProcessStep::TimedOut,
            result = read_bounded_line(&mut stdout, prepared.event_limit) => ProcessStep::Line(result),
        };
        match next {
            ProcessStep::Line(Ok(Some(line))) => {
                if let Err(error) = decoder.push(&line, sink) {
                    let detail = format!("undecodable Codex event: {}", error.into_detail());
                    force_kill(&mut child).await;
                    let _ = stderr_task.await;
                    return decoder.provider_error(&detail);
                }
            }
            ProcessStep::Line(Ok(None)) => break,
            ProcessStep::Line(Err(error)) => {
                force_kill(&mut child).await;
                let _ = stderr_task.await;
                return decoder.boundary_loss(LossCause::StreamProtocolViolation {
                    detail: error.to_string(),
                });
            }
            ProcessStep::Cancelled => {
                interrupt_then_kill(&mut child, prepared.interrupt_grace).await;
                let _ = stderr_task.await;
                return decoder.boundary_loss(LossCause::CancellationRequested);
            }
            ProcessStep::TimedOut => {
                force_kill(&mut child).await;
                let _ = stderr_task.await;
                return decoder.boundary_loss(LossCause::TimedOut(TransportFacts::new(
                    "Codex CLI process exceeded its exchange timeout",
                )));
            }
        }
    }

    let status = tokio::select! {
        biased;
        status = child.wait() => Some(status),
        () = &mut *cancellation => {
            interrupt_then_kill(&mut child, prepared.interrupt_grace).await;
            None
        },
        () = tokio::time::sleep_until(deadline) => {
            force_kill(&mut child).await;
            let _ = stderr_task.await;
            return decoder.boundary_loss(LossCause::TimedOut(TransportFacts::new(
                "Codex CLI process exceeded its exchange timeout",
            )));
        },
    };
    let stderr = match stderr_task.await {
        Ok(Ok(stderr)) => stderr,
        Ok(Err(error)) => format!("could not read Codex stderr: {error}"),
        Err(error) => format!("Codex stderr reader failed: {error}"),
    };
    let Some(status) = status else {
        return decoder.boundary_loss(LossCause::CancellationRequested);
    };
    match status {
        Ok(status) if status.success() => decoder.finish(sink),
        Ok(status) => {
            let message = if stderr.trim().is_empty() {
                format!("Codex CLI exited with status {status}")
            } else {
                format!("Codex CLI exited with status {status}: {stderr}")
            };
            decoder.provider_error_after_exit(&message)
        }
        Err(error) => decoder.boundary_loss(LossCause::TransportFailed(TransportFacts::new(
            format!("could not wait for Codex CLI process: {error}"),
        ))),
    }
}

enum ProcessStep {
    Line(std::io::Result<Option<Vec<u8>>>),
    Cancelled,
    TimedOut,
}

enum InputStep {
    Written(std::io::Result<()>),
    Cancelled,
    TimedOut,
}

async fn read_bounded_line<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    limit: usize,
) -> std::io::Result<Option<Vec<u8>>> {
    let mut line = Vec::new();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return if line.is_empty() {
                Ok(None)
            } else {
                Ok(Some(line))
            };
        }
        let newline = available.iter().position(|byte| *byte == b'\n');
        let take = newline.map_or(available.len(), |index| index + 1);
        if line.len().saturating_add(take) > limit {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("Codex JSONL event exceeded the {limit}-byte limit"),
            ));
        }
        line.extend_from_slice(&available[..take]);
        reader.consume(take);
        if newline.is_some() {
            while matches!(line.last(), Some(b'\n' | b'\r')) {
                line.pop();
            }
            return Ok(Some(line));
        }
    }
}

async fn read_bounded_output<R: AsyncRead + Unpin>(
    mut reader: R,
    limit: usize,
) -> std::io::Result<String> {
    let mut retained = Vec::with_capacity(limit.min(8192));
    let mut buffer = [0_u8; 8192];
    let mut truncated = false;
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        let admitted = (limit - retained.len()).min(read);
        retained.extend_from_slice(&buffer[..admitted]);
        truncated |= admitted < read;
    }
    let mut output = String::from_utf8_lossy(&retained).into_owned();
    if truncated {
        output.push_str("… [truncated]");
    }
    Ok(crate::redaction::redact_text(&output))
}

async fn interrupt_then_kill(child: &mut Child, grace: Duration) {
    #[cfg(unix)]
    {
        if let Some(raw_pid) = child.id()
            && let Some(pid) = rustix::process::Pid::from_raw(raw_pid as i32)
        {
            let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::INT);
            if matches!(tokio::time::timeout(grace, child.wait()).await, Ok(Ok(_))) {
                return;
            }
        }
    }
    force_kill(child).await;
}

async fn force_kill(child: &mut Child) {
    let _ = child.start_kill();
    let _ = child.wait().await;
}

fn pre_exchange_transport_loss(detail: impl Into<String>) -> TerminalEvidence {
    pre_exchange_boundary_loss(LossCause::TransportFailed(TransportFacts::new(detail)))
}

fn pre_exchange_boundary_loss(cause: LossCause) -> TerminalEvidence {
    TerminalEvidence::BoundaryLoss(signalbox_model_runtime::BoundaryLossEvidence {
        cause,
        exchange: signalbox_model_runtime::ExchangeFacts::default(),
        reported_model: None,
        finish_reported: None,
        usage: signalbox_model_runtime::TokenUsage::unreported(),
    })
}

fn already_fired(signal: &mut CancellationSignal) -> bool {
    let waker = std::task::Waker::noop();
    let mut context = Context::from_waker(waker);
    Pin::new(signal).poll(&mut context).is_ready()
}

#[cfg(test)]
mod tests {
    use super::read_bounded_line;

    #[tokio::test]
    async fn bounded_line_rejects_an_unterminated_oversize_event() {
        let input = b"12345".as_slice();
        let mut reader = tokio::io::BufReader::new(input);

        let error = read_bounded_line(&mut reader, 4)
            .await
            .expect_err("the fifth byte exceeds the configured event bound");

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }
}
