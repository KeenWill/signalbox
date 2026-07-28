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
    let environment = match allowlisted_environment(|name| std::env::var_os(name)) {
        Ok(environment) => environment,
        Err(name) => {
            // Nothing was sent: the rejection precedes `SendCommenced` and the
            // spawn itself. The diagnostic names only the variable, never its
            // value.
            return TerminalEvidence::ProvenUnsent(ProvenUnsentEvidence {
                cause: UnsentCause::ConnectFailed(TransportFacts::new(format!(
                    "inherited `{name}` embeds URL userinfo; the Codex CLI would receive \
                     that credential verbatim and could reflect it in output the adapter \
                     can only shape-redact, so the exchange is refused — remove the \
                     credential from the proxy URL"
                ))),
            });
        }
    };
    for (name, value) in environment {
        command.env(name, value);
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
            // Work-first: if the leader already exited on its own while a
            // descendant kept the upload blocked, its definitive status
            // outranks upload cancellation. Continue through stdout and exit
            // classification, treating the interrupted upload as incomplete.
            if leader_exited_without_reaping(child.id()) {
                Some(std::io::Error::other(
                    "request upload cancelled after the Codex CLI leader had exited",
                ))
            } else {
                interrupt_then_kill(
                    &mut child,
                    remaining_interrupt_grace(prepared.interrupt_grace, deadline),
                )
                .await;
                abort_stderr_task(&mut stderr_task).await;
                return pre_exchange_boundary_loss(LossCause::CancellationRequested);
            }
        }
        InputStep::TimedOut => {
            // Work-first, mirroring the cancellation arm: a leader that
            // already exited on its own while a descendant kept the upload
            // blocked has a definitive, waitable status; force-killing here
            // would launder a provider rejection — or a completed turn with
            // an incomplete upload — into timeout loss. Continue through
            // stdout and exit classification with the upload recorded as
            // incomplete.
            if leader_exited_without_reaping(child.id()) {
                Some(std::io::Error::other(
                    "request upload timed out after the Codex CLI leader had exited",
                ))
            } else {
                force_kill(&mut child).await;
                abort_stderr_task(&mut stderr_task).await;
                return pre_exchange_boundary_loss(LossCause::TimedOut(TransportFacts::new(
                    "Codex CLI process exceeded its exchange timeout",
                )));
            }
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
                    // Serde details can quote provider-controlled bytes, so
                    // the detail consults the held lookbehind state before
                    // the sink flushes, exactly as failure messages do: a
                    // detail that extends a held credential marker is
                    // suppressed whole.
                    let detail = redacting_sink.redact_terminal_failure_text(&format!(
                        "undecodable Codex event: {}",
                        error.into_detail()
                    ));
                    force_kill(&mut child).await;
                    abort_stderr_task(&mut stderr_task).await;
                    redacting_sink.finish();
                    return decoder.provider_error(&detail);
                }
                if !decoder.terminal_observed()
                    && stdout.buffer().is_empty()
                    && already_fired(cancellation)
                {
                    // Work-first: an already-exited leader's status is
                    // definitive and outranks a simultaneous cancellation.
                    if let Some((status, detail)) =
                        reap_exited_leader(&mut child, &mut stderr_task).await
                    {
                        reaped_status = Some(status);
                        deadline_stderr = Some(detail);
                        break;
                    }
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
                // Work-first: a leader that has already exited on its own is
                // definitive evidence a simultaneous cancellation must not
                // discard — even after a terminal marker, because a nonzero
                // exit must classify as a provider error rather than let
                // cancellation launder it into the recorded completion. Probe
                // without reaping and, if it exited, hand the status to the
                // exit-classification path below.
                if let Some((status, detail)) =
                    reap_exited_leader(&mut child, &mut stderr_task).await
                {
                    reaped_status = Some(status);
                    deadline_stderr = Some(detail);
                    break;
                }
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
                        // A leader that wrote and closed stderr before exiting,
                        // or a descendant whose stderr write end the group kill
                        // just closed, leaves classifiable failure text buffered
                        // in the reader. Await it under the bounded drain so a
                        // credential rejection or quota failure keeps its typed
                        // kind instead of degrading to the synthetic cleanup
                        // message; `is_finished()` is not yet true right after
                        // the kill, so aborting here would discard that text.
                        let stderr_detail = drain_stderr_after_cleanup(
                            &mut stderr_task,
                            "Codex stderr was unavailable at the process-cleanup deadline",
                        )
                        .await;
                        reaped_status = Some(Ok(status));
                        deadline_stderr = Some(stderr_detail);
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
            // Probe first: an already-exited leader's status is definitive and
            // outranks a cancellation even after a terminal marker or while a
            // descendant holds stderr open — a nonzero exit must classify as a
            // provider error rather than let cancellation launder a failed
            // invocation into the recorded completion.
            if let Some((status, detail)) =
                reap_exited_leader(&mut child, &mut stderr_task).await
            {
                reaped_status = Some(status);
                detail
            } else if !terminal_observed {
                interrupt_then_kill(
                    &mut child,
                    remaining_interrupt_grace(prepared.interrupt_grace, deadline),
                )
                .await;
                abort_stderr_task(&mut stderr_task).await;
                redacting_sink.finish();
                return decoder.boundary_loss(LossCause::CancellationRequested);
            } else {
                let cleanup_grace = remaining_interrupt_grace(prepared.interrupt_grace, deadline);
                interrupt_then_kill(&mut child, cleanup_grace).await;
                abort_stderr_task(&mut stderr_task).await;
                // Cancellation does not launder an incomplete request upload: a
                // nominal completion still demotes to boundary loss exactly as
                // on the normal exit path below, because the adapter cannot
                // prove the full authorized frontier reached the CLI.
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
                    reaped_status = Some(Ok(status));
                    // Same bounded drain as the exit-wait deadline: a descendant
                    // whose stderr the group kill just closed may still hold
                    // buffered classifiable text, so await the reader instead of
                    // aborting it and losing a typed provider failure.
                    drain_stderr_after_cleanup(
                        &mut stderr_task,
                        "Codex stderr was unavailable at the process-cleanup deadline",
                    )
                    .await
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
                    () = &mut *cancellation => {
                        // Re-probe: the leader may have exited nonzero while the
                        // blocking waiter was scheduling; preserve that
                        // definitive status through classification below rather
                        // than laundering it via cancellation cleanup.
                        if leader_exited_without_reaping(child.id()) {
                            Ok(())
                        } else {
                            interrupt_then_kill(
                                &mut child,
                                remaining_interrupt_grace(prepared.interrupt_grace, deadline),
                            )
                            .await;
                            // A cancellation after a terminal marker drives
                            // cleanup but cannot replace the definitive
                            // evidence, exactly as on the stdout and stderr
                            // waits above.
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
                    },
                    () = tokio::time::sleep_until(deadline) => {
                        // Re-probe before timeout cleanup so a leader that
                        // exited while the waiter scheduled keeps its status.
                        if leader_exited_without_reaping(child.id()) {
                            Ok(())
                        } else {
                            force_kill(&mut child).await;
                            redacting_sink.finish();
                            return decoder.boundary_loss(timeout_cause());
                        }
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
            // The stderr text consults the held lookbehind state on its own,
            // before any adapter-owned status prose is prefixed: inserted
            // prose between a held credential-marker fragment and its stderr
            // continuation would otherwise keep the pair from rejoining, and
            // the continuation would survive the stateless stderr redaction.
            let stderr_detail = redacting_sink.redact_terminal_failure_text(&stderr);
            // The emitted message carries only sanitized stderr; the failure
            // is classified from the bounded raw stderr so an explicit error
            // phrase sharing a line with a consumed credential marker still
            // reaches the classifier.
            let (message, classification) = if !stderr_detail.trim().is_empty() {
                (
                    format!("Codex CLI exited with status {status}: {stderr_detail}"),
                    format!("Codex CLI exited with status {status}: {}", stderr.trim()),
                )
            } else if let Some(error) = input_error {
                let message =
                    format!("Codex CLI exited with status {status} after stdin failed: {error}");
                (message.clone(), message)
            } else {
                let message = format!("Codex CLI exited with status {status}");
                (message.clone(), message)
            };
            // Evidence is built before the sink flushes so the failure
            // message still sees the held cross-fragment redaction state.
            let evidence =
                decoder.provider_error_after_exit(&message, &classification, &redacting_sink);
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

/// `CODEX_HOME` names the CLI's login store — with `HOME` supplying its
/// `$HOME/.codex` fallback — and the child interprets both after
/// `current_dir` moves it to the configured working root, so a relative
/// operator value would silently re-root the credential store beneath the
/// working directory. Absolutize both against the parent's own current
/// directory; every other allowlisted value passes through unchanged, and an
/// unabsolutizable value (for example an empty one) is passed verbatim for
/// the CLI itself to reject.
fn spawn_environment_value(name: &str, value: std::ffi::OsString) -> std::ffi::OsString {
    if name != "CODEX_HOME" && name != "HOME" {
        return value;
    }
    match std::path::absolute(std::path::Path::new(&value)) {
        Ok(path) => path.into_os_string(),
        Err(_) => value,
    }
}

/// The allowlisted variables whose values are proxy URLs. `NO_PROXY` is a
/// host list, never a URL with an authority, so it is not checked for
/// userinfo.
const PROXY_URL_VARIABLES: &[&str] = &[
    "ALL_PROXY",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "all_proxy",
    "http_proxy",
    "https_proxy",
];

/// Assembles the allowlisted child environment through the injectable `read`,
/// rejecting (with the offending variable's name, never its value) a proxy
/// URL that embeds userinfo. Such a credential would transit to the child
/// verbatim, and a CLI that reflects its proxy configuration in an error or
/// event would hand the password to output the adapter can only shape-redact
/// — `redact_text` has no proxy-userinfo rule, so the value must never reach
/// the child at all (INV-035).
fn allowlisted_environment(
    read: impl Fn(&str) -> Option<std::ffi::OsString>,
) -> Result<Vec<(&'static str, std::ffi::OsString)>, &'static str> {
    let mut environment = Vec::new();
    for name in CODEX_ENVIRONMENT_ALLOWLIST {
        if let Some(value) = read(name) {
            if proxy_value_carries_userinfo(name, &value) {
                return Err(name);
            }
            environment.push((*name, spawn_environment_value(name, value)));
        }
    }
    Ok(environment)
}

/// Whether a proxy variable's value embeds URL userinfo
/// (`scheme://user:secret@host`). The authority is the span after any
/// `scheme://` up to the first `/`, `?`, or `#`; userinfo is an `@` inside
/// it. A value that cannot be read as UTF-8 cannot be verified
/// credential-free and fails closed.
fn proxy_value_carries_userinfo(name: &str, value: &std::ffi::OsStr) -> bool {
    if !PROXY_URL_VARIABLES.contains(&name) {
        return false;
    }
    let Some(text) = value.to_str() else {
        return true;
    };
    let after_scheme = text.split_once("://").map_or(text, |(_, rest)| rest);
    let authority = after_scheme.split(['/', '?', '#']).next().unwrap_or("");
    authority.contains('@')
}

/// Reaps a leader that has already exited on its own, returning its status and
/// stderr detail so a cancellation racing that exit hands the definitive
/// provider-error evidence to the exit-classification path instead of
/// reporting cancellation loss. `None` when the leader is still running.
async fn reap_exited_leader(
    child: &mut SupervisedChild,
    stderr_task: &mut tokio::task::JoinHandle<std::io::Result<String>>,
) -> Option<(std::io::Result<std::process::ExitStatus>, String)> {
    if !leader_exited_without_reaping(child.id()) {
        return None;
    }
    kill_process_group(child.id());
    let Ok(Some(status)) = child.try_wait() else {
        return None;
    };
    child.disarm();
    let detail = drain_stderr_after_cleanup(
        stderr_task,
        "Codex stderr was unavailable after cancellation",
    )
    .await;
    Some((Ok(status), detail))
}

/// Await a stderr reader after the process group has been killed, under a short
/// bound.
///
/// Killing the group closes the descendants' stderr write ends, so the reader
/// reaches EOF and finishes with its already-buffered classifiable failure
/// text. Await it — rather than aborting an `is_finished()`-not-yet reader,
/// which drops that buffered text and degrades a recognizable provider failure
/// (a credential rejection, a quota exhaustion) to `Unrecognized`. Only a reader
/// still held open past the bound — a descendant that ignored the kill — is
/// aborted and reported with `unavailable_message`.
async fn drain_stderr_after_cleanup(
    stderr_task: &mut tokio::task::JoinHandle<std::io::Result<String>>,
    unavailable_message: &str,
) -> String {
    match tokio::time::timeout(POST_KILL_REAP_BOUND, &mut *stderr_task).await {
        Ok(result) => stderr_result(result),
        Err(_) => {
            abort_stderr_task(stderr_task).await;
            unavailable_message.to_string()
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
    // Raw bounded stderr is retained so its exit-status classification sees an
    // error phrase that credential-marker redaction would consume; the sole
    // exposure point statefully sanitizes it before it leaves the adapter.
    Ok(output)
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
            // Bounded like `force_kill`: a leader stuck in uninterruptible
            // kernel I/O after SIGKILL is left to its drop guard rather than
            // hanging the exchange past its deadline.
            if let Ok(Ok(_)) = tokio::time::timeout(POST_KILL_REAP_BOUND, child.wait()).await {
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

/// After a group kill the leader is normally waitable at once; bound the reap
/// so a leader stuck in uninterruptible kernel I/O cannot hang the exchange
/// past its deadline. On timeout the child is left for its drop guard, which
/// re-signals the group, rather than blocking indefinitely.
const POST_KILL_REAP_BOUND: Duration = Duration::from_secs(5);

async fn force_kill(child: &mut SupervisedChild) {
    kill_process_group(child.id());
    let _ = child.start_kill();
    if let Ok(Ok(_)) = tokio::time::timeout(POST_KILL_REAP_BOUND, child.wait()).await {
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
    use super::{allowlisted_environment, read_bounded_line, spawn_environment_value};

    #[test]
    fn codex_home_is_absolutized_for_the_child() {
        let value =
            spawn_environment_value("CODEX_HOME", std::ffi::OsString::from("relative-home"));

        assert!(std::path::Path::new(&value).is_absolute());
    }

    #[test]
    fn fallback_home_is_absolutized_for_the_child() {
        let value = spawn_environment_value("HOME", std::ffi::OsString::from("relative-home"));

        assert!(std::path::Path::new(&value).is_absolute());
    }

    #[test]
    fn other_allowlisted_environment_values_pass_through_unchanged() {
        let value = spawn_environment_value("PATH", std::ffi::OsString::from("relative:paths"));

        assert_eq!(value, std::ffi::OsString::from("relative:paths"));
    }

    /// Assembles the environment with exactly `name=value` set and asserts
    /// the assembly refuses it by name, keeping each named case a
    /// straight-line invocation.
    #[track_caller]
    fn assert_environment_rejects(name: &'static str, value: &str) {
        let result = allowlisted_environment(|queried| {
            (queried == name).then(|| std::ffi::OsString::from(value))
        });

        assert_eq!(result, Err(name), "`{name}={value}` must be rejected");
    }

    /// Assembles the environment with exactly `name=value` set and asserts
    /// it passes through unchanged.
    #[track_caller]
    fn assert_environment_passes(name: &'static str, value: &str) {
        let result = allowlisted_environment(|queried| {
            (queried == name).then(|| std::ffi::OsString::from(value))
        });

        assert_eq!(
            result,
            Ok(vec![(name, std::ffi::OsString::from(value))]),
            "`{name}={value}` must pass through"
        );
    }

    /// INV-035: a proxy URL embedding authority userinfo is refused by name.
    #[test]
    fn credential_bearing_proxy_url_is_rejected() {
        assert_environment_rejects(
            "HTTP_PROXY",
            "http://alice:opaque-proxy-value@proxy.internal:8080",
        );
    }

    /// INV-035: username-only userinfo is still an inherited secret shape.
    #[test]
    fn password_less_proxy_userinfo_is_rejected() {
        assert_environment_rejects("HTTPS_PROXY", "https://alice@proxy.internal");
    }

    /// INV-035: a schemeless proxy value with userinfo is refused too.
    #[test]
    fn schemeless_proxy_userinfo_is_rejected() {
        assert_environment_rejects("ALL_PROXY", "alice:opaque-proxy-value@proxy.internal:8080");
    }

    /// INV-035: the lowercase variable spellings share the refusal.
    #[test]
    fn lowercase_proxy_userinfo_is_rejected() {
        assert_environment_rejects(
            "http_proxy",
            "socks5://alice:opaque-proxy-value@proxy.internal",
        );
    }

    /// A credential-free proxy URL passes through unchanged.
    #[test]
    fn credential_free_proxy_url_passes_through() {
        assert_environment_passes("HTTP_PROXY", "http://proxy.internal:8080");
    }

    /// An `@` confined to the URL path is not authority userinfo.
    #[test]
    fn path_confined_at_sign_passes_through() {
        assert_environment_passes("HTTPS_PROXY", "https://proxy.internal/path/we@ird");
    }

    /// `NO_PROXY` is a host list, never a URL with an authority.
    #[test]
    fn no_proxy_host_list_passes_through() {
        assert_environment_passes("NO_PROXY", "internal,@odd-but-not-a-url");
    }

    /// Non-proxy allowlisted variables are not URL-checked.
    #[test]
    fn non_proxy_variables_are_not_url_checked() {
        assert_environment_passes("TERM", "user:secret@not-a-proxy-variable");
    }

    /// A proxy value that is not UTF-8 cannot be verified credential-free
    /// and fails closed.
    #[cfg(unix)]
    #[test]
    fn non_utf8_proxy_value_is_rejected() {
        use std::os::unix::ffi::OsStringExt;

        let value = std::ffi::OsString::from_vec(vec![0x68, 0x74, 0xff, 0xfe]);
        let result =
            allowlisted_environment(|queried| (queried == "HTTP_PROXY").then(|| value.clone()));

        assert_eq!(result, Err("HTTP_PROXY"));
    }

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
