//! One operation, one Codex CLI process spawn.

use std::future::Future;
use std::ops::{Deref, DerefMut};
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
use crate::redaction::RedactingSink;
use crate::translate::{TranslationError, translate};
use crate::wire::OUTPUT_SCHEMA;

const CODEX_ENVIRONMENT_ALLOWLIST: &[&str] = &[
    "ALL_PROXY",
    "CODEX_HOME",
    "COLORTERM",
    "HOME",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "NO_PROXY",
    "PATH",
    "SSL_CERT_DIR",
    "SSL_CERT_FILE",
    "TEMP",
    "TERM",
    "TMP",
    "TMPDIR",
    "XDG_CACHE_HOME",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "all_proxy",
    "http_proxy",
    "https_proxy",
    "no_proxy",
];

const PROCESS_GROUP_SUPERVISION_SUPPORTED: bool = cfg!(all(
    unix,
    not(any(
        target_os = "cygwin",
        target_os = "horizon",
        target_os = "openbsd",
        target_os = "redox",
        target_os = "wasi"
    ))
));

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

struct SupervisedChild {
    child: Child,
    process_group_id: Option<u32>,
    armed: bool,
}

impl SupervisedChild {
    fn new(child: Child) -> Self {
        let process_group_id = child.id();
        Self {
            child,
            process_group_id,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Deref for SupervisedChild {
    type Target = Child;

    fn deref(&self) -> &Self::Target {
        &self.child
    }
}

impl DerefMut for SupervisedChild {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.child
    }
}

impl Drop for SupervisedChild {
    fn drop(&mut self) {
        if self.armed {
            kill_process_group(self.process_group_id);
        }
    }
}

/// Why a [`CodexCliRuntime`] could not be constructed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodexCliConstructionError {
    /// Process-tree supervision is unavailable on this host platform.
    UnsupportedPlatform,
    /// No executable path was configured.
    EmptyExecutable,
    /// The executable path is relative and would change meaning under the
    /// configured child working directory.
    RelativeExecutable,
    /// The working directory does not exist or is not a directory.
    InvalidWorkingDirectory,
    /// The working directory is relative and would be resolved twice by the
    /// child process and its `--cd` argument.
    RelativeWorkingDirectory,
    /// Whole-process timeout is zero or cannot be represented by the runtime
    /// clock.
    InvalidExchangeTimeout,
    /// Interrupt grace is zero.
    InvalidInterruptGrace,
    /// One of the process-output evidence bounds is zero.
    InvalidOutputLimit,
}

impl std::fmt::Display for CodexCliConstructionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedPlatform => {
                formatter.write_str("Codex CLI runtime requires Unix process-group supervision")
            }
            Self::EmptyExecutable => formatter.write_str("Codex executable path is empty"),
            Self::RelativeExecutable => {
                formatter.write_str("Codex executable path must be absolute")
            }
            Self::InvalidWorkingDirectory => {
                formatter.write_str("Codex working directory is not an existing directory")
            }
            Self::RelativeWorkingDirectory => {
                formatter.write_str("Codex working directory must be absolute")
            }
            Self::InvalidExchangeTimeout => {
                formatter.write_str("exchange timeout must be positive and representable")
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
        if !PROCESS_GROUP_SUPERVISION_SUPPORTED {
            return Err(CodexCliConstructionError::UnsupportedPlatform);
        }
        if config.executable.as_os_str().is_empty() {
            return Err(CodexCliConstructionError::EmptyExecutable);
        }
        if !config.executable.is_absolute() {
            return Err(CodexCliConstructionError::RelativeExecutable);
        }
        if !config.working_directory.is_absolute() {
            return Err(CodexCliConstructionError::RelativeWorkingDirectory);
        }
        if !config.working_directory.is_dir() {
            return Err(CodexCliConstructionError::InvalidWorkingDirectory);
        }
        if config.exchange_timeout.is_zero()
            || tokio::time::Instant::now()
                .checked_add(config.exchange_timeout)
                .is_none()
        {
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
        let mut translated = match translate(&operation) {
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
        // The child interprets `--output-schema` after `current_dir` moves it
        // to the configured working root, so a schema path that is relative
        // there — as under a relative `TMPDIR` — would name a file that
        // preparation never created. Create the file under the absolutized
        // temporary directory so its retained path cannot be relative.
        let temporary_directory = match std::path::absolute(std::env::temp_dir()) {
            Ok(directory) => directory,
            Err(error) => {
                return PreparationOutcome::Defect {
                    correlation,
                    defect: PreparationDefect::RequestConstructionFailed {
                        detail: format!(
                            "could not absolutize the temporary directory for the \
                             output-schema file: {error}"
                        ),
                    },
                };
            }
        };
        let mut output_schema = match tempfile::Builder::new()
            .prefix("signalbox-codex-output-")
            .suffix(".json")
            .tempfile_in(temporary_directory)
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
        let prompt = std::mem::take(&mut translated.prompt);
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
        _cancellation: CancellationSignal,
    ) -> PreparationOutcome<C, Self::Prepared> {
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
        .env_clear()
        .arg("exec")
        .arg("--json")
        .arg("--ephemeral")
        .arg("--ignore-user-config")
        .arg("--ignore-rules")
        .arg("--strict-config")
        .arg("--disable")
        .arg("shell_tool")
        .arg("--disable")
        .arg("unified_exec")
        .arg("--disable")
        .arg("skill_search")
        .arg("--config")
        .arg("project_doc_max_bytes=0")
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
    for name in CODEX_ENVIRONMENT_ALLOWLIST {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    #[cfg(unix)]
    command.process_group(0);

    sink.observe(Observation {
        correlation: prepared.correlation.clone(),
        fact: ObservationFact::SendCommenced,
    });
    let mut child = match command.spawn() {
        Ok(child) => SupervisedChild::new(child),
        Err(error) => {
            return TerminalEvidence::ProvenUnsent(ProvenUnsentEvidence {
                cause: UnsentCause::ConnectFailed(TransportFacts::new(error.to_string())),
            });
        }
    };
    let Some(deadline) = tokio::time::Instant::now().checked_add(prepared.exchange_timeout) else {
        force_kill(&mut child).await;
        return pre_exchange_transport_loss(
            "Codex CLI exchange timeout cannot be represented by the runtime clock",
        );
    };
    let Some(mut stdin) = child.stdin.take() else {
        force_kill(&mut child).await;
        return pre_exchange_transport_loss("spawned Codex process has no stdin");
    };
    let Some(stdout) = child.stdout.take() else {
        force_kill(&mut child).await;
        return pre_exchange_transport_loss("spawned Codex process has no stdout");
    };
    let Some(stderr) = child.stderr.take() else {
        force_kill(&mut child).await;
        return pre_exchange_transport_loss("spawned Codex process has no stderr");
    };
    let stderr_limit = prepared.stderr_limit;
    let mut stderr_task =
        tokio::spawn(async move { read_bounded_output(stderr, stderr_limit).await });
    let mut decoder = EventDecoder::new(
        prepared.correlation,
        prepared.delivery,
        &prepared.translated,
    );
    let mut redacting_sink = RedactingSink::new(sink);
    let input_step = {
        let send_prompt = async {
            stdin.write_all(&prepared.prompt).await?;
            stdin.shutdown().await
        };
        tokio::pin!(send_prompt);
        // The pending work future is polled before the control signals, so an
        // upload result already available in the same poll wins over
        // simultaneous cancellation, matching the work-first rule both
        // execution stages share.
        tokio::select! {
            biased;
            result = &mut send_prompt => InputStep::Written(result),
            () = &mut *cancellation => InputStep::Cancelled,
            () = tokio::time::sleep_until(deadline) => InputStep::TimedOut,
        }
    };
    let input_error = match input_step {
        InputStep::Written(Ok(())) => None,
        InputStep::Written(Err(error)) => Some(error),
        InputStep::Cancelled => {
            interrupt_then_kill(
                &mut child,
                remaining_interrupt_grace(prepared.interrupt_grace, deadline),
            )
            .await;
            abort_stderr_task(&mut stderr_task).await;
            return pre_exchange_boundary_loss(LossCause::CancellationRequested);
        }
        InputStep::TimedOut => {
            force_kill(&mut child).await;
            abort_stderr_task(&mut stderr_task).await;
            return pre_exchange_boundary_loss(LossCause::TimedOut(TransportFacts::new(
                "Codex CLI process exceeded its exchange timeout",
            )));
        }
    };
    drop(stdin);

    let mut stdout = BufReader::new(stdout);
    let mut reaped_status = None;
    let mut deadline_stderr = None;
    loop {
        let terminal_observed = decoder.terminal_observed();
        let next = tokio::select! {
            biased;
            result = read_bounded_line(&mut stdout, prepared.event_limit) => ProcessStep::Line(result),
            () = &mut *cancellation => ProcessStep::Cancelled,
            () = tokio::time::sleep_until(deadline) => ProcessStep::TimedOut,
        };
        match next {
            ProcessStep::Line(Ok(Some(line))) => {
                if let Err(error) = decoder.push(&line, &mut redacting_sink) {
                    let detail = format!("undecodable Codex event: {}", error.into_detail());
                    force_kill(&mut child).await;
                    abort_stderr_task(&mut stderr_task).await;
                    redacting_sink.finish();
                    return decoder.provider_error(&detail);
                }
                if !decoder.terminal_observed()
                    && stdout.buffer().is_empty()
                    && already_fired(cancellation)
                {
                    interrupt_then_kill(
                        &mut child,
                        remaining_interrupt_grace(prepared.interrupt_grace, deadline),
                    )
                    .await;
                    abort_stderr_task(&mut stderr_task).await;
                    redacting_sink.finish();
                    return decoder.boundary_loss(LossCause::CancellationRequested);
                }
                if !decoder.terminal_observed()
                    && stdout.buffer().is_empty()
                    && tokio::time::Instant::now() >= deadline
                {
                    force_kill(&mut child).await;
                    abort_stderr_task(&mut stderr_task).await;
                    redacting_sink.finish();
                    return decoder.boundary_loss(LossCause::TimedOut(TransportFacts::new(
                        "Codex CLI process exceeded its exchange timeout",
                    )));
                }
            }
            ProcessStep::Line(Ok(None)) => break,
            ProcessStep::Line(Err(error)) => {
                force_kill(&mut child).await;
                abort_stderr_task(&mut stderr_task).await;
                redacting_sink.finish();
                return decoder.boundary_loss(LossCause::StreamProtocolViolation {
                    detail: error.to_string(),
                });
            }
            ProcessStep::Cancelled => {
                interrupt_then_kill(
                    &mut child,
                    remaining_interrupt_grace(prepared.interrupt_grace, deadline),
                )
                .await;
                abort_stderr_task(&mut stderr_task).await;
                // A cancellation that arrives after the terminal marker
                // drives cleanup but cannot replace the definitive evidence,
                // exactly as on the stderr wait below.
                if terminal_observed {
                    let evidence = if let Some(error) = input_error {
                        decoder.boundary_loss_unless_provider_failure(
                            incomplete_upload_cause(&error),
                            &redacting_sink,
                        )
                    } else {
                        decoder.finish(&mut redacting_sink)
                    };
                    redacting_sink.finish();
                    return evidence;
                }
                redacting_sink.finish();
                return decoder.boundary_loss(LossCause::CancellationRequested);
            }
            ProcessStep::TimedOut => {
                // An inherited stdout handle can outlive a leader that
                // already exited on its own; that exit stays definitive —
                // an observed terminal marker or the exit status — and only
                // a leader that cleanup itself had to kill becomes typed
                // timeout loss.
                let process_group_id = child.id();
                let exited_before_cleanup = leader_exited_without_reaping(process_group_id);
                kill_process_group(process_group_id);
                match child.try_wait() {
                    Ok(Some(status))
                        if exited_before_cleanup || !was_killed_by_group_cleanup(&status) =>
                    {
                        child.disarm();
                        abort_stderr_task(&mut stderr_task).await;
                        reaped_status = Some(Ok(status));
                        deadline_stderr = Some(
                            "Codex stderr was unavailable at the process-cleanup deadline"
                                .to_string(),
                        );
                        break;
                    }
                    Ok(Some(_)) | Ok(None) | Err(_) => {
                        force_kill(&mut child).await;
                        abort_stderr_task(&mut stderr_task).await;
                        redacting_sink.finish();
                        return decoder.boundary_loss(timeout_cause());
                    }
                }
            }
        }
    }

    let terminal_observed = decoder.terminal_observed();
    let stderr = if let Some(stderr) = deadline_stderr {
        stderr
    } else {
        tokio::select! {
        biased;
        result = &mut stderr_task => stderr_result(result),
        () = &mut *cancellation => {
            if !terminal_observed {
                interrupt_then_kill(
                    &mut child,
                    remaining_interrupt_grace(prepared.interrupt_grace, deadline),
                )
                .await;
                abort_stderr_task(&mut stderr_task).await;
                redacting_sink.finish();
                return decoder.boundary_loss(LossCause::CancellationRequested);
            }
            let cleanup_grace = remaining_interrupt_grace(prepared.interrupt_grace, deadline);
            interrupt_then_kill(&mut child, cleanup_grace).await;
            abort_stderr_task(&mut stderr_task).await;
            // Cancellation does not launder an incomplete request upload: a
            // nominal completion still demotes to boundary loss exactly as on
            // the normal exit path below, because the adapter cannot prove the
            // full authorized frontier reached the CLI.
            let evidence = if let Some(error) = input_error {
                decoder.boundary_loss_unless_provider_failure(
                    incomplete_upload_cause(&error),
                    &redacting_sink,
                )
            } else {
                decoder.finish(&mut redacting_sink)
            };
            redacting_sink.finish();
            return evidence;
        },
        () = tokio::time::sleep_until(deadline) => {
            let process_group_id = child.id();
            // A leader that already exited on its own — even by a kill
            // signal, as under an out-of-memory kill — is observed on the
            // still-waitable identity before cleanup signals the group, so a
            // pre-existing signal exit stays distinguishable from cleanup.
            let exited_before_cleanup = leader_exited_without_reaping(process_group_id);
            // `try_wait` reaps an exited group leader. Signal while that
            // leader still pins the process-group identity, so a disappearing
            // last descendant cannot make the numeric id reusable first.
            kill_process_group(process_group_id);
            match child.try_wait() {
                Ok(Some(status))
                    if exited_before_cleanup || !was_killed_by_group_cleanup(&status) =>
                {
                    child.disarm();
                    abort_stderr_task(&mut stderr_task).await;
                    reaped_status = Some(Ok(status));
                    "Codex stderr was unavailable at the process-cleanup deadline".to_string()
                }
                Ok(Some(_)) | Ok(None) | Err(_) => {
                    force_kill(&mut child).await;
                    abort_stderr_task(&mut stderr_task).await;
                    redacting_sink.finish();
                    return decoder.boundary_loss(timeout_cause());
                }
            }
        },
        }
    };
    let status = match reaped_status {
        Some(status) => status,
        None => {
            // The blocking waiter cannot be ready on its first poll — it must
            // schedule onto the blocking pool — so an already-fired control
            // signal would win the select below even though the leader's
            // definitive status is already waitable. Probe synchronously
            // first, so an already-exited leader keeps its exit evidence.
            let exit_ready = if leader_exited_without_reaping(child.id()) {
                Ok(())
            } else {
                let exit_wait = wait_for_exit_without_reaping(child.id());
                tokio::pin!(exit_wait);
                tokio::select! {
                    biased;
                    result = &mut exit_wait => result,
                    () = &mut *cancellation, if !terminal_observed => {
                        interrupt_then_kill(
                            &mut child,
                            remaining_interrupt_grace(prepared.interrupt_grace, deadline),
                        )
                        .await;
                        redacting_sink.finish();
                        return decoder.boundary_loss(LossCause::CancellationRequested);
                    },
                    () = tokio::time::sleep_until(deadline) => {
                        force_kill(&mut child).await;
                        redacting_sink.finish();
                        return decoder.boundary_loss(timeout_cause());
                    },
                }
            };
            if let Err(error) = exit_ready {
                force_kill(&mut child).await;
                redacting_sink.finish();
                return decoder.boundary_loss(LossCause::TransportFailed(TransportFacts::new(
                    format!("could not observe Codex CLI process exit safely: {error}"),
                )));
            }
            // The exited leader remains waitable, so its process identity
            // cannot be reused while the original process group is signaled.
            // Reap it only after every surviving descendant has been killed.
            kill_process_group(child.id());
            let status = child.wait().await;
            if status.is_ok() {
                child.disarm();
            }
            status
        }
    };

    match status {
        Ok(status) if status.success() => {
            let evidence = if let Some(error) = input_error {
                decoder.boundary_loss_unless_provider_failure(
                    incomplete_upload_cause(&error),
                    &redacting_sink,
                )
            } else {
                decoder.finish(&mut redacting_sink)
            };
            redacting_sink.finish();
            evidence
        }
        Ok(status) => {
            let message = if !stderr.trim().is_empty() {
                format!("Codex CLI exited with status {status}: {stderr}")
            } else if let Some(error) = input_error {
                format!("Codex CLI exited with status {status} after stdin failed: {error}")
            } else {
                format!("Codex CLI exited with status {status}")
            };
            // Evidence is built before the sink flushes so the failure
            // message still sees the held cross-fragment redaction state.
            let evidence = decoder.provider_error_after_exit(&message, &redacting_sink);
            redacting_sink.finish();
            evidence
        }
        Err(error) => {
            redacting_sink.finish();
            decoder.boundary_loss(LossCause::TransportFailed(TransportFacts::new(format!(
                "could not wait for Codex CLI process: {error}"
            ))))
        }
    }
}

fn incomplete_upload_cause(error: &std::io::Error) -> LossCause {
    LossCause::TransportFailed(TransportFacts::new(format!(
        "Codex stdin closed before the full request upload completed: {error}"
    )))
}

fn stderr_result(result: Result<std::io::Result<String>, tokio::task::JoinError>) -> String {
    match result {
        Ok(Ok(stderr)) => stderr,
        Ok(Err(error)) => format!("could not read Codex stderr: {error}"),
        Err(error) => format!("Codex stderr reader failed: {error}"),
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

async fn interrupt_then_kill(child: &mut SupervisedChild, grace: Duration) {
    #[cfg(unix)]
    {
        let process_group_id = child.id();
        if let Some(raw_pid) = process_group_id
            && let Some(pid) = rustix::process::Pid::from_raw(raw_pid as i32)
        {
            let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::INT);
            tokio::time::sleep(grace).await;
            kill_process_group(process_group_id);
            let _ = child.start_kill();
            if child.wait().await.is_ok() {
                child.disarm();
            }
            return;
        }
    }
    force_kill(child).await;
}

fn remaining_interrupt_grace(grace: Duration, deadline: tokio::time::Instant) -> Duration {
    grace.min(deadline.saturating_duration_since(tokio::time::Instant::now()))
}

async fn force_kill(child: &mut SupervisedChild) {
    kill_process_group(child.id());
    let _ = child.start_kill();
    if child.wait().await.is_ok() {
        child.disarm();
    }
}

fn kill_process_group(process_group_id: Option<u32>) {
    #[cfg(unix)]
    if let Some(raw_pid) = process_group_id
        && let Some(pid) = rustix::process::Pid::from_raw(raw_pid as i32)
    {
        let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
    }
    #[cfg(not(unix))]
    let _ = process_group_id;
}

fn was_killed_by_group_cleanup(status: &std::process::ExitStatus) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;

        status.signal() == Some(rustix::process::Signal::KILL.as_raw())
    }
    #[cfg(not(unix))]
    {
        let _ = status;
        false
    }
}

/// Reports whether the group leader has already exited, without reaping it
/// and without blocking: a `NOHANG`/`NOWAIT` probe of the still-waitable
/// identity. `false` on probe failure or on platforms without the probe, so
/// classification falls back to the conservative cleanup-caused reading.
#[cfg(all(
    unix,
    not(any(
        target_os = "cygwin",
        target_os = "horizon",
        target_os = "openbsd",
        target_os = "redox",
        target_os = "wasi"
    ))
))]
fn leader_exited_without_reaping(process_group_id: Option<u32>) -> bool {
    let Some(raw_pid) = process_group_id else {
        return false;
    };
    let Some(pid) = rustix::process::Pid::from_raw(raw_pid as i32) else {
        return false;
    };
    rustix::process::waitid(
        rustix::process::WaitId::Pid(pid),
        rustix::process::WaitIdOptions::EXITED
            | rustix::process::WaitIdOptions::NOWAIT
            | rustix::process::WaitIdOptions::NOHANG,
    )
    .ok()
    .flatten()
    .is_some()
}

#[cfg(not(all(
    unix,
    not(any(
        target_os = "cygwin",
        target_os = "horizon",
        target_os = "openbsd",
        target_os = "redox",
        target_os = "wasi"
    ))
)))]
fn leader_exited_without_reaping(_process_group_id: Option<u32>) -> bool {
    false
}

#[cfg(all(
    unix,
    not(any(
        target_os = "cygwin",
        target_os = "horizon",
        target_os = "openbsd",
        target_os = "redox",
        target_os = "wasi"
    ))
))]
async fn wait_for_exit_without_reaping(process_group_id: Option<u32>) -> std::io::Result<()> {
    let raw_pid = process_group_id.ok_or_else(|| {
        std::io::Error::other("spawned Codex process has no process-group identity")
    })?;
    let pid = rustix::process::Pid::from_raw(raw_pid as i32).ok_or_else(|| {
        std::io::Error::other("spawned Codex process has an invalid process-group identity")
    })?;
    tokio::task::spawn_blocking(move || {
        let status = rustix::process::waitid(
            rustix::process::WaitId::Pid(pid),
            rustix::process::WaitIdOptions::EXITED | rustix::process::WaitIdOptions::NOWAIT,
        )
        .map_err(std::io::Error::from)?;
        status
            .map(drop)
            .ok_or_else(|| std::io::Error::other("Codex process exit was not observable"))
    })
    .await
    .map_err(|error| std::io::Error::other(format!("Codex exit waiter failed: {error}")))?
}

#[cfg(not(all(
    unix,
    not(any(
        target_os = "cygwin",
        target_os = "horizon",
        target_os = "openbsd",
        target_os = "redox",
        target_os = "wasi"
    ))
)))]
async fn wait_for_exit_without_reaping(_process_group_id: Option<u32>) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "non-reaping process wait is unavailable",
    ))
}

async fn abort_stderr_task(stderr_task: &mut tokio::task::JoinHandle<std::io::Result<String>>) {
    stderr_task.abort();
    let _ = stderr_task.await;
}

fn pre_exchange_transport_loss(detail: impl Into<String>) -> TerminalEvidence {
    pre_exchange_boundary_loss(LossCause::TransportFailed(TransportFacts::new(detail)))
}

fn timeout_cause() -> LossCause {
    LossCause::TimedOut(TransportFacts::new(
        "Codex CLI process exceeded its exchange timeout",
    ))
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
